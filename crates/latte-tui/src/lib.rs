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
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use thiserror::Error;

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
    alternate: bool,
    mouse: bool,
    paste: bool,
}

/// Observable entry boundary used by PTY fault-injection tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStage {
    SignalRegistered,
    Raw,
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
        let terminal_lock = TERMINAL_LOCK
            .lock()
            .map_err(|_| io::Error::other("terminal session lock poisoned"))?;
        let restored = Arc::new(Mutex::new(false));
        let stages = Arc::new(Mutex::new(TerminalStages::default()));
        let previous_hook = Arc::new(Mutex::new(None));
        let mut guard = Self {
            restored: Arc::clone(&restored),
            stages: Arc::clone(&stages),
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
        enable_raw_mode()?;
        stages.lock().unwrap().raw = true;
        hook(TerminalStage::Raw);
        guard.check_interrupted()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        stages.lock().unwrap().alternate = true;
        hook(TerminalStage::Alternate);
        guard.check_interrupted()?;
        execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
        stages.lock().unwrap().mouse = true;
        hook(TerminalStage::Mouse);
        guard.check_interrupted()?;
        execute!(io::stdout(), crossterm::event::EnableBracketedPaste)?;
        stages.lock().unwrap().paste = true;
        hook(TerminalStage::Paste);
        guard.check_interrupted()?;
        let hook_restored = Arc::clone(&restored);
        let hook_stages = Arc::clone(&stages);
        let previous = panic::take_hook();
        *previous_hook.lock().unwrap() = Some(previous);
        let hook_previous = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            restore_once(&hook_restored, &hook_stages);
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

    /// Restores committed stages with a callback immediately before each cleanup action.
    pub fn restore_with_hook(&mut self, hook: impl FnMut(TerminalCleanupStage)) {
        restore_once_with_hook(&self.restored, &self.stages, hook);
    }

    #[cfg(test)]
    fn test_guard(flag: Arc<Mutex<bool>>) -> Self {
        let terminal_lock = TERMINAL_LOCK.lock().unwrap();
        Self {
            restored: flag,
            stages: Arc::new(Mutex::new(TerminalStages::default())),
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
        restore_once(&self.restored, &self.stages);
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

fn restore_once(restored: &Arc<Mutex<bool>>, stages: &Arc<Mutex<TerminalStages>>) {
    restore_once_with_hook(restored, stages, |_| {});
}

fn restore_once_with_hook(
    restored: &Arc<Mutex<bool>>,
    stages: &Arc<Mutex<TerminalStages>>,
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
            let _ = execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
            stages.paste = false;
        }
        if stages.mouse {
            hook(TerminalCleanupStage::Mouse);
            let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
            stages.mouse = false;
        }
        if stages.alternate {
            hook(TerminalCleanupStage::Alternate);
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            stages.alternate = false;
        }
        if stages.raw {
            hook(TerminalCleanupStage::Raw);
            let _ = disable_raw_mode();
            stages.raw = false;
        }
    }
    *done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_restoration_is_idempotent() {
        let flag = Arc::new(Mutex::new(false));
        {
            let guard = TerminalGuard::test_guard(Arc::clone(&flag));
            drop(guard);
        }
        assert!(*flag.lock().unwrap());
        restore_once(&flag, &Arc::new(Mutex::new(TerminalStages::default())));
        assert!(*flag.lock().unwrap());
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
}
