//! Ratatui presentation client for Lattecode's renderer-neutral runtime protocol.
#![allow(clippy::missing_errors_doc)]

use std::{
    io::{self, IsTerminal},
    panic,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use latte_core::{
    EventEnvelope, PermissionDecision, RunId, RunState, RunStatus, RuntimeCommand, RuntimeEvent,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use thiserror::Error;

/// Responsive layout selected only from the current terminal size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
    Safety,
}

/// Which presentation surface receives navigation input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Runs,
    Timeline,
    Details,
    Prompt,
    Permission,
}

/// Connectivity is presentation state, never runtime truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
    SnapshotRequired,
}

/// Renderer-neutral input to the pure reducer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiInput {
    Key(KeyEvent),
    Resize(u16, u16),
    Snapshot(Vec<RunState>),
    Event(EventEnvelope),
    Lagged,
    Connected,
    Disconnected,
    UnknownEffect { run_id: RunId, effect_id: String },
    CommandError(String),
    CommandCompleted(String),
    Tick,
}

/// Effect requested by the reducer. The caller alone may dispatch commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    Command(RuntimeCommand),
    RefreshSnapshots,
    Quit,
    ReconcileUnknown { run_id: RunId, effect_id: String },
}

/// Abstract authoritative projection boundary implemented outside this crate.
pub trait ProjectionClient {
    fn snapshots(&mut self) -> Result<Vec<RunState>, String>;
    fn poll(&mut self) -> ProjectionPoll;
}

/// Non-blocking projection notification. Events are wake-ups; snapshots remain authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionPoll {
    Event,
    Empty,
    Lagged(u64),
    Closed,
    Error(String),
}

/// Typed command boundary implemented by a runtime adapter outside this crate.
pub trait CommandSink {
    fn submit(&mut self, action: UiAction) -> Result<(), String>;
}
impl<F: FnMut(UiAction) -> Result<(), String>> CommandSink for F {
    fn submit(&mut self, action: UiAction) -> Result<(), String> {
        self(action)
    }
}

/// Presentation-only state. It contains no engine authority.
#[derive(Clone, Debug)]
pub struct UiModel {
    pub runs: Vec<RunState>,
    pub selected: usize,
    pub focus: Focus,
    pub connection: ConnectionState,
    pub timeline: Vec<String>,
    pub scroll: u16,
    pub prompt: String,
    pub input: String,
    pub help: bool,
    pub size: (u16, u16),
    pub unknown_effects: std::collections::BTreeMap<RunId, String>,
    pub status: String,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            runs: vec![],
            selected: 0,
            focus: Focus::Runs,
            connection: ConnectionState::Connected,
            timeline: vec![],
            scroll: 0,
            prompt: String::new(),
            input: String::new(),
            help: false,
            size: (80, 24),
            unknown_effects: std::collections::BTreeMap::new(),
            status: "Ready".into(),
        }
    }
}

impl UiModel {
    #[must_use]
    pub const fn layout_mode(&self) -> LayoutMode {
        match self.size {
            (w, h) if w < 36 || h < 10 => LayoutMode::Safety,
            (w, _) if w < 72 => LayoutMode::Narrow,
            (w, _) if w < 112 => LayoutMode::Medium,
            _ => LayoutMode::Wide,
        }
    }
    #[must_use]
    pub fn selected_run(&self) -> Option<&RunState> {
        self.runs.get(self.selected)
    }
    #[must_use]
    pub fn authority_enabled(&self) -> bool {
        self.connection == ConnectionState::Connected
            && self
                .selected_run()
                .is_none_or(|run| !self.unknown_effects.contains_key(&run.run_id))
    }
}

/// Pure state reducer. Enter deliberately never approves a permission.
#[allow(clippy::too_many_lines)]
pub fn reduce(model: &mut UiModel, input: UiInput) -> Vec<UiAction> {
    match input {
        UiInput::Resize(w, h) => model.size = (w, h),
        UiInput::Connected => {
            model.connection = ConnectionState::Connected;
            model.status = "Connected".into();
        }
        UiInput::Disconnected => {
            model.connection = ConnectionState::Disconnected;
            model.status = "Disconnected: actions disabled".into();
        }
        UiInput::Lagged => {
            model.connection = ConnectionState::SnapshotRequired;
            model.status = "Event gap: snapshot required".into();
            return vec![UiAction::RefreshSnapshots];
        }
        UiInput::UnknownEffect { run_id, effect_id } => {
            model.unknown_effects.insert(run_id, effect_id);
            model.status = "Unknown effect: retry is unsafe".into();
        }
        UiInput::CommandError(error) => {
            model.status = format!("Command rejected: {error}");
        }
        UiInput::CommandCompleted(message) => model.status = message,
        UiInput::Snapshot(mut runs) => {
            runs.sort_by_key(|r| std::cmp::Reverse(r.revision));
            model.runs = runs;
            model.selected = model.selected.min(model.runs.len().saturating_sub(1));
            model.connection = ConnectionState::Connected;
            model.status = "Snapshot synchronized".into();
        }
        UiInput::Event(envelope) => {
            let Some(run) = model.runs.iter_mut().find(|r| r.run_id == envelope.run_id) else {
                model.connection = ConnectionState::SnapshotRequired;
                return vec![UiAction::RefreshSnapshots];
            };
            if envelope.revision != run.revision.saturating_add(1) {
                model.connection = ConnectionState::SnapshotRequired;
                return vec![UiAction::RefreshSnapshots];
            }
            run.revision = envelope.revision;
            if let RuntimeEvent::StateChanged { status } = envelope.event {
                run.status = status;
            }
            model
                .timeline
                .push(format!("r{} {:?}", envelope.revision, envelope.event));
        }
        UiInput::Key(key) => return reduce_key(model, key),
        UiInput::Tick => {}
    }
    Vec::new()
}

#[allow(clippy::too_many_lines)]
fn reduce_key(model: &mut UiModel, key: KeyEvent) -> Vec<UiAction> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return vec![UiAction::Quit];
    }
    if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
        return vec![UiAction::Quit];
    }
    if key.code == KeyCode::Char('?') {
        model.help = !model.help;
        return vec![];
    }
    if key.code == KeyCode::Tab {
        model.focus = match model.focus {
            Focus::Runs => Focus::Timeline,
            Focus::Timeline => Focus::Details,
            Focus::Details => Focus::Prompt,
            Focus::Prompt | Focus::Permission => Focus::Runs,
        };
        return vec![];
    }
    if key.code == KeyCode::Char('r') && model.connection != ConnectionState::Connected {
        return vec![UiAction::RefreshSnapshots];
    }
    if !model.authority_enabled() {
        if key.code == KeyCode::Char('x')
            && let (Some(run), Some(effect)) = (
                model.selected_run(),
                model
                    .selected_run()
                    .and_then(|run| model.unknown_effects.get(&run.run_id))
                    .cloned(),
            )
        {
            return vec![UiAction::ReconcileUnknown {
                run_id: run.run_id,
                effect_id: effect,
            }];
        }
        return vec![];
    }
    let selected = model.selected_run().cloned();
    if let Some(run) = selected {
        if run.status == RunStatus::WaitingPermission {
            model.focus = Focus::Permission;
            let Some(request) = run.pending_permission.as_ref() else {
                return vec![];
            };
            // Explicit mnemonic keys only. Enter is intentionally a no-op (deny is visual default).
            if key.code == KeyCode::Char('d') {
                return vec![UiAction::Command(RuntimeCommand::ResolvePermission {
                    run_id: run.run_id,
                    request_id: request.request_id.clone(),
                    expected_revision: run.revision,
                    decision: PermissionDecision::Deny,
                })];
            }
            if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return vec![UiAction::Command(RuntimeCommand::ResolvePermission {
                    run_id: run.run_id,
                    request_id: request.request_id.clone(),
                    expected_revision: run.revision,
                    decision: PermissionDecision::Allow,
                })];
            }
        }
        if key.code == KeyCode::Char('c')
            && matches!(
                run.status,
                RunStatus::Queued
                    | RunStatus::Running
                    | RunStatus::WaitingInput
                    | RunStatus::WaitingPermission
            )
        {
            return vec![UiAction::Command(RuntimeCommand::Cancel {
                run_id: run.run_id,
                expected_revision: run.revision,
            })];
        }
        if key.code == KeyCode::Char('R') && run.status == RunStatus::Interrupted {
            return vec![UiAction::Command(RuntimeCommand::Resume {
                run_id: run.run_id,
                expected_revision: run.revision,
            })];
        }
        if run.status == RunStatus::WaitingInput
            && let Some(request) = run.pending_input.as_ref()
        {
            match key.code {
                KeyCode::Esc => {
                    return vec![UiAction::Command(RuntimeCommand::Cancel {
                        run_id: run.run_id,
                        expected_revision: run.revision,
                    })];
                }
                KeyCode::Enter if !model.input.is_empty() => {
                    let value = std::mem::take(&mut model.input);
                    return vec![UiAction::Command(RuntimeCommand::ProvideInput {
                        run_id: run.run_id,
                        request_id: request.request_id.clone(),
                        expected_revision: run.revision,
                        value,
                    })];
                }
                KeyCode::Backspace => {
                    model.input.pop();
                    return vec![];
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && model.input.len() < 16 * 1024 =>
                {
                    model.input.push(ch);
                    return vec![];
                }
                _ => return vec![],
            }
        }
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => model.selected = model.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            model.selected = (model.selected + 1).min(model.runs.len().saturating_sub(1));
        }
        KeyCode::PageUp => model.scroll = model.scroll.saturating_sub(10),
        KeyCode::PageDown => model.scroll = model.scroll.saturating_add(10),
        KeyCode::Backspace if model.focus == Focus::Prompt => {
            model.prompt.pop();
        }
        KeyCode::Char(ch) if model.focus == Focus::Prompt => model.prompt.push(ch),
        KeyCode::Enter if model.focus == Focus::Prompt && !model.prompt.trim().is_empty() => {
            let prompt = std::mem::take(&mut model.prompt);
            return vec![UiAction::Command(RuntimeCommand::Run { prompt })];
        }
        _ => {}
    }
    vec![]
}

/// Draws the model on any Ratatui backend, including `TestBackend`.
pub fn render(frame: &mut Frame<'_>, model: &UiModel) {
    let area = frame.area();
    if model.layout_mode() == LayoutMode::Safety {
        frame.render_widget(
            Paragraph::new("Terminal too small\nResize or press q")
                .block(Block::default().borders(Borders::ALL).title("Lattecode")),
            area,
        );
        return;
    }
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(area);
    let body = match model.layout_mode() {
        LayoutMode::Wide => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(45),
                Constraint::Percentage(30),
            ])
            .split(outer[0]),
        LayoutMode::Medium => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(outer[0]),
        LayoutMode::Narrow | LayoutMode::Safety => Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(outer[0]),
    };
    render_runs(frame, model, body[0]);
    render_detail(frame, model, body[1]);
    if body.len() > 2 {
        render_timeline(frame, model, body[2]);
    }
    let warning = if let Some(effect) = model
        .selected_run()
        .and_then(|run| model.unknown_effects.get(&run.run_id))
    {
        format!("UNKNOWN {effect}: [x] reconcile+abort; retry disabled")
    } else if let Some(run) = model
        .selected_run()
        .filter(|r| r.status == RunStatus::WaitingPermission)
    {
        format!(
            "DENY default | [d] deny | [Ctrl-a] allow | {}",
            run.pending_permission
                .as_ref()
                .map_or("permission", |p| p.description.as_str())
        )
    } else if let Some(run) = model
        .selected_run()
        .filter(|r| r.status == RunStatus::WaitingInput)
    {
        format!(
            "INPUT: {} | Enter submit | Esc cancel | {} bytes",
            run.pending_input
                .as_ref()
                .map_or("input", |p| p.prompt.as_str()),
            model.input.len()
        )
    } else {
        "[?] help [Tab] focus [j/k] select [c] cancel [R] resume [q] quit".into()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{:?} · {}", model.connection, model.status),
                Style::default().fg(if model.connection == ConnectionState::Connected {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            )),
            Line::from(warning),
        ])
        .block(Block::default().borders(Borders::ALL)),
        outer[1],
    );
    if model.help {
        let popup = centered(area, 70, 60);
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(Paragraph::new("Keys\n j/k select  Tab focus  ? help\n d deny  Ctrl-a allow (Enter never approves)\n c cancel  R resume interrupted\n r refresh after disconnect/gap\n x reconcile unknown and abort  q quit").wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title("Help")), popup);
    }
}

fn render_runs(frame: &mut Frame<'_>, model: &UiModel, area: Rect) {
    let items = model
        .runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            ListItem::new(format!(
                "{} {} {:?} r{}",
                if i == model.selected { ">" } else { " " },
                &r.run_id.to_string()[..8],
                r.status,
                r.revision
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Runs")),
        area,
    );
}
fn render_detail(frame: &mut Frame<'_>, model: &UiModel, area: Rect) {
    let text = model.selected_run().map_or_else(
        || "No runs. Focus Prompt and enter a request.".into(),
        |r| {
            format!(
                "Run: {}\nStatus: {:?}\nRevision: {}\nFailure: {}\nHandoff: {}",
                r.run_id,
                r.status,
                r.revision,
                r.failure.as_ref().map_or("-", |f| f.message.as_str()),
                r.handoff.as_ref().map_or("-", |h| h.summary.as_str())
            )
        },
    );
    frame.render_widget(
        Paragraph::new(text)
            .scroll((model.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Details")),
        area,
    );
}
fn render_timeline(frame: &mut Frame<'_>, model: &UiModel, area: Rect) {
    frame.render_widget(
        Paragraph::new(model.timeline.join("\n"))
            .scroll((model.scroll, 0))
            .block(Block::default().borders(Borders::ALL).title("Timeline")),
        area,
    );
}
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(v[1])[1]
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
    /// Panics only if the process-local stage mutex is poisoned; the partially-created guard
    /// still rolls back raw/alternate terminal modes during unwinding.
    pub fn enter() -> Result<Self, TuiError> {
        Self::enter_with_hook(|_| {})
    }
    /// Enters with a callback after each committed stage, before interrupt checking.
    ///
    /// # Panics
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

/// Runs the UI and returns typed actions to the supplied authority adapter.
pub fn run<P: ProjectionClient, S: CommandSink>(
    projection: &mut P,
    sink: S,
) -> Result<(), TuiError> {
    let (_feedback_tx, feedback_rx) = std::sync::mpsc::channel();
    run_with_feedback(projection, sink, &feedback_rx)
}

/// Runs with an asynchronous command-service feedback channel.
pub fn run_with_feedback<P: ProjectionClient, S: CommandSink>(
    projection: &mut P,
    mut sink: S,
    feedback: &std::sync::mpsc::Receiver<Result<String, String>>,
) -> Result<(), TuiError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TuiError::NonTty);
    }
    let guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let size = terminal.size()?;
    let mut model = UiModel {
        size: (size.width, size.height),
        ..UiModel::default()
    };
    let _ = reduce(
        &mut model,
        UiInput::Snapshot(projection.snapshots().map_err(TuiError::Action)?),
    );
    loop {
        if guard.interrupted.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TuiError::Interrupted);
        }
        while let Ok(message) = feedback.try_recv() {
            let input = match message {
                Ok(message) => UiInput::CommandCompleted(message),
                Err(error) => UiInput::CommandError(error),
            };
            let _ = reduce(&mut model, input);
            let runs = projection.snapshots().map_err(TuiError::Action)?;
            let _ = reduce(&mut model, UiInput::Snapshot(runs));
        }
        terminal.draw(|f| render(f, &model))?;
        loop {
            match projection.poll() {
                ProjectionPoll::Event => {
                    let runs = projection.snapshots().map_err(TuiError::Action)?;
                    let _ = reduce(&mut model, UiInput::Snapshot(runs));
                }
                ProjectionPoll::Empty => break,
                ProjectionPoll::Lagged(_) => {
                    for action in reduce(&mut model, UiInput::Lagged) {
                        if action == UiAction::RefreshSnapshots {
                            let runs = projection.snapshots().map_err(TuiError::Action)?;
                            let _ = reduce(&mut model, UiInput::Snapshot(runs));
                        }
                    }
                    break;
                }
                ProjectionPoll::Closed => {
                    let _ = reduce(&mut model, UiInput::Disconnected);
                    break;
                }
                ProjectionPoll::Error(error) => {
                    let _ = reduce(&mut model, UiInput::CommandError(error));
                    break;
                }
            }
        }
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    for action in reduce(&mut model, UiInput::Key(key)) {
                        if action == UiAction::Quit {
                            return Ok(());
                        }
                        if action == UiAction::RefreshSnapshots {
                            let runs = projection.snapshots().map_err(TuiError::Action)?;
                            let _ = reduce(&mut model, UiInput::Snapshot(runs));
                        } else {
                            sink.submit(action).map_err(TuiError::Action)?;
                        }
                    }
                }
                Event::Resize(w, h) => {
                    reduce(&mut model, UiInput::Resize(w, h));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{IdSource, PendingPermission, SystemIdSource};
    use ratatui::{Terminal, backend::TestBackend};
    fn run(status: RunStatus) -> RunState {
        let mut r = RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        r.status = status;
        r
    }
    #[test]
    fn responsive_boundaries() {
        let mut m = UiModel::default();
        for (size, expected) in [
            ((120, 30), LayoutMode::Wide),
            ((90, 30), LayoutMode::Medium),
            ((50, 20), LayoutMode::Narrow),
            ((20, 5), LayoutMode::Safety),
        ] {
            reduce(&mut m, UiInput::Resize(size.0, size.1));
            assert_eq!(m.layout_mode(), expected);
        }
    }
    #[test]
    fn permission_enter_never_approves_and_deny_is_explicit() {
        let mut r = run(RunStatus::WaitingPermission);
        r.pending_permission = Some(PendingPermission {
            request_id: "p".into(),
            operation_digest: "digest".into(),
            description: "write".into(),
        });
        let mut m = UiModel {
            runs: vec![r],
            ..UiModel::default()
        };
        assert!(
            reduce(
                &mut m,
                UiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            )
            .is_empty()
        );
        assert!(matches!(
            reduce(
                &mut m,
                UiInput::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::ResolvePermission {
                decision: PermissionDecision::Deny,
                ..
            })]
        ));
    }
    #[test]
    fn sigint_key_exits_through_guarded_loop() {
        let mut model = UiModel::default();
        assert_eq!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            ),
            vec![UiAction::Quit]
        );
    }
    #[test]
    fn process_sigint_is_scoped_and_observed_outside_signal_handler() {
        let guard = TerminalGuard::test_signal_guard();
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        for _ in 0..1000 {
            if guard.interrupted.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(guard.interrupted.load(std::sync::atomic::Ordering::SeqCst));
        drop(guard);
    }
    #[test]
    fn waiting_input_editor_submits_exact_request_but_permission_enter_stays_safe() {
        let mut state = run(RunStatus::WaitingInput);
        state.revision = 7;
        state.pending_input = Some(latte_core::PendingInput {
            request_id: "i-1".into(),
            prompt: "value?".into(),
        });
        let mut model = UiModel {
            runs: vec![state],
            ..UiModel::default()
        };
        assert!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            )
            .is_empty()
        );
        assert_eq!(model.input, "x");
        assert!(
            matches!(reduce(&mut model,UiInput::Key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE))).as_slice(),[UiAction::Command(RuntimeCommand::ProvideInput{request_id,value,..})] if request_id=="i-1" && value=="x")
        );
    }
    #[test]
    fn disconnected_gap_and_unknown_fail_closed() {
        let mut m = UiModel {
            runs: vec![run(RunStatus::Running)],
            ..UiModel::default()
        };
        reduce(&mut m, UiInput::Disconnected);
        assert!(
            reduce(
                &mut m,
                UiInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            )
            .is_empty()
        );
        assert_eq!(
            reduce(&mut m, UiInput::Lagged),
            vec![UiAction::RefreshSnapshots]
        );
        let run_id = m.runs[0].run_id;
        reduce(
            &mut m,
            UiInput::UnknownEffect {
                run_id,
                effect_id: "e".into(),
            },
        );
        assert!(!m.authority_enabled());
    }
    #[test]
    fn test_backend_renders_every_layout() {
        for (w, h) in [(120, 30), (90, 25), (50, 20), (20, 5)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            let m = UiModel {
                size: (w, h),
                runs: vec![run(RunStatus::Running)],
                ..UiModel::default()
            };
            terminal.draw(|f| render(f, &m)).unwrap();
            let text = format!("{:?}", terminal.backend().buffer());
            assert!(text.contains("Lattecode") || text.contains("Runs"));
        }
    }
    #[test]
    fn arbitrary_keys_do_not_panic() {
        let mut m = UiModel {
            runs: vec![run(RunStatus::Running)],
            ..UiModel::default()
        };
        for code in 0_u8..=127 {
            let ch = char::from(code);
            let _ = reduce(
                &mut m,
                UiInput::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            );
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
        restore_once(&flag, &Arc::new(Mutex::new(TerminalStages::default())));
        assert!(*flag.lock().unwrap());
    }
    #[test]
    fn non_tty_contract_is_typed() {
        assert_eq!(
            TuiError::NonTty.to_string(),
            "interactive TUI requires a TTY; use --json list/show/run/resume for non-interactive use"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reducer_covers_navigation_commands_events_and_feedback() {
        let mut running = run(RunStatus::Running);
        running.revision = 3;
        let run_id = running.run_id;
        let mut model = UiModel {
            runs: vec![running, run(RunStatus::Interrupted)],
            timeline: vec!["started".into()],
            ..UiModel::default()
        };

        assert!(reduce(&mut model, UiInput::Connected).is_empty());
        assert_eq!(model.status, "Connected");
        reduce(&mut model, UiInput::CommandCompleted("done".into()));
        assert_eq!(model.status, "done");
        reduce(&mut model, UiInput::CommandError("bad".into()));
        assert!(model.status.contains("bad"));
        reduce(&mut model, UiInput::Tick);

        for expected in [Focus::Timeline, Focus::Details, Focus::Prompt, Focus::Runs] {
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            );
            assert_eq!(model.focus, expected);
        }
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        );
        assert!(model.help);
        assert_eq!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            ),
            vec![UiAction::Quit]
        );

        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::Cancel { .. })]
        ));
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        assert_eq!(model.selected, 1);
        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::Resume { .. })]
        ));
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        );
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        );
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        );

        let actions = reduce(
            &mut model,
            UiInput::Event(EventEnvelope {
                protocol_version: latte_core::PROTOCOL_VERSION,
                event_id: latte_core::EventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                run_id,
                revision: 4,
                event: RuntimeEvent::StateChanged {
                    status: RunStatus::Interrupted,
                },
            }),
        );
        assert!(actions.is_empty());
        assert_eq!(model.runs[0].status, RunStatus::Interrupted);
        assert_eq!(model.timeline.len(), 2);
        assert_eq!(
            reduce(
                &mut model,
                UiInput::Event(EventEnvelope {
                    protocol_version: latte_core::PROTOCOL_VERSION,
                    event_id: latte_core::EventId::from_uuid(
                        SystemIdSource::default().next_uuid_v7(),
                    ),
                    run_id,
                    revision: 8,
                    event: RuntimeEvent::StateChanged {
                        status: RunStatus::Running,
                    },
                })
            ),
            vec![UiAction::RefreshSnapshots]
        );
        let absent = run(RunStatus::Running).run_id;
        assert_eq!(
            reduce(
                &mut model,
                UiInput::Event(EventEnvelope {
                    protocol_version: latte_core::PROTOCOL_VERSION,
                    event_id: latte_core::EventId::from_uuid(
                        SystemIdSource::default().next_uuid_v7(),
                    ),
                    run_id: absent,
                    revision: 1,
                    event: RuntimeEvent::StateChanged {
                        status: RunStatus::Running,
                    },
                })
            ),
            vec![UiAction::RefreshSnapshots]
        );
    }

    #[test]
    fn reducer_covers_prompt_permission_input_and_reconciliation() {
        let mut model = UiModel {
            focus: Focus::Prompt,
            ..UiModel::default()
        };
        for ch in "build".chars() {
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            );
        }
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::Run { prompt })] if prompt == "buil"
        ));

        let mut permission = run(RunStatus::WaitingPermission);
        permission.revision = 9;
        permission.pending_permission = Some(PendingPermission {
            request_id: "approve".into(),
            operation_digest: "digest".into(),
            description: "write file".into(),
        });
        model = UiModel {
            runs: vec![permission],
            ..UiModel::default()
        };
        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::ResolvePermission {
                decision: PermissionDecision::Allow,
                ..
            })]
        ));

        let mut waiting = run(RunStatus::WaitingInput);
        waiting.pending_input = Some(latte_core::PendingInput {
            request_id: "input".into(),
            prompt: "value".into(),
        });
        model = UiModel {
            runs: vec![waiting],
            input: "ab".into(),
            ..UiModel::default()
        };
        reduce(
            &mut model,
            UiInput::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        assert_eq!(model.input, "a");
        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            )
            .as_slice(),
            [UiAction::Command(RuntimeCommand::Cancel { .. })]
        ));

        let run_id = model.runs[0].run_id;
        reduce(
            &mut model,
            UiInput::UnknownEffect {
                run_id,
                effect_id: "effect".into(),
            },
        );
        assert!(matches!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            )
            .as_slice(),
            [UiAction::ReconcileUnknown { effect_id, .. }] if effect_id == "effect"
        ));
        reduce(&mut model, UiInput::Disconnected);
        assert_eq!(
            reduce(
                &mut model,
                UiInput::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            ),
            vec![UiAction::RefreshSnapshots]
        );
    }

    #[test]
    fn render_covers_empty_help_permission_input_unknown_and_details() {
        let cases = [
            UiModel {
                size: (120, 30),
                help: true,
                ..UiModel::default()
            },
            {
                let mut state = run(RunStatus::WaitingPermission);
                state.pending_permission = Some(PendingPermission {
                    request_id: "p".into(),
                    operation_digest: "digest".into(),
                    description: "mutate".into(),
                });
                UiModel {
                    size: (120, 30),
                    runs: vec![state],
                    ..UiModel::default()
                }
            },
            {
                let mut state = run(RunStatus::WaitingInput);
                state.pending_input = Some(latte_core::PendingInput {
                    request_id: "i".into(),
                    prompt: "answer".into(),
                });
                UiModel {
                    size: (90, 25),
                    runs: vec![state],
                    input: "value".into(),
                    ..UiModel::default()
                }
            },
            {
                let state = run(RunStatus::Running);
                let run_id = state.run_id;
                UiModel {
                    size: (50, 20),
                    runs: vec![state],
                    unknown_effects: [(run_id, "uncertain".into())].into_iter().collect(),
                    timeline: vec!["declared".into(), "started".into()],
                    scroll: 1,
                    ..UiModel::default()
                }
            },
        ];
        for model in cases {
            let backend = TestBackend::new(model.size.0, model.size.1);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &model)).unwrap();
            assert!(!format!("{:?}", terminal.backend().buffer()).is_empty());
        }
    }
}
