//! Ratatui terminal lifecycle support for Latte Code's transcript UI.
//!
//! The crate exposes one conversation-first presentation surface in
//! [`thread`].  Terminal state is kept here so the reducer remains pure and
//! every interactive entrypoint gets the same transactional cleanup contract.
#![allow(clippy::missing_errors_doc)]

use std::{
    io, panic,
    sync::{Arc, Mutex},
};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use thiserror::Error;

pub mod command;
pub mod thread;

/// Connectivity is presentation state, never runtime truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
    SnapshotRequired,
}

/// Interactive runner errors.
#[derive(Debug, Error)]
pub enum TuiError {
    #[error(
        "interactive TUI requires a TTY; use --json list/show/run/resume for non-interactive use"
    )]
    NonTty,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("runtime action failed: {0}")]
    Action(String),
    #[error("interrupted")]
    Interrupted,
}

/// RAII terminal session. Restoration is idempotent and also installed for panics.
pub struct TerminalGuard {
    restored: Arc<Mutex<bool>>,
    stages: Arc<Mutex<TerminalStages>>,
    ops: Arc<dyn TerminalOps>,
    previous_hook: Arc<Mutex<Option<PanicHook>>>,
    _terminal_lock: std::sync::MutexGuard<'static, ()>,
    interrupted: Arc<std::sync::atomic::AtomicBool>,
    signal_id: Option<signal_hook::SigId>,
}

static TERMINAL_LOCK: Mutex<()> = Mutex::new(());
type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct TerminalStages {
    raw: bool,
    keyboard: bool,
    alternate: bool,
    mouse: bool,
    paste: bool,
}

trait TerminalOps: Send + Sync {
    fn enable_raw(&self) -> io::Result<()>;
    fn push_keyboard(&self) -> io::Result<()>;
    fn enter_alternate(&self) -> io::Result<()>;
    fn enable_mouse(&self) -> io::Result<()>;
    fn enable_paste(&self) -> io::Result<()>;
    fn disable_paste(&self) -> io::Result<()>;
    fn disable_mouse(&self) -> io::Result<()>;
    fn leave_alternate(&self) -> io::Result<()>;
    fn pop_keyboard(&self) -> io::Result<()>;
    fn disable_raw(&self) -> io::Result<()>;
}

struct CrosstermTerminalOps;

impl TerminalOps for CrosstermTerminalOps {
    fn enable_raw(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn push_keyboard(&self) -> io::Result<()> {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
    }

    fn enter_alternate(&self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn enable_mouse(&self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }

    fn enable_paste(&self) -> io::Result<()> {
        execute!(io::stdout(), crossterm::event::EnableBracketedPaste)
    }

    fn disable_paste(&self) -> io::Result<()> {
        execute!(io::stdout(), crossterm::event::DisableBracketedPaste)
    }

    fn disable_mouse(&self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }

    fn leave_alternate(&self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn pop_keyboard(&self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
    }

    fn disable_raw(&self) -> io::Result<()> {
        disable_raw_mode()
    }
}

/// Observable entry boundary used by PTY fault-injection tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStage {
    SignalRegistered,
    Raw,
    Keyboard,
    Alternate,
    Mouse,
    Paste,
}

/// Observable cleanup boundary used by PTY signal-injection tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCleanupStage {
    Paste,
    Mouse,
    Alternate,
    Keyboard,
    Raw,
}

impl TerminalGuard {
    /// Enters terminal modes transactionally.
    ///
    /// # Panics
    ///
    /// Panics only if the process-local stage mutex is poisoned; the partially-created guard
    /// still rolls back raw/alternate terminal modes during unwinding.
    pub fn enter() -> Result<Self, TuiError> {
        Self::enter_with_hook(|_| {})
    }

    /// Enters with a callback after each committed stage, before interrupt checking.
    ///
    /// # Panics
    ///
    /// Panics if the private stage mutex is poisoned or if the supplied hook panics; the
    /// already-constructed guard still restores every committed stage during unwinding.
    pub fn enter_with_hook(mut hook: impl FnMut(TerminalStage)) -> Result<Self, TuiError> {
        let ops: Arc<dyn TerminalOps> = Arc::new(CrosstermTerminalOps);
        Self::enter_with_hook_and_ops(&mut hook, &ops)
    }

    fn enter_with_hook_and_ops(
        mut hook: impl FnMut(TerminalStage),
        ops: &Arc<dyn TerminalOps>,
    ) -> Result<Self, TuiError> {
        let terminal_lock = TERMINAL_LOCK
            .lock()
            .map_err(|_| io::Error::other("terminal session lock poisoned"))?;
        let restored = Arc::new(Mutex::new(false));
        let stages = Arc::new(Mutex::new(TerminalStages::default()));
        let previous_hook = Arc::new(Mutex::new(None));
        let mut guard = Self {
            restored: Arc::clone(&restored),
            stages: Arc::clone(&stages),
            ops: Arc::clone(ops),
            previous_hook: Arc::clone(&previous_hook),
            _terminal_lock: terminal_lock,
            interrupted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            signal_id: None,
        };
        guard.signal_id = Some(signal_hook::flag::register(
            signal_hook::consts::SIGINT,
            Arc::clone(&guard.interrupted),
        )?);
        hook(TerminalStage::SignalRegistered);
        guard.check_interrupted()?;
        ops.enable_raw()?;
        stages.lock().unwrap().raw = true;
        hook(TerminalStage::Raw);
        guard.check_interrupted()?;
        match ops.push_keyboard() {
            Ok(()) => {
                stages.lock().unwrap().keyboard = true;
                hook(TerminalStage::Keyboard);
                guard.check_interrupted()?;
            }
            // The legacy Windows console does not implement the protocol. A
            // VT terminal that does not understand it safely ignores the CSI
            // sequence and reports success, while a supported terminal uses
            // it to distinguish Shift+Enter from Enter.
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => return Err(error.into()),
        }
        ops.enter_alternate()?;
        stages.lock().unwrap().alternate = true;
        hook(TerminalStage::Alternate);
        guard.check_interrupted()?;
        ops.enable_mouse()?;
        stages.lock().unwrap().mouse = true;
        hook(TerminalStage::Mouse);
        guard.check_interrupted()?;
        ops.enable_paste()?;
        stages.lock().unwrap().paste = true;
        hook(TerminalStage::Paste);
        guard.check_interrupted()?;
        let hook_restored = Arc::clone(&restored);
        let hook_stages = Arc::clone(&stages);
        let hook_ops = Arc::clone(ops);
        let previous = panic::take_hook();
        *previous_hook.lock().unwrap() = Some(previous);
        let hook_previous = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            restore_once(&hook_restored, &hook_stages, &hook_ops);
            if let Ok(previous) = hook_previous.lock()
                && let Some(previous) = previous.as_ref()
            {
                previous(info);
            }
        }));
        Ok(guard)
    }

    fn check_interrupted(&self) -> Result<(), TuiError> {
        if self.interrupted.load(std::sync::atomic::Ordering::SeqCst) {
            Err(TuiError::Interrupted)
        } else {
            Ok(())
        }
    }

    /// Returns whether scoped SIGINT was observed.
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.interrupted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Consumes one scoped SIGINT observation. After terminal entry, the TUI
    /// converts this edge into the same reducer input as Ctrl+C so keyboard
    /// bytes and host-delivered signals share one confirmation contract.
    #[must_use]
    pub fn take_interrupted(&self) -> bool {
        self.interrupted
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Restores committed stages with a callback immediately before each cleanup action.
    pub fn restore_with_hook(&mut self, hook: impl FnMut(TerminalCleanupStage)) {
        restore_once_with_hook(&self.restored, &self.stages, &self.ops, hook);
    }

    #[cfg(test)]
    fn test_guard(flag: Arc<Mutex<bool>>) -> Self {
        let terminal_lock = TERMINAL_LOCK.lock().unwrap();
        Self {
            restored: flag,
            stages: Arc::new(Mutex::new(TerminalStages::default())),
            ops: Arc::new(CrosstermTerminalOps),
            previous_hook: Arc::new(Mutex::new(None)),
            _terminal_lock: terminal_lock,
            interrupted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            signal_id: None,
        }
    }

    #[cfg(test)]
    fn test_signal_guard() -> Self {
        let mut guard = Self::test_guard(Arc::new(Mutex::new(false)));
        guard.signal_id = Some(
            signal_hook::flag::register(
                signal_hook::consts::SIGINT,
                Arc::clone(&guard.interrupted),
            )
            .unwrap(),
        );
        guard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_once(&self.restored, &self.stages, &self.ops);
        if let Some(id) = self.signal_id.take() {
            signal_hook::low_level::unregister(id);
        }
        if let Ok(mut previous) = self.previous_hook.lock()
            && let Some(previous) = previous.take()
        {
            let _ = panic::take_hook();
            panic::set_hook(previous);
        }
    }
}

fn restore_once(
    restored: &Arc<Mutex<bool>>,
    stages: &Arc<Mutex<TerminalStages>>,
    ops: &Arc<dyn TerminalOps>,
) {
    restore_once_with_hook(restored, stages, ops, |_| {});
}

fn restore_once_with_hook(
    restored: &Arc<Mutex<bool>>,
    stages: &Arc<Mutex<TerminalStages>>,
    ops: &Arc<dyn TerminalOps>,
    mut hook: impl FnMut(TerminalCleanupStage),
) {
    let Ok(mut done) = restored.lock() else {
        return;
    };
    if *done {
        return;
    }
    if let Ok(mut stages) = stages.lock() {
        if stages.paste {
            hook(TerminalCleanupStage::Paste);
            let _ = ops.disable_paste();
            stages.paste = false;
        }
        if stages.mouse {
            hook(TerminalCleanupStage::Mouse);
            let _ = ops.disable_mouse();
            stages.mouse = false;
        }
        if stages.alternate {
            hook(TerminalCleanupStage::Alternate);
            let _ = ops.leave_alternate();
            stages.alternate = false;
        }
        if stages.keyboard {
            hook(TerminalCleanupStage::Keyboard);
            let _ = ops.pop_keyboard();
            stages.keyboard = false;
        }
        if stages.raw {
            hook(TerminalCleanupStage::Raw);
            let _ = ops.disable_raw();
            stages.raw = false;
        }
    }
    *done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum MockOperation {
        EnableRaw,
        PushKeyboard,
        EnterAlternate,
        EnableMouse,
        EnablePaste,
        DisablePaste,
        DisableMouse,
        LeaveAlternate,
        PopKeyboard,
        DisableRaw,
    }

    struct MockTerminalOps {
        operations: Arc<Mutex<Vec<MockOperation>>>,
        keyboard_error: Option<io::ErrorKind>,
        mouse_error: Option<io::ErrorKind>,
    }

    impl MockTerminalOps {
        fn record(&self, operation: MockOperation) {
            self.operations.lock().unwrap().push(operation);
        }
    }

    impl TerminalOps for MockTerminalOps {
        fn enable_raw(&self) -> io::Result<()> {
            self.record(MockOperation::EnableRaw);
            Ok(())
        }

        fn push_keyboard(&self) -> io::Result<()> {
            self.record(MockOperation::PushKeyboard);
            self.keyboard_error.map_or(Ok(()), |kind| {
                Err(io::Error::new(kind, "injected keyboard stage failure"))
            })
        }

        fn enter_alternate(&self) -> io::Result<()> {
            self.record(MockOperation::EnterAlternate);
            Ok(())
        }

        fn enable_mouse(&self) -> io::Result<()> {
            self.record(MockOperation::EnableMouse);
            self.mouse_error.map_or(Ok(()), |kind| {
                Err(io::Error::new(kind, "injected mouse stage failure"))
            })
        }

        fn enable_paste(&self) -> io::Result<()> {
            self.record(MockOperation::EnablePaste);
            Ok(())
        }

        fn disable_paste(&self) -> io::Result<()> {
            self.record(MockOperation::DisablePaste);
            Ok(())
        }

        fn disable_mouse(&self) -> io::Result<()> {
            self.record(MockOperation::DisableMouse);
            Ok(())
        }

        fn leave_alternate(&self) -> io::Result<()> {
            self.record(MockOperation::LeaveAlternate);
            Ok(())
        }

        fn pop_keyboard(&self) -> io::Result<()> {
            self.record(MockOperation::PopKeyboard);
            Ok(())
        }

        fn disable_raw(&self) -> io::Result<()> {
            self.record(MockOperation::DisableRaw);
            Ok(())
        }
    }

    #[test]
    fn guard_restoration_is_idempotent() {
        let flag = Arc::new(Mutex::new(false));
        {
            let guard = TerminalGuard::test_guard(Arc::clone(&flag));
            drop(guard);
        }
        assert!(*flag.lock().unwrap());
        let ops: Arc<dyn TerminalOps> = Arc::new(CrosstermTerminalOps);
        restore_once(
            &flag,
            &Arc::new(Mutex::new(TerminalStages::default())),
            &ops,
        );
        assert!(*flag.lock().unwrap());
    }

    #[test]
    fn unsupported_keyboard_enhancement_falls_back_without_pop() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: Some(io::ErrorKind::Unsupported),
            mouse_error: None,
        });
        let mut stages = Vec::new();

        let guard =
            TerminalGuard::enter_with_hook_and_ops(|stage| stages.push(stage), &ops).unwrap();
        drop(guard);

        assert_eq!(
            stages,
            vec![
                TerminalStage::SignalRegistered,
                TerminalStage::Raw,
                TerminalStage::Alternate,
                TerminalStage::Mouse,
                TerminalStage::Paste,
            ]
        );
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::EnableMouse,
                MockOperation::EnablePaste,
                MockOperation::DisablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn successful_keyboard_enhancement_restores_every_mode_in_reverse_order() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });

        let guard = TerminalGuard::enter_with_hook_and_ops(|_| {}, &ops).unwrap();
        drop(guard);

        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::EnableMouse,
                MockOperation::EnablePaste,
                MockOperation::DisablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn keyboard_enhancement_failure_rolls_back_raw_without_pop() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: Some(io::ErrorKind::BrokenPipe),
            mouse_error: None,
        });
        let mut stages = Vec::new();

        let result = TerminalGuard::enter_with_hook_and_ops(|stage| stages.push(stage), &ops);
        let Err(TuiError::Io(error)) = result else {
            panic!("keyboard-stage failure must be returned as I/O");
        };

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            stages,
            vec![TerminalStage::SignalRegistered, TerminalStage::Raw]
        );
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn mouse_capture_failure_rolls_back_alternate_keyboard_and_raw_modes() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: Some(io::ErrorKind::BrokenPipe),
        });
        let mut stages = Vec::new();

        let result = TerminalGuard::enter_with_hook_and_ops(|stage| stages.push(stage), &ops);
        let Err(TuiError::Io(error)) = result else {
            panic!("mouse-stage failure must be returned as I/O");
        };

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            stages,
            vec![
                TerminalStage::SignalRegistered,
                TerminalStage::Raw,
                TerminalStage::Keyboard,
                TerminalStage::Alternate,
            ]
        );
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::EnableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn crossterm_mouse_capture_commands_are_writable() {
        let ops = CrosstermTerminalOps;
        // Ignore failures on hosts without a controlling TTY so the test does
        // not leave the console in a partially-enabled state on Windows.
        let _ = ops.enable_mouse();
        let _ = ops.disable_mouse();
    }

    #[test]
    fn scoped_sigint_is_observed_outside_the_signal_handler() {
        let guard = TerminalGuard::test_signal_guard();
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        for _ in 0..1000 {
            if guard.interrupted() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(guard.interrupted());
    }

    #[test]
    fn scoped_sigint_can_be_consumed_as_an_edge() {
        let guard = TerminalGuard::test_signal_guard();
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        for _ in 0..1000 {
            if guard.interrupted() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(guard.take_interrupted());
        assert!(!guard.take_interrupted());
        assert!(!guard.interrupted());
    }

    #[test]
    fn explicit_restoration_and_interrupt_checks_share_one_idempotent_guard_contract() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });
        let restored = Arc::new(Mutex::new(false));
        let stages = Arc::new(Mutex::new(TerminalStages {
            raw: true,
            keyboard: true,
            alternate: true,
            mouse: true,
            paste: true,
        }));
        let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let terminal_lock = TERMINAL_LOCK.lock().unwrap();
        let mut guard = TerminalGuard {
            restored: Arc::clone(&restored),
            stages,
            ops,
            previous_hook: Arc::new(Mutex::new(None)),
            _terminal_lock: terminal_lock,
            interrupted,
            signal_id: None,
        };

        assert!(matches!(
            guard.check_interrupted(),
            Err(TuiError::Interrupted)
        ));
        assert!(guard.interrupted());
        assert!(guard.take_interrupted());
        assert!(!guard.interrupted());
        assert!(guard.check_interrupted().is_ok());

        let mut cleanup = Vec::new();
        guard.restore_with_hook(|stage| cleanup.push(stage));
        assert_eq!(
            cleanup,
            vec![
                TerminalCleanupStage::Paste,
                TerminalCleanupStage::Mouse,
                TerminalCleanupStage::Alternate,
                TerminalCleanupStage::Keyboard,
                TerminalCleanupStage::Raw,
            ]
        );
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::DisablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
        guard.restore_with_hook(|_| panic!("cleanup must be idempotent"));
        drop(guard);
        assert!(*restored.lock().unwrap());
        assert_eq!(operations.lock().unwrap().len(), 5);
    }

    #[test]
    fn restore_once_with_hook_restores_all_enabled_stages() {
        let restored = Arc::new(Mutex::new(false));
        let stages = Arc::new(Mutex::new(TerminalStages {
            raw: true,
            keyboard: true,
            alternate: true,
            mouse: true,
            paste: true,
        }));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });
        let hooks = Arc::new(Mutex::new(Vec::new()));
        let hooks_clone = Arc::clone(&hooks);
        restore_once_with_hook(&restored, &stages, &ops, move |stage| {
            hooks_clone.lock().unwrap().push(stage);
        });
        assert!(*restored.lock().unwrap());
        // All stages disabled in reverse order.
        assert_eq!(
            operations.lock().unwrap().as_slice(),
            &[
                MockOperation::DisablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
        // Hook called for each stage.
        assert_eq!(hooks.lock().unwrap().len(), 5);
        // All stages cleared.
        let stages = stages.lock().unwrap();
        assert!(!stages.raw);
        assert!(!stages.keyboard);
        assert!(!stages.alternate);
        assert!(!stages.mouse);
        assert!(!stages.paste);
    }

    #[test]
    fn restore_once_with_hook_is_idempotent() {
        let restored = Arc::new(Mutex::new(true)); // Already done.
        let stages = Arc::new(Mutex::new(TerminalStages {
            raw: true,
            keyboard: true,
            alternate: true,
            mouse: true,
            paste: true,
        }));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });
        restore_once_with_hook(&restored, &stages, &ops, |_| {
            panic!("hook must not be called when already done");
        });
        // No operations performed.
        assert_eq!(operations.lock().unwrap().len(), 0);
    }

    struct StageFailureOps {
        operations: Arc<Mutex<Vec<MockOperation>>>,
        fail_raw: bool,
        fail_alternate: bool,
        fail_paste: bool,
    }

    impl TerminalOps for StageFailureOps {
        fn enable_raw(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::EnableRaw);
            if self.fail_raw {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected raw stage failure",
                ));
            }
            Ok(())
        }

        fn push_keyboard(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::PushKeyboard);
            Ok(())
        }

        fn enter_alternate(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::EnterAlternate);
            if self.fail_alternate {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected alternate stage failure",
                ));
            }
            Ok(())
        }

        fn enable_mouse(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::EnableMouse);
            Ok(())
        }

        fn enable_paste(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::EnablePaste);
            if self.fail_paste {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected paste stage failure",
                ));
            }
            Ok(())
        }

        fn disable_paste(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::DisablePaste);
            Ok(())
        }

        fn disable_mouse(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::DisableMouse);
            Ok(())
        }

        fn leave_alternate(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::LeaveAlternate);
            Ok(())
        }

        fn pop_keyboard(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::PopKeyboard);
            Ok(())
        }

        fn disable_raw(&self) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(MockOperation::DisableRaw);
            Ok(())
        }
    }

    #[test]
    fn crossterm_ops_emit_escape_sequences_and_raw_mode_transitions_without_a_tty() {
        let ops = CrosstermTerminalOps;
        // Raw mode requires a controlling TTY; the transition is still
        // exercised here and any failure is ignored because the test host is
        // allowed to lack a terminal.  All results are ignored so that
        // disable_raw() runs even when an intermediate operation fails,
        // preventing the console from being left in raw mode on Windows.
        let _ = ops.enable_raw();
        let _ = ops.push_keyboard();
        let _ = ops.enter_alternate();
        let _ = ops.enable_paste();
        let _ = ops.disable_paste();
        let _ = ops.leave_alternate();
        let _ = ops.pop_keyboard();
        let _ = ops.disable_raw();
    }

    #[test]
    fn enter_without_a_tty_reports_the_io_failure_or_restores_transactionally() {
        let result = TerminalGuard::enter();
        match result {
            // On some platforms (e.g. Windows) enable_raw_mode can succeed
            // without a controlling TTY; the guard must still restore
            // transactionally on drop.
            Ok(guard) => drop(guard),
            Err(TuiError::Io(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn enter_with_hook_without_a_tty_reports_the_io_failure_or_restores_transactionally() {
        let result = TerminalGuard::enter_with_hook(|_| {});
        match result {
            Ok(guard) => drop(guard),
            Err(TuiError::Io(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn raw_stage_failure_aborts_entry_before_any_other_stage() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(StageFailureOps {
            operations: Arc::clone(&operations),
            fail_raw: true,
            fail_alternate: false,
            fail_paste: false,
        });
        let result = TerminalGuard::enter_with_hook_and_ops(|_| {}, &ops);
        let Err(TuiError::Io(error)) = result else {
            panic!("raw-stage failure must be returned as I/O");
        };
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(*operations.lock().unwrap(), vec![MockOperation::EnableRaw]);
    }

    #[test]
    fn alternate_stage_failure_rolls_back_keyboard_and_raw_modes() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(StageFailureOps {
            operations: Arc::clone(&operations),
            fail_raw: false,
            fail_alternate: true,
            fail_paste: false,
        });
        let result = TerminalGuard::enter_with_hook_and_ops(|_| {}, &ops);
        let Err(TuiError::Io(error)) = result else {
            panic!("alternate-stage failure must be returned as I/O");
        };
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn paste_stage_failure_rolls_back_mouse_alternate_keyboard_and_raw_modes() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(StageFailureOps {
            operations: Arc::clone(&operations),
            fail_raw: false,
            fail_alternate: false,
            fail_paste: true,
        });
        let result = TerminalGuard::enter_with_hook_and_ops(|_| {}, &ops);
        let Err(TuiError::Io(error)) = result else {
            panic!("paste-stage failure must be returned as I/O");
        };
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::EnableMouse,
                MockOperation::EnablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }

    #[test]
    fn restore_once_is_safe_when_the_restored_flag_is_poisoned() {
        let restored = Arc::new(Mutex::new(false));
        let poisoned = Arc::clone(&restored);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison the restored flag on purpose");
        })
        .join();
        assert!(restored.is_poisoned());
        let stages = Arc::new(Mutex::new(TerminalStages::default()));
        let ops: Arc<dyn TerminalOps> = Arc::new(CrosstermTerminalOps);
        restore_once(&restored, &stages, &ops);
    }

    #[test]
    fn restore_once_marks_done_when_stages_are_poisoned() {
        let restored = Arc::new(Mutex::new(false));
        let stages = Arc::new(Mutex::new(TerminalStages {
            raw: true,
            ..TerminalStages::default()
        }));
        let poisoned = Arc::clone(&stages);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison the stages on purpose");
        })
        .join();
        assert!(stages.is_poisoned());
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });
        restore_once(&restored, &stages, &ops);
        assert!(*restored.lock().unwrap());
        assert!(operations.lock().unwrap().is_empty());
    }

    #[test]
    fn panic_hook_restores_terminal_and_chains_to_the_previous_hook() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let ops: Arc<dyn TerminalOps> = Arc::new(MockTerminalOps {
            operations: Arc::clone(&operations),
            keyboard_error: None,
            mouse_error: None,
        });
        let previous_observed = Arc::new(Mutex::new(false));
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new({
            let previous_observed = Arc::clone(&previous_observed);
            move |_info| {
                *previous_observed.lock().unwrap() = true;
            }
        }));
        let guard = TerminalGuard::enter_with_hook_and_ops(|_| {}, &ops).unwrap();
        // Panic on a worker thread so the process-wide TERMINAL_LOCK held by
        // the guard is never poisoned; the installed hook still runs.
        let _ = std::thread::spawn(|| panic!("trigger scoped panic hook")).join();
        drop(guard);
        let _ = panic::take_hook();
        panic::set_hook(original_hook);
        assert!(*previous_observed.lock().unwrap());
        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                MockOperation::EnableRaw,
                MockOperation::PushKeyboard,
                MockOperation::EnterAlternate,
                MockOperation::EnableMouse,
                MockOperation::EnablePaste,
                MockOperation::DisablePaste,
                MockOperation::DisableMouse,
                MockOperation::LeaveAlternate,
                MockOperation::PopKeyboard,
                MockOperation::DisableRaw,
            ]
        );
    }
}
