//! Transcript-first Thread v2 presentation.
//!
//! This reducer owns only local text, focus, scroll, and a single safe
//! follow-up queue. It has no provider, repository, or effect authority.

use crate::{ConnectionState, TuiError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use latte_core::{
    ThreadEvent, ThreadEventEnvelope, ThreadId, ThreadLifecycle, ThreadPendingRequest,
    ThreadSnapshot, ThreadTransientProgress, TranscriptKind,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use std::{
    io::{self, IsTerminal},
    sync::mpsc::Receiver,
    time::Duration,
};
use unicode_segmentation::UnicodeSegmentation;

/// Thread-only projection boundary; snapshots are authoritative after any
/// event gap and transient progress must be discarded.
pub trait ThreadProjectionClient {
    fn snapshots(&mut self) -> Result<Vec<ThreadSnapshot>, String>;
    fn poll(&mut self) -> ThreadProjectionPoll;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadProjectionPoll {
    Event,
    Empty,
    Lagged(u64),
    Closed,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ThreadUiInput {
    Key(KeyEvent),
    Resize(u16, u16),
    Snapshot(Vec<ThreadSnapshot>),
    Event(ThreadEventEnvelope),
    Progress(ThreadTransientProgress),
    Lagged,
    Connected,
    Disconnected,
    CommandError(String),
    CommandCompleted(String),
    Tick,
}

/// UI commands are thread-level requests. The caller maps them to the
/// headless service; the reducer never executes effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadUiAction {
    Start {
        prompt: String,
    },
    FollowUp {
        thread_id: ThreadId,
        expected_thread_revision: u64,
        prompt: String,
    },
    Cancel {
        thread_id: ThreadId,
    },
    ResolvePermission {
        thread_id: ThreadId,
        request_id: String,
        allow: bool,
    },
    ProvideInput {
        thread_id: ThreadId,
        request_id: String,
        value: String,
    },
    /// Explicit terminal acknowledgement that an already-started external
    /// effect has an unknown outcome. This is deliberately separate from
    /// normal permission approval and requires its own confirmation chord.
    ReconcileUnknown {
        thread_id: ThreadId,
        effect_id: String,
    },
    RefreshSnapshots,
    Quit,
}

#[derive(Clone, Debug)]
pub struct ThreadUiModel {
    pub sessions: Vec<ThreadSnapshot>,
    pub selected: usize,
    pub composer: String,
    pub input: String,
    pub scroll: u16,
    pub connection: ConnectionState,
    pub progress: Vec<ThreadTransientProgress>,
    pub status: String,
    pub help: bool,
    pub sessions_overlay: bool,
    pub size: (u16, u16),
    /// At most one local queued composer submission, dispatched only after a
    /// freshly loaded completed/ready snapshot.
    pub queued_follow_up: Option<String>,
    /// A reconciliation acknowledgement is high-impact: it records the
    /// unknown effect as failed and terminalizes the linked child. Opening
    /// this state is not itself an action; only Ctrl+A can dispatch it.
    pub reconciliation_confirmation: Option<(ThreadId, String)>,
    /// Event-only hint retained until the next authoritative snapshot. Normal
    /// projection snapshots derive the same identifier from their durable
    /// failure card, so this is never an authority source.
    pub reconciliation_hint: Option<(ThreadId, String)>,
}

impl Default for ThreadUiModel {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            composer: String::new(),
            input: String::new(),
            scroll: 0,
            connection: ConnectionState::Connected,
            progress: Vec::new(),
            status: "Ready".into(),
            help: false,
            sessions_overlay: false,
            size: (80, 24),
            queued_follow_up: None,
            reconciliation_confirmation: None,
            reconciliation_hint: None,
        }
    }
}

impl ThreadUiModel {
    #[must_use]
    pub fn selected_thread(&self) -> Option<&ThreadSnapshot> {
        self.sessions.get(self.selected)
    }
    #[must_use]
    pub fn authority_enabled(&self) -> bool {
        self.connection == ConnectionState::Connected
    }
    #[must_use]
    pub fn shows_sidebar(&self) -> bool {
        self.size.0 >= 92 && !self.sessions_overlay
    }
}

#[allow(clippy::too_many_lines)]
pub fn reduce(model: &mut ThreadUiModel, input: ThreadUiInput) -> Vec<ThreadUiAction> {
    match input {
        ThreadUiInput::Resize(width, height) => model.size = (width, height),
        ThreadUiInput::Connected => {
            model.connection = ConnectionState::Connected;
            model.progress.clear();
            model.status = "Connected".into();
        }
        ThreadUiInput::Disconnected => {
            model.connection = ConnectionState::Disconnected;
            model.status = "Disconnected: actions disabled".into();
        }
        ThreadUiInput::Lagged => {
            model.connection = ConnectionState::SnapshotRequired;
            model.progress.clear();
            model.status = "Event gap: reloading transcript snapshot".into();
            return vec![ThreadUiAction::RefreshSnapshots];
        }
        ThreadUiInput::Snapshot(mut sessions) => {
            let selected_id = model.selected_thread().map(|thread| thread.thread_id);
            sessions.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.sequence));
            model.sessions = sessions;
            model.selected = selected_id
                .and_then(|id| {
                    model
                        .sessions
                        .iter()
                        .position(|thread| thread.thread_id == id)
                })
                .unwrap_or_else(|| model.selected.min(model.sessions.len().saturating_sub(1)));
            model.connection = ConnectionState::Connected;
            model.progress.clear();
            model.status = "Transcript synchronized".into();
            model.reconciliation_hint = model.selected_thread().and_then(|thread| {
                reconciliation_effect_from_snapshot(thread)
                    .map(|effect_id| (thread.thread_id, effect_id))
            });
            if model
                .reconciliation_confirmation
                .as_ref()
                .is_some_and(|(thread_id, effect_id)| {
                    model.selected_thread().is_none_or(|thread| {
                        thread.thread_id != *thread_id
                            || thread.lifecycle != ThreadLifecycle::ReconciliationRequired
                            || reconciliation_effect_id(model, thread).as_deref()
                                != Some(effect_id.as_str())
                    })
                })
            {
                model.reconciliation_confirmation = None;
            }
            if let (Some(prompt), Some(thread)) =
                (model.queued_follow_up.take(), model.selected_thread())
                && thread.lifecycle == ThreadLifecycle::Ready
            {
                return vec![ThreadUiAction::FollowUp {
                    thread_id: thread.thread_id,
                    expected_thread_revision: thread.revision,
                    prompt,
                }];
            }
        }
        ThreadUiInput::Event(event) => {
            let Some(thread) = model
                .sessions
                .iter_mut()
                .find(|thread| thread.thread_id == event.thread_id)
            else {
                return vec![ThreadUiAction::RefreshSnapshots];
            };
            if event.revision != thread.revision.saturating_add(1) {
                model.connection = ConnectionState::SnapshotRequired;
                model.progress.clear();
                return vec![ThreadUiAction::RefreshSnapshots];
            }
            thread.revision = event.revision;
            thread.sequence = event.sequence;
            match event.event {
                ThreadEvent::LifecycleChanged { lifecycle, .. } => thread.lifecycle = lifecycle,
                ThreadEvent::TranscriptAppended { entry } => thread.transcript.entries.push(entry),
                ThreadEvent::RunLinked { run } => {
                    thread.latest_run_id = Some(run.run_id);
                    thread.active_run_id = Some(run.run_id);
                    thread.runs.push(run);
                    thread.lifecycle = ThreadLifecycle::Running;
                }
                ThreadEvent::ReconciliationRequired { effect_id, .. } => {
                    thread.lifecycle = ThreadLifecycle::ReconciliationRequired;
                    model.reconciliation_hint = Some((thread.thread_id, effect_id));
                }
            }
        }
        ThreadUiInput::Progress(progress) => {
            if model.connection == ConnectionState::Connected && model.progress.len() < 64 {
                model.progress.push(progress);
            }
        }
        ThreadUiInput::CommandError(error) => model.status = format!("Command rejected: {error}"),
        ThreadUiInput::CommandCompleted(message) => model.status = message,
        ThreadUiInput::Key(key) => return reduce_key(model, key),
        ThreadUiInput::Tick => {}
    }
    Vec::new()
}

#[allow(clippy::too_many_lines)]
fn reduce_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(thread) = model
            .selected_thread()
            .filter(|_| model.authority_enabled())
        {
            return vec![ThreadUiAction::Cancel {
                thread_id: thread.thread_id,
            }];
        }
        return Vec::new();
    }
    if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
        return vec![ThreadUiAction::Quit];
    }
    if key.code == KeyCode::Char('?') {
        model.help = !model.help;
        return Vec::new();
    }
    if key.code == KeyCode::Char('s') && key.modifiers.is_empty() {
        model.sessions_overlay = !model.sessions_overlay;
        return Vec::new();
    }
    if key.code == KeyCode::Char('r') && model.connection != ConnectionState::Connected {
        return vec![ThreadUiAction::RefreshSnapshots];
    }
    if !model.authority_enabled() {
        return Vec::new();
    }
    if let Some((thread_id, effect_id)) = model.reconciliation_confirmation.clone() {
        // Enter is deliberately inert while the confirmation card is open:
        // it must never be able to acknowledge a potentially executed effect.
        if key.code == KeyCode::Enter {
            return Vec::new();
        }
        if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
            model.reconciliation_confirmation = None;
            return vec![ThreadUiAction::ReconcileUnknown {
                thread_id,
                effect_id,
            }];
        }
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('d') && key.modifiers.is_empty())
        {
            model.reconciliation_confirmation = None;
            model.status = "Reconciliation acknowledgement cancelled".into();
        }
        return Vec::new();
    }
    if let Some(thread) = model.selected_thread().cloned() {
        if key.code == KeyCode::Char('r')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && thread.lifecycle == ThreadLifecycle::ReconciliationRequired
            && let Some(effect_id) = reconciliation_effect_id(model, &thread)
        {
            model.reconciliation_confirmation = Some((thread.thread_id, effect_id));
            model.status =
                "Reconciliation acknowledgement open: Ctrl+A confirms; Enter does nothing".into();
            return Vec::new();
        }
        if let Some(ThreadPendingRequest::Permission { request_id, .. }) = thread.pending.as_ref() {
            // Enter is intentionally not handled here and can never approve.
            if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
                return vec![ThreadUiAction::ResolvePermission {
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    allow: false,
                }];
            }
            if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return vec![ThreadUiAction::ResolvePermission {
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    allow: true,
                }];
            }
        }
        if let Some(ThreadPendingRequest::Input { request_id, .. }) = thread.pending.as_ref() {
            if is_submit(key) && !model.input.trim().is_empty() {
                return vec![ThreadUiAction::ProvideInput {
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    value: std::mem::take(&mut model.input),
                }];
            }
            if key.code == KeyCode::Backspace {
                pop_grapheme(&mut model.input);
                return Vec::new();
            }
            if let KeyCode::Char(value) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && model.input.len() < 16 * 1024
            {
                model.input.push(value);
                return Vec::new();
            }
        }
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => model.selected = model.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            model.selected = (model.selected + 1).min(model.sessions.len().saturating_sub(1));
        }
        KeyCode::PageUp => model.scroll = model.scroll.saturating_sub(8),
        KeyCode::PageDown => model.scroll = model.scroll.saturating_add(8),
        KeyCode::Backspace => pop_grapheme(&mut model.composer),
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.composer.push('\n');
        }
        KeyCode::F(5) | KeyCode::Enter if is_submit(key) => return submit_composer(model),
        KeyCode::Char(value)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && model.composer.len() < 16 * 1024 =>
        {
            model.composer.push(value);
        }
        _ => {}
    }
    Vec::new()
}

fn is_submit(key: KeyEvent) -> bool {
    (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        || key.code == KeyCode::F(5)
}

fn submit_composer(model: &mut ThreadUiModel) -> Vec<ThreadUiAction> {
    let prompt = std::mem::take(&mut model.composer);
    if prompt.trim().is_empty() {
        return Vec::new();
    }
    match model.selected_thread() {
        None => vec![ThreadUiAction::Start { prompt }],
        Some(thread) if thread.lifecycle == ThreadLifecycle::Ready => {
            vec![ThreadUiAction::FollowUp {
                thread_id: thread.thread_id,
                expected_thread_revision: thread.revision,
                prompt,
            }]
        }
        Some(_) if model.queued_follow_up.is_none() => {
            model.queued_follow_up = Some(prompt);
            model.status = "One follow-up queued for the next completed snapshot".into();
            Vec::new()
        }
        Some(_) => {
            model.status = "A follow-up is already queued".into();
            Vec::new()
        }
    }
}

fn pop_grapheme(value: &mut String) {
    if let Some((index, _)) = value.grapheme_indices(true).next_back() {
        value.truncate(index);
    }
}

/// Renders the responsive transcript, secondary sessions list, fixed composer,
/// status strip, and non-durable progress without exposing checkpoint JSON.
pub fn render(frame: &mut Frame<'_>, model: &ThreadUiModel) {
    let area = frame.area();
    if area.width < 34 || area.height < 9 {
        frame.render_widget(
            Paragraph::new("Terminal too small\nResize or press q").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Latte Code Thread"),
            ),
            area,
        );
        return;
    }
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(5),
            Constraint::Length(2),
        ])
        .split(area);
    if model.shows_sidebar() {
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(24), Constraint::Percentage(76)])
            .split(vertical[0]);
        render_sessions(frame, model, horizontal[0]);
        render_transcript(frame, model, horizontal[1]);
    } else {
        render_transcript(frame, model, vertical[0]);
    }
    frame.render_widget(
        Paragraph::new(model.composer.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Composer · Enter newline · Ctrl+Enter/F5 send"),
            ),
        vertical[1],
    );
    let pending = model
        .selected_thread()
        .and_then(|thread| thread.pending.as_ref())
        .map_or_else(
            || "Ctrl+C interrupt · s sessions · ? help".into(),
            |pending| match pending {
                ThreadPendingRequest::Permission { description, .. } => format!(
                    "Permission: {description} · [d] deny · [Ctrl+A] allow · Enter never approves"
                ),
                ThreadPendingRequest::Input { prompt, .. } => {
                    format!("Input: {prompt} · Ctrl+Enter/F5 submit")
                }
            },
        );
    frame.render_widget(
        Paragraph::new(format!(
            "{:?} · {} · {pending}",
            model.connection, model.status
        ))
        .style(
            Style::default().fg(if model.connection == ConnectionState::Connected {
                Color::Green
            } else {
                Color::Yellow
            }),
        ),
        vertical[2],
    );
    if model.sessions_overlay && !model.shows_sidebar() {
        let overlay = centered(area, 78, 72);
        frame.render_widget(Clear, overlay);
        render_sessions(frame, model, overlay);
    }
    if model.help {
        let overlay = centered(area, 78, 65);
        frame.render_widget(Clear, overlay);
        frame.render_widget(Paragraph::new("Transcript thread\nEnter inserts a newline; Ctrl+Enter or F5 sends.\nCtrl+C interrupts an active child.\nd denies and Ctrl+A allows only the focused permission.\nCtrl+R opens a reconciliation acknowledgement; Ctrl+A confirms it.\ns opens sessions; j/k select; PgUp/PgDn scroll.\nEvent gaps clear progress and reload a snapshot.").wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title("Help")), overlay);
    }
}

fn render_sessions(frame: &mut Frame<'_>, model: &ThreadUiModel, area: Rect) {
    let sessions = model
        .sessions
        .iter()
        .enumerate()
        .map(|(index, thread)| {
            ListItem::new(format!(
                "{} {} {:?} r{}",
                if index == model.selected { ">" } else { " " },
                &thread.thread_id.to_string()[..8],
                thread.lifecycle,
                thread.revision
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(sessions).block(Block::default().borders(Borders::ALL).title("Sessions")),
        area,
    );
}

fn render_transcript(frame: &mut Frame<'_>, model: &ThreadUiModel, area: Rect) {
    let mut lines = Vec::new();
    if let Some(thread) = model.selected_thread() {
        if thread.transcript.has_more {
            lines.push(Line::from(Span::styled(
                "[… earlier transcript cards are omitted from this bounded current view]",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
        }
        for entry in &thread.transcript.entries {
            let label = match entry.kind {
                TranscriptKind::User => "you",
                TranscriptKind::Assistant => "assistant",
                TranscriptKind::ToolCall => "tool call",
                TranscriptKind::ToolResult => "tool result",
                TranscriptKind::Permission => "permission",
                TranscriptKind::Input => "input",
                TranscriptKind::Failure => "failure",
                TranscriptKind::Completion => "complete",
                TranscriptKind::System => "system",
            };
            let color = match entry.kind {
                TranscriptKind::User => Color::Cyan,
                TranscriptKind::Assistant => Color::White,
                TranscriptKind::Failure => Color::Red,
                TranscriptKind::Permission | TranscriptKind::Input => Color::Yellow,
                TranscriptKind::Completion => Color::Green,
                _ => Color::Gray,
            };
            lines.push(Line::from(Span::styled(
                format!("[{label}] {}", entry.text),
                Style::default().fg(color),
            )));
            lines.push(Line::from(""));
        }
        if thread.lifecycle == ThreadLifecycle::ReconciliationRequired
            && let Some(effect_id) = reconciliation_effect_id(model, thread)
        {
            lines.push(Line::from(Span::styled(
                format!(
                    "[reconciliation required] An already-started effect ({effect_id}) has an unknown outcome."
                ),
                Style::default().fg(Color::Yellow),
            )));
            if model
                .reconciliation_confirmation
                .as_ref()
                .is_some_and(|(thread_id, _)| *thread_id == thread.thread_id)
            {
                lines.push(Line::from(Span::styled(
                    "Confirm acknowledgement: it records the unknown effect as failed and aborts this child. [Ctrl+A] confirm · [d]/Esc cancel · Enter does nothing",
                    Style::default().fg(Color::Yellow),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "Review first: [Ctrl+R] open acknowledgement · Enter does nothing",
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(""));
        }
        if let Some(ThreadPendingRequest::Permission { description, .. }) = thread.pending.as_ref()
        {
            // This is deliberately rendered as an inline card rather than
            // only in the one-line status strip: approval requires the user
            // to see a bounded operation summary before Ctrl+A can dispatch.
            let description = permission_context(description);
            lines.push(Line::from(Span::styled(
                format!("[permission] {description}"),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                "Decision required: [d] deny · [Ctrl+A] allow · Enter never approves",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }
    } else {
        lines.push(Line::from("Start a conversation with the composer."));
    }
    for progress in &model.progress {
        lines.push(Line::from(Span::styled(
            progress_text(progress),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((model.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Conversation")),
        area,
    );
}

fn reconciliation_effect_from_snapshot(thread: &ThreadSnapshot) -> Option<String> {
    if thread.lifecycle != ThreadLifecycle::ReconciliationRequired {
        return None;
    }
    thread.transcript.entries.iter().rev().find_map(|entry| {
        if entry.kind != TranscriptKind::Failure {
            return None;
        }
        let payload = entry.payload.as_ref()?;
        if payload.get("status").and_then(|value| value.as_str()) != Some("unknown") {
            return None;
        }
        let effect_id = payload.get("effect_id").and_then(|value| value.as_str())?;
        if effect_id.is_empty() || effect_id.len() > 512 || effect_id.chars().any(char::is_control)
        {
            return None;
        }
        Some(effect_id.to_owned())
    })
}

fn reconciliation_effect_id(model: &ThreadUiModel, thread: &ThreadSnapshot) -> Option<String> {
    model
        .reconciliation_hint
        .as_ref()
        .filter(|(thread_id, _)| *thread_id == thread.thread_id)
        .map(|(_, effect_id)| effect_id.clone())
        .or_else(|| reconciliation_effect_from_snapshot(thread))
}

fn permission_context(description: &str) -> String {
    const CAP: usize = 360;
    let redacted = latte_core::redact_thread_text(description);
    let mut output = String::with_capacity(redacted.len().min(CAP));
    for ch in redacted.chars() {
        if ch.is_control() {
            continue;
        }
        if output.len() + ch.len_utf8() > CAP {
            output.push('…');
            break;
        }
        output.push(ch);
    }
    if output.is_empty() {
        "[operation summary unavailable]".into()
    } else {
        output
    }
}

fn progress_text(progress: &ThreadTransientProgress) -> String {
    match progress {
        ThreadTransientProgress::ProviderAttempt { number, .. } => {
            format!("… provider attempt {number}")
        }
        ThreadTransientProgress::AssistantDelta { text, .. } => format!("… {text}"),
        ThreadTransientProgress::ToolProgress { name, detail, .. } => format!("… {name}: {detail}"),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([
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
    .split(vertical[1])[1]
}

/// Thread terminal loop. Like the v1 loop it only dispatches actions through
/// the supplied callback and never calls an engine directly.
///
/// # Errors
///
/// Returns a typed terminal, projection, or action-dispatch failure.
fn apply_thread_actions(
    projection: &mut dyn ThreadProjectionClient,
    model: &mut ThreadUiModel,
    sink: &mut impl FnMut(ThreadUiAction) -> Result<(), String>,
    actions: Vec<ThreadUiAction>,
) -> Result<bool, TuiError> {
    for action in actions {
        match action {
            ThreadUiAction::Quit => return Ok(true),
            ThreadUiAction::RefreshSnapshots => {
                // A snapshot refresh is a typed projection operation, not a
                // runtime effect. Consume it in the terminal adapter so a
                // production command closure cannot accidentally ignore the
                // recovery path after a broadcast receiver reports Lagged.
                let snapshots = projection.snapshots().map_err(TuiError::Action)?;
                let next = reduce(model, ThreadUiInput::Snapshot(snapshots));
                if apply_thread_actions(projection, model, sink, next)? {
                    return Ok(true);
                }
            }
            action => sink(action).map_err(TuiError::Action)?,
        }
    }
    Ok(false)
}

pub fn run_with_feedback_and_progress(
    projection: &mut dyn ThreadProjectionClient,
    mut sink: impl FnMut(ThreadUiAction) -> Result<(), String>,
    feedback: &Receiver<Result<String, String>>,
    progress: &Receiver<ThreadTransientProgress>,
) -> Result<(), TuiError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TuiError::NonTty);
    }
    let guard = crate::TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut model = ThreadUiModel::default();
    let snapshots = projection.snapshots().map_err(TuiError::Action)?;
    let initial_actions = reduce(&mut model, ThreadUiInput::Snapshot(snapshots));
    if apply_thread_actions(projection, &mut model, &mut sink, initial_actions)? {
        return Ok(());
    }
    loop {
        terminal.draw(|frame| render(frame, &model))?;
        if guard.interrupted() {
            return Err(TuiError::Interrupted);
        }
        match projection.poll() {
            ThreadProjectionPoll::Event => {
                let snapshots = projection.snapshots().map_err(TuiError::Action)?;
                let actions = reduce(&mut model, ThreadUiInput::Snapshot(snapshots));
                if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                    return Ok(());
                }
            }
            ThreadProjectionPoll::Lagged(_) => {
                let actions = reduce(&mut model, ThreadUiInput::Lagged);
                if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                    return Ok(());
                }
            }
            ThreadProjectionPoll::Closed => {
                reduce(&mut model, ThreadUiInput::Disconnected);
            }
            ThreadProjectionPoll::Error(error) => {
                reduce(&mut model, ThreadUiInput::CommandError(error));
            }
            ThreadProjectionPoll::Empty => {}
        }
        while let Ok(result) = feedback.try_recv() {
            match result {
                Ok(message) => reduce(&mut model, ThreadUiInput::CommandCompleted(message)),
                Err(error) => reduce(&mut model, ThreadUiInput::CommandError(error)),
            };
        }
        while let Ok(update) = progress.try_recv() {
            reduce(&mut model, ThreadUiInput::Progress(update));
        }
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) => {
                    let actions = reduce(&mut model, ThreadUiInput::Key(key));
                    if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                        return Ok(());
                    }
                }
                Event::Resize(width, height) => {
                    reduce(&mut model, ThreadUiInput::Resize(width, height));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{
        IdSource, RunId, SystemIdSource, ThreadEvent, ThreadEventEnvelope, ThreadEventId,
        ThreadProviderBindingV2, TranscriptEntry, TranscriptEntryId,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::VecDeque;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> ThreadUiInput {
        ThreadUiInput::Key(KeyEvent::new(code, modifiers))
    }

    fn snapshot(lifecycle: ThreadLifecycle) -> ThreadSnapshot {
        let ids = SystemIdSource::default();
        ThreadSnapshot {
            thread_id: ThreadId::from_uuid(ids.next_uuid_v7()),
            revision: 1,
            sequence: 1,
            lifecycle,
            binding: ThreadProviderBindingV2 {
                version: 1,
                provider_name: "p".into(),
                provider_type: "t".into(),
                protocol: "p".into(),
                model: "m".into(),
                config_fingerprint: "c".into(),
                tools_fingerprint: "t".into(),
                aliases: std::collections::BTreeMap::default(),
                credential_ref_id: "ref".into(),
                data_scope_id: "scope".into(),
                credential_generation: 1,
            },
            latest_run_id: None,
            active_run_id: None,
            pending: None,
            runs: vec![],
            transcript: latte_core::TranscriptPage {
                entries: vec![TranscriptEntry {
                    entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
                    sequence: 1,
                    run_id: None,
                    kind: TranscriptKind::User,
                    text: "hello".into(),
                    payload: None,
                    source_key: "u".into(),
                    created_at_ms: 1,
                }],
                next_after: Some(1),
                has_more: false,
            },
        }
    }

    struct ScriptedProjection {
        snapshots: VecDeque<Vec<ThreadSnapshot>>,
        poll: ThreadProjectionPoll,
    }

    impl ThreadProjectionClient for ScriptedProjection {
        fn snapshots(&mut self) -> Result<Vec<ThreadSnapshot>, String> {
            self.snapshots
                .pop_front()
                .ok_or_else(|| "no scripted snapshot".into())
        }

        fn poll(&mut self) -> ThreadProjectionPoll {
            self.poll.clone()
        }
    }

    #[test]
    fn lagged_projection_refreshes_current_snapshot_without_command_sink_help() {
        let mut stale = snapshot(ThreadLifecycle::Running);
        stale.transcript.entries[0].text = "stale card".into();
        let mut current = stale.clone();
        current.revision = 2;
        current.sequence = 2;
        current.lifecycle = ThreadLifecycle::Ready;
        current.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: 2,
            run_id: None,
            kind: TranscriptKind::Completion,
            text: "current card after event gap".into(),
            payload: None,
            source_key: "current".into(),
            created_at_ms: 2,
        });
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![current]]),
            poll: ThreadProjectionPoll::Lagged(3),
        };
        let mut model = ThreadUiModel::default();
        reduce(&mut model, ThreadUiInput::Snapshot(vec![stale]));
        model
            .progress
            .push(ThreadTransientProgress::AssistantDelta {
                run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                text: "non-durable delta".into(),
            });
        let actions = match projection.poll() {
            ThreadProjectionPoll::Lagged(_) => reduce(&mut model, ThreadUiInput::Lagged),
            other => panic!("expected lagged projection, got {other:?}"),
        };
        let mut dispatched = Vec::new();
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut |action| {
                    dispatched.push(action);
                    Ok(())
                },
                actions,
            )
            .unwrap()
        );
        assert_eq!(model.connection, ConnectionState::Connected);
        assert!(model.progress.is_empty());
        assert_eq!(model.sessions[0].lifecycle, ThreadLifecycle::Ready);
        assert!(
            model.sessions[0]
                .transcript
                .entries
                .iter()
                .any(|entry| entry.text == "current card after event gap")
        );
        assert!(
            dispatched.is_empty(),
            "RefreshSnapshots is consumed by the projection adapter, not ignored by production sink"
        );
    }

    #[test]
    fn reconciliation_requires_ctrl_r_then_ctrl_a_and_enter_is_inert() {
        let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
        let effect_id = "thread-effect-safe-42";
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: 2,
            run_id: None,
            kind: TranscriptKind::Failure,
            text: "effect outcome unknown; reconciliation required".into(),
            payload: Some(serde_json::json!({
                "effect_id": effect_id,
                "status": "unknown"
            })),
            source_key: "unknown-effect".into(),
            created_at_ms: 2,
        });
        let thread_id = thread.thread_id;
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            ..Default::default()
        };
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert!(model.reconciliation_confirmation.is_none());
        assert!(reduce(&mut model, key(KeyCode::Char('r'), KeyModifiers::CONTROL)).is_empty());
        assert_eq!(
            model.reconciliation_confirmation,
            Some((thread_id, effect_id.into()))
        );
        let composer_before = model.composer.clone();
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(model.composer, composer_before);
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::ReconcileUnknown {
                thread_id,
                effect_id: effect_id.into(),
            }]
        );
    }
    #[test]
    fn composer_is_multiline_and_permission_enter_never_approves() {
        let mut model = ThreadUiModel {
            sessions: vec![snapshot(ThreadLifecycle::Ready)],
            ..Default::default()
        };
        reduce(
            &mut model,
            ThreadUiInput::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        reduce(
            &mut model,
            ThreadUiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(model.composer, "x\n");
        assert!(matches!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE))
            )
            .as_slice(),
            [ThreadUiAction::FollowUp { .. }]
        ));
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        model.sessions[0].pending = Some(ThreadPendingRequest::Permission {
            run_id: run,
            request_id: "p".into(),
            description: "write".into(),
            expected_run_revision: 2,
        });
        assert!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            )
            .is_empty()
        );
    }

    #[test]
    fn permission_card_shows_redacted_bounded_operation_context_before_ctrl_a() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::WaitingPermission);
        thread.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-card".into(),
            description: "Write src/generated.rs (create or replace; 43 bytes of content) api_key=live-secret-value\n\u{1b}[31m".into(),
            expected_run_revision: 2,
        });
        let mut model = ThreadUiModel {
            sessions: vec![thread.clone()],
            size: (100, 20),
            ..Default::default()
        };
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::ResolvePermission {
                thread_id: thread.thread_id,
                request_id: "permission-card".into(),
                allow: true,
            }]
        );
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[permission] Write src/generated.rs"));
        assert!(rendered.contains("create or replace; 43 bytes"));
        assert!(rendered.contains("Decision required: [d] deny"));
        assert!(rendered.contains("Ctrl+A] allow"));
        assert!(!rendered.contains("live-secret-value"));
        assert!(!rendered.contains('\u{1b}'));
    }
    #[test]
    fn gap_clears_progress_and_safe_queue_waits_for_snapshot() {
        let mut model = ThreadUiModel {
            sessions: vec![snapshot(ThreadLifecycle::Running)],
            composer: "later".into(),
            ..Default::default()
        };
        reduce(
            &mut model,
            ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
                run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                text: "partial".into(),
            }),
        );
        assert!(!model.progress.is_empty());
        assert!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE))
            )
            .is_empty()
        );
        assert!(model.queued_follow_up.is_some());
        assert_eq!(
            reduce(&mut model, ThreadUiInput::Lagged),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        assert!(model.progress.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reducer_dispatches_only_typed_thread_actions_for_all_active_states() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut ready = snapshot(ThreadLifecycle::Ready);
        ready.latest_run_id = Some(run_id);
        ready.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: latte_core::ThreadRunStatus::Completed,
            run_revision: 3,
            completed_at_ms: Some(3),
        });
        let mut model = ThreadUiModel::default();
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('h'), KeyModifiers::NONE)),
            Vec::<ThreadUiAction>::new()
        );
        assert_eq!(model.composer, "h");
        assert_eq!(
            reduce(&mut model, key(KeyCode::F(5), KeyModifiers::NONE)),
            vec![ThreadUiAction::Start { prompt: "h".into() }]
        );

        reduce(&mut model, ThreadUiInput::Snapshot(vec![ready.clone()]));
        reduce(&mut model, key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(matches!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::CONTROL)).as_slice(),
            [ThreadUiAction::FollowUp { thread_id, expected_thread_revision: 1, prompt }]
                if *thread_id == ready.thread_id && prompt == "f"
        ));

        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: Some(run_id),
            kind: TranscriptKind::Assistant,
            text: "durable assistant".into(),
            payload: None,
            source_key: "assistant".into(),
            created_at_ms: 2,
        };
        assert!(
            reduce(
                &mut model,
                ThreadUiInput::Event(ThreadEventEnvelope {
                    protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
                    event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
                    thread_id: ready.thread_id,
                    revision: 2,
                    sequence: 2,
                    event: ThreadEvent::TranscriptAppended {
                        entry: entry.clone()
                    },
                }),
            )
            .is_empty()
        );
        assert_eq!(model.sessions[0].transcript.entries.last(), Some(&entry));
        assert_eq!(
            reduce(
                &mut model,
                ThreadUiInput::Event(ThreadEventEnvelope {
                    protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
                    event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
                    thread_id: ready.thread_id,
                    revision: 4,
                    sequence: 3,
                    event: ThreadEvent::LifecycleChanged {
                        lifecycle: ThreadLifecycle::Ready,
                        run_id: Some(run_id),
                    },
                }),
            ),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        assert_eq!(model.connection, ConnectionState::SnapshotRequired);

        reduce(&mut model, ThreadUiInput::Snapshot(vec![ready.clone()]));
        model.sessions[0].lifecycle = ThreadLifecycle::WaitingPermission;
        model.sessions[0].pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-1".into(),
            description: "write file".into(),
            expected_run_revision: 4,
        });
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('d'), KeyModifiers::NONE)),
            vec![ThreadUiAction::ResolvePermission {
                thread_id: ready.thread_id,
                request_id: "permission-1".into(),
                allow: false,
            }]
        );
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::ResolvePermission {
                thread_id: ready.thread_id,
                request_id: "permission-1".into(),
                allow: true,
            }]
        );
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::Cancel {
                thread_id: ready.thread_id
            }]
        );

        model.sessions[0].lifecycle = ThreadLifecycle::WaitingInput;
        model.sessions[0].pending = Some(ThreadPendingRequest::Input {
            run_id,
            request_id: "input-1".into(),
            prompt: "need value".into(),
            expected_run_revision: 4,
        });
        assert!(reduce(&mut model, key(KeyCode::Char('é'), KeyModifiers::NONE)).is_empty());
        assert_eq!(model.input, "é");
        assert!(reduce(&mut model, key(KeyCode::Backspace, KeyModifiers::NONE)).is_empty());
        assert!(model.input.is_empty());
        reduce(&mut model, key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(
            reduce(&mut model, key(KeyCode::F(5), KeyModifiers::NONE)),
            vec![ThreadUiAction::ProvideInput {
                thread_id: ready.thread_id,
                request_id: "input-1".into(),
                value: "v".into(),
            }]
        );

        reduce(&mut model, ThreadUiInput::Disconnected);
        assert!(reduce(&mut model, key(KeyCode::Char('x'), KeyModifiers::NONE)).is_empty());
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('r'), KeyModifiers::NONE)),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        reduce(&mut model, ThreadUiInput::Connected);
        reduce(&mut model, ThreadUiInput::Resize(120, 40));
        assert!(model.shows_sidebar());
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('q'), KeyModifiers::NONE)),
            vec![ThreadUiAction::Quit]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn render_covers_transcript_cards_secondary_sessions_and_small_terminal() {
        let ids = SystemIdSource::default();
        let mut first = snapshot(ThreadLifecycle::WaitingPermission);
        first.pending = Some(ThreadPendingRequest::Permission {
            run_id: RunId::from_uuid(ids.next_uuid_v7()),
            request_id: "p".into(),
            description: "approve write".into(),
            expected_run_revision: 1,
        });
        first.transcript.entries = [
            TranscriptKind::User,
            TranscriptKind::Assistant,
            TranscriptKind::ToolCall,
            TranscriptKind::ToolResult,
            TranscriptKind::Permission,
            TranscriptKind::Input,
            TranscriptKind::Failure,
            TranscriptKind::Completion,
            TranscriptKind::System,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: u64::try_from(index + 1).unwrap(),
            run_id: None,
            kind,
            text: format!("card-{index}"),
            payload: Some(serde_json::json!({"private_checkpoint":"never render this"})),
            source_key: format!("card-{index}"),
            created_at_ms: 1,
        })
        .collect();
        // The list projection intentionally carries only a bounded tail. The
        // renderer must disclose that fact rather than presenting the tail as
        // a complete conversation history.
        first.transcript.has_more = true;
        let mut second = snapshot(ThreadLifecycle::WaitingInput);
        second.pending = Some(ThreadPendingRequest::Input {
            run_id: RunId::from_uuid(ids.next_uuid_v7()),
            request_id: "i".into(),
            prompt: "type a value".into(),
            expected_run_revision: 1,
        });
        let mut model = ThreadUiModel {
            sessions: vec![first, second],
            size: (120, 40),
            composer: "multi\nline".into(),
            progress: vec![
                ThreadTransientProgress::ProviderAttempt {
                    run_id: RunId::from_uuid(ids.next_uuid_v7()),
                    number: 2,
                },
                ThreadTransientProgress::AssistantDelta {
                    run_id: RunId::from_uuid(ids.next_uuid_v7()),
                    text: "delta".into(),
                },
                ThreadTransientProgress::ToolProgress {
                    run_id: RunId::from_uuid(ids.next_uuid_v7()),
                    name: "read_file".into(),
                    detail: "note.txt".into(),
                },
            ],
            help: false,
            ..Default::default()
        };
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Conversation"));
        assert!(rendered.contains("Composer"));
        assert!(rendered.contains("Sessions"));
        assert!(rendered.contains("[tool result] card-3"));
        assert!(rendered.contains("earlier transcript cards are omitted"));
        assert!(rendered.contains("provider attempt 2"));
        assert!(!rendered.contains("private_checkpoint"));

        model.help = true;
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let help = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(help.contains("Transcript thread"));

        model.size = (70, 30);
        model.help = false;
        model.sessions_overlay = true;
        let backend = TestBackend::new(70, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let overlay = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(overlay.contains("Sessions"));

        model.size = (30, 8);
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let safety = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(safety.contains("Terminal too small"));
        assert!(is_submit(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)));
        assert!(is_submit(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));
        assert!(!is_submit(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert_eq!(progress_text(&model.progress[2]), "… read_file: note.txt");
        let mut graphemes = "a👩‍💻".to_owned();
        pop_grapheme(&mut graphemes);
        assert_eq!(graphemes, "a");
        assert_eq!(
            centered(Rect::new(0, 0, 100, 100), 50, 50),
            Rect::new(25, 25, 50, 50)
        );
    }
}
