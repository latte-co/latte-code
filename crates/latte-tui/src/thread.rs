//! Transcript-first Thread v2 presentation.
//!
//! This reducer owns only local text, focus, scroll, and a single safe
//! follow-up queue. It has no provider, repository, or effect authority.

use crate::{
    ConnectionState, TuiError,
    command::{
        BUILTINS, BuiltinCommand, CommandDescriptor, SlashResolution, resolve_slash,
        slash_suggestions,
    },
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use latte_core::{
    RunId, ThreadEvent, ThreadEventEnvelope, ThreadId, ThreadLifecycle, ThreadPendingRequest,
    ThreadRunStatus, ThreadSessionSummary, ThreadSnapshot, ThreadTransientProgress,
    TranscriptEntry, TranscriptKind, redact_thread_text,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    buffer::CellWidth,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::{
    collections::{BTreeSet, VecDeque},
    io::{self, IsTerminal},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(2);
const CTRL_C_DUPLICATE_WINDOW: Duration = Duration::from_millis(120);
const PROVIDER_SETUP_GUIDANCE: &str = "Provider setup required: configure default_model and providers in ~/.latte/latte-code.jsonc, then restart Latte Code";
const MODEL_NOT_CONFIGURED: &str = "Not configured";

/// Thread-only projection boundary; snapshots are authoritative after any
/// event gap and transient progress must be discarded.
pub trait ThreadProjectionClient {
    fn snapshots(&mut self) -> Result<Vec<ThreadSnapshot>, String>;
    fn session_catalog(&mut self) -> Result<Vec<ThreadSessionSummary>, String> {
        self.snapshots().map(|snapshots| {
            snapshots
                .into_iter()
                .map(|snapshot| ThreadSessionSummary {
                    thread_id: snapshot.thread_id,
                    title: snapshot
                        .transcript
                        .entries
                        .iter()
                        .find(|entry| entry.kind == TranscriptKind::User)
                        .map_or_else(|| "Untitled session".into(), |entry| entry.text.clone()),
                    workspace_root: String::new(),
                    parent_thread_id: None,
                    lifecycle: snapshot.lifecycle,
                    provider_name: snapshot.binding.provider_name,
                    model: snapshot.binding.model,
                    created_at_ms: snapshot
                        .transcript
                        .entries
                        .first()
                        .map_or(0, |entry| entry.created_at_ms),
                    updated_at_ms: snapshot
                        .transcript
                        .entries
                        .last()
                        .map_or(0, |entry| entry.created_at_ms),
                })
                .collect()
        })
    }
    fn session(&mut self, thread_id: ThreadId) -> Result<ThreadSnapshot, String> {
        self.snapshots()?
            .into_iter()
            .find(|snapshot| snapshot.thread_id == thread_id)
            .ok_or_else(|| format!("session {thread_id} was not found"))
    }
    fn exact_session_catalog(&mut self, query: &str) -> Result<Vec<ThreadSessionSummary>, String> {
        self.session_catalog().map(|sessions| {
            sessions
                .into_iter()
                .filter(|session| session.thread_id.to_string() == query || session.title == query)
                .collect()
        })
    }
    fn exact_session(&mut self, query: &str) -> Result<Option<ThreadSnapshot>, String> {
        let matches = self.exact_session_catalog(query)?;
        let [session] = matches.as_slice() else {
            return Ok(None);
        };
        self.session(session.thread_id).map(Some)
    }
    fn search_session_catalog(&mut self, query: &str) -> Result<Vec<ThreadSessionSummary>, String> {
        let query = query.trim().to_lowercase();
        self.session_catalog().map(|sessions| {
            sessions
                .into_iter()
                .filter(|session| {
                    query.is_empty()
                        || session.title.to_lowercase().contains(&query)
                        || session.thread_id.to_string().contains(&query)
                })
                .collect()
        })
    }
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
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    /// Confirms that the current model state reached a terminal frame. This
    /// arms only the exact permission request visible in that frame.
    FrameRendered,
    Snapshot(Vec<ThreadSnapshot>),
    SessionCatalog(Vec<ThreadSessionSummary>),
    SessionCatalogReady {
        sessions: Vec<ThreadSessionSummary>,
        query: Option<String>,
    },
    SessionOpened(Box<ThreadSnapshot>),
    Event(ThreadEventEnvelope),
    Progress(ThreadTransientProgress),
    Lagged,
    Connected,
    Disconnected,
    CommandError(String),
    CommandCompleted(String),
    SubmissionAssigned {
        submission_id: u64,
        thread_id: ThreadId,
    },
    SubmissionError {
        submission_id: u64,
    },
    SubmissionCompleted {
        submission_id: u64,
    },
    InputSubmissionError {
        submission_id: u64,
    },
    InputSubmissionCompleted {
        submission_id: u64,
    },
    ModelSwitchError {
        switch_id: u64,
        error: String,
    },
    ModelSwitchCompleted {
        switch_id: u64,
    },
    Tick,
}

/// Runtime completion delivered back to the terminal adapter. Submission
/// feedback carries the reducer-issued identity so an old async failure can
/// never restore a newer prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadUiFeedback {
    SubmissionAssigned {
        submission_id: u64,
        thread_id: ThreadId,
    },
    SubmissionResult {
        submission_id: u64,
        result: Result<String, String>,
    },
    InputSubmissionResult {
        submission_id: u64,
        result: Result<String, String>,
    },
    ModelSwitchResult {
        switch_id: u64,
        result: Result<String, String>,
    },
    Command(Result<String, String>),
    SessionManagement(Result<SessionManagementOutcome, String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionManagementOutcome {
    Updated(String),
    Forked(ThreadId),
}

impl ThreadUiFeedback {
    #[must_use]
    pub const fn assigned(submission_id: u64, thread_id: ThreadId) -> Self {
        Self::SubmissionAssigned {
            submission_id,
            thread_id,
        }
    }

    #[must_use]
    pub fn submission(submission_id: u64, result: Result<String, String>) -> Self {
        Self::SubmissionResult {
            submission_id,
            result,
        }
    }

    #[must_use]
    pub fn input_submission(submission_id: u64, result: Result<String, String>) -> Self {
        Self::InputSubmissionResult {
            submission_id,
            result,
        }
    }

    #[must_use]
    pub fn command(result: Result<String, String>) -> Self {
        Self::Command(result)
    }

    #[must_use]
    pub fn session_management(result: Result<SessionManagementOutcome, String>) -> Self {
        Self::SessionManagement(result)
    }

    #[must_use]
    pub fn model_switch(switch_id: u64, result: Result<String, String>) -> Self {
        Self::ModelSwitchResult { switch_id, result }
    }
}

/// UI commands are thread-level requests. The caller maps them to the
/// headless service; the reducer never executes effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadUiAction {
    Start {
        submission_id: u64,
        prompt: String,
    },
    StartWithModel {
        submission_id: u64,
        prompt: String,
        provider_name: String,
        model: String,
    },
    FollowUp {
        submission_id: u64,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        prompt: String,
    },
    QueueFollowUp {
        submission_id: u64,
        thread_id: ThreadId,
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
        submission_id: u64,
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
    ShowSessions {
        query: Option<String>,
    },
    SearchSessions {
        query: String,
    },
    RenameSession {
        thread_id: ThreadId,
        title: String,
    },
    ForkSession {
        thread_id: ThreadId,
        title: Option<String>,
    },
    OpenSession {
        thread_id: ThreadId,
    },
    SwitchModel {
        switch_id: u64,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        provider_name: String,
        model: String,
    },
    RefreshSnapshots,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSubmission {
    pub submission_id: u64,
    pub prompt: String,
    pub thread_id: Option<ThreadId>,
    pub after_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInputSubmission {
    pub submission_id: u64,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub request_id: String,
    pub value: String,
    pub after_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingModelSwitch {
    pub switch_id: u64,
    pub thread_id: ThreadId,
    pub provider_name: String,
    pub model: String,
}

/// Secret-free startup information used only to present the environment that
/// the CLI has already resolved. Provider credentials and configuration
/// fingerprints deliberately have no representation here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadStartupPresentation {
    pub default_provider: String,
    pub default_model: String,
    pub model_catalog: Vec<ThreadModelOption>,
    pub workspace_display: String,
    pub permission_mode: ThreadPermissionMode,
}

/// Secret-free provider/model row displayed by `/model`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadModelOption {
    pub provider_name: String,
    pub model: String,
    pub name: Option<String>,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelPickerState {
    query: String,
    index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadPermissionMode {
    Ask,
}

impl ThreadPermissionMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
        }
    }

    const fn card_label(self) -> &'static str {
        match self {
            Self::Ask => "Ask mode",
        }
    }
}

/// The composer is the default and owns every printable character. Transcript
/// shortcuts are reachable only after an explicit mode switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThreadFocus {
    #[default]
    Composer,
    Navigation,
}

/// Explicit conversation target. A new draft has no durable Session identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveConversation {
    NewSessionDraft,
    Session(ThreadId),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SlashPopupState {
    #[default]
    Eligible,
    Dismissed,
}

#[derive(Clone, Debug, Default)]
struct SubmissionRefreshState {
    composer: bool,
    input: bool,
}

#[derive(Clone, Debug)]
pub struct ThreadUiModel {
    /// Environment metadata resolved by the startup composition root. This is
    /// display-only and intentionally excludes every credential-bearing
    /// provider field.
    pub startup: Option<ThreadStartupPresentation>,
    /// Projection order is authoritative. The TUI keeps the focused thread by
    /// identity across refreshes but never compares per-thread sequence values.
    pub sessions: Vec<ThreadSnapshot>,
    pub session_catalog: Vec<ThreadSessionSummary>,
    pub active_conversation: Option<ActiveConversation>,
    pub selected: usize,
    pub composer: String,
    pub input: String,
    /// Wrapped display rows above the current transcript tail. Zero follows
    /// new durable cards; `u16::MAX` represents the oldest available row.
    pub scroll: u16,
    pub connection: ConnectionState,
    pub progress: Vec<ThreadTransientProgress>,
    pub status: String,
    pub help: bool,
    /// Explicit command palette opened by Ctrl+P. Keeping this as local UI
    /// state means no palette command can bypass the typed runtime boundary.
    pub command_palette: bool,
    pub command_index: usize,
    slash_index: usize,
    slash_popup_state: SlashPopupState,
    pub session_picker: bool,
    pub session_index: usize,
    model_picker: Option<ModelPickerState>,
    draft_model: Option<(String, String)>,
    pub focus: ThreadFocus,
    pub navigation_index: usize,
    pub expanded_actions: BTreeSet<String>,
    pub size: (u16, u16),
    /// At most one local queued composer submission, dispatched only after a
    /// freshly loaded completed/ready snapshot.
    pub queued_follow_up: Option<String>,
    /// Locally projected user message retained until a matching durable user
    /// transcript entry arrives. While present the composer is intentionally
    /// locked, preventing duplicate Enter dispatches.
    pub pending_submission: Option<PendingSubmission>,
    /// A next-message draft displaced while an earlier submission is restored
    /// after a persistence failure. It returns to the composer after the
    /// restored submission is durably accepted, so eager typing is never lost.
    deferred_composer_draft: Option<String>,
    /// A request-scoped input value retained until its exact durable input
    /// card is observed. It is independent from the composer submission so a
    /// provider/configuration failure cannot consume or restore the wrong
    /// editor.
    pub pending_input_submission: Option<PendingInputSubmission>,
    /// The composer remains locked until the authoritative snapshot contains
    /// the selected binding, preventing a follow-up from racing the switch.
    pub pending_model_switch: Option<PendingModelSwitch>,
    /// Permission decisions are accepted only after the exact request has
    /// reached a rendered frame. Projection updates are processed before
    /// terminal input, so this barrier prevents an already-buffered `d` or
    /// Ctrl+A from resolving a request the user has never seen.
    rendered_permission_request: Option<(ThreadId, String)>,
    input_target: Option<(ThreadId, String)>,
    submission_refresh: SubmissionRefreshState,
    /// Secret-safe presentation copy for a correlated failure before the
    /// submission became durable. Provider/runtime failures are transcript
    /// cards and never take this prompt-restoration path.
    pub submission_error: Option<String>,
    pub next_submission_id: u64,
    pub next_model_switch_id: u64,
    /// A reconciliation acknowledgement is high-impact: it records the
    /// unknown effect as failed and terminalizes the linked child. Opening
    /// this state is not itself an action; only Ctrl+A can dispatch it.
    pub reconciliation_confirmation: Option<(ThreadId, String)>,
    /// Event-only hint retained until the next authoritative snapshot. Normal
    /// projection snapshots derive the same identifier from their durable
    /// failure card, so this is never an authority source.
    pub reconciliation_hint: Option<(ThreadId, String)>,
    /// A first Ctrl+C interrupts active work and arms a short, explicit exit
    /// confirmation. Only a second Ctrl+C inside the window exits the TUI.
    pub ctrl_c_exit_armed_until: Option<Instant>,
    /// Some PTY hosts surface one physical Ctrl+C as both a key event and a
    /// SIGINT. Debounce those duplicate delivery paths without weakening the
    /// two-press confirmation.
    pub ctrl_c_last_observed_at: Option<Instant>,
}

impl Default for ThreadUiModel {
    fn default() -> Self {
        Self {
            startup: None,
            sessions: Vec::new(),
            session_catalog: Vec::new(),
            active_conversation: None,
            selected: 0,
            composer: String::new(),
            input: String::new(),
            scroll: 0,
            connection: ConnectionState::Connected,
            progress: Vec::new(),
            status: "Ready".into(),
            help: false,
            command_palette: false,
            command_index: 0,
            slash_index: 0,
            slash_popup_state: SlashPopupState::Eligible,
            session_picker: false,
            session_index: 0,
            model_picker: None,
            draft_model: None,
            focus: ThreadFocus::Composer,
            navigation_index: 0,
            expanded_actions: BTreeSet::new(),
            size: (80, 24),
            queued_follow_up: None,
            pending_submission: None,
            deferred_composer_draft: None,
            pending_input_submission: None,
            pending_model_switch: None,
            rendered_permission_request: None,
            input_target: None,
            submission_refresh: SubmissionRefreshState::default(),
            submission_error: None,
            next_submission_id: 1,
            next_model_switch_id: 1,
            reconciliation_confirmation: None,
            reconciliation_hint: None,
            ctrl_c_exit_armed_until: None,
            ctrl_c_last_observed_at: None,
        }
    }
}

impl ThreadUiModel {
    #[must_use]
    pub fn with_startup(startup: ThreadStartupPresentation) -> Self {
        let draft_model = (!startup.default_provider.is_empty()
            && !startup.default_model.is_empty())
        .then(|| {
            (
                startup.default_provider.clone(),
                startup.default_model.clone(),
            )
        });
        let status = if draft_model.is_some() {
            "Ready".into()
        } else {
            PROVIDER_SETUP_GUIDANCE.into()
        };
        Self {
            startup: Some(startup),
            draft_model,
            status,
            active_conversation: Some(ActiveConversation::NewSessionDraft),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn selected_thread(&self) -> Option<&ThreadSnapshot> {
        match self.active_conversation {
            Some(ActiveConversation::NewSessionDraft) => None,
            Some(ActiveConversation::Session(thread_id)) => self
                .sessions
                .iter()
                .find(|thread| thread.thread_id == thread_id),
            None => self.sessions.get(self.selected),
        }
    }
    #[must_use]
    pub fn authority_enabled(&self) -> bool {
        self.connection == ConnectionState::Connected
    }

    fn mark_selected_permission_rendered(&mut self) {
        self.rendered_permission_request = self.selected_thread().and_then(|thread| {
            let ThreadPendingRequest::Permission { request_id, .. } = thread.pending.as_ref()?
            else {
                return None;
            };
            Some((thread.thread_id, request_id.clone()))
        });
    }

    fn permission_was_rendered(&self, thread_id: ThreadId, request_id: &str) -> bool {
        self.rendered_permission_request.as_ref().is_some_and(
            |(rendered_thread_id, rendered_request_id)| {
                *rendered_thread_id == thread_id && rendered_request_id == request_id
            },
        )
    }
}

#[allow(clippy::too_many_lines)]
pub fn reduce(model: &mut ThreadUiModel, input: ThreadUiInput) -> Vec<ThreadUiAction> {
    match input {
        ThreadUiInput::Resize(width, height) => model.size = (width, height),
        ThreadUiInput::FrameRendered => model.mark_selected_permission_rendered(),
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
        ThreadUiInput::Snapshot(sessions) => {
            let selected_id = model.selected_thread().map(|thread| thread.thread_id);
            model.sessions = sessions;
            if model.active_conversation == Some(ActiveConversation::NewSessionDraft)
                && let Some(pending) = model.pending_submission.as_ref()
                && let Some(thread_id) = pending.thread_id
                && model
                    .sessions
                    .iter()
                    .any(|thread| thread.thread_id == thread_id)
            {
                model.active_conversation = Some(ActiveConversation::Session(thread_id));
            }
            model.selected = selected_id
                .or(match model.active_conversation {
                    Some(ActiveConversation::Session(thread_id)) => Some(thread_id),
                    Some(ActiveConversation::NewSessionDraft) | None => None,
                })
                .and_then(|id| {
                    model
                        .sessions
                        .iter()
                        .position(|thread| thread.thread_id == id)
                })
                .unwrap_or(0)
                .min(model.sessions.len().saturating_sub(1));
            model.navigation_index = model
                .navigation_index
                .min(action_keys(model.selected_thread()).len().saturating_sub(1));
            model.connection = ConnectionState::Connected;
            model.progress.clear();
            model.status = "Transcript synchronized".into();
            if model.selected_thread().is_some_and(|thread| {
                thread.lifecycle == ThreadLifecycle::ReconciliationRequired
                    || matches!(
                        thread.pending,
                        Some(ThreadPendingRequest::Permission { .. })
                    )
            }) {
                model.command_palette = false;
                model.help = false;
            }
            model.reconciliation_hint = model.selected_thread().and_then(|thread| {
                reconciliation_effect_from_snapshot(thread)
                    .map(|effect_id| (thread.thread_id, effect_id))
            });
            reconcile_pending_submission(model);
            reconcile_pending_input_submission(model);
            reconcile_pending_model_switch(model);
            finalize_failed_submissions_after_snapshot(model);
            restore_stranded_follow_up(model);
            synchronize_input_target(model);
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
            if model.queued_follow_up.is_some()
                && let Some((thread_id, expected_thread_revision)) = model
                    .selected_thread()
                    .filter(|thread| thread.lifecycle == ThreadLifecycle::Ready)
                    .map(|thread| (thread.thread_id, thread.revision))
                && let Some(submission_id) = model
                    .pending_submission
                    .as_ref()
                    .map(|submission| submission.submission_id)
            {
                let prompt = model.queued_follow_up.take().unwrap_or_default();
                return vec![ThreadUiAction::FollowUp {
                    submission_id,
                    thread_id,
                    expected_thread_revision,
                    prompt,
                }];
            }
        }
        ThreadUiInput::SessionCatalog(sessions) => {
            model.session_catalog = sessions;
            model.session_index = model
                .session_index
                .min(model.session_catalog.len().saturating_sub(1));
        }
        ThreadUiInput::SessionCatalogReady { sessions, query } => {
            model.session_catalog = sessions;
            model.session_index = 0;
            let Some(query) = query.filter(|query| !query.is_empty()) else {
                model.session_picker = true;
                model.status = if model.session_catalog.is_empty() {
                    "No saved sessions".into()
                } else {
                    "Select a session to resume".into()
                };
                return Vec::new();
            };
            let matches = model
                .session_catalog
                .iter()
                .filter(|session| session.thread_id.to_string() == query || session.title == query)
                .map(|session| session.thread_id)
                .collect::<Vec<_>>();
            if matches.len() == 1 {
                return vec![ThreadUiAction::OpenSession {
                    thread_id: matches[0],
                }];
            }
            model.session_picker = true;
            model.status = if model.session_catalog.is_empty() {
                format!("No exact session match for {query}")
            } else if matches.is_empty() {
                format!(
                    "{} sessions match {query}; choose one",
                    model.session_catalog.len()
                )
            } else {
                format!("Multiple sessions match {query}; choose one")
            };
        }
        ThreadUiInput::SessionOpened(snapshot) => {
            let snapshot = *snapshot;
            let thread_id = snapshot.thread_id;
            model.sessions = vec![snapshot];
            model.selected = 0;
            model.active_conversation = Some(ActiveConversation::Session(thread_id));
            model.session_picker = false;
            model.command_palette = false;
            model.help = false;
            model.progress.clear();
            model.scroll = 0;
            model.status = format!("Resumed session {thread_id}");
            model.reconciliation_hint = model.selected_thread().and_then(|thread| {
                reconciliation_effect_from_snapshot(thread)
                    .map(|effect_id| (thread.thread_id, effect_id))
            });
            reconcile_pending_submission(model);
            reconcile_pending_input_submission(model);
            reconcile_pending_model_switch(model);
            finalize_failed_submissions_after_snapshot(model);
            restore_stranded_follow_up(model);
            synchronize_input_target(model);
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
                ThreadEvent::BindingChanged { .. } => {
                    model.connection = ConnectionState::SnapshotRequired;
                    return vec![ThreadUiAction::RefreshSnapshots];
                }
                ThreadEvent::ReconciliationRequired { effect_id, .. } => {
                    thread.lifecycle = ThreadLifecycle::ReconciliationRequired;
                    model.reconciliation_hint = Some((thread.thread_id, effect_id));
                }
            }
            reconcile_pending_submission(model);
            reconcile_pending_input_submission(model);
            synchronize_input_target(model);
        }
        ThreadUiInput::Progress(progress) => record_progress(model, progress),
        ThreadUiInput::CommandError(error) => model.status = format!("Command rejected: {error}"),
        ThreadUiInput::CommandCompleted(message) => model.status = message,
        ThreadUiInput::ModelSwitchError { switch_id, error } => {
            if model
                .pending_model_switch
                .as_ref()
                .is_some_and(|pending| pending.switch_id == switch_id)
            {
                model.pending_model_switch = None;
                model.status = format!("Model switch rejected: {error}");
            }
        }
        ThreadUiInput::ModelSwitchCompleted { switch_id } => {
            if model
                .pending_model_switch
                .as_ref()
                .is_some_and(|pending| pending.switch_id == switch_id)
            {
                model.status = "Model switch accepted; synchronizing session".into();
                return vec![ThreadUiAction::RefreshSnapshots];
            }
        }
        ThreadUiInput::SubmissionAssigned {
            submission_id,
            thread_id,
        } => {
            let assigned = if let Some(pending) = model
                .pending_submission
                .as_mut()
                .filter(|pending| pending.submission_id == submission_id)
            {
                pending.thread_id = Some(thread_id);
                true
            } else {
                false
            };
            if assigned {
                if model.active_conversation == Some(ActiveConversation::NewSessionDraft)
                    && let Some(index) = model
                        .sessions
                        .iter()
                        .position(|thread| thread.thread_id == thread_id)
                {
                    model.active_conversation = Some(ActiveConversation::Session(thread_id));
                    model.selected = index;
                }
                reconcile_pending_submission(model);
            }
        }
        ThreadUiInput::SubmissionError { submission_id } => {
            if model
                .pending_submission
                .as_ref()
                .is_some_and(|pending| pending.submission_id == submission_id)
            {
                model.submission_refresh.composer = true;
                model.status = "Submission result uncertain; checking durable transcript".into();
                return vec![ThreadUiAction::RefreshSnapshots];
            }
        }
        ThreadUiInput::SubmissionCompleted { submission_id } => {
            if model
                .pending_submission
                .as_ref()
                .is_some_and(|pending| pending.submission_id == submission_id)
            {
                model.status = "Submission accepted; synchronizing transcript".into();
            }
        }
        ThreadUiInput::InputSubmissionError { submission_id } => {
            if model
                .pending_input_submission
                .as_ref()
                .is_some_and(|pending| pending.submission_id == submission_id)
            {
                model.submission_refresh.input = true;
                model.status = "Input result uncertain; checking durable transcript".into();
                return vec![ThreadUiAction::RefreshSnapshots];
            }
        }
        ThreadUiInput::InputSubmissionCompleted { submission_id } => {
            if model
                .pending_input_submission
                .as_ref()
                .is_some_and(|pending| pending.submission_id == submission_id)
            {
                model.status = "Input accepted; synchronizing transcript".into();
            }
        }
        ThreadUiInput::Key(key) => return reduce_key(model, key),
        ThreadUiInput::Mouse(mouse) => reduce_mouse(model, mouse),
        ThreadUiInput::Paste(value) => reduce_paste(model, &value),
        ThreadUiInput::Tick => {}
    }
    Vec::new()
}

#[allow(clippy::too_many_lines)]
fn reduce_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    reduce_key_at(model, key, Instant::now())
}

fn reduce_mouse(model: &mut ThreadUiModel, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => model.scroll = model.scroll.saturating_add(3),
        MouseEventKind::ScrollDown => model.scroll = model.scroll.saturating_sub(3),
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
fn reduce_key_at(model: &mut ThreadUiModel, key: KeyEvent, now: Instant) -> Vec<ThreadUiAction> {
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    if key.code == KeyCode::F(10) {
        return vec![ThreadUiAction::Quit];
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if model.ctrl_c_last_observed_at.is_some_and(|observed_at| {
            now.saturating_duration_since(observed_at) <= CTRL_C_DUPLICATE_WINDOW
        }) {
            return Vec::new();
        }
        model.ctrl_c_last_observed_at = Some(now);
        if model
            .ctrl_c_exit_armed_until
            .is_some_and(|deadline| now <= deadline)
        {
            model.ctrl_c_exit_armed_until = None;
            model.ctrl_c_last_observed_at = None;
            return vec![ThreadUiAction::Quit];
        }
        model.ctrl_c_exit_armed_until = Some(now + CTRL_C_EXIT_WINDOW);
        if let Some(thread) = model.selected_thread().filter(|thread| {
            model.authority_enabled()
                && matches!(
                    thread.lifecycle,
                    ThreadLifecycle::Running
                        | ThreadLifecycle::WaitingPermission
                        | ThreadLifecycle::WaitingInput
                )
        }) {
            return vec![ThreadUiAction::Cancel {
                thread_id: thread.thread_id,
            }];
        }
        return Vec::new();
    }
    model.ctrl_c_exit_armed_until = None;
    model.ctrl_c_last_observed_at = None;
    if key.code == KeyCode::Char('r')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && model.connection != ConnectionState::Connected
    {
        return vec![ThreadUiAction::RefreshSnapshots];
    }
    if let Some((thread_id, effect_id)) = model.reconciliation_confirmation.clone() {
        // Enter is deliberately inert while the confirmation card is open:
        // it must never be able to acknowledge a potentially executed effect.
        if key.code == KeyCode::Enter {
            return Vec::new();
        }
        if model.authority_enabled()
            && key.code == KeyCode::Char('a')
            && key.modifiers == KeyModifiers::CONTROL
        {
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
        if thread.lifecycle == ThreadLifecycle::ReconciliationRequired {
            if model.authority_enabled()
                && key.code == KeyCode::Char('r')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && let Some(effect_id) = reconciliation_effect_id(model, &thread)
            {
                model.reconciliation_confirmation = Some((thread.thread_id, effect_id));
                model.status =
                    "Reconciliation acknowledgement open: Ctrl+A confirms; Enter does nothing"
                        .into();
            }
            // Reconciliation owns the entire event. In particular, printable
            // keys and Enter cannot leak into either text buffer.
            return Vec::new();
        }
        if let Some(ThreadPendingRequest::Permission { request_id, .. }) = thread.pending.as_ref() {
            if model.authority_enabled()
                && model.permission_was_rendered(thread.thread_id, request_id)
                && key.code == KeyCode::Char('d')
                && key.modifiers.is_empty()
            {
                return vec![ThreadUiAction::ResolvePermission {
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    allow: false,
                }];
            }
            if model.authority_enabled()
                && model.permission_was_rendered(thread.thread_id, request_id)
                && key.code == KeyCode::Char('a')
                && key.modifiers == KeyModifiers::CONTROL
            {
                return vec![ThreadUiAction::ResolvePermission {
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    allow: true,
                }];
            }
            // Permission owns the entire event. Enter, printable characters,
            // and navigation keys cannot mutate any pending input/composer.
            return Vec::new();
        }
        if let Some(ThreadPendingRequest::Input {
            run_id, request_id, ..
        }) = thread.pending.as_ref()
        {
            if model.pending_input_submission.is_some() {
                return Vec::new();
            }
            if model.authority_enabled() && is_submit(key) && !model.input.trim().is_empty() {
                let value = std::mem::take(&mut model.input);
                let submission_id = model.next_submission_id;
                model.next_submission_id = model.next_submission_id.saturating_add(1);
                model.pending_input_submission = Some(PendingInputSubmission {
                    submission_id,
                    thread_id: thread.thread_id,
                    run_id: *run_id,
                    request_id: request_id.clone(),
                    value: value.clone(),
                    after_sequence: thread.sequence,
                });
                model.submission_refresh.input = false;
                return vec![ThreadUiAction::ProvideInput {
                    submission_id,
                    thread_id: thread.thread_id,
                    request_id: request_id.clone(),
                    value,
                }];
            }
            if key.code == KeyCode::Backspace {
                pop_grapheme(&mut model.input);
                return Vec::new();
            }
            if is_newline(key) && model.input.len() < 16 * 1024 {
                model.input.push('\n');
                return Vec::new();
            }
            if let KeyCode::Char(value) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && model.input.len() < 16 * 1024
            {
                model.input.push(value);
                return Vec::new();
            }
            // Waiting-input also owns the event; unhandled keys never fall
            // through into the follow-up composer.
            return Vec::new();
        }
    }
    if model.model_picker.is_some() {
        return reduce_model_picker_key(model, key);
    }
    if model.session_picker {
        return reduce_session_picker_key(model, key);
    }
    if model.command_palette {
        return reduce_palette_key(model, key);
    }
    if let Some(actions) = reduce_slash_popup_key(model, key) {
        return actions;
    }
    if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
        model.command_palette = true;
        model.command_index = 0;
        model.help = false;
        return Vec::new();
    }
    match model.focus {
        ThreadFocus::Composer => reduce_composer_key(model, key),
        ThreadFocus::Navigation => reduce_navigation_key(model, key),
    }
}

fn slash_popup_suggestions(model: &ThreadUiModel) -> Vec<&'static CommandDescriptor> {
    if model.focus != ThreadFocus::Composer
        || model.pending_submission.is_some()
        || model.slash_popup_state == SlashPopupState::Dismissed
        || model.help
        || model.command_palette
        || model.session_picker
        || model.model_picker.is_some()
        || model.selected_thread().is_some_and(|thread| {
            matches!(
                thread.lifecycle,
                ThreadLifecycle::WaitingPermission
                    | ThreadLifecycle::WaitingInput
                    | ThreadLifecycle::ReconciliationRequired
            ) || thread.pending.is_some()
        })
    {
        return Vec::new();
    }
    slash_suggestions(&model.composer)
}

fn reduce_slash_popup_key(model: &mut ThreadUiModel, key: KeyEvent) -> Option<Vec<ThreadUiAction>> {
    let suggestions = slash_popup_suggestions(model);
    if suggestions.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            model.slash_popup_state = SlashPopupState::Dismissed;
            Some(Vec::new())
        }
        KeyCode::Up if key.modifiers.is_empty() => {
            model.slash_index = model.slash_index.saturating_sub(1);
            Some(Vec::new())
        }
        KeyCode::Down if key.modifiers.is_empty() => {
            model.slash_index = (model.slash_index + 1).min(suggestions.len().saturating_sub(1));
            Some(Vec::new())
        }
        KeyCode::Enter
            if model.authority_enabled()
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL) =>
        {
            let command = suggestions[model.slash_index.min(suggestions.len().saturating_sub(1))];
            if !matches!(
                command.id,
                BuiltinCommand::New | BuiltinCommand::Sessions | BuiltinCommand::Model
            ) || session_switch_available(model)
            {
                model.composer.clear();
                model.slash_index = 0;
                model.slash_popup_state = SlashPopupState::Eligible;
            }
            Some(dispatch_builtin(model, command.id, String::new()))
        }
        _ => None,
    }
}

fn reduce_palette_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    match key.code {
        KeyCode::Esc => {
            model.command_palette = false;
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            model.command_palette = false;
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            model.command_index = model.command_index.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            model.command_index = (model.command_index + 1).min(BUILTINS.len() - 1);
        }
        KeyCode::Enter => {
            let command = &BUILTINS[model.command_index.min(BUILTINS.len().saturating_sub(1))];
            model.command_palette = false;
            return dispatch_builtin(model, command.id, String::new());
        }
        _ => {}
    }
    Vec::new()
}

fn reduce_session_picker_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    match key.code {
        KeyCode::Esc => model.session_picker = false,
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            model.session_index = model.session_index.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            model.session_index =
                (model.session_index + 1).min(model.session_catalog.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            if let Some(session) = model.session_catalog.get(model.session_index) {
                return vec![ThreadUiAction::OpenSession {
                    thread_id: session.thread_id,
                }];
            }
        }
        _ => {}
    }
    Vec::new()
}

fn filtered_model_options(model: &ThreadUiModel) -> Vec<ThreadModelOption> {
    let Some(startup) = model.startup.as_ref() else {
        return Vec::new();
    };
    let query = model
        .model_picker
        .as_ref()
        .map_or("", |picker| picker.query.trim());
    let query = query.to_ascii_lowercase();
    startup
        .model_catalog
        .iter()
        .filter(|option| {
            query.is_empty()
                || option.provider_name.to_ascii_lowercase().contains(&query)
                || option.model.to_ascii_lowercase().contains(&query)
                || option
                    .name
                    .as_ref()
                    .is_some_and(|name| name.to_ascii_lowercase().contains(&query))
                || format!("{}/{}", option.provider_name, option.model)
                    .to_ascii_lowercase()
                    .contains(&query)
        })
        .cloned()
        .collect()
}

fn selected_provider_model(model: &ThreadUiModel) -> Option<(String, String)> {
    model.selected_thread().map_or_else(
        || model.draft_model.clone(),
        |thread| {
            Some((
                thread.binding.provider_name.clone(),
                thread.binding.model.clone(),
            ))
        },
    )
}

fn open_model_picker(model: &mut ThreadUiModel) {
    let options = model
        .startup
        .as_ref()
        .map_or(&[][..], |startup| startup.model_catalog.as_slice());
    if options.is_empty() {
        model.status = PROVIDER_SETUP_GUIDANCE.into();
        return;
    }
    let selected = selected_provider_model(model);
    let index = selected
        .and_then(|(provider, selected_model)| {
            options.iter().position(|option| {
                option.provider_name == provider && option.model == selected_model
            })
        })
        .unwrap_or(0);
    model.model_picker = Some(ModelPickerState {
        query: String::new(),
        index,
    });
    model.help = false;
    model.command_palette = false;
    model.session_picker = false;
}

fn reduce_model_picker_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    let options = filtered_model_options(model);
    let Some(picker) = model.model_picker.as_mut() else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Esc => model.model_picker = None,
        KeyCode::Up if key.modifiers.is_empty() => {
            picker.index = picker.index.saturating_sub(1);
        }
        KeyCode::Down if key.modifiers.is_empty() => {
            picker.index = (picker.index + 1).min(options.len().saturating_sub(1));
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            pop_grapheme(&mut picker.query);
            picker.index = 0;
        }
        KeyCode::Char(value) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if picker.query.len() < 256 && !value.is_control() {
                picker.query.push(value);
                picker.index = 0;
            }
        }
        KeyCode::Enter if key.modifiers.is_empty() => {
            let Some(option) = options.get(picker.index).cloned() else {
                return Vec::new();
            };
            model.model_picker = None;
            if let Some(thread) = model.selected_thread() {
                if !thread.lifecycle.accepts_follow_up()
                    || thread.active_run_id.is_some()
                    || thread.pending.is_some()
                {
                    model.status =
                        "Model switching is disabled while work or a request is active".into();
                    return Vec::new();
                }
                if thread.binding.provider_name == option.provider_name
                    && thread.binding.model == option.model
                {
                    model.status = format!(
                        "Model already selected: {}/{}",
                        option.provider_name, option.model
                    );
                    return Vec::new();
                }
                let thread_id = thread.thread_id;
                let expected_thread_revision = thread.revision;
                let switch_id = model.next_model_switch_id;
                model.next_model_switch_id = model.next_model_switch_id.saturating_add(1);
                model.pending_model_switch = Some(PendingModelSwitch {
                    switch_id,
                    thread_id,
                    provider_name: option.provider_name.clone(),
                    model: option.model.clone(),
                });
                model.status = format!(
                    "Switching model to {}/{}",
                    option.provider_name, option.model
                );
                return vec![ThreadUiAction::SwitchModel {
                    switch_id,
                    thread_id,
                    expected_thread_revision,
                    provider_name: option.provider_name,
                    model: option.model,
                }];
            }
            model.draft_model = Some((option.provider_name.clone(), option.model.clone()));
            model.status = format!(
                "New sessions will use {}/{}",
                option.provider_name, option.model
            );
        }
        _ => {}
    }
    Vec::new()
}

fn session_switch_available(model: &ThreadUiModel) -> bool {
    model.pending_submission.is_none()
        && model.pending_model_switch.is_none()
        && model.selected_thread().is_none_or(|thread| {
            thread.active_run_id.is_none()
                && thread.pending.is_none()
                && thread.lifecycle != ThreadLifecycle::ReconciliationRequired
        })
}

fn dispatch_builtin(
    model: &mut ThreadUiModel,
    command: BuiltinCommand,
    argument: String,
) -> Vec<ThreadUiAction> {
    if matches!(
        command,
        BuiltinCommand::New
            | BuiltinCommand::Sessions
            | BuiltinCommand::Model
            | BuiltinCommand::Rename
            | BuiltinCommand::Fork
    ) && !session_switch_available(model)
    {
        model.status = "Session switching is disabled while work or a request is active".into();
        return Vec::new();
    }
    match command {
        BuiltinCommand::New => {
            model.sessions.clear();
            model.selected = 0;
            model.active_conversation = Some(ActiveConversation::NewSessionDraft);
            model.session_picker = false;
            model.model_picker = None;
            model.draft_model = model.startup.as_ref().and_then(|startup| {
                (!startup.default_provider.is_empty() && !startup.default_model.is_empty()).then(
                    || {
                        (
                            startup.default_provider.clone(),
                            startup.default_model.clone(),
                        )
                    },
                )
            });
            model.help = false;
            model.progress.clear();
            model.scroll = 0;
            model.status = "New conversation draft".into();
            Vec::new()
        }
        BuiltinCommand::Sessions => dispatch_session_search(model, &argument),
        BuiltinCommand::Rename => {
            selected_session_action(model, |thread_id| ThreadUiAction::RenameSession {
                thread_id,
                title: argument,
            })
        }
        BuiltinCommand::Fork => {
            selected_session_action(model, |thread_id| ThreadUiAction::ForkSession {
                thread_id,
                title: (!argument.is_empty()).then_some(argument),
            })
        }
        BuiltinCommand::Model => {
            open_model_picker(model);
            Vec::new()
        }
        BuiltinCommand::Help => {
            model.help = true;
            Vec::new()
        }
        BuiltinCommand::Navigation => {
            model.focus = ThreadFocus::Navigation;
            model.help = false;
            Vec::new()
        }
        BuiltinCommand::Refresh => vec![ThreadUiAction::RefreshSnapshots],
        BuiltinCommand::Quit => vec![ThreadUiAction::Quit],
    }
}

fn selected_session_action(
    model: &mut ThreadUiModel,
    action: impl FnOnce(ThreadId) -> ThreadUiAction,
) -> Vec<ThreadUiAction> {
    let Some(thread_id) = model.selected_thread().map(|thread| thread.thread_id) else {
        model.status = "This command requires an open saved session".into();
        return Vec::new();
    };
    vec![action(thread_id)]
}

fn parse_thread_id_argument(value: &str) -> Option<ThreadId> {
    serde_json::from_value(serde_json::Value::String(value.into())).ok()
}

fn dispatch_session_search(model: &mut ThreadUiModel, argument: &str) -> Vec<ThreadUiAction> {
    if argument.is_empty() {
        return vec![ThreadUiAction::ShowSessions { query: None }];
    }
    if parse_thread_id_argument(argument).is_some() {
        return vec![ThreadUiAction::ShowSessions {
            query: Some(argument.into()),
        }];
    }
    if argument
        .split_whitespace()
        .any(|part| part.starts_with("--"))
    {
        model.status = "The /sessions command accepts only a current-workspace query".into();
        return Vec::new();
    }
    vec![ThreadUiAction::SearchSessions {
        query: argument.into(),
    }]
}

fn reduce_composer_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    match key.code {
        KeyCode::Esc => {
            model.focus = ThreadFocus::Navigation;
            model.help = false;
        }
        KeyCode::Backspace => {
            pop_grapheme(&mut model.composer);
            model.slash_index = 0;
            model.slash_popup_state = SlashPopupState::Eligible;
        }
        KeyCode::Enter if is_newline(key) => {
            model.composer.push('\n');
            model.slash_index = 0;
            model.slash_popup_state = SlashPopupState::Eligible;
        }
        KeyCode::F(5) | KeyCode::Enter
            if is_submit(key)
                && model.authority_enabled()
                && model.pending_submission.is_none() =>
        {
            return submit_composer(model);
        }
        KeyCode::Char(value)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && model.composer.len() < 16 * 1024 =>
        {
            model.composer.push(value);
            model.slash_index = 0;
            model.slash_popup_state = SlashPopupState::Eligible;
        }
        _ => {}
    }
    Vec::new()
}

fn reduce_navigation_key(model: &mut ThreadUiModel, key: KeyEvent) -> Vec<ThreadUiAction> {
    let keys = action_keys(model.selected_thread());
    match key.code {
        KeyCode::Esc | KeyCode::Char('i') => {
            model.focus = ThreadFocus::Composer;
            model.help = false;
        }
        KeyCode::Char('q') if key.modifiers.is_empty() => return vec![ThreadUiAction::Quit],
        KeyCode::Char('?') if key.modifiers.is_empty() => model.help = !model.help,
        KeyCode::Up | KeyCode::Char('k') => {
            model.navigation_index = model.navigation_index.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            model.navigation_index = (model.navigation_index + 1).min(keys.len().saturating_sub(1));
        }
        KeyCode::PageUp => model.scroll = model.scroll.saturating_add(8),
        KeyCode::PageDown => model.scroll = model.scroll.saturating_sub(8),
        KeyCode::Home => model.scroll = u16::MAX,
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(key) = keys.get(model.navigation_index)
                && !model.expanded_actions.remove(key)
            {
                model.expanded_actions.insert(key.clone());
            }
        }
        KeyCode::Left => {
            if let Some(key) = keys.get(model.navigation_index) {
                model.expanded_actions.remove(key);
            }
        }
        KeyCode::Right => {
            if let Some(key) = keys.get(model.navigation_index) {
                model.expanded_actions.insert(key.clone());
            }
        }
        _ => {}
    }
    Vec::new()
}

fn reduce_paste(model: &mut ThreadUiModel, value: &str) {
    model.ctrl_c_exit_armed_until = None;
    model.ctrl_c_last_observed_at = None;
    let blocked = model.pending_input_submission.is_some()
        || model.reconciliation_confirmation.is_some()
        || model.selected_thread().is_some_and(|thread| {
            thread.lifecycle == ThreadLifecycle::ReconciliationRequired
                || matches!(
                    thread.pending,
                    Some(ThreadPendingRequest::Permission { .. })
                )
        });
    if blocked {
        return;
    }
    if model
        .selected_thread()
        .is_some_and(|thread| matches!(thread.pending, Some(ThreadPendingRequest::Input { .. })))
    {
        append_editor_text(&mut model.input, value, 16 * 1024);
    } else if model.focus == ThreadFocus::Composer {
        append_editor_text(&mut model.composer, value, 16 * 1024);
        model.slash_index = 0;
        model.slash_popup_state = SlashPopupState::Eligible;
    }
}

fn record_progress(model: &mut ThreadUiModel, progress: ThreadTransientProgress) {
    if model.connection != ConnectionState::Connected {
        return;
    }
    match progress {
        ThreadTransientProgress::AssistantDelta { run_id, text } => {
            if let Some(ThreadTransientProgress::AssistantDelta {
                run_id: current_run,
                text: current,
            }) = model.progress.last_mut()
                && *current_run == run_id
            {
                append_bounded(current, &text, 16 * 1024);
                return;
            }
            if model.progress.len() < 64 {
                model
                    .progress
                    .push(ThreadTransientProgress::AssistantDelta { run_id, text });
            }
        }
        ThreadTransientProgress::ProviderAttempt { run_id, number } => {
            if let Some(ThreadTransientProgress::ProviderAttempt {
                run_id: current_run,
                number: current,
            }) = model.progress.iter_mut().rev().find(|item| {
                matches!(item, ThreadTransientProgress::ProviderAttempt { run_id: candidate, .. } if *candidate == run_id)
            }) {
                *current_run = run_id;
                *current = number;
            } else if model.progress.len() < 64 {
                model
                    .progress
                    .push(ThreadTransientProgress::ProviderAttempt { run_id, number });
            }
        }
        ThreadTransientProgress::ToolProgress {
            run_id,
            name,
            detail,
        } => {
            if let Some(ThreadTransientProgress::ToolProgress {
                detail: current, ..
            }) = model.progress.iter_mut().rev().find(|item| {
                matches!(item, ThreadTransientProgress::ToolProgress { run_id: candidate, name: candidate_name, .. } if *candidate == run_id && candidate_name == &name)
            }) {
                *current = detail;
            } else if model.progress.len() < 64 {
                model.progress.push(ThreadTransientProgress::ToolProgress {
                    run_id,
                    name,
                    detail,
                });
            }
        }
    }
}

fn append_bounded(target: &mut String, value: &str, cap: usize) {
    for ch in value.chars() {
        if target.len() + ch.len_utf8() > cap {
            break;
        }
        target.push(ch);
    }
}

fn append_editor_text(target: &mut String, value: &str, cap: usize) {
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        if target.len() + ch.len_utf8() > cap {
            break;
        }
        target.push(ch);
    }
}

fn is_submit(key: KeyEvent) -> bool {
    key.code == KeyCode::F(5)
        || (key.code == KeyCode::Enter
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL))
}

fn is_newline(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers == KeyModifiers::SHIFT
}

fn submit_slash_candidate(model: &mut ThreadUiModel) -> Option<Vec<ThreadUiAction>> {
    match resolve_slash(&model.composer) {
        SlashResolution::ValidationError(error) => {
            model.status = error;
            Some(Vec::new())
        }
        SlashResolution::Command {
            descriptor,
            argument,
        } => {
            if matches!(
                descriptor.id,
                BuiltinCommand::New | BuiltinCommand::Sessions | BuiltinCommand::Model
            ) && !session_switch_available(model)
            {
                model.status =
                    "Session switching is disabled while work or a request is active".into();
                return Some(Vec::new());
            }
            model.composer.clear();
            Some(dispatch_builtin(model, descriptor.id, argument))
        }
        SlashResolution::Prompt => None,
    }
}

fn submit_composer(model: &mut ThreadUiModel) -> Vec<ThreadUiAction> {
    if model.pending_submission.is_some() || model.composer.trim().is_empty() {
        return Vec::new();
    }
    if model.pending_model_switch.is_some() {
        model.status = "Wait for the model switch to finish before submitting".into();
        return Vec::new();
    }
    if let Some(actions) = submit_slash_candidate(model) {
        return actions;
    }
    if model.selected_thread().is_none()
        && model.startup.is_some()
        && selected_provider_model(model).is_none()
    {
        model.status = PROVIDER_SETUP_GUIDANCE.into();
        return Vec::new();
    }
    if let Some(thread) = model.selected_thread()
        && thread.lifecycle != ThreadLifecycle::Ready
        && !(thread.lifecycle == ThreadLifecycle::Running
            && thread.active_run_id.is_some()
            && thread.pending.is_none())
    {
        model.status =
            "This session has no runnable child; use /new or /sessions before submitting".into();
        return Vec::new();
    }
    let prompt = std::mem::take(&mut model.composer);
    let submission_id = model.next_submission_id;
    model.next_submission_id = model.next_submission_id.saturating_add(1);
    let (thread_id, after_sequence) = model.selected_thread().map_or((None, 0), |thread| {
        (
            Some(thread.thread_id),
            thread
                .transcript
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .max()
                .unwrap_or(0),
        )
    });
    model.pending_submission = Some(PendingSubmission {
        submission_id,
        prompt: prompt.clone(),
        thread_id,
        after_sequence,
    });
    model.submission_error = None;
    match model.selected_thread() {
        None => match selected_provider_model(model) {
            Some((provider_name, selected_model))
                if model.startup.as_ref().is_some_and(|startup| {
                    startup.default_provider != provider_name
                        || startup.default_model != selected_model
                }) =>
            {
                vec![ThreadUiAction::StartWithModel {
                    submission_id,
                    prompt,
                    provider_name,
                    model: selected_model,
                }]
            }
            _ => vec![ThreadUiAction::Start {
                submission_id,
                prompt,
            }],
        },
        Some(thread) if thread.lifecycle == ThreadLifecycle::Ready => {
            vec![ThreadUiAction::FollowUp {
                submission_id,
                thread_id: thread.thread_id,
                expected_thread_revision: thread.revision,
                prompt,
            }]
        }
        Some(thread)
            if thread.lifecycle == ThreadLifecycle::Running
                && thread.active_run_id.is_some()
                && thread.pending.is_none()
                && model.queued_follow_up.is_none() =>
        {
            let thread_id = thread.thread_id;
            model.status = "Follow-up queued behind the active turn".into();
            vec![ThreadUiAction::QueueFollowUp {
                submission_id,
                thread_id,
                prompt,
            }]
        }
        Some(_) => {
            model.status = "A follow-up is already queued".into();
            model.pending_submission = None;
            model.composer = prompt;
            Vec::new()
        }
    }
}

fn reconcile_pending_submission(model: &mut ThreadUiModel) {
    let Some(pending) = model.pending_submission.as_ref() else {
        return;
    };
    let Some(thread_id) = pending.thread_id else {
        return;
    };
    let durable = model.sessions.iter().any(|thread| {
        thread.thread_id == thread_id
            && thread.transcript.entries.iter().any(|entry| {
                entry.kind == TranscriptKind::User
                    && entry.sequence > pending.after_sequence
                    && is_submission_user_card(entry)
                    && entry.text == redact_thread_text(&pending.prompt)
            })
    });
    if durable {
        model.pending_submission = None;
        model.queued_follow_up = None;
        model.submission_error = None;
        model.submission_refresh.composer = false;
        if model.composer.is_empty()
            && let Some(draft) = model.deferred_composer_draft.take()
        {
            model.composer = draft;
        }
    }
}

fn is_submission_user_card(entry: &TranscriptEntry) -> bool {
    entry.source_key == "thread:create:user"
        || (entry.source_key.starts_with("follow-up:") && entry.source_key.ends_with(":user"))
}

fn restore_pending_submission(model: &mut ThreadUiModel) {
    const MESSAGE: &str = "Unable to persist submission. Prompt restored for retry.";
    let Some(pending) = model.pending_submission.take() else {
        return;
    };
    model.queued_follow_up = None;
    if !model.composer.is_empty() {
        model.deferred_composer_draft = Some(std::mem::take(&mut model.composer));
    }
    model.composer = pending.prompt;
    model.status = MESSAGE.into();
    model.submission_error = Some(MESSAGE.into());
    model.submission_refresh.composer = false;
}

fn reconcile_pending_input_submission(model: &mut ThreadUiModel) {
    let Some(pending) = model.pending_input_submission.as_ref() else {
        return;
    };
    let source_key = format!("{}:input:{}:card", pending.run_id, pending.request_id);
    let durable = model.sessions.iter().any(|thread| {
        thread.thread_id == pending.thread_id
            && thread.transcript.entries.iter().any(|entry| {
                entry.kind == TranscriptKind::User
                    && entry.sequence > pending.after_sequence
                    && entry.source_key == source_key
                    && entry.text == redact_thread_text(&pending.value)
            })
    });
    if durable {
        model.pending_input_submission = None;
        model.submission_refresh.input = false;
        model.input.clear();
    }
}

fn reconcile_pending_model_switch(model: &mut ThreadUiModel) {
    let Some(pending) = model.pending_model_switch.as_ref() else {
        return;
    };
    let durable = model.sessions.iter().any(|thread| {
        thread.thread_id == pending.thread_id
            && thread.binding.provider_name == pending.provider_name
            && thread.binding.model == pending.model
    });
    if durable {
        let provider_name = pending.provider_name.clone();
        let selected_model = pending.model.clone();
        model.pending_model_switch = None;
        model.status = format!("Model switched to {provider_name}/{selected_model}");
    }
}

fn finalize_failed_submissions_after_snapshot(model: &mut ThreadUiModel) {
    if model.submission_refresh.composer && model.pending_submission.is_some() {
        restore_pending_submission(model);
    }
    if model.submission_refresh.input
        && let Some(pending) = model.pending_input_submission.take()
    {
        model.input = pending.value;
        model.input_target = Some((pending.thread_id, pending.request_id));
        model.submission_refresh.input = false;
        model.status = "Unable to persist input. Input restored for retry.".into();
    }
}

fn restore_stranded_follow_up(model: &mut ThreadUiModel) {
    let stranded = model.queued_follow_up.is_some()
        && model.pending_submission.is_some()
        && model.selected_thread().is_some_and(|thread| {
            thread.active_run_id.is_none()
                && thread.pending.is_none()
                && thread.lifecycle != ThreadLifecycle::Ready
        });
    if stranded {
        restore_pending_submission(model);
        model.status =
            "Queued follow-up was not submitted because the active child ended; prompt restored"
                .into();
    }
}

fn synchronize_input_target(model: &mut ThreadUiModel) {
    if model.pending_input_submission.is_some() {
        return;
    }
    let target = model
        .selected_thread()
        .and_then(|thread| match thread.pending.as_ref() {
            Some(ThreadPendingRequest::Input { request_id, .. }) => {
                Some((thread.thread_id, request_id.clone()))
            }
            Some(ThreadPendingRequest::Permission { .. }) | None => None,
        });
    if model.input_target != target {
        model.input.clear();
        model.input_target = target;
    }
}

fn pop_grapheme(value: &mut String) {
    if let Some((index, _)) = value.grapheme_indices(true).next_back() {
        value.truncate(index);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityState {
    Recorded,
    Running,
    Waiting,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VisualState {
    Idle,
    Active,
    Permission,
    Reconciliation,
    Complete,
}

pub const TERMINAL: Color = Color::Rgb(32, 44, 49);
pub const TERMINAL_DEEP: Color = Color::Rgb(24, 34, 38);
pub const SURFACE: Color = Color::Rgb(42, 55, 61);
pub const SURFACE_STRONG: Color = Color::Rgb(52, 67, 74);
pub const LINE: Color = Color::Rgb(83, 97, 104);
pub const LINE_SOFT: Color = Color::Rgb(57, 71, 77);
pub const TEXT: Color = Color::Rgb(244, 243, 239);
pub const TEXT_SOFT: Color = Color::Rgb(199, 200, 196);
pub const MUTED: Color = Color::Rgb(146, 153, 155);
pub const FAINT: Color = Color::Rgb(102, 113, 118);
pub const LATTE: Color = Color::Rgb(231, 187, 114);
pub const LATTE_BRIGHT: Color = Color::Rgb(244, 207, 142);
pub const GREEN: Color = Color::Rgb(113, 217, 154);
pub const CYAN: Color = Color::Rgb(123, 199, 232);
pub const RED: Color = Color::Rgb(240, 122, 120);
pub const AMBER: Color = Color::Rgb(239, 183, 99);
pub const DIFF_ADD: Color = Color::Rgb(168, 223, 183);
pub const DIFF_REMOVE: Color = Color::Rgb(220, 153, 150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewportTier {
    Wide,
    Medium,
    Narrow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdleComposition {
    Expanded,
    Wide,
    Stacked,
    Compact,
}

impl IdleComposition {
    const fn for_area(area: Rect, tier: ViewportTier) -> Self {
        match (tier, area.height) {
            (ViewportTier::Wide, 52..) => Self::Expanded,
            (ViewportTier::Wide, _) => Self::Wide,
            (ViewportTier::Medium, _) => Self::Stacked,
            (ViewportTier::Narrow, _) => Self::Compact,
        }
    }

    const fn header_height(self) -> u16 {
        match self {
            Self::Expanded => 20,
            Self::Wide | Self::Compact => 12,
            Self::Stacked => 15,
        }
    }
}

impl ViewportTier {
    const fn for_width(width: u16) -> Self {
        if width >= 110 {
            Self::Wide
        } else if width >= 78 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }

    const fn compact_inset(self) -> u16 {
        match self {
            Self::Wide | Self::Medium => 3,
            Self::Narrow => 2,
        }
    }

    const fn transcript_inset(self) -> u16 {
        match self {
            Self::Wide | Self::Medium => 4,
            Self::Narrow => 2,
        }
    }

    const fn idle_inset(self) -> u16 {
        match self {
            Self::Wide => 7,
            Self::Medium => 4,
            Self::Narrow => 2,
        }
    }

    const fn idle_composer_inset(self) -> u16 {
        match self {
            Self::Wide => 6,
            Self::Medium => 4,
            Self::Narrow => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewportLayout {
    app: Rect,
    tier: ViewportTier,
    header: Rect,
    transcript: Rect,
    composer: Rect,
    transcript_inset: u16,
    composer_inset: u16,
    idle_composition: IdleComposition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActionResult {
    text: String,
    failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PermissionPresentation {
    operation: String,
    target: String,
    scope: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PresentationItem {
    Message {
        kind: TranscriptKind,
        text: String,
    },
    Completion {
        text: String,
        handoff: Option<latte_core::Handoff>,
    },
    Action {
        key: String,
        tool_call_id: Option<String>,
        effect_id: Option<String>,
        name: String,
        summary: String,
        metadata: Vec<(String, String)>,
        state: ActivityState,
        result: Option<ActionResult>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunPresentation {
    run_id: Option<RunId>,
    heading: String,
    items: Vec<PresentationItem>,
}

/// Pure, display-only projection. It consumes only the public redacted
/// transcript and run summaries, groups cards by run, and pairs tool calls and
/// results by their public `tool_call_id` when that identifier is present.
#[allow(clippy::too_many_lines)]
fn project_transcript(thread: &ThreadSnapshot) -> Vec<RunPresentation> {
    let mut groups: Vec<(Option<RunId>, Vec<&TranscriptEntry>)> = Vec::new();
    for entry in &thread.transcript.entries {
        if let Some((_, entries)) = groups
            .iter_mut()
            .find(|(run_id, _)| *run_id == entry.run_id)
        {
            entries.push(entry);
        } else {
            groups.push((entry.run_id, vec![entry]));
        }
    }
    groups
        .into_iter()
        .map(|(run_id, entries)| {
            let heading = run_heading(thread, run_id);
            let mut items = Vec::new();
            for entry in entries {
                match entry.kind {
                    TranscriptKind::ToolCall => {
                        let tool_call_id = payload_string(entry, &["descriptor", "tool_call_id"])
                            .or_else(|| payload_string(entry, &["tool_call_id"]));
                        let effect_id = payload_string(entry, &["descriptor", "effect_id"])
                            .or_else(|| payload_string(entry, &["effect_id"]));
                        let name = payload_string(entry, &["descriptor", "name"])
                            .or_else(|| payload_string(entry, &["name"]))
                            .unwrap_or_else(|| "Tool action".into());
                        let state = if run_id == thread.active_run_id {
                            match thread.lifecycle {
                                ThreadLifecycle::WaitingPermission => ActivityState::Waiting,
                                ThreadLifecycle::Running | ThreadLifecycle::WaitingInput => {
                                    ActivityState::Running
                                }
                                _ => ActivityState::Recorded,
                            }
                        } else {
                            ActivityState::Recorded
                        };
                        items.push(PresentationItem::Action {
                            key: format!("action:{}", entry.entry_id),
                            tool_call_id,
                            effect_id,
                            name: presentation_text(&name, 80),
                            summary: presentation_text(&entry.text, 360),
                            metadata: tool_metadata(entry),
                            state,
                            result: None,
                        });
                    }
                    TranscriptKind::ToolResult => {
                        let tool_call_id = payload_string(entry, &["tool_call_id"])
                            .or_else(|| payload_string(entry, &["descriptor", "tool_call_id"]));
                        let failed = entry
                            .payload
                            .as_ref()
                            .is_some_and(|payload| payload.get("error").is_some());
                        let result = ActionResult {
                            text: presentation_text(&entry.text, 640),
                            failed,
                        };
                        let paired = tool_call_id.as_ref().and_then(|candidate| {
                            items.iter_mut().rev().find_map(|item| match item {
                                PresentationItem::Action {
                                    tool_call_id,
                                    state,
                                    result: slot,
                                    ..
                                } if tool_call_id.as_ref() == Some(candidate) && slot.is_none() => {
                                    *state = if failed {
                                        ActivityState::Failed
                                    } else {
                                        ActivityState::Succeeded
                                    };
                                    *slot = Some(result.clone());
                                    Some(())
                                }
                                _ => None,
                            })
                        });
                        if paired.is_none() {
                            let name = payload_string(entry, &["name"])
                                .unwrap_or_else(|| "Tool result".into());
                            items.push(PresentationItem::Action {
                                key: format!("result:{}", entry.entry_id),
                                tool_call_id,
                                effect_id: None,
                                name: presentation_text(&name, 80),
                                summary: result.text.clone(),
                                metadata: Vec::new(),
                                state: if failed {
                                    ActivityState::Failed
                                } else {
                                    ActivityState::Succeeded
                                },
                                result: None,
                            });
                        }
                    }
                    TranscriptKind::System
                        if payload_string(entry, &["status"]).as_deref() == Some("started") =>
                    {
                        let effect_id = payload_string(entry, &["effect_id"]);
                        let folded = effect_id.as_ref().and_then(|candidate| {
                            items.iter_mut().rev().find(|item| {
                                matches!(item, PresentationItem::Action { effect_id, .. } if effect_id.as_ref() == Some(candidate))
                            })
                        });
                        if let Some(PresentationItem::Action { state, .. }) = folded {
                            *state = ActivityState::Running;
                        } else {
                            items.push(PresentationItem::Message {
                                kind: TranscriptKind::System,
                                text: presentation_text(&entry.text, 2 * 1024),
                            });
                        }
                    }
                    TranscriptKind::Completion => {
                        items.push(PresentationItem::Completion {
                            text: presentation_text(&entry.text, 2 * 1024),
                            handoff: completion_handoff(entry),
                        });
                    }
                    kind => {
                        let text = presentation_text(&entry.text, 2 * 1024);
                        if !text.is_empty() {
                            items.push(PresentationItem::Message { kind, text });
                        }
                    }
                }
            }
            RunPresentation {
                run_id,
                heading,
                items,
            }
        })
        .collect()
}

fn payload_string(entry: &TranscriptEntry, path: &[&str]) -> Option<String> {
    let mut value = entry.payload.as_ref()?;
    for part in path {
        value = value.get(*part)?;
    }
    let value = value.as_str()?;
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return None;
    }
    Some(latte_core::redact_thread_text(value))
}

fn completion_handoff(entry: &TranscriptEntry) -> Option<latte_core::Handoff> {
    let payload = entry.payload.as_ref()?;
    let value = payload.get("handoff").unwrap_or(payload).clone();
    serde_json::from_value::<latte_core::Handoff>(value)
        .ok()
        .map(|handoff| latte_core::Handoff {
            summary: presentation_text(&handoff.summary, 2 * 1024),
            files_changed: handoff
                .files_changed
                .into_iter()
                .take(128)
                .map(|path| presentation_text(&path, 512))
                .filter(|path| !path.is_empty())
                .collect(),
            evidence: handoff
                .evidence
                .into_iter()
                .take(128)
                .map(|evidence| latte_core::Evidence {
                    name: presentation_text(&evidence.name, 256),
                    status: evidence.status,
                    summary: presentation_text(&evidence.summary, 1024),
                })
                .collect(),
        })
}

fn tool_metadata(entry: &TranscriptEntry) -> Vec<(String, String)> {
    let Some(descriptor) = entry
        .payload
        .as_ref()
        .and_then(|payload| payload.get("descriptor"))
    else {
        return Vec::new();
    };
    let Some(input) = descriptor.get("input") else {
        return Vec::new();
    };
    let mut metadata = Vec::new();
    for (label, key) in [("Target", "path"), ("Query", "query"), ("Directory", "cwd")] {
        if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
            let value = presentation_text(value, 360);
            if !value.is_empty() {
                metadata.push((label.into(), value));
            }
        }
    }
    if let Some(argv) = input.get("argv").and_then(serde_json::Value::as_array) {
        let command = argv
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(|part| presentation_text(part, 160))
            .collect::<Vec<_>>()
            .join(" ");
        if !command.is_empty() {
            metadata.push(("Command".into(), presentation_text(&command, 512)));
        }
    }
    metadata
}

fn presentation_text(value: &str, cap: usize) -> String {
    let redacted = latte_core::redact_thread_text(value);
    let mut output = String::with_capacity(redacted.len().min(cap));
    for ch in redacted.chars() {
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        if output.len() + ch.len_utf8() > cap {
            output.push('…');
            break;
        }
        output.push(ch);
    }
    output
}

fn run_heading(thread: &ThreadSnapshot, run_id: Option<RunId>) -> String {
    let Some(run_id) = run_id else {
        return "Conversation".into();
    };
    thread
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .map_or_else(
            || "Run activity".into(),
            |run| format!("Run {} · {}", run.ordinal, run_status_label(run.status)),
        )
}

const fn run_status_label(status: ThreadRunStatus) -> &'static str {
    match status {
        ThreadRunStatus::Queued => "Queued",
        ThreadRunStatus::Running => "Running",
        ThreadRunStatus::Cancelling => "Cancelling",
        ThreadRunStatus::WaitingPermission => "Waiting permission",
        ThreadRunStatus::WaitingInput => "Waiting input",
        ThreadRunStatus::Interrupted => "Interrupted",
        ThreadRunStatus::Failed => "Failed",
        ThreadRunStatus::Completed => "Completed",
    }
}

fn action_keys(thread: Option<&ThreadSnapshot>) -> Vec<String> {
    thread.map_or_else(Vec::new, |thread| {
        project_transcript(thread)
            .into_iter()
            .flat_map(|group| group.items)
            .filter_map(|item| match item {
                PresentationItem::Action { key, .. } => Some(key),
                PresentationItem::Message { .. } | PresentationItem::Completion { .. } => None,
            })
            .collect()
    })
}

/// Renders one focused transcript, fixed composer, and non-durable progress
/// without exposing checkpoint or private payload JSON.
pub fn render(frame: &mut Frame<'_>, model: &ThreadUiModel) {
    let area = app_rect(frame.area());
    let visual_state = visual_state(model);
    frame.render_widget(
        Block::default().style(Style::default().bg(TERMINAL)),
        frame.area(),
    );
    if area.width == 0 || area.height == 0 {
        return;
    }
    let layout = viewport_layout(area, visual_state, model);
    render_header(frame, model, visual_state, layout);
    if visual_state == VisualState::Idle {
        render_welcome(frame, model, layout);
    } else {
        render_transcript(frame, model, visual_state, layout);
    }
    render_composer(frame, model, visual_state, layout);
    let blocking = matches!(
        visual_state,
        VisualState::Permission | VisualState::Reconciliation
    );
    if !blocking {
        render_slash_suggestions(frame, model, layout);
    }
    if model.help && !blocking {
        let overlay = centered(area, 78, 65);
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new("Single-session transcript\nComposer owns every printable character. Enter sends; Shift+Enter inserts a newline.\nThe mouse wheel or PgUp/PgDn scrolls history. Esc enters Navigation; j/k selects actions, and Enter/Space expands. Esc or i returns to Composer; q quits only from Navigation; F10 quits from either mode.\nCtrl+C interrupts active work; press it again within 2 seconds to exit. Permission: d denies, Ctrl+A allows, and Enter does nothing.\nReconciliation: Ctrl+R opens acknowledgement, Ctrl+A confirms, and Enter does nothing.\nEvent gaps clear transient progress and reload an authoritative snapshot.")
                .style(Style::default().fg(TEXT_SOFT).bg(TERMINAL_DEEP))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(LINE))
                        .title(" Help "),
                ),
            overlay,
        );
    }
    if model.command_palette && !blocking {
        render_command_palette(frame, model, area);
    }
    if model.session_picker && !blocking {
        render_session_picker(frame, model, area);
    }
    if model.model_picker.is_some() && !blocking {
        render_model_picker(frame, model, area);
    }
}

fn render_slash_suggestions(frame: &mut Frame<'_>, model: &ThreadUiModel, layout: ViewportLayout) {
    let suggestions = slash_popup_suggestions(model);
    let available_height = layout.composer.y.saturating_sub(layout.app.y);
    let inset = bounded_inset(layout.composer.width, layout.composer_inset);
    let width = layout
        .composer
        .width
        .saturating_sub(inset.saturating_mul(2));
    if suggestions.is_empty() || available_height < 4 || width < 12 {
        return;
    }
    let height = u16::try_from(suggestions.len().saturating_add(3))
        .unwrap_or(available_height)
        .min(available_height);
    let visible = usize::from(height.saturating_sub(3));
    if visible == 0 {
        return;
    }
    let selected = model.slash_index.min(suggestions.len().saturating_sub(1));
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(suggestions.len().saturating_sub(visible));
    let mut lines = suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, command)| {
            let selected = index == selected;
            let argument = command
                .argument_hint
                .map_or_else(String::new, |hint| format!(" {hint}"));
            Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(LATTE_BRIGHT),
                ),
                Span::styled(
                    format!("/{}{}  {}", command.name, argument, command.description),
                    Style::default()
                        .fg(if selected { TEXT } else { TEXT_SOFT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(Span::styled(
        "↑/↓ select · Enter run · Esc close",
        Style::default().fg(MUTED),
    )));
    let overlay = Rect::new(
        layout.composer.x.saturating_add(inset),
        layout.composer.y.saturating_sub(height),
        width,
        height,
    );
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(TERMINAL_DEEP))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(LINE))
                    .title(" Suggestions "),
            ),
        overlay,
    );
}

fn render_command_palette(frame: &mut Frame<'_>, model: &ThreadUiModel, area: Rect) {
    let overlay = centered(area, 62, 34);
    frame.render_widget(Clear, overlay);
    let lines = BUILTINS
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let selected = index == model.command_index;
            Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(LATTE_BRIGHT),
                ),
                Span::styled(
                    format!("/{:<12} {}", command.name, command.description),
                    Style::default()
                        .fg(if selected { TEXT } else { TEXT_SOFT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        })
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "↑/↓ select · Enter run · Esc close",
            Style::default().fg(MUTED),
        ))))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(TERMINAL_DEEP))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(LINE))
                    .title(" Commands "),
            ),
        overlay,
    );
}

fn render_session_picker(frame: &mut Frame<'_>, model: &ThreadUiModel, area: Rect) {
    let overlay = centered(area, 76, 54);
    frame.render_widget(Clear, overlay);
    let mut lines = model
        .session_catalog
        .iter()
        .take(50)
        .enumerate()
        .flat_map(|(index, session)| {
            let selected = index == model.session_index;
            let marker = if selected { "› " } else { "  " };
            let title = presentation_text(&session.title, 120);
            let workspace = presentation_text(&session.workspace_root, 180);
            [
                Line::from(vec![
                    Span::styled(marker, Style::default().fg(LATTE_BRIGHT)),
                    Span::styled(
                        title,
                        Style::default()
                            .fg(if selected { TEXT } else { TEXT_SOFT })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(
                        format!("  {} · {:?}", session.model, session.lifecycle),
                        Style::default().fg(MUTED),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("    {workspace}  ·  {}", session.thread_id),
                    Style::default().fg(FAINT),
                )),
            ]
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No saved sessions",
            Style::default().fg(MUTED),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ select · Enter resume · Esc close",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(TERMINAL_DEEP))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(LINE))
                    .title(" Sessions "),
            ),
        overlay,
    );
}

fn render_model_picker(frame: &mut Frame<'_>, model: &ThreadUiModel, area: Rect) {
    let overlay = centered(area, 72, 68);
    frame.render_widget(Clear, overlay);
    let options = filtered_model_options(model);
    let selected_index = model.model_picker.as_ref().map_or(0, |picker| {
        picker.index.min(options.len().saturating_sub(1))
    });
    let current = selected_provider_model(model);
    let visible_rows = usize::from(overlay.height.saturating_sub(7)).max(1);
    let start = selected_index
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(options.len().saturating_sub(visible_rows));
    let mut previous_provider: Option<&str> = None;
    let mut lines = Vec::new();
    for (index, option) in options.iter().enumerate().skip(start).take(visible_rows) {
        if previous_provider != Some(option.provider_name.as_str()) {
            lines.push(Line::from(Span::styled(
                format!(" {}", option.provider_name),
                Style::default()
                    .fg(LATTE_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )));
            previous_provider = Some(&option.provider_name);
        }
        let selected = index == selected_index;
        let is_current = current.as_ref().is_some_and(|(provider, selected_model)| {
            provider == &option.provider_name && selected_model == &option.model
        });
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(LATTE_BRIGHT),
            ),
            Span::styled(
                if is_current { "● " } else { "  " },
                Style::default().fg(LATTE_BRIGHT),
            ),
            Span::styled(
                option.name.clone().unwrap_or_else(|| option.model.clone()),
                Style::default()
                    .fg(if selected { TEXT } else { TEXT_SOFT })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                option
                    .name
                    .as_ref()
                    .map_or_else(String::new, |_| format!("  {}", option.model)),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if option.is_default { "  default" } else { "" },
                Style::default().fg(MUTED),
            ),
        ]));
    }
    if options.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matching provider models",
            Style::default().fg(MUTED),
        )));
    }
    let query = model
        .model_picker
        .as_ref()
        .map_or("", |picker| picker.query.as_str());
    lines.insert(
        0,
        Line::from(vec![
            Span::styled(" Search: ", Style::default().fg(MUTED)),
            Span::styled(query, Style::default().fg(TEXT)),
        ]),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Type to filter · ↑/↓ select · Enter switch · Esc close",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(TERMINAL_DEEP))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(LINE))
                    .title(" Select provider / model "),
            ),
        overlay,
    );
}

fn app_rect(area: Rect) -> Rect {
    let width = area.width.min(160);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y,
        width,
        area.height,
    )
}

fn bounded_inset(width: u16, desired: u16) -> u16 {
    desired.min(width.saturating_sub(1) / 2)
}

fn viewport_layout(area: Rect, state: VisualState, model: &ThreadUiModel) -> ViewportLayout {
    let tier = ViewportTier::for_width(area.width);
    let idle_composition = IdleComposition::for_area(area, tier);
    let desired_header_height = match state {
        VisualState::Idle => idle_composition.header_height(),
        VisualState::Active
        | VisualState::Permission
        | VisualState::Reconciliation
        | VisualState::Complete => match tier {
            ViewportTier::Wide | ViewportTier::Medium => 3,
            ViewportTier::Narrow => 2,
        },
    };
    let mut base_composer_height = match (state, idle_composition, tier) {
        (VisualState::Idle, IdleComposition::Expanded, _) | (_, _, ViewportTier::Narrow) => 5,
        (_, _, ViewportTier::Wide | ViewportTier::Medium) => 4,
    };
    if matches!(state, VisualState::Permission | VisualState::Reconciliation) && area.height < 18 {
        base_composer_height = 3;
    }
    let desired_composer_inset = if state == VisualState::Idle {
        tier.idle_composer_inset()
    } else {
        tier.compact_inset()
    };
    let composer_inset = bounded_inset(area.width, desired_composer_inset);
    let editor = editor_text(model);
    let editor_width = area
        .width
        .saturating_sub(composer_inset.saturating_mul(2).saturating_add(2))
        .max(1);
    let extra = u16::try_from(
        composer_text_layout(editor.as_str(), editor_width)
            .rows
            .len()
            .saturating_sub(1)
            .min(4),
    )
    .unwrap_or(4);
    let desired_composer_height = base_composer_height + extra;
    let compact_idle_header = state == VisualState::Idle
        && (area.width < 40
            || area.height
                < desired_header_height
                    .saturating_add(desired_composer_height)
                    .saturating_add(1));
    let desired_header_height = if compact_idle_header {
        match area.height {
            0..=2 => 0,
            3..=5 => 1,
            6..=9 => 2,
            _ => 5,
        }
    } else {
        desired_header_height
    };
    let minimum_composer_height = area.height.min(2);
    let header_height =
        desired_header_height.min(area.height.saturating_sub(minimum_composer_height));
    let reserve_transcript = u16::from(
        area.height
            >= header_height
                .saturating_add(minimum_composer_height)
                .saturating_add(1),
    );
    let composer_height = desired_composer_height.min(
        area.height
            .saturating_sub(header_height)
            .saturating_sub(reserve_transcript),
    );
    let transcript_height = area
        .height
        .saturating_sub(header_height)
        .saturating_sub(composer_height);
    let transcript_inset = bounded_inset(area.width, tier.transcript_inset());
    ViewportLayout {
        app: area,
        tier,
        header: Rect::new(area.x, area.y, area.width, header_height),
        transcript: Rect::new(
            area.x,
            area.y + header_height,
            area.width,
            transcript_height,
        ),
        composer: Rect::new(
            area.x,
            area.bottom().saturating_sub(composer_height),
            area.width,
            composer_height,
        ),
        transcript_inset,
        composer_inset,
        idle_composition,
    }
}

fn editor_text(model: &ThreadUiModel) -> String {
    if model
        .selected_thread()
        .is_some_and(|thread| matches!(thread.pending, Some(ThreadPendingRequest::Input { .. })))
    {
        model.input.clone()
    } else {
        model.composer.clone()
    }
}

fn wrapped_line_count(text: &str, width: u16) -> usize {
    wrap_text(text, width).len().max(1)
}

fn visual_state(model: &ThreadUiModel) -> VisualState {
    let Some(thread) = model.selected_thread() else {
        return if model.pending_submission.is_some() {
            VisualState::Active
        } else {
            VisualState::Idle
        };
    };
    if thread.lifecycle == ThreadLifecycle::ReconciliationRequired {
        return VisualState::Reconciliation;
    }
    if thread.lifecycle == ThreadLifecycle::WaitingPermission
        || matches!(
            thread.pending,
            Some(ThreadPendingRequest::Permission { .. })
        )
    {
        return VisualState::Permission;
    }
    let latest_run_completed = thread.latest_run_id.is_some_and(|latest_run_id| {
        thread
            .runs
            .iter()
            .find(|run| run.run_id == latest_run_id)
            .is_some_and(|run| run.status == ThreadRunStatus::Completed)
    });
    let latest_transcript_card_is_completion = thread
        .transcript
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind != TranscriptKind::System)
        .is_some_and(|entry| entry.kind == TranscriptKind::Completion);
    if thread.lifecycle == ThreadLifecycle::Ready
        && (latest_run_completed || latest_transcript_card_is_completion)
    {
        return VisualState::Complete;
    }
    VisualState::Active
}

fn render_header(
    frame: &mut Frame<'_>,
    model: &ThreadUiModel,
    visual_state: VisualState,
    layout: ViewportLayout,
) {
    if layout.header.height == 0 {
        return;
    }
    if visual_state == VisualState::Idle {
        render_welcome_header(frame, model, layout);
        return;
    }
    let Some(thread) = model.selected_thread() else {
        let inset = bounded_inset(layout.app.width, layout.tier.compact_inset());
        let width = layout.app.width.saturating_sub(inset * 2);
        let x = layout.app.x + inset;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("●", Style::default().fg(CYAN)),
                Span::styled(
                    format!("  Latte Code  v{}  ·  Starting", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ])),
            Rect::new(x, layout.header.y, width, 1),
        );
        if layout.header.height >= 2 {
            render_rule(
                frame,
                Rect::new(x, layout.header.bottom().saturating_sub(1), width, 1),
                LINE_SOFT,
            );
        }
        return;
    };
    let (status, color) = match visual_state {
        VisualState::Reconciliation => ("Reconciliation required", AMBER),
        VisualState::Permission => ("Waiting for approval", AMBER),
        VisualState::Complete => ("Ready", GREEN),
        VisualState::Active => (
            lifecycle_label(thread.lifecycle),
            lifecycle_color(thread.lifecycle),
        ),
        VisualState::Idle => unreachable!(),
    };
    let inset = bounded_inset(layout.app.width, layout.tier.compact_inset());
    let width = layout.app.width.saturating_sub(inset * 2);
    let x = layout.app.x + inset;
    let header = format!("●  Latte Code  v{}  ·  {status}", env!("CARGO_PKG_VERSION"));
    let repository = if width >= 56 {
        model.startup.as_ref().map(|startup| {
            presentation_text(
                &startup.workspace_display,
                usize::from(width).saturating_mul(4),
            )
        })
    } else {
        None
    };
    let repository_width = repository.as_ref().map_or(0, |value| {
        u16::try_from(display_width(value))
            .unwrap_or(width / 2)
            .min(width / 2)
    });
    let header_width = if repository_width == 0 {
        width
    } else {
        width.saturating_sub(repository_width.saturating_add(2))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("●", Style::default().fg(color)),
            Span::styled(
                header.trim_start_matches('●'),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(x, layout.header.y, header_width, 1),
    );
    if let Some(repository) = repository.filter(|value| !value.is_empty()) {
        frame.render_widget(
            Paragraph::new(clip_to_width(&repository, usize::from(repository_width)))
                .style(Style::default().fg(CYAN)),
            Rect::new(
                x + width.saturating_sub(repository_width),
                layout.header.y,
                repository_width,
                1,
            ),
        );
    }
    if layout.header.height >= 2 {
        render_rule(
            frame,
            Rect::new(x, layout.header.bottom().saturating_sub(1), width, 1),
            LINE_SOFT,
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_welcome_header(frame: &mut Frame<'_>, model: &ThreadUiModel, layout: ViewportLayout) {
    if layout.header.height < layout.idle_composition.header_height() || layout.app.width < 40 {
        render_constrained_welcome_header(frame, model, layout);
        return;
    }
    let inset = bounded_inset(layout.app.width, layout.tier.idle_inset());
    let x = layout.app.x + inset;
    let cup_top = match layout.idle_composition {
        IdleComposition::Expanded => layout.header.y + 5,
        IdleComposition::Wide => layout.header.y + 3,
        IdleComposition::Stacked | IdleComposition::Compact => layout.header.y + 1,
    };
    // Match the prototype's very-narrow treatment below the established
    // 72-column layout: the mark becomes slightly smaller and drops its third
    // steam line while the product and environment remain visible.
    let cup: &[&str] = if layout.app.width < 60 {
        &[" ╱ ╱", "╭───╮", "│   ├╮", "╰───╯╯"]
    } else {
        &[" ╱ ╱ ╱", "╭────╮", "│    ├╮", "╰────╯╯"]
    };
    let cup_width = cup
        .iter()
        .map(|row| display_width(row))
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or(12);
    for (offset, row) in cup.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(Span::styled(*row, Style::default().fg(LATTE))),
            Rect::new(
                x,
                cup_top + u16::try_from(offset).unwrap_or_default(),
                cup_width,
                1,
            ),
        );
    }
    let (text_x, title_y) = match layout.idle_composition {
        IdleComposition::Expanded | IdleComposition::Wide | IdleComposition::Stacked => {
            (x + cup_width + 3, cup_top + 1)
        }
        IdleComposition::Compact => (x + cup_width + 2, cup_top + 1),
    };
    let brand_right = if matches!(
        layout.idle_composition,
        IdleComposition::Expanded | IdleComposition::Wide
    ) {
        layout.app.x + layout.app.width.saturating_mul(52) / 100
    } else {
        layout.app.right().saturating_sub(inset)
    };
    let available = brand_right.saturating_sub(text_x);
    if available >= 16 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Latte Code",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(MUTED),
                ),
            ])),
            Rect::new(text_x, title_y, available, 1),
        );
    }
    if let Some(startup) = model.startup.as_ref() {
        let card = match layout.idle_composition {
            IdleComposition::Expanded | IdleComposition::Wide => {
                let width = layout.app.width.saturating_mul(42) / 100;
                Rect::new(
                    layout.app.right().saturating_sub(inset + width),
                    layout.header.y
                        + if layout.idle_composition == IdleComposition::Expanded {
                            4
                        } else {
                            2
                        },
                    width,
                    7,
                )
            }
            IdleComposition::Stacked => Rect::new(
                x,
                layout.header.y + 7,
                layout.app.width.saturating_sub(inset * 2),
                7,
            ),
            IdleComposition::Compact => Rect::new(
                x,
                layout.header.y + 5,
                layout.app.width.saturating_sub(inset * 2),
                5,
            ),
        };
        render_environment_card(frame, model, startup, card);
    }
    render_rule(
        frame,
        Rect::new(
            x,
            layout.header.bottom().saturating_sub(1),
            layout.app.width.saturating_sub(inset * 2),
            1,
        ),
        LINE_SOFT,
    );
}

fn render_constrained_welcome_header(
    frame: &mut Frame<'_>,
    model: &ThreadUiModel,
    layout: ViewportLayout,
) {
    if layout.header.height == 0 {
        return;
    }
    let inset = bounded_inset(layout.app.width, layout.tier.idle_inset());
    let x = layout.app.x + inset;
    let width = layout.app.width.saturating_sub(inset * 2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(LATTE)),
            Span::styled(
                format!("Latte Code  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(x, layout.header.y, width, 1),
    );
    let data_rows = layout.header.height.saturating_sub(2);
    if let Some(startup) = model.startup.as_ref() {
        let model_name = selected_provider_model(model).map_or_else(
            || {
                if startup.default_model.is_empty() {
                    MODEL_NOT_CONFIGURED.into()
                } else {
                    presentation_text(&startup.default_model, 96)
                }
            },
            |(_, selected_model)| presentation_text(&selected_model, 96),
        );
        let workspace = presentation_text(&startup.workspace_display, 180);
        let permission = startup.permission_mode.card_label();
        let rows = match data_rows {
            0 => Vec::new(),
            1 => vec![("directory:", workspace, CYAN)],
            2 => vec![
                ("model:", model_name, TEXT),
                ("directory:", workspace, CYAN),
            ],
            _ => vec![
                ("model:", model_name, TEXT),
                ("directory:", workspace, CYAN),
                ("permissions:", permission.into(), AMBER),
            ],
        };
        for (offset, (label, value, color)) in rows.into_iter().enumerate() {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{label} "), Style::default().fg(MUTED)),
                    Span::styled(value, Style::default().fg(color)),
                ])),
                Rect::new(
                    x,
                    layout.header.y + 1 + u16::try_from(offset).unwrap_or_default(),
                    width,
                    1,
                ),
            );
        }
    }
    if layout.header.height >= 2 {
        render_rule(
            frame,
            Rect::new(x, layout.header.bottom().saturating_sub(1), width, 1),
            LINE_SOFT,
        );
    }
}

fn render_environment_card(
    frame: &mut Frame<'_>,
    ui: &ThreadUiModel,
    startup: &ThreadStartupPresentation,
    area: Rect,
) {
    let model = selected_provider_model(ui).map_or_else(
        || {
            if startup.default_model.is_empty() {
                MODEL_NOT_CONFIGURED.into()
            } else {
                presentation_text(&startup.default_model, 96)
            }
        },
        |(_, selected_model)| presentation_text(&selected_model, 96),
    );
    let workspace = presentation_text(&startup.workspace_display, 180);
    let pad = area.height >= 7;
    let mut lines = Vec::with_capacity(4);
    if pad {
        lines.push(Line::from(""));
    }
    for (label, value, color) in [
        ("model:", model.as_str(), TEXT),
        ("directory:", workspace.as_str(), TEXT),
        ("permissions:", startup.permission_mode.card_label(), AMBER),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<13}"), Style::default().fg(MUTED)),
            Span::styled(value.to_owned(), Style::default().fg(color)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(TERMINAL_DEEP))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(LINE))
                    .style(Style::default().bg(TERMINAL_DEEP)),
            ),
        area,
    );
}

fn render_welcome(frame: &mut Frame<'_>, model: &ThreadUiModel, layout: ViewportLayout) {
    if layout.transcript.height == 0 {
        return;
    }
    let inset = bounded_inset(layout.app.width, layout.tier.idle_inset());
    let width = layout.app.width.saturating_sub(inset * 2);
    let x = layout.app.x + inset;
    let top_padding = match layout.idle_composition {
        IdleComposition::Expanded => 4,
        IdleComposition::Wide => 3,
        IdleComposition::Stacked => 2,
        IdleComposition::Compact => 1,
    };
    let mut y = layout.transcript.y + top_padding;
    if let Some(error) = model.submission_error.as_deref() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("! ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                    Span::styled(error.to_owned(), Style::default().fg(RED)),
                ]),
                Line::from(Span::styled(
                    "Your prompt has been restored in the composer.",
                    Style::default().fg(TEXT_SOFT),
                )),
            ]),
            Rect::new(x, y, width, 2),
        );
        y += 3;
    }
    let tip = Line::from(vec![
        Span::styled(
            "Tip: ",
            Style::default()
                .fg(LATTE_BRIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Describe an outcome. Latte Code will inspect, edit, and verify the repository.",
            Style::default().fg(TEXT_SOFT),
        ),
    ]);
    let tip_text =
        "Tip: Describe an outcome. Latte Code will inspect, edit, and verify the repository.";
    let tip_height = u16::try_from(wrapped_line_count(tip_text, width))
        .unwrap_or(2)
        .max(1);
    frame.render_widget(
        Paragraph::new(tip)
            .style(Style::default().bg(TERMINAL))
            .wrap(Wrap { trim: false }),
        Rect::new(x, y, width, tip_height),
    );
    if model.connection != ConnectionState::Connected {
        let status_y = layout.transcript.bottom().saturating_sub(1);
        frame.render_widget(
            Paragraph::new(format!(
                "{} · actions disabled until the transcript can be refreshed",
                connection_label(model.connection)
            ))
            .style(Style::default().fg(AMBER)),
            Rect::new(x, status_y, width, 1),
        );
    }
}

#[allow(clippy::too_many_lines)]
fn render_transcript(
    frame: &mut Frame<'_>,
    model: &ThreadUiModel,
    visual_state: VisualState,
    layout: ViewportLayout,
) {
    let inset = layout.transcript_inset;
    let area = Rect::new(
        layout.app.x + inset,
        layout.transcript.y,
        layout.app.width.saturating_sub(inset * 2),
        layout.transcript.height,
    );
    let mut lines = Vec::new();
    let mut action_index = 0_usize;
    if area.height >= 6 {
        lines.push(Line::from(""));
        lines.push(Line::from(""));
    }
    if let Some(pending) = model.pending_submission.as_ref() {
        render_message_lines(
            &mut lines,
            TranscriptKind::User,
            &pending.prompt,
            area.width,
        );
    }
    if let Some(thread) = model.selected_thread() {
        if thread.transcript.has_more {
            lines.push(Line::from(Span::styled(
                "[… earlier transcript cards are omitted from this bounded current view]",
                Style::default().fg(FAINT),
            )));
            lines.push(Line::from(""));
        }
        for group in project_transcript(thread) {
            let phase_color = group
                .run_id
                .and_then(|run_id| thread.runs.iter().find(|run| run.run_id == run_id))
                .map_or(TEXT_SOFT, |run| run_status_color(run.status));
            let last_action_index = group
                .items
                .iter()
                .rposition(|item| matches!(item, PresentationItem::Action { .. }));
            let mut heading_rendered = group.run_id.is_none();
            for (item_index, item) in group.items.into_iter().enumerate() {
                if !heading_rendered
                    && !matches!(
                        item,
                        PresentationItem::Message {
                            kind: TranscriptKind::User,
                            ..
                        }
                    )
                {
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled(" ● ", Style::default().fg(phase_color)),
                        Span::styled(
                            if visual_state == VisualState::Complete {
                                "Completed".to_owned()
                            } else {
                                group.heading.clone()
                            },
                            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    heading_rendered = true;
                }
                match item {
                    PresentationItem::Message { kind, text } => {
                        render_message_lines(&mut lines, kind, &text, area.width);
                    }
                    PresentationItem::Completion { text, handoff } => {
                        render_completion_lines(&mut lines, &text, handoff.as_ref(), area.width);
                    }
                    PresentationItem::Action {
                        key,
                        name,
                        summary,
                        metadata,
                        state,
                        result,
                        ..
                    } => {
                        let selected = model.focus == ThreadFocus::Navigation
                            && model.navigation_index == action_index;
                        let expanded = model.expanded_actions.contains(&key);
                        let (symbol, color) = activity_style(state);
                        let branch = if last_action_index == Some(item_index) {
                            "└─"
                        } else {
                            "├─"
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                if selected { "› " } else { "  " },
                                Style::default().fg(CYAN),
                            ),
                            Span::styled(branch, Style::default().fg(LINE)),
                            Span::styled(format!("{symbol} "), Style::default().fg(color)),
                            Span::styled(
                                name,
                                Style::default().fg(TEXT_SOFT).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!("  {summary}"), Style::default().fg(TEXT)),
                        ]));
                        if expanded {
                            for (label, value) in &metadata {
                                for (index, detail) in wrap_text(
                                    &format!("{label}: {value}"),
                                    area.width.saturating_sub(7),
                                )
                                .into_iter()
                                .take(4)
                                .enumerate()
                                {
                                    lines.push(Line::from(vec![
                                        Span::styled("      │ ", Style::default().fg(LINE)),
                                        Span::styled(
                                            detail,
                                            Style::default().fg(if index == 0 {
                                                TEXT_SOFT
                                            } else {
                                                MUTED
                                            }),
                                        ),
                                    ]));
                                }
                            }
                        }
                        if expanded && let Some(result) = result.as_ref() {
                            for detail in wrap_text(&result.text, area.width.saturating_sub(7))
                                .into_iter()
                                .take(6)
                            {
                                lines.push(Line::from(vec![
                                    Span::styled("      │ ", Style::default().fg(LINE)),
                                    Span::styled(
                                        detail,
                                        Style::default().fg(if result.failed {
                                            RED
                                        } else {
                                            MUTED
                                        }),
                                    ),
                                ]));
                            }
                        }
                        if expanded && (!metadata.is_empty() || result.is_some()) {
                            lines.push(Line::from(""));
                        }
                        action_index = action_index.saturating_add(1);
                    }
                }
            }
            lines.push(Line::from(""));
        }
        if let Some(ThreadPendingRequest::Input { prompt, .. }) = thread.pending.as_ref() {
            lines.push(Line::from(Span::styled(
                format!("? Input required · {}", presentation_text(prompt, 360)),
                Style::default().fg(AMBER),
            )));
            lines.push(Line::from(""));
        }
    }
    for progress in &model.progress {
        if model
            .selected_thread()
            .is_none_or(|thread| thread.active_run_id != Some(progress_run_id(progress)))
        {
            continue;
        }
        render_progress(&mut lines, progress);
    }
    let rendered_line_count = wrapped_presentation_line_count(&lines, area.width);
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(TEXT).bg(TERMINAL))
        .wrap(Wrap { trim: false });
    let maximum_top_offset = rendered_line_count.saturating_sub(usize::from(area.height));
    let rows_above_tail = usize::from(model.scroll).min(maximum_top_offset);
    let top_offset = maximum_top_offset.saturating_sub(rows_above_tail);
    frame.render_widget(
        paragraph.scroll((u16::try_from(top_offset).unwrap_or(u16::MAX), 0)),
        area,
    );
    render_blocking_card(frame, model, visual_state, area);
}

/// Counts the rows produced by ratatui's `Wrap { trim: false }` contract
/// without enabling its unstable rendered-line inspection feature. Styling is
/// irrelevant to wrapping, so this mirrors the word/whitespace width state
/// machine using the same styled grapheme stream and terminal cell widths.
fn wrapped_presentation_line_count(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    lines
        .iter()
        .map(|line| wrapped_presentation_line_height(line, width))
        .sum()
}

fn wrapped_presentation_line_height(line: &Line<'_>, width: u16) -> usize {
    let mut wrapped_lines = 0_usize;
    let mut line_width = 0_u16;
    let mut line_has_content = false;
    let mut word_width = 0_u16;
    let mut word_has_content = false;
    let mut whitespace_width = 0_u16;
    let mut whitespace = VecDeque::<u16>::new();
    let mut non_whitespace_previous = false;

    for grapheme in line
        .spans
        .iter()
        .flat_map(|span| span.styled_graphemes(Style::default()))
    {
        let is_whitespace = grapheme.is_whitespace();
        let symbol_width = grapheme.symbol.cell_width();
        if symbol_width > width {
            continue;
        }
        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = !line_has_content
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;
        if word_found || untrimmed_overflow {
            line_width = line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width);
            line_has_content |= !whitespace.is_empty() || word_has_content;
            whitespace.clear();
            whitespace_width = 0;
            word_width = 0;
            word_has_content = false;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            wrapped_lines = wrapped_lines.saturating_add(1);
            line_width = 0;
            line_has_content = false;
            while let Some(next_width) = whitespace.front().copied() {
                if next_width > remaining_width {
                    break;
                }
                whitespace.pop_front();
                whitespace_width = whitespace_width.saturating_sub(next_width);
                remaining_width = remaining_width.saturating_sub(next_width);
            }
            if is_whitespace && whitespace.is_empty() {
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            whitespace.push_back(symbol_width);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            word_has_content = true;
        }
        non_whitespace_previous = !is_whitespace;
    }

    line_has_content |= !whitespace.is_empty() || word_has_content;
    if line_has_content {
        wrapped_lines = wrapped_lines.saturating_add(1);
    }
    wrapped_lines.max(1)
}

/// Permission and reconciliation are blocking UI, not transcript history.
/// Render their cards after the scrollable transcript so a pre-existing scroll
/// offset can never move the required decision out of view.
fn render_blocking_card(
    frame: &mut Frame<'_>,
    model: &ThreadUiModel,
    visual_state: VisualState,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(thread) = model.selected_thread() else {
        return;
    };
    let mut lines = Vec::new();
    let mut compact_lines = Vec::new();
    match visual_state {
        VisualState::Permission => {
            let Some(ThreadPendingRequest::Permission {
                request_id,
                description,
                ..
            }) = thread.pending.as_ref()
            else {
                return;
            };
            let permission = permission_presentation(thread, request_id, description);
            render_permission_card(&mut lines, &permission, area.width);
            compact_lines.extend([
                Line::from(Span::styled(
                    format!("! Permission · {}", permission.operation),
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(permission.target, Style::default().fg(TEXT))),
                Line::from(Span::styled(
                    "d deny · Ctrl+A allow once",
                    Style::default().fg(AMBER),
                )),
            ]);
        }
        VisualState::Reconciliation => {
            let Some(effect_id) = reconciliation_effect_id(model, thread) else {
                return;
            };
            render_reconciliation_card(&mut lines, model, thread, &effect_id, area.width);
            compact_lines.extend([
                Line::from(Span::styled(
                    "! Reconciliation required",
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("Effect · {}", presentation_text(&effect_id, 160)),
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "Ctrl+R review · Enter does nothing",
                    Style::default().fg(AMBER),
                )),
            ]);
        }
        VisualState::Idle | VisualState::Active | VisualState::Complete => return,
    }
    let card_height = u16::try_from(lines.len()).unwrap_or(area.height);
    let compact = card_height.saturating_add(2) > area.height;
    if compact {
        lines = compact_lines;
    }
    let card_height = u16::try_from(lines.len()).unwrap_or(area.height);
    // Blocking decisions belong to the active tail of the waterfall. Pin the
    // card immediately above the composer instead of at the transcript top,
    // where a user watching the latest tool row can easily miss it.
    let bottom_inset = u16::from(area.height > card_height);
    let visible_height = card_height.min(area.height.saturating_sub(bottom_inset));
    let pinned = Rect::new(
        area.x,
        area.bottom()
            .saturating_sub(visible_height)
            .saturating_sub(bottom_inset),
        area.width,
        visible_height,
    );
    frame.render_widget(Clear, pinned);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(TERMINAL)),
        pinned,
    );
}

fn render_message_lines(
    lines: &mut Vec<Line<'static>>,
    kind: TranscriptKind,
    text: &str,
    width: u16,
) {
    if kind == TranscriptKind::User {
        let wrapped = wrap_text(text, width.saturating_sub(5));
        let height = wrapped.len().saturating_add(2).max(3);
        for row in 0..height {
            let content = if row == 1 {
                format!("  › {}", wrapped.first().map_or("", String::as_str))
            } else if row > 1 && row - 1 < wrapped.len() {
                format!("    {}", wrapped[row - 1])
            } else {
                String::new()
            };
            lines.push(surface_line(&content, width, SURFACE_STRONG, Some(LATTE)));
        }
        lines.push(Line::from(""));
        return;
    }
    if kind == TranscriptKind::Completion {
        for (index, row) in wrap_text(text, width.saturating_sub(3))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(vec![
                Span::styled(
                    if index == 0 { " • " } else { "   " },
                    Style::default().fg(TEXT),
                ),
                Span::styled(row, Style::default().fg(TEXT)),
            ]));
        }
        return;
    }
    let (prefix, color, bold) = match kind {
        TranscriptKind::User | TranscriptKind::Completion => unreachable!(),
        TranscriptKind::Assistant => (" • ", TEXT, false),
        TranscriptKind::Permission => (" ! Permission · ", AMBER, false),
        TranscriptKind::Input => (" ? Input · ", AMBER, false),
        TranscriptKind::Failure => (" ! Failed · ", RED, true),
        TranscriptKind::System => (" · ", MUTED, false),
        TranscriptKind::ToolCall | TranscriptKind::ToolResult => (" · ", TEXT_SOFT, false),
    };
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    let prefix_width = u16::try_from(display_width(prefix)).unwrap_or(width);
    for (index, row) in wrap_text(text, width.saturating_sub(prefix_width))
        .into_iter()
        .enumerate()
    {
        lines.push(Line::from(Span::styled(
            format!("{}{row}", if index == 0 { prefix } else { "   " }),
            style,
        )));
    }
}

fn render_completion_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    handoff: Option<&latte_core::Handoff>,
    width: u16,
) {
    for (index, row) in wrap_text(text, width.saturating_sub(3))
        .into_iter()
        .enumerate()
    {
        lines.push(Line::from(vec![
            Span::styled(
                if index == 0 { " • " } else { "   " },
                Style::default().fg(TEXT),
            ),
            Span::styled(row, Style::default().fg(TEXT)),
        ]));
    }
    let Some(handoff) = handoff else {
        return;
    };
    if handoff.files_changed.is_empty() && handoff.evidence.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    if !handoff.files_changed.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" │ ", Style::default().fg(GREEN)),
            Span::styled(
                "CHANGED",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ]));
        for path in &handoff.files_changed {
            for (index, row) in wrap_text(path, width.saturating_sub(7))
                .into_iter()
                .enumerate()
            {
                lines.push(Line::from(vec![
                    Span::styled(" │ ", Style::default().fg(GREEN)),
                    Span::styled(
                        if index == 0 { "  " } else { "    " },
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(row, Style::default().fg(TEXT_SOFT)),
                ]));
            }
        }
    }
    if !handoff.evidence.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" │ ", Style::default().fg(GREEN)),
            Span::styled(
                "VERIFIED",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ]));
        for evidence in &handoff.evidence {
            let (symbol, color) = match evidence.status {
                latte_core::VerificationStatus::Passed => ("✓", GREEN),
                latte_core::VerificationStatus::Failed => ("×", RED),
                latte_core::VerificationStatus::NotRun => ("·", AMBER),
            };
            let detail = if evidence.summary.is_empty() {
                evidence.name.clone()
            } else {
                format!("{} · {}", evidence.name, evidence.summary)
            };
            for (index, row) in wrap_text(&detail, width.saturating_sub(8))
                .into_iter()
                .enumerate()
            {
                lines.push(Line::from(vec![
                    Span::styled(" │ ", Style::default().fg(GREEN)),
                    Span::styled(
                        if index == 0 {
                            format!("{symbol} ")
                        } else {
                            "  ".into()
                        },
                        Style::default().fg(color),
                    ),
                    Span::styled(row, Style::default().fg(TEXT_SOFT)),
                ]));
            }
        }
    }
}

fn render_permission_card(
    lines: &mut Vec<Line<'static>>,
    permission: &PermissionPresentation,
    width: u16,
) {
    let card_width = width.saturating_sub(2).clamp(20, 76);
    card_top(lines, "Permission required", card_width);
    card_row(
        lines,
        "Latte Code needs approval before this operation.",
        card_width,
        MUTED,
    );
    card_divider(lines, card_width);
    card_row(
        lines,
        &format!("Operation  {}", permission.operation),
        card_width,
        TEXT,
    );
    card_row(
        lines,
        &format!("Target     {}", permission.target),
        card_width,
        TEXT,
    );
    card_row(
        lines,
        &format!("Scope      {}", permission.scope),
        card_width,
        TEXT_SOFT,
    );
    card_row(
        lines,
        "[d] Deny  ·  [Ctrl+A] Allow once  ·  Enter does nothing",
        card_width,
        AMBER,
    );
    card_bottom(lines, card_width);
}

fn render_reconciliation_card(
    lines: &mut Vec<Line<'static>>,
    model: &ThreadUiModel,
    thread: &ThreadSnapshot,
    effect_id: &str,
    width: u16,
) {
    let card_width = width.saturating_sub(2).clamp(20, 76);
    card_top(lines, "Reconciliation required", card_width);
    card_row(
        lines,
        "The effect started, but its outcome is unknown.",
        card_width,
        MUTED,
    );
    card_divider(lines, card_width);
    card_row(
        lines,
        &format!("Effect  {}", presentation_text(effect_id, 160)),
        card_width,
        TEXT,
    );
    card_row(lines, "Outcome unknown", card_width, AMBER);
    let open = model
        .reconciliation_confirmation
        .as_ref()
        .is_some_and(|(thread_id, _)| *thread_id == thread.thread_id);
    card_row(
        lines,
        if open {
            "Ctrl+A confirm failed · d/Esc cancel · Enter does nothing"
        } else {
            "Ctrl+R review acknowledgement · Enter does nothing"
        },
        card_width,
        AMBER,
    );
    card_bottom(lines, card_width);
}

fn card_top(lines: &mut Vec<Line<'static>>, title: &str, width: u16) {
    let inner = usize::from(width.saturating_sub(2));
    let label = format!(" {title} ");
    let rule = "─".repeat(inner.saturating_sub(display_width(&label)));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("┌{label}{rule}┐"),
            Style::default()
                .fg(AMBER)
                .bg(SURFACE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
}

fn card_row(lines: &mut Vec<Line<'static>>, text: &str, width: u16, color: Color) {
    let inner = usize::from(width.saturating_sub(4));
    let clipped = clip_to_width(text, inner);
    let padding = " ".repeat(inner.saturating_sub(display_width(&clipped)));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("│ ", Style::default().fg(AMBER).bg(SURFACE)),
        Span::styled(clipped, Style::default().fg(color).bg(SURFACE)),
        Span::styled(padding, Style::default().bg(SURFACE)),
        Span::styled(" │", Style::default().fg(AMBER).bg(SURFACE)),
    ]));
}

fn card_divider(lines: &mut Vec<Line<'static>>, width: u16) {
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("├{}┤", "─".repeat(usize::from(width.saturating_sub(2)))),
            Style::default().fg(LINE).bg(SURFACE),
        ),
    ]));
}

fn card_bottom(lines: &mut Vec<Line<'static>>, width: u16) {
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("└{}┘", "─".repeat(usize::from(width.saturating_sub(2)))),
            Style::default().fg(AMBER).bg(SURFACE),
        ),
    ]));
}

fn render_progress(lines: &mut Vec<Line<'static>>, progress: &ThreadTransientProgress) {
    match progress {
        ThreadTransientProgress::AssistantDelta { text, .. } => lines.push(Line::from(vec![
            Span::styled(" • ", Style::default().fg(CYAN)),
            Span::styled(
                presentation_text(text, 2 * 1024),
                Style::default().fg(TEXT_SOFT),
            ),
        ])),
        _ => lines.push(Line::from(Span::styled(
            format!("  └─ ◌ {}", progress_text(progress)),
            Style::default().fg(MUTED),
        ))),
    }
}

#[allow(clippy::too_many_lines)]
fn render_composer(
    frame: &mut Frame<'_>,
    model: &ThreadUiModel,
    visual_state: VisualState,
    layout: ViewportLayout,
) {
    let (text, placeholder, color) =
        if model.pending_submission.is_some() && model.composer.is_empty() {
            ("Submitting…".into(), true, CYAN)
        } else {
            match model.selected_thread() {
                Some(thread) if thread.lifecycle == ThreadLifecycle::ReconciliationRequired => (
                    "Resolve the unknown effect outcome before continuing".into(),
                    true,
                    AMBER,
                ),
                Some(thread) => match thread.pending.as_ref() {
                    Some(ThreadPendingRequest::Permission { .. }) => (
                        "Resolve the permission request to continue".into(),
                        true,
                        AMBER,
                    ),
                    Some(ThreadPendingRequest::Input { prompt, .. }) => (
                        if model.input.is_empty() {
                            presentation_text(prompt, 160)
                        } else {
                            model.input.clone()
                        },
                        model.input.is_empty(),
                        AMBER,
                    ),
                    None => (
                        if model.composer.is_empty() {
                            if visual_state == VisualState::Complete {
                                "Ask a follow-up…".into()
                            } else {
                                "Ask Latte Code to change this repository…".into()
                            }
                        } else {
                            model.composer.clone()
                        },
                        model.composer.is_empty(),
                        if model.focus == ThreadFocus::Composer {
                            LATTE
                        } else {
                            FAINT
                        },
                    ),
                },
                None => (
                    if model.composer.is_empty() {
                        "Ask Latte Code to change this repository…".into()
                    } else {
                        model.composer.clone()
                    },
                    model.composer.is_empty(),
                    LATTE,
                ),
            }
        };
    let area = layout.composer;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inset = bounded_inset(area.width, layout.composer_inset);
    let inner = Rect::new(
        area.x + inset,
        area.y,
        area.width.saturating_sub(inset * 2),
        area.height,
    );
    let text_style = if placeholder {
        Style::default().fg(FAINT)
    } else {
        Style::default().fg(TEXT)
    };
    let show_top_rule = area.height >= 3;
    if show_top_rule {
        render_rule(frame, Rect::new(inner.x, area.y, inner.width, 1), LINE);
    }
    let footer_rows = if area.height <= 1 {
        0
    } else if layout.tier == ViewportTier::Narrow && area.height >= 4 {
        2
    } else {
        1
    };
    let top_rows = u16::from(show_top_rule);
    let show_divider = area.height >= top_rows.saturating_add(footer_rows).saturating_add(2);
    let divider_rows = u16::from(show_divider);
    let prompt_height = area
        .height
        .saturating_sub(top_rows)
        .saturating_sub(divider_rows)
        .saturating_sub(footer_rows)
        .max(1);
    let prompt_y = area.y.saturating_add(top_rows);
    let content_width = inner.width.saturating_sub(2).max(1);
    let editor_layout = composer_text_layout(&editor_text(model), content_width);
    let prompt_rows = if placeholder {
        vec![clip_to_width(&text, usize::from(content_width))]
    } else {
        editor_layout.rows.clone()
    };
    let visible_start = prompt_rows
        .len()
        .saturating_sub(usize::from(prompt_height.max(1)));
    let visible_rows = prompt_rows
        .iter()
        .enumerate()
        .skip(visible_start)
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 { "› " } else { "  " },
                    Style::default().fg(color),
                ),
                Span::styled(row.clone(), text_style),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible_rows).style(Style::default().bg(TERMINAL)),
        Rect::new(inner.x, prompt_y, inner.width, prompt_height),
    );
    let footer_y = area.bottom().saturating_sub(footer_rows);
    if show_divider {
        render_rule(
            frame,
            Rect::new(inner.x, footer_y.saturating_sub(1), inner.width, 1),
            LINE_SOFT,
        );
    }
    let (left, right) = composer_meta(model, visual_state);
    if footer_rows == 2 {
        frame.render_widget(
            Paragraph::new(left).style(Style::default().fg(TEXT_SOFT)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(right).style(Style::default().fg(MUTED)),
            Rect::new(inner.x, footer_y + 1, inner.width, 1),
        );
    } else if footer_rows == 1 && layout.tier == ViewportTier::Narrow {
        frame.render_widget(
            Paragraph::new(right).style(Style::default().fg(MUTED)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else if footer_rows == 1 {
        let right_width = u16::try_from(display_width(&right).min(usize::from(inner.width)))
            .unwrap_or(inner.width);
        frame.render_widget(
            Paragraph::new(left).style(Style::default().fg(TEXT_SOFT)),
            Rect::new(
                inner.x,
                footer_y,
                inner.width.saturating_sub(right_width),
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(right).style(Style::default().fg(MUTED)),
            Rect::new(
                inner.right().saturating_sub(right_width),
                footer_y,
                right_width,
                1,
            ),
        );
    }
    let disabled = matches!(
        visual_state,
        VisualState::Permission | VisualState::Reconciliation
    );
    if !disabled && model.focus == ThreadFocus::Composer && inner.width > 0 {
        let visible_start = editor_layout
            .rows
            .len()
            .saturating_sub(usize::from(prompt_height.max(1)));
        let cursor_row = u16::try_from(editor_layout.caret_row.saturating_sub(visible_start))
            .unwrap_or_default();
        let cursor_column = u16::try_from(editor_layout.caret_column).unwrap_or_default();
        let cursor_x = inner
            .x
            .saturating_add(2)
            .saturating_add(cursor_column)
            .min(inner.right().saturating_sub(1));
        let cursor_y = prompt_y
            .saturating_add(cursor_row)
            .min(prompt_y.saturating_add(prompt_height).saturating_sub(1));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn composer_meta(model: &ThreadUiModel, state: VisualState) -> (String, String) {
    if model.pending_submission.is_some() {
        return (
            "Submitting".into(),
            "Compose next prompt · Shift+Enter newline".into(),
        );
    }
    let Some(thread) = model.selected_thread() else {
        if selected_provider_model(model).is_none()
            && model
                .startup
                .as_ref()
                .is_some_and(|startup| startup.default_model.is_empty())
        {
            return (
                "Provider setup required".into(),
                "Configure ~/.latte/latte-code.jsonc · restart Latte Code".into(),
            );
        }
        if model.submission_error.is_some() {
            return (
                "Submission failed · prompt restored".into(),
                "Enter retry · Shift+Enter newline · Ctrl+P commands".into(),
            );
        }
        let left = model.startup.as_ref().map_or_else(String::new, |startup| {
            let selected = selected_provider_model(model).map_or_else(
                || {
                    if startup.default_model.is_empty() {
                        MODEL_NOT_CONFIGURED.into()
                    } else {
                        startup.default_model.clone()
                    }
                },
                |(_, selected_model)| selected_model,
            );
            format!(
                "Build · {} · {}",
                presentation_text(&selected, 80),
                startup.permission_mode.label()
            )
        });
        return (left, "Ctrl+Enter send · Ctrl+P commands".into());
    };
    let model_name = presentation_text(&thread.binding.model, 80);
    let left = match state {
        VisualState::Permission => format!("Attention · {model_name}"),
        VisualState::Reconciliation => format!("Reconciliation · {model_name}"),
        VisualState::Complete => format!("Ready for follow-up · {model_name}"),
        VisualState::Active => format!("{} · {model_name}", lifecycle_label(thread.lifecycle)),
        VisualState::Idle => unreachable!(),
    };
    let right = if model
        .ctrl_c_exit_armed_until
        .is_some_and(|deadline| Instant::now() <= deadline)
    {
        "Ctrl+C again to exit".into()
    } else if model.connection != ConnectionState::Connected {
        "Ctrl+R refresh".into()
    } else if state == VisualState::Reconciliation {
        if model.reconciliation_confirmation.is_some() {
            "Ctrl+A confirm failed · d/Esc cancel · Enter does nothing".into()
        } else {
            "Ctrl+R review · Enter does nothing".into()
        }
    } else if state == VisualState::Permission {
        "d deny · Ctrl+A allow once · Enter does nothing".into()
    } else if matches!(thread.pending, Some(ThreadPendingRequest::Input { .. })) {
        "Enter send · Shift+Enter newline".into()
    } else if model.focus == ThreadFocus::Navigation {
        "j/k select · Enter expand · Esc composer".into()
    } else {
        "Enter send · Shift+Enter newline · Ctrl+P commands".into()
    };
    (left, right)
}

#[cfg(test)]
fn status_line(model: &ThreadUiModel) -> String {
    composer_meta(model, visual_state(model)).1
}

fn render_rule(frame: &mut Frame<'_>, area: Rect, color: Color) {
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width)))
            .style(Style::default().fg(color).bg(TERMINAL)),
        area,
    );
}

fn surface_line(
    content: &str,
    width: u16,
    background: Color,
    rail: Option<Color>,
) -> Line<'static> {
    let width = usize::from(width);
    let clipped = clip_to_width(content, width.saturating_sub(1));
    let padding = " ".repeat(width.saturating_sub(1 + display_width(&clipped)));
    Line::from(vec![
        Span::styled(
            if rail.is_some() { "▎" } else { " " },
            Style::default()
                .fg(rail.unwrap_or(background))
                .bg(background),
        ),
        Span::styled(
            format!("{clipped}{padding}"),
            Style::default().fg(TEXT).bg(background),
        ),
    ])
}

fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for source in text.split('\n') {
        if source.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut column = 0_usize;
        for grapheme in source.graphemes(true) {
            let grapheme_width = grapheme_width_at(grapheme, column);
            if column > 0 && column.saturating_add(grapheme_width) > width {
                rows.push(std::mem::take(&mut row));
                column = 0;
            }
            if grapheme == "\t" {
                let spaces = grapheme_width_at(grapheme, column).min(width);
                row.push_str(&" ".repeat(spaces));
                column = column.saturating_add(spaces);
            } else {
                row.push_str(grapheme);
                column = column.saturating_add(grapheme_width_at(grapheme, column));
            }
            if column >= width {
                rows.push(std::mem::take(&mut row));
                column = 0;
            }
        }
        if !row.is_empty() || column == 0 && rows.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComposerTextLayout {
    rows: Vec<String>,
    caret_row: usize,
    caret_column: usize,
}

fn composer_text_layout(text: &str, width: u16) -> ComposerTextLayout {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut column = 0_usize;

    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            rows.push(std::mem::take(&mut row));
            column = 0;
            continue;
        }

        let grapheme_width = grapheme_width_at(grapheme, column);
        if column > 0 && column.saturating_add(grapheme_width) > width {
            rows.push(std::mem::take(&mut row));
            column = 0;
        }

        let grapheme_width = grapheme_width_at(grapheme, column);
        if grapheme == "\t" {
            row.push_str(&" ".repeat(grapheme_width.min(width)));
            column = column.saturating_add(grapheme_width.min(width));
        } else {
            row.push_str(grapheme);
            column = column.saturating_add(grapheme_width);
        }
    }

    // Keep an exact-width row pending until the next grapheme: an explicit
    // newline consumes that boundary, while end-of-input needs a new caret row.
    rows.push(row);
    if column >= width {
        rows.push(String::new());
        column = 0;
    }
    ComposerTextLayout {
        caret_row: rows.len().saturating_sub(1),
        caret_column: column,
        rows,
    }
}

fn clip_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut column = 0_usize;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            break;
        }
        let grapheme_width = grapheme_width_at(grapheme, column);
        if column.saturating_add(grapheme_width) > width {
            break;
        }
        if grapheme == "\t" {
            output.push_str(&" ".repeat(grapheme_width));
        } else {
            output.push_str(grapheme);
        }
        column = column.saturating_add(grapheme_width);
    }
    output
}

fn display_width(text: &str) -> usize {
    let mut column = 0_usize;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            column = 0;
        } else {
            column = column.saturating_add(grapheme_width_at(grapheme, column));
        }
    }
    column
}

fn grapheme_width_at(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        4 - column % 4
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

const fn lifecycle_label(lifecycle: ThreadLifecycle) -> &'static str {
    match lifecycle {
        ThreadLifecycle::Ready => "Ready",
        ThreadLifecycle::Running => "Running",
        ThreadLifecycle::WaitingPermission => "Waiting permission",
        ThreadLifecycle::WaitingInput => "Waiting input",
        ThreadLifecycle::Interrupted => "Interrupted",
        ThreadLifecycle::Failed => "Failed",
        ThreadLifecycle::ReconciliationRequired => "Reconciliation required",
    }
}

const fn lifecycle_color(lifecycle: ThreadLifecycle) -> Color {
    match lifecycle {
        ThreadLifecycle::Ready => GREEN,
        ThreadLifecycle::Running => CYAN,
        ThreadLifecycle::WaitingPermission
        | ThreadLifecycle::WaitingInput
        | ThreadLifecycle::ReconciliationRequired
        | ThreadLifecycle::Interrupted => AMBER,
        ThreadLifecycle::Failed => RED,
    }
}

const fn run_status_color(status: ThreadRunStatus) -> Color {
    match status {
        ThreadRunStatus::Queued | ThreadRunStatus::Running | ThreadRunStatus::Cancelling => CYAN,
        ThreadRunStatus::WaitingPermission
        | ThreadRunStatus::WaitingInput
        | ThreadRunStatus::Interrupted => AMBER,
        ThreadRunStatus::Failed => RED,
        ThreadRunStatus::Completed => GREEN,
    }
}

const fn activity_style(state: ActivityState) -> (&'static str, Color) {
    match state {
        ActivityState::Recorded => ("·", MUTED),
        ActivityState::Running => ("◌", CYAN),
        ActivityState::Waiting => ("!", AMBER),
        ActivityState::Succeeded => ("✓", GREEN),
        ActivityState::Failed => ("×", RED),
    }
}

const fn connection_label(connection: ConnectionState) -> &'static str {
    match connection {
        ConnectionState::Connected => "Connected",
        ConnectionState::Disconnected => "Disconnected",
        ConnectionState::SnapshotRequired => "Snapshot required",
    }
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

fn permission_presentation(
    thread: &ThreadSnapshot,
    request_id: &str,
    description: &str,
) -> PermissionPresentation {
    let descriptor = thread.transcript.entries.iter().rev().find_map(|entry| {
        if entry.kind != TranscriptKind::ToolCall {
            return None;
        }
        let descriptor = entry.payload.as_ref()?.get("descriptor")?;
        (descriptor
            .get("effect_id")
            .and_then(serde_json::Value::as_str)
            == Some(request_id))
        .then_some(descriptor)
    });
    let name = descriptor
        .and_then(|value| value.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(|value| presentation_text(value, 80));
    let operation = match name.as_deref() {
        Some("write_file") => "Write file".into(),
        Some("edit_file") => "Edit file".into(),
        Some("process") => "Run process".into(),
        Some("read_file") => "Read file".into(),
        Some("list_directory") => "List directory".into(),
        Some("search") => "Search workspace".into(),
        Some(value) if !value.is_empty() => value.replace('_', " "),
        _ => "Repository operation".into(),
    };
    let input = descriptor.and_then(|value| value.get("input"));
    let target = input
        .and_then(|value| value.get("path"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            input
                .and_then(|value| value.get("cwd"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            input
                .and_then(|value| value.get("query"))
                .and_then(serde_json::Value::as_str)
        })
        .map_or_else(
            || "Not exposed by runtime".into(),
            |value| presentation_text(value, 360),
        );
    PermissionPresentation {
        operation,
        target,
        scope: permission_context(description),
    }
}

fn progress_text(progress: &ThreadTransientProgress) -> String {
    match progress {
        ThreadTransientProgress::ProviderAttempt { number, .. } => {
            format!("… provider attempt {number}")
        }
        ThreadTransientProgress::AssistantDelta { text, .. } => {
            format!("… {}", presentation_text(text, 2 * 1024))
        }
        ThreadTransientProgress::ToolProgress { name, detail, .. } => format!(
            "… {}: {}",
            presentation_text(name, 80),
            presentation_text(detail, 360)
        ),
    }
}

const fn progress_run_id(progress: &ThreadTransientProgress) -> RunId {
    match progress {
        ThreadTransientProgress::ProviderAttempt { run_id, .. }
        | ThreadTransientProgress::AssistantDelta { run_id, .. }
        | ThreadTransientProgress::ToolProgress { run_id, .. } => *run_id,
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
            ThreadUiAction::ShowSessions { query } => {
                if let Some(snapshot) = query
                    .as_deref()
                    .map(|query| projection.exact_session(query))
                    .transpose()
                    .map_err(TuiError::Action)?
                    .flatten()
                {
                    let next = reduce(model, ThreadUiInput::SessionOpened(Box::new(snapshot)));
                    if apply_thread_actions(projection, model, sink, next)? {
                        return Ok(true);
                    }
                    continue;
                }
                let sessions = if let Some(query) = query.as_deref() {
                    projection
                        .exact_session_catalog(query)
                        .map_err(TuiError::Action)?
                } else {
                    projection.session_catalog().map_err(TuiError::Action)?
                };
                let next = reduce(
                    model,
                    ThreadUiInput::SessionCatalogReady { sessions, query },
                );
                if apply_thread_actions(projection, model, sink, next)? {
                    return Ok(true);
                }
            }
            ThreadUiAction::SearchSessions { query } => {
                let sessions = projection
                    .search_session_catalog(&query)
                    .map_err(TuiError::Action)?;
                let next = reduce(
                    model,
                    ThreadUiInput::SessionCatalogReady {
                        sessions,
                        query: (!query.is_empty()).then_some(query),
                    },
                );
                if apply_thread_actions(projection, model, sink, next)? {
                    return Ok(true);
                }
            }
            ThreadUiAction::OpenSession { thread_id } => {
                let snapshot = match projection.session(thread_id) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        reduce(model, ThreadUiInput::CommandError(error));
                        continue;
                    }
                };
                let next = reduce(model, ThreadUiInput::SessionOpened(Box::new(snapshot)));
                if apply_thread_actions(projection, model, sink, next)? {
                    return Ok(true);
                }
            }
            ThreadUiAction::RefreshSnapshots => {
                // A snapshot refresh is a typed projection operation, not a
                // runtime effect. Consume it in the terminal adapter so a
                // production command closure cannot accidentally ignore the
                // recovery path after a broadcast receiver reports Lagged.
                let next = match model.active_conversation {
                    Some(ActiveConversation::Session(thread_id)) => {
                        let snapshot = projection.session(thread_id).map_err(TuiError::Action)?;
                        reduce(model, ThreadUiInput::SessionOpened(Box::new(snapshot)))
                    }
                    Some(ActiveConversation::NewSessionDraft)
                        if model.pending_submission.is_none() =>
                    {
                        let sessions = projection.session_catalog().map_err(TuiError::Action)?;
                        reduce(model, ThreadUiInput::SessionCatalog(sessions))
                    }
                    Some(ActiveConversation::NewSessionDraft) | None => {
                        let snapshots = projection.snapshots().map_err(TuiError::Action)?;
                        reduce(model, ThreadUiInput::Snapshot(snapshots))
                    }
                };
                if apply_thread_actions(projection, model, sink, next)? {
                    return Ok(true);
                }
            }
            action => sink(action).map_err(TuiError::Action)?,
        }
    }
    Ok(false)
}

fn drain_runtime_updates(
    model: &mut ThreadUiModel,
    feedback: &Receiver<ThreadUiFeedback>,
    progress: &Receiver<ThreadTransientProgress>,
) -> (bool, Vec<ThreadUiAction>) {
    let mut changed = false;
    let mut actions = Vec::new();
    while let Ok(result) = feedback.try_recv() {
        let next = match result {
            ThreadUiFeedback::SubmissionAssigned {
                submission_id,
                thread_id,
            } => reduce(
                model,
                ThreadUiInput::SubmissionAssigned {
                    submission_id,
                    thread_id,
                },
            ),
            ThreadUiFeedback::SubmissionResult {
                submission_id,
                result: Ok(_),
            } => reduce(model, ThreadUiInput::SubmissionCompleted { submission_id }),
            ThreadUiFeedback::SubmissionResult {
                submission_id,
                result: Err(_),
            } => reduce(model, ThreadUiInput::SubmissionError { submission_id }),
            ThreadUiFeedback::InputSubmissionResult {
                submission_id,
                result: Ok(_),
            } => reduce(
                model,
                ThreadUiInput::InputSubmissionCompleted { submission_id },
            ),
            ThreadUiFeedback::InputSubmissionResult {
                submission_id,
                result: Err(_),
            } => reduce(model, ThreadUiInput::InputSubmissionError { submission_id }),
            ThreadUiFeedback::ModelSwitchResult {
                switch_id,
                result: Ok(_),
            } => reduce(model, ThreadUiInput::ModelSwitchCompleted { switch_id }),
            ThreadUiFeedback::ModelSwitchResult {
                switch_id,
                result: Err(error),
            } => reduce(model, ThreadUiInput::ModelSwitchError { switch_id, error }),
            ThreadUiFeedback::Command(Ok(message)) => {
                reduce(model, ThreadUiInput::CommandCompleted(message))
            }
            ThreadUiFeedback::Command(Err(error))
            | ThreadUiFeedback::SessionManagement(Err(error)) => {
                reduce(model, ThreadUiInput::CommandError(error))
            }
            ThreadUiFeedback::SessionManagement(Ok(SessionManagementOutcome::Updated(message))) => {
                let mut actions = reduce(model, ThreadUiInput::CommandCompleted(message));
                actions.push(ThreadUiAction::RefreshSnapshots);
                actions
            }
            ThreadUiFeedback::SessionManagement(Ok(SessionManagementOutcome::Forked(
                thread_id,
            ))) => {
                vec![ThreadUiAction::OpenSession { thread_id }]
            }
        };
        actions.extend(next);
        changed = true;
    }
    while let Ok(update) = progress.try_recv() {
        reduce(model, ThreadUiInput::Progress(update));
        changed = true;
    }
    (changed, actions)
}

pub fn run_with_feedback_and_progress(
    projection: &mut dyn ThreadProjectionClient,
    startup: ThreadStartupPresentation,
    mut sink: impl FnMut(ThreadUiAction) -> Result<(), String>,
    feedback: &Receiver<ThreadUiFeedback>,
    progress: &Receiver<ThreadTransientProgress>,
) -> Result<(), TuiError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TuiError::NonTty);
    }
    let guard = crate::TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut model = ThreadUiModel::with_startup(startup);
    let sessions = projection.session_catalog().map_err(TuiError::Action)?;
    let initial_actions = reduce(&mut model, ThreadUiInput::SessionCatalog(sessions));
    if apply_thread_actions(projection, &mut model, &mut sink, initial_actions)? {
        return Ok(());
    }
    let mut redraw = true;
    loop {
        if model
            .ctrl_c_exit_armed_until
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            model.ctrl_c_exit_armed_until = None;
            redraw = true;
        }
        if redraw {
            terminal.draw(|frame| render(frame, &model))?;
            reduce(&mut model, ThreadUiInput::FrameRendered);
            redraw = false;
        }
        if guard.take_interrupted() {
            let actions = reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            );
            if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                return Ok(());
            }
            redraw = true;
        }
        match projection.poll() {
            ThreadProjectionPoll::Event => {
                let actions = vec![ThreadUiAction::RefreshSnapshots];
                if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                    return Ok(());
                }
                redraw = true;
            }
            ThreadProjectionPoll::Lagged(_) => {
                let actions = reduce(&mut model, ThreadUiInput::Lagged);
                if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                    return Ok(());
                }
                redraw = true;
            }
            ThreadProjectionPoll::Closed => {
                if model.connection != ConnectionState::Disconnected {
                    reduce(&mut model, ThreadUiInput::Disconnected);
                    redraw = true;
                }
            }
            ThreadProjectionPoll::Error(error) => {
                let next = format!("Command rejected: {error}");
                if model.status != next {
                    reduce(&mut model, ThreadUiInput::CommandError(error));
                    redraw = true;
                }
            }
            ThreadProjectionPoll::Empty => {}
        }
        let (updates_changed, actions) = drain_runtime_updates(&mut model, feedback, progress);
        if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
            return Ok(());
        }
        if updates_changed {
            redraw = true;
        }
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) => {
                    let actions = reduce(&mut model, ThreadUiInput::Key(key));
                    if apply_thread_actions(projection, &mut model, &mut sink, actions)? {
                        return Ok(());
                    }
                    redraw = true;
                }
                Event::Mouse(mouse) => {
                    reduce(&mut model, ThreadUiInput::Mouse(mouse));
                    redraw = true;
                }
                Event::Resize(width, height) => {
                    reduce(&mut model, ThreadUiInput::Resize(width, height));
                    redraw = true;
                }
                Event::Paste(value) => {
                    reduce(&mut model, ThreadUiInput::Paste(value));
                    redraw = true;
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
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
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

    fn running_snapshot() -> ThreadSnapshot {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.latest_run_id = Some(run_id);
        thread.active_run_id = Some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 0,
            status: ThreadRunStatus::Running,
            run_revision: 1,
            completed_at_ms: None,
        });
        thread
    }

    fn session_summary(
        snapshot: &ThreadSnapshot,
        title: &str,
        workspace_root: &str,
    ) -> ThreadSessionSummary {
        ThreadSessionSummary {
            thread_id: snapshot.thread_id,
            title: title.into(),
            workspace_root: workspace_root.into(),
            parent_thread_id: None,
            lifecycle: snapshot.lifecycle,
            provider_name: snapshot.binding.provider_name.clone(),
            model: snapshot.binding.model.clone(),
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn assert_terminal_session_switching_available(lifecycle: ThreadLifecycle) {
        let mut sessions = ThreadUiModel {
            sessions: vec![snapshot(lifecycle)],
            composer: "/sessions".into(),
            ..Default::default()
        };
        assert_eq!(
            reduce(&mut sessions, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::ShowSessions { query: None }]
        );

        let mut new = ThreadUiModel {
            sessions: vec![snapshot(lifecycle)],
            composer: "/new".into(),
            ..Default::default()
        };
        assert!(reduce(&mut new, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            new.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );
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
    #[allow(clippy::too_many_lines)]
    fn projection_defaults_and_action_adapter_cover_exact_open_and_refresh_paths() {
        let ready = snapshot(ThreadLifecycle::Ready);
        let thread_id = ready.thread_id;
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert_eq!(projection.session_catalog().unwrap()[0].title, "hello");

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert_eq!(projection.session(thread_id).unwrap().thread_id, thread_id);

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()], vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert_eq!(
            projection
                .exact_session("hello")
                .unwrap()
                .unwrap()
                .thread_id,
            thread_id
        );

        let mut sink = |_| Ok(());
        let mut missing_model = ThreadUiModel::with_startup(test_startup());
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()], vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut missing_model,
                &mut sink,
                vec![ThreadUiAction::ShowSessions {
                    query: Some("missing".into())
                }]
            )
            .unwrap()
        );

        let mut model = ThreadUiModel::with_startup(test_startup());
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()], vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::ShowSessions {
                    query: Some("hello".into())
                }]
            )
            .unwrap()
        );
        assert_eq!(model.selected_thread().unwrap().thread_id, thread_id);

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::OpenSession { thread_id }]
            )
            .unwrap()
        );

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::RefreshSnapshots]
            )
            .unwrap()
        );

        model.active_conversation = Some(ActiveConversation::NewSessionDraft);
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::RefreshSnapshots]
            )
            .unwrap()
        );

        model.active_conversation = None;
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![ready]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::RefreshSnapshots]
            )
            .unwrap()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn feedback_projection_inputs_and_all_event_variants_preserve_authority() {
        assert_eq!(
            ThreadUiFeedback::submission(7, Ok("accepted".into())),
            ThreadUiFeedback::SubmissionResult {
                submission_id: 7,
                result: Ok("accepted".into())
            }
        );
        assert_eq!(
            ThreadUiFeedback::command(Err("offline".into())),
            ThreadUiFeedback::Command(Err("offline".into()))
        );
        assert_eq!(
            ThreadUiFeedback::model_switch(9, Ok("accepted".into())),
            ThreadUiFeedback::ModelSwitchResult {
                switch_id: 9,
                result: Ok("accepted".into())
            }
        );

        let ids = SystemIdSource::default();
        let mut thread = snapshot(ThreadLifecycle::Running);
        let thread_id = thread.thread_id;
        assert_eq!(
            ThreadUiFeedback::assigned(8, thread_id),
            ThreadUiFeedback::SubmissionAssigned {
                submission_id: 8,
                thread_id
            }
        );
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut model = ThreadUiModel::default();
        reduce(&mut model, ThreadUiInput::Resize(99, 31));
        assert_eq!(model.size, (99, 31));
        reduce(&mut model, ThreadUiInput::Disconnected);
        assert!(!model.authority_enabled());
        reduce(
            &mut model,
            ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
                run_id,
                text: "discarded while disconnected".into(),
            }),
        );
        assert!(model.progress.is_empty());
        reduce(&mut model, ThreadUiInput::Connected);
        assert!(model.authority_enabled());
        reduce(&mut model, ThreadUiInput::CommandError("stale".into()));
        assert_eq!(model.status, "Command rejected: stale");
        reduce(&mut model, ThreadUiInput::CommandCompleted("done".into()));
        assert_eq!(model.status, "done");
        assert!(reduce(&mut model, ThreadUiInput::Tick).is_empty());

        thread.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission".into(),
            description: "write".into(),
            expected_run_revision: 1,
        });
        model.command_palette = true;
        model.help = true;
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread.clone()]));
        assert!(!model.command_palette);
        assert!(!model.help);

        let unknown = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
            thread_id: ThreadId::from_uuid(ids.next_uuid_v7()),
            revision: 2,
            sequence: 2,
            event: ThreadEvent::LifecycleChanged {
                lifecycle: ThreadLifecycle::Ready,
                run_id: None,
            },
        };
        assert_eq!(
            reduce(&mut model, ThreadUiInput::Event(unknown)),
            vec![ThreadUiAction::RefreshSnapshots]
        );

        model.sessions[0].pending = None;
        let lifecycle = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
            thread_id,
            revision: 2,
            sequence: 2,
            event: ThreadEvent::LifecycleChanged {
                lifecycle: ThreadLifecycle::Ready,
                run_id: None,
            },
        };
        assert!(reduce(&mut model, ThreadUiInput::Event(lifecycle)).is_empty());
        assert_eq!(model.sessions[0].lifecycle, ThreadLifecycle::Ready);

        let run = latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Running,
            run_revision: 1,
            completed_at_ms: None,
        };
        let linked = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
            thread_id,
            revision: 3,
            sequence: 3,
            event: ThreadEvent::RunLinked { run: run.clone() },
        };
        assert!(reduce(&mut model, ThreadUiInput::Event(linked)).is_empty());
        assert_eq!(model.sessions[0].active_run_id, Some(run_id));
        assert_eq!(model.sessions[0].runs, vec![run]);

        let reconciliation = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(ids.next_uuid_v7()),
            thread_id,
            revision: 4,
            sequence: 4,
            event: ThreadEvent::ReconciliationRequired {
                run_id,
                effect_id: "effect-1".into(),
            },
        };
        assert!(reduce(&mut model, ThreadUiInput::Event(reconciliation)).is_empty());
        assert_eq!(
            model.reconciliation_hint,
            Some((thread_id, "effect-1".into()))
        );
    }

    #[test]
    fn progress_editor_and_submission_boundaries_are_bounded_and_correlated() {
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let mut model = ThreadUiModel::default();
        for text in ["one", " two"] {
            record_progress(
                &mut model,
                ThreadTransientProgress::AssistantDelta {
                    run_id,
                    text: text.into(),
                },
            );
        }
        for number in [1, 2] {
            record_progress(
                &mut model,
                ThreadTransientProgress::ProviderAttempt { run_id, number },
            );
        }
        for detail in ["starting", "complete"] {
            record_progress(
                &mut model,
                ThreadTransientProgress::ToolProgress {
                    run_id,
                    name: "read_file".into(),
                    detail: detail.into(),
                },
            );
        }
        assert_eq!(model.progress.len(), 3);
        assert!(matches!(
            &model.progress[0],
            ThreadTransientProgress::AssistantDelta { text, .. } if text == "one two"
        ));
        assert!(matches!(
            &model.progress[1],
            ThreadTransientProgress::ProviderAttempt { number: 2, .. }
        ));
        assert!(matches!(
            &model.progress[2],
            ThreadTransientProgress::ToolProgress { detail, .. } if detail == "complete"
        ));

        let mut editor = String::new();
        append_editor_text(&mut editor, "safe\u{1b}[31mred\u{7}\n\tend", 12);
        assert!(!editor.contains('\u{1b}'));
        assert!(!editor.contains('\u{7}'));
        assert!(editor.len() <= 12);
        let mut bounded = "ab".into();
        append_bounded(&mut bounded, "éz", 4);
        assert_eq!(bounded, "abé");

        model.composer = "first".into();
        assert!(matches!(
            submit_composer(&mut model).as_slice(),
            [ThreadUiAction::Start { .. }]
        ));
        let first_id = model.pending_submission.as_ref().unwrap().submission_id;
        reduce(
            &mut model,
            ThreadUiInput::SubmissionCompleted {
                submission_id: first_id,
            },
        );
        assert!(model.status.contains("synchronizing"));
        reduce(
            &mut model,
            ThreadUiInput::SubmissionError {
                submission_id: first_id + 1,
            },
        );
        assert!(model.pending_submission.is_some());
        reduce(
            &mut model,
            ThreadUiInput::SubmissionError {
                submission_id: first_id,
            },
        );
        reduce(&mut model, ThreadUiInput::Snapshot(Vec::new()));
        assert_eq!(model.composer, "first");
        assert!(model.pending_submission.is_none());
    }

    #[test]
    fn projection_helpers_reject_private_or_malformed_payloads_and_keep_safe_metadata() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.active_run_id = Some(run_id);
        thread.transcript.entries = vec![
            transcript_entry(
                &ids,
                1,
                Some(run_id),
                TranscriptKind::ToolCall,
                "run",
                Some(serde_json::json!({
                    "descriptor": {
                        "effect_id": "effect-process",
                        "tool_call_id": "call-process",
                        "name": "process",
                        "input": {
                            "path": "src/lib.rs",
                            "query": "needle",
                            "cwd": ".",
                            "argv": ["cargo", "test", 7]
                        }
                    }
                })),
            ),
            transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::System,
                "started",
                Some(serde_json::json!({"status":"started","effect_id":"effect-process"})),
            ),
            transcript_entry(
                &ids,
                3,
                Some(run_id),
                TranscriptKind::ToolResult,
                "orphan failed",
                Some(serde_json::json!({"tool_call_id":"other","name":"fallback","error":{}})),
            ),
            transcript_entry(
                &ids,
                4,
                Some(run_id),
                TranscriptKind::Completion,
                "done",
                Some(serde_json::json!({"handoff":"invalid"})),
            ),
            transcript_entry(
                &ids,
                5,
                None,
                TranscriptKind::System,
                "standalone started",
                Some(serde_json::json!({"status":"started","effect_id":"unmatched"})),
            ),
        ];
        let groups = project_transcript(&thread);
        assert_eq!(groups.len(), 2);
        let actions = groups
            .iter()
            .flat_map(|group| &group.items)
            .filter(|item| matches!(item, PresentationItem::Action { .. }))
            .count();
        assert_eq!(actions, 2);
        let metadata = tool_metadata(&thread.transcript.entries[0]);
        assert!(
            metadata
                .iter()
                .any(|(label, value)| label == "Target" && value == "src/lib.rs")
        );
        assert!(
            metadata
                .iter()
                .any(|(label, value)| label == "Query" && value == "needle")
        );
        assert!(
            metadata
                .iter()
                .any(|(label, value)| label == "Directory" && value == ".")
        );
        assert!(
            metadata
                .iter()
                .any(|(label, value)| label == "Command" && value == "cargo test")
        );
        assert!(completion_handoff(&thread.transcript.entries[3]).is_none());

        let invalid = transcript_entry(
            &ids,
            6,
            None,
            TranscriptKind::Failure,
            "invalid",
            Some(serde_json::json!({"value":"bad\nidentifier"})),
        );
        assert!(payload_string(&invalid, &["missing"]).is_none());
        assert!(payload_string(&invalid, &["value"]).is_none());
        assert_eq!(run_heading(&thread, None), "Conversation");
        assert_eq!(run_heading(&thread, Some(run_id)), "Run activity");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn adapter_helpers_cover_refresh_errors_feedback_and_safe_reconciliation_parsing() {
        let mut projection = ScriptedProjection {
            snapshots: VecDeque::new(),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut model = ThreadUiModel::default();
        let mut sink = |_| Ok(());
        assert!(matches!(
            apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::RefreshSnapshots]
            ),
            Err(TuiError::Action(error)) if error == "no scripted snapshot"
        ));
        assert!(
            apply_thread_actions(
                &mut projection,
                &mut model,
                &mut sink,
                vec![ThreadUiAction::Quit]
            )
            .unwrap()
        );
        let thread_id = snapshot(ThreadLifecycle::Ready).thread_id;
        let mut failing_sink = |_| Err("rejected".into());
        assert!(matches!(
            apply_thread_actions(
                &mut projection,
                &mut model,
                &mut failing_sink,
                vec![ThreadUiAction::Cancel { thread_id }]
            ),
            Err(TuiError::Action(error)) if error == "rejected"
        ));

        let (feedback_tx, feedback_rx) = std::sync::mpsc::channel();
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
        model.pending_submission = Some(PendingSubmission {
            submission_id: 7,
            prompt: "restore this prompt".into(),
            thread_id: None,
            after_sequence: 0,
        });
        let ids = SystemIdSource::default();
        model.pending_input_submission = Some(PendingInputSubmission {
            submission_id: 8,
            thread_id: ThreadId::from_uuid(ids.next_uuid_v7()),
            run_id: RunId::from_uuid(ids.next_uuid_v7()),
            request_id: "input-1".into(),
            value: "restore this input".into(),
            after_sequence: 0,
        });
        let switch_thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        model.pending_model_switch = Some(PendingModelSwitch {
            switch_id: 9,
            thread_id: switch_thread_id,
            provider_name: "p".into(),
            model: "next".into(),
        });
        feedback_tx
            .send(ThreadUiFeedback::assigned(7, switch_thread_id))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::submission(7, Ok("accepted".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::submission(7, Err("rejected".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::input_submission(8, Ok("accepted".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::input_submission(
                8,
                Err("rejected".into()),
            ))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::command(Ok("command done".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::command(Err("command failed".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::model_switch(9, Ok("accepted".into())))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::model_switch(99, Err("stale".into())))
            .unwrap();
        progress_tx
            .send(ThreadTransientProgress::ProviderAttempt {
                run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                number: 1,
            })
            .unwrap();
        let (changed, actions) = drain_runtime_updates(&mut model, &feedback_rx, &progress_rx);
        assert!(changed);
        assert_eq!(
            actions,
            vec![
                ThreadUiAction::RefreshSnapshots,
                ThreadUiAction::RefreshSnapshots,
                ThreadUiAction::RefreshSnapshots
            ]
        );
        assert_eq!(model.status, "Model switch accepted; synchronizing session");
        assert!(model.pending_submission.is_some());
        reduce(&mut model, ThreadUiInput::Snapshot(Vec::new()));
        assert!(model.pending_submission.is_none());
        assert_eq!(model.composer, "restore this prompt");
        assert!(model.progress.is_empty());
        assert_eq!(
            drain_runtime_updates(&mut model, &feedback_rx, &progress_rx),
            (false, Vec::new())
        );

        let mut safe = snapshot(ThreadLifecycle::ReconciliationRequired);
        safe.transcript.entries.push(transcript_entry(
            &SystemIdSource::default(),
            2,
            None,
            TranscriptKind::Failure,
            "unknown",
            Some(serde_json::json!({"status":"unknown","effect_id":"safe-effect"})),
        ));
        assert_eq!(
            reconciliation_effect_from_snapshot(&safe).as_deref(),
            Some("safe-effect")
        );
        safe.transcript.entries.last_mut().unwrap().payload =
            Some(serde_json::json!({"status":"unknown","effect_id":"bad\neffect"}));
        assert!(reconciliation_effect_from_snapshot(&safe).is_none());
        safe.lifecycle = ThreadLifecycle::Ready;
        assert!(reconciliation_effect_from_snapshot(&safe).is_none());
        assert_eq!(
            permission_context("\n\r"),
            "[operation summary unavailable]"
        );
        assert!(permission_context(&"x".repeat(500)).ends_with('…'));
        assert_eq!(connection_label(ConnectionState::Connected), "Connected");
        assert_eq!(
            connection_label(ConnectionState::Disconnected),
            "Disconnected"
        );
        assert_eq!(
            connection_label(ConnectionState::SnapshotRequired),
            "Snapshot required"
        );
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
    fn snapshot_focus_uses_projection_order_and_preserves_identity() {
        let mut first = snapshot(ThreadLifecycle::Ready);
        first.sequence = 1;
        let mut second = snapshot(ThreadLifecycle::Running);
        second.sequence = 99;
        let first_id = first.thread_id;
        let second_id = second.thread_id;
        let mut model = ThreadUiModel::default();

        reduce(
            &mut model,
            ThreadUiInput::Snapshot(vec![first.clone(), second.clone()]),
        );
        assert_eq!(
            model.selected_thread().map(|thread| thread.thread_id),
            Some(first_id)
        );

        reduce(&mut model, ThreadUiInput::Snapshot(vec![second, first]));
        assert_eq!(model.selected, 1);
        assert_eq!(
            model.selected_thread().map(|thread| thread.thread_id),
            Some(first_id)
        );
        assert_ne!(first_id, second_id);
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
    fn test_backend_renders_reconciliation_as_a_scoped_attention_card() {
        let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
        let effect_id = "authoritative-reconciliation-effect-43";
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
            source_key: "unknown-effect-render".into(),
            created_at_ms: 2,
        });
        let model = ThreadUiModel {
            sessions: vec![thread],
            ..Default::default()
        };

        let screen = rendered(&model, 100, 30);

        assert!(screen.contains("┌ Reconciliation required"));
        assert!(screen.contains(effect_id));
        assert!(screen.contains("Ctrl+R review acknowledgement · Enter does nothing"));
        assert!(!screen.contains("Transcript · Navigation"));
        assert!(!screen.contains("Composer ·"));
    }

    #[test]
    fn enter_sends_shift_enter_inserts_newline_and_permission_enter_is_inert() {
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
            ThreadUiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        );
        assert_eq!(model.composer, "x\n");
        assert!(matches!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            )
            .as_slice(),
            [ThreadUiAction::FollowUp { prompt, .. }] if prompt == "x\n"
        ));
        assert!(model.composer.is_empty());
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        model.sessions[0].pending = Some(ThreadPendingRequest::Permission {
            run_id: run,
            request_id: "p".into(),
            description: "write".into(),
            expected_run_revision: 2,
        });
        model.composer = "kept".into();
        model.input = "also kept".into();
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert!(
                reduce(
                    &mut model,
                    ThreadUiInput::Key(KeyEvent::new(KeyCode::Enter, modifiers))
                )
                .is_empty()
            );
        }
        assert_eq!(model.composer, "kept");
        assert_eq!(model.input, "also kept");

        let ids = SystemIdSource::default();
        let failed_run = RunId::from_uuid(ids.next_uuid_v7());
        let mut retryable_failure = snapshot(ThreadLifecycle::Ready);
        retryable_failure.latest_run_id = Some(failed_run);
        retryable_failure.runs.push(latte_core::ThreadRunSummary {
            run_id: failed_run,
            parent_run_id: None,
            ordinal: 0,
            status: ThreadRunStatus::Failed,
            run_revision: 2,
            completed_at_ms: Some(2),
        });
        retryable_failure.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: Some(failed_run),
            kind: TranscriptKind::Failure,
            text: "provider configuration failed".into(),
            payload: None,
            source_key: "provider-failure".into(),
            created_at_ms: 2,
        });
        let mut after_error = ThreadUiModel {
            sessions: vec![retryable_failure],
            ..Default::default()
        };
        assert!(
            reduce(
                &mut after_error,
                key(KeyCode::Char('a'), KeyModifiers::NONE)
            )
            .is_empty()
        );
        assert!(reduce(&mut after_error, key(KeyCode::Enter, KeyModifiers::SHIFT)).is_empty());
        assert!(
            reduce(
                &mut after_error,
                key(KeyCode::Char('b'), KeyModifiers::NONE)
            )
            .is_empty()
        );
        assert_eq!(after_error.composer, "a\nb");
        assert!(after_error.pending_submission.is_none());
    }

    #[test]
    fn permission_allow_requires_exact_ctrl_a() {
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::WaitingPermission);
        thread.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-exact-chord".into(),
            description: "write".into(),
            expected_run_revision: 2,
        });
        let thread_id = thread.thread_id;
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            composer: "kept".into(),
            input: "also kept".into(),
            ..Default::default()
        };

        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            assert!(reduce(&mut model, key(KeyCode::Char('a'), modifiers)).is_empty());
        }
        assert_eq!(model.composer, "kept");
        assert_eq!(model.input, "also kept");
        assert!(
            reduce(&mut model, key(KeyCode::Char('d'), KeyModifiers::NONE)).is_empty(),
            "a buffered denial must not resolve an unseen permission request"
        );
        reduce(&mut model, ThreadUiInput::FrameRendered);
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::ResolvePermission {
                thread_id,
                request_id: "permission-exact-chord".into(),
                allow: true,
            }]
        );
    }

    #[test]
    fn reconciliation_confirm_requires_exact_ctrl_a() {
        let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
        let effect_id = "effect-exact-confirm";
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
            source_key: "unknown-effect-exact-confirm".into(),
            created_at_ms: 2,
        });
        let thread_id = thread.thread_id;
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            reconciliation_confirmation: Some((thread_id, effect_id.into())),
            composer: "kept".into(),
            input: "also kept".into(),
            ..Default::default()
        };

        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            assert!(reduce(&mut model, key(KeyCode::Char('a'), modifiers)).is_empty());
            assert!(model.reconciliation_confirmation.is_some());
        }
        assert_eq!(model.composer, "kept");
        assert_eq!(model.input, "also kept");
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::ReconcileUnknown {
                thread_id,
                effect_id: effect_id.into(),
            }]
        );
        assert!(model.reconciliation_confirmation.is_none());
    }

    #[test]
    fn empty_enter_is_inert_and_plain_enter_dispatches_exactly_one_start() {
        let mut model = ThreadUiModel::default();

        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        model.composer = "  \t".into();
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(model.composer, "  \t");

        model.composer = "inspect this repository".into();
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::Start {
                submission_id: 1,
                prompt: "inspect this repository".into(),
            }]
        );
        assert!(model.composer.is_empty());

        model.composer = "release must not submit".into();
        assert!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )),
            )
            .is_empty()
        );
        assert_eq!(model.composer, "release must not submit");

        let mut compatibility = ThreadUiModel {
            composer: "submit with the advertised idle chord".into(),
            ..Default::default()
        };
        assert_eq!(
            reduce(
                &mut compatibility,
                key(KeyCode::Enter, KeyModifiers::CONTROL)
            ),
            vec![ThreadUiAction::Start {
                submission_id: 1,
                prompt: "submit with the advertised idle chord".into(),
            }]
        );
    }

    #[test]
    fn submission_is_immediately_visible_and_duplicate_enter_is_inert() {
        let mut model = ThreadUiModel {
            composer: "optimistic-sentinel".into(),
            ..Default::default()
        };

        assert!(matches!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).as_slice(),
            [ThreadUiAction::Start {
                submission_id: 1,
                prompt
            }] if prompt == "optimistic-sentinel"
        ));
        assert!(rendered(&model, 120, 40).contains("optimistic-sentinel"));
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert!(reduce_paste_for_test(&mut model, "duplicate").is_empty());
        assert_eq!(model.composer, "duplicate");
        assert_eq!(
            model
                .pending_submission
                .as_ref()
                .map(|pending| pending.prompt.as_str()),
            Some("optimistic-sentinel")
        );
    }

    #[test]
    fn matching_durable_user_entry_replaces_optimistic_card_exactly_once() {
        let ids = SystemIdSource::default();
        let mut model = ThreadUiModel {
            composer: "durable-sentinel".into(),
            ..Default::default()
        };
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        let mut thread = snapshot(ThreadLifecycle::Running);
        reduce(
            &mut model,
            ThreadUiInput::SubmissionAssigned {
                submission_id: 1,
                thread_id: thread.thread_id,
            },
        );
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread.clone()]));
        assert!(model.pending_submission.is_some());
        assert!(rendered(&model, 120, 40).contains("durable-sentinel"));
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: None,
            kind: TranscriptKind::User,
            text: "durable-sentinel".into(),
            payload: None,
            source_key: "thread:create:user".into(),
            created_at_ms: 2,
        });

        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread]));

        assert!(model.pending_submission.is_none());
        assert_eq!(
            rendered(&model, 120, 40)
                .matches("durable-sentinel")
                .count(),
            1
        );
    }

    #[test]
    fn failed_submission_restoration_defers_and_then_restores_the_next_draft() {
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let mut model = ThreadUiModel {
            composer: "next draft\nsecond line".into(),
            pending_submission: Some(PendingSubmission {
                submission_id: 1,
                prompt: "retry me".into(),
                thread_id: Some(thread_id),
                after_sequence: 0,
            }),
            ..Default::default()
        };

        restore_pending_submission(&mut model);
        assert_eq!(model.composer, "retry me");
        assert_eq!(
            model.deferred_composer_draft.as_deref(),
            Some("next draft\nsecond line")
        );

        model.composer.clear();
        model.pending_submission = Some(PendingSubmission {
            submission_id: 2,
            prompt: "retry me".into(),
            thread_id: Some(thread_id),
            after_sequence: 0,
        });
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.thread_id = thread_id;
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 1,
            run_id: thread.active_run_id,
            kind: TranscriptKind::User,
            text: "retry me".into(),
            payload: None,
            source_key: "thread:create:user".into(),
            created_at_ms: 1,
        });

        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread]));
        assert!(model.pending_submission.is_none());
        assert!(model.deferred_composer_draft.is_none());
        assert_eq!(model.composer, "next draft\nsecond line");
    }

    #[test]
    fn submission_reconciliation_uses_redacted_text_and_submission_source_identity() {
        let ids = SystemIdSource::default();
        let secret_prompt = "token=provider-secret";
        let mut model = ThreadUiModel {
            composer: secret_prompt.into(),
            ..Default::default()
        };
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        let mut thread = running_snapshot();
        reduce(
            &mut model,
            ThreadUiInput::SubmissionAssigned {
                submission_id: 1,
                thread_id: thread.thread_id,
            },
        );
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: thread.active_run_id,
            kind: TranscriptKind::User,
            text: redact_thread_text(secret_prompt),
            payload: None,
            source_key: format!(
                "{}:input:answer:card",
                thread.active_run_id.expect("running fixture")
            ),
            created_at_ms: 2,
        });
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread.clone()]));
        assert!(model.pending_submission.is_some());

        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 3,
            run_id: thread.active_run_id,
            kind: TranscriptKind::User,
            text: redact_thread_text(secret_prompt),
            payload: None,
            source_key: "thread:create:user".into(),
            created_at_ms: 3,
        });
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread]));
        assert!(model.pending_submission.is_none());
    }

    #[test]
    fn terminal_session_rejects_new_prompt_and_restores_a_rejected_mailbox_submission() {
        let mut terminal = snapshot(ThreadLifecycle::Failed);
        let mut model = ThreadUiModel {
            sessions: vec![terminal.clone()],
            composer: "keep me".into(),
            ..Default::default()
        };
        assert!(submit_composer(&mut model).is_empty());
        assert_eq!(model.composer, "keep me");
        assert!(model.pending_submission.is_none());

        let running = running_snapshot();
        let thread_id = running.thread_id;
        reduce(&mut model, ThreadUiInput::Snapshot(vec![running]));
        model.composer = "queued once".into();
        let actions = submit_composer(&mut model);
        assert!(matches!(
            actions.as_slice(),
            [ThreadUiAction::QueueFollowUp { thread_id: observed, prompt, .. }]
                if *observed == thread_id && prompt == "queued once"
        ));
        let submission_id = model.pending_submission.as_ref().unwrap().submission_id;
        reduce(&mut model, ThreadUiInput::SubmissionError { submission_id });
        terminal.thread_id = thread_id;
        terminal.active_run_id = None;
        terminal.pending = None;
        reduce(&mut model, ThreadUiInput::Snapshot(vec![terminal]));
        assert_eq!(model.composer, "queued once");
        assert!(model.pending_submission.is_none());
        assert!(session_switch_available(&model));
    }

    #[test]
    fn input_submission_restores_only_after_exact_snapshot_reconciliation() {
        let ids = SystemIdSource::default();
        let mut thread = running_snapshot();
        let run_id = thread.active_run_id.expect("running fixture");
        thread.lifecycle = ThreadLifecycle::WaitingInput;
        thread.pending = Some(ThreadPendingRequest::Input {
            run_id,
            request_id: "request-1".into(),
            prompt: "value".into(),
            expected_run_revision: 1,
        });
        let thread_id = thread.thread_id;
        let mut model = ThreadUiModel::default();
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread.clone()]));
        reduce(&mut model, key(KeyCode::Char('a'), KeyModifiers::NONE));
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::SHIFT));
        reduce(&mut model, key(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(matches!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).as_slice(),
            [ThreadUiAction::ProvideInput { value, .. }] if value == "a\nb"
        ));
        assert!(model.input.is_empty());
        assert_eq!(
            reduce(
                &mut model,
                ThreadUiInput::InputSubmissionError { submission_id: 1 }
            ),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        assert!(model.input.is_empty());
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread.clone()]));
        assert_eq!(model.input, "a\nb");

        model.input.clear();
        reduce(&mut model, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).as_slice(),
            [ThreadUiAction::ProvideInput { .. }]
        ));
        thread.sequence = 2;
        thread.lifecycle = ThreadLifecycle::Running;
        thread.pending = None;
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: Some(run_id),
            kind: TranscriptKind::User,
            text: "x".into(),
            payload: None,
            source_key: format!("{run_id}:input:request-1:card"),
            created_at_ms: 2,
        });
        reduce(&mut model, ThreadUiInput::Snapshot(vec![thread]));
        assert!(model.pending_input_submission.is_none());
        assert!(model.input.is_empty());
        assert_eq!(model.sessions[0].thread_id, thread_id);
    }

    #[test]
    fn correlated_failure_restores_exact_prompt_and_stale_feedback_is_ignored() {
        let exact = "retry this\nwithout losing whitespace  ";
        let mut model = ThreadUiModel {
            composer: exact.into(),
            ..Default::default()
        };
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));

        reduce(
            &mut model,
            ThreadUiInput::SubmissionError { submission_id: 99 },
        );
        assert!(model.composer.is_empty());
        assert!(model.pending_submission.is_some());

        reduce(
            &mut model,
            ThreadUiInput::SubmissionError { submission_id: 1 },
        );
        reduce(&mut model, ThreadUiInput::Snapshot(Vec::new()));
        assert_eq!(model.composer, exact);
        assert!(model.pending_submission.is_none());
        let screen = rendered(&model, 120, 40);
        assert!(screen.contains("Unable to persist submission"));
        assert!(screen.contains("prompt has been restored"));
        assert!(!screen.contains("OPENAI_API_KEY=super-secret"));

        let retry = reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            retry.as_slice(),
            [ThreadUiAction::Start {
                submission_id: 2,
                prompt
            }] if prompt == exact
        ));
        reduce(
            &mut model,
            ThreadUiInput::SubmissionError { submission_id: 1 },
        );
        assert!(model.composer.is_empty());
        assert_eq!(
            model
                .pending_submission
                .as_ref()
                .map(|pending| pending.submission_id),
            Some(2)
        );
    }

    fn reduce_paste_for_test(model: &mut ThreadUiModel, value: &str) -> Vec<ThreadUiAction> {
        reduce(model, ThreadUiInput::Paste(value.into()))
    }

    #[test]
    fn reconciliation_consumes_plain_and_shift_enter_without_mutation() {
        let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
        let ids = SystemIdSource::default();
        thread.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: None,
            kind: TranscriptKind::Failure,
            text: "effect outcome unknown; reconciliation required".into(),
            payload: Some(serde_json::json!({
                "effect_id": "effect-shift-enter-inert",
                "status": "unknown"
            })),
            source_key: "unknown-effect-shift-enter".into(),
            created_at_ms: 2,
        });
        let thread_revision = thread.revision;
        let pending_before = thread.pending.clone();
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            composer: "kept".into(),
            input: "also kept".into(),
            ..Default::default()
        };

        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert!(reduce(&mut model, key(KeyCode::Enter, modifiers)).is_empty());
        }
        assert_eq!(model.composer, "kept");
        assert_eq!(model.input, "also kept");
        assert_eq!(model.sessions[0].revision, thread_revision);
        assert_eq!(model.sessions[0].pending, pending_before);
        assert!(model.reconciliation_confirmation.is_none());
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
        reduce(&mut model, ThreadUiInput::FrameRendered);
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
        assert!(rendered.contains("Permission required"));
        assert!(rendered.contains("Write src/generated.rs"));
        assert!(rendered.contains("create or replace; 43 bytes"));
        assert!(rendered.contains("[d] Deny"));
        assert!(rendered.contains("[Ctrl+A] Allow once"));
        assert!(rendered.contains("Enter does nothing"));
        assert!(!rendered.contains("live-secret-value"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn process_permission_card_is_pinned_at_the_active_waterfall_tail() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let request_id = "process-permission-card";
        let mut thread = snapshot(ThreadLifecycle::WaitingPermission);
        thread.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: request_id.into(),
            description: "Run argv: git status (cwd: .)".into(),
            expected_run_revision: 2,
        });
        thread.transcript.entries.push(transcript_entry(
            &ids,
            2,
            Some(run_id),
            TranscriptKind::ToolCall,
            "Run argv: git status (cwd: .)",
            Some(serde_json::json!({
                "descriptor": {
                    "effect_id": request_id,
                    "name": "process",
                    "input": {
                        "argv": ["git", "status"],
                        "cwd": "."
                    }
                }
            })),
        ));
        let buffer = rendered_buffer(
            &ThreadUiModel {
                sessions: vec![thread],
                ..Default::default()
            },
            120,
            40,
        );
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let (_, card_y) = find_symbol(&buffer, "┌").expect("process permission card");

        assert!(card_y > 20, "permission card must stay near the composer");
        assert!(rendered.contains("Permission required"));
        assert!(rendered.contains("Operation  Run process"));
        assert!(rendered.contains("Target     ."));
        assert!(rendered.contains("Run argv: git status (cwd: .)"));
        assert!(rendered.contains("[d] Deny"));
        assert!(rendered.contains("[Ctrl+A] Allow once"));
    }

    #[test]
    fn gap_clears_progress_after_mailbox_submission() {
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let mut model = ThreadUiModel {
            sessions: vec![running_snapshot()],
            composer: "later".into(),
            ..Default::default()
        };
        reduce(
            &mut model,
            ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
                run_id,
                text: "partial".into(),
            }),
        );
        reduce(
            &mut model,
            ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
                run_id,
                text: " delta".into(),
            }),
        );
        assert!(matches!(
            model.progress.as_slice(),
            [ThreadTransientProgress::AssistantDelta { text, .. }] if text == "partial delta"
        ));
        assert!(!model.progress.is_empty());
        assert!(matches!(
            reduce(
                &mut model,
                ThreadUiInput::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE))
            )
            .as_slice(),
            [ThreadUiAction::QueueFollowUp { prompt, .. }] if prompt == "later"
        ));
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
            vec![ThreadUiAction::Start {
                submission_id: 1,
                prompt: "h".into()
            }]
        );

        reduce(
            &mut model,
            ThreadUiInput::SubmissionError { submission_id: 1 },
        );
        reduce(&mut model, ThreadUiInput::Snapshot(Vec::new()));
        model.composer.clear();

        reduce(&mut model, ThreadUiInput::Snapshot(vec![ready.clone()]));
        reduce(&mut model, key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(matches!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::CONTROL)).as_slice(),
            [ThreadUiAction::FollowUp { thread_id, expected_thread_revision: 1, prompt, .. }]
                if *thread_id == ready.thread_id && prompt == "f"
        ));

        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 2,
            run_id: Some(run_id),
            kind: TranscriptKind::User,
            text: "f".into(),
            payload: None,
            source_key: format!("follow-up:{run_id}:user"),
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
        reduce(&mut model, ThreadUiInput::FrameRendered);
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
        let input_submission_id = model.next_submission_id;
        assert_eq!(
            reduce(&mut model, key(KeyCode::F(5), KeyModifiers::NONE)),
            vec![ThreadUiAction::ProvideInput {
                submission_id: input_submission_id,
                thread_id: ready.thread_id,
                request_id: "input-1".into(),
                value: "v".into(),
            }]
        );

        reduce(&mut model, ThreadUiInput::Disconnected);
        assert!(reduce(&mut model, key(KeyCode::Char('x'), KeyModifiers::NONE)).is_empty());
        assert!(model.input.is_empty());
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        reduce(&mut model, ThreadUiInput::Connected);
        reduce(&mut model, ThreadUiInput::Resize(120, 40));
        model.sessions[0].lifecycle = ThreadLifecycle::Ready;
        model.sessions[0].pending = None;
        assert_eq!(model.focus, ThreadFocus::Composer);
        assert!(reduce(&mut model, key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
        assert_eq!(model.focus, ThreadFocus::Navigation);
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('q'), KeyModifiers::NONE)),
            vec![ThreadUiAction::Quit]
        );
    }

    #[test]
    fn composer_owns_reserved_printables_until_navigation_is_explicit() {
        let mut model = ThreadUiModel::default();
        for value in ['q', 's', 'j', 'k', '?'] {
            assert!(reduce(&mut model, key(KeyCode::Char(value), KeyModifiers::NONE)).is_empty());
        }
        reduce(&mut model, ThreadUiInput::Paste("\n粘贴\u{1b}[31m".into()));
        assert_eq!(model.composer, "qsjk?\n粘贴");
        assert!(!model.help);

        assert!(reduce(&mut model, key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
        assert_eq!(model.focus, ThreadFocus::Navigation);
        assert!(reduce(&mut model, key(KeyCode::Char('?'), KeyModifiers::NONE)).is_empty());
        assert!(model.help);
        assert_eq!(model.composer, "qsjk?\n粘贴");
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('q'), KeyModifiers::NONE)),
            vec![ThreadUiAction::Quit]
        );

        model.focus = ThreadFocus::Composer;
        assert_eq!(
            reduce(&mut model, key(KeyCode::F(10), KeyModifiers::NONE)),
            vec![ThreadUiAction::Quit]
        );
    }

    #[test]
    fn ctrl_p_command_palette_executes_every_advertised_command() {
        let mut model = ThreadUiModel::with_startup(test_startup());
        let ctrl_p = || key(KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert!(reduce(&mut model, ctrl_p()).is_empty());
        assert!(model.command_palette);
        assert!(rendered(&model, 100, 30).contains("Commands"));
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            model.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );

        reduce(&mut model, ctrl_p());
        for _ in 0..BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Model)
            .expect("model command")
        {
            reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        }
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(model.model_picker.is_some());
        reduce(&mut model, key(KeyCode::Esc, KeyModifiers::NONE));

        reduce(&mut model, ctrl_p());
        for _ in 0..BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Help)
            .expect("help command")
        {
            reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        }
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(model.help);

        model.help = false;
        reduce(&mut model, ctrl_p());
        for _ in 0..BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Navigation)
            .expect("navigation command")
        {
            reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        }
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(model.focus, ThreadFocus::Navigation);

        reduce(&mut model, ctrl_p());
        for _ in 0..BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Refresh)
            .expect("refresh command")
        {
            reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::RefreshSnapshots]
        );

        reduce(&mut model, ctrl_p());
        for _ in 0..BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Quit)
            .expect("quit command")
        {
            reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::Quit]
        );

        let mut blocked = permission_model();
        assert!(reduce(&mut blocked, ctrl_p()).is_empty());
        assert!(!blocked.command_palette);
        assert!(reduce(&mut blocked, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn model_picker_groups_filters_and_switches_draft_or_ready_session() {
        let mut draft = ThreadUiModel::with_startup(test_startup());
        draft.composer = "/model".into();
        assert!(reduce(&mut draft, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        let screen = rendered(&draft, 100, 32);
        assert!(screen.contains("Select provider / model"));
        assert!(screen.contains("runtime-provider"));
        assert!(screen.contains("Runtime Stable"));
        assert!(screen.contains("Runtime Fast"));
        assert!(screen.contains("runtime-model-fast"));
        assert!(screen.contains("other-provider"));
        for value in "stable".chars() {
            reduce(&mut draft, key(KeyCode::Char(value), KeyModifiers::NONE));
        }
        assert_eq!(
            filtered_model_options(&draft)
                .iter()
                .map(|option| option.model.as_str())
                .collect::<Vec<_>>(),
            ["runtime-model"]
        );
        for _ in 0.."stable".len() {
            reduce(&mut draft, key(KeyCode::Backspace, KeyModifiers::NONE));
        }
        for value in "other".chars() {
            reduce(&mut draft, key(KeyCode::Char(value), KeyModifiers::NONE));
        }
        assert!(rendered(&draft, 100, 32).contains("other-model"));
        assert!(reduce(&mut draft, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        draft.composer = "use selected model".into();
        assert!(matches!(
            reduce(&mut draft, key(KeyCode::Enter, KeyModifiers::NONE)).as_slice(),
            [ThreadUiAction::StartWithModel {
                provider_name,
                model,
                prompt,
                ..
            }] if provider_name == "other-provider"
                && model == "other-model"
                && prompt == "use selected model"
        ));

        let mut ready = snapshot(ThreadLifecycle::Ready);
        ready.binding.provider_name = "runtime-provider".into();
        ready.binding.model = "runtime-model".into();
        let thread_id = ready.thread_id;
        let revision = ready.revision;
        let mut session = ThreadUiModel::with_startup(test_startup());
        reduce(&mut session, ThreadUiInput::SessionOpened(Box::new(ready)));
        session.composer = "/model".into();
        reduce(&mut session, key(KeyCode::Enter, KeyModifiers::NONE));
        for value in "fast".chars() {
            reduce(&mut session, key(KeyCode::Char(value), KeyModifiers::NONE));
        }
        assert_eq!(
            reduce(&mut session, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::SwitchModel {
                switch_id: 1,
                thread_id,
                expected_thread_revision: revision,
                provider_name: "runtime-provider".into(),
                model: "runtime-model-fast".into(),
            }]
        );
        session.composer = "must wait".into();
        assert!(reduce(&mut session, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(session.composer, "must wait");
        assert_eq!(
            session.status,
            "Wait for the model switch to finish before submitting"
        );
        assert_eq!(
            reduce(
                &mut session,
                ThreadUiInput::ModelSwitchCompleted { switch_id: 1 }
            ),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        assert!(session.pending_model_switch.is_some());
        session.sessions[0].binding.model = "runtime-model-fast".into();
        let refreshed = session.sessions.clone();
        assert!(reduce(&mut session, ThreadUiInput::Snapshot(refreshed)).is_empty());
        assert!(session.pending_model_switch.is_none());
        assert_eq!(
            session.status,
            "Model switched to runtime-provider/runtime-model-fast"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn model_picker_navigation_empty_current_active_and_failed_states_are_total() {
        let mut draft = ThreadUiModel::with_startup(test_startup());
        open_model_picker(&mut draft);
        assert!(reduce(&mut draft, key(KeyCode::Down, KeyModifiers::NONE)).is_empty());
        assert!(reduce(&mut draft, key(KeyCode::Up, KeyModifiers::NONE)).is_empty());
        reduce(&mut draft, key(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(filtered_model_options(&draft).is_empty());
        assert!(rendered(&draft, 100, 32).contains("No matching provider models"));
        assert!(reduce(&mut draft, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert!(draft.model_picker.is_some());
        reduce(&mut draft, key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(!filtered_model_options(&draft).is_empty());
        reduce(&mut draft, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(draft.model_picker.is_none());

        let mut ready = snapshot(ThreadLifecycle::Ready);
        ready.binding.provider_name = "runtime-provider".into();
        ready.binding.model = "runtime-model".into();
        let mut current = ThreadUiModel::with_startup(test_startup());
        reduce(&mut current, ThreadUiInput::SessionOpened(Box::new(ready)));
        open_model_picker(&mut current);
        assert!(reduce(&mut current, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            current.status,
            "Model already selected: runtime-provider/runtime-model"
        );

        let mut active = ThreadUiModel::with_startup(test_startup());
        reduce(
            &mut active,
            ThreadUiInput::SessionOpened(Box::new(running_snapshot())),
        );
        open_model_picker(&mut active);
        for value in "fast".chars() {
            reduce(&mut active, key(KeyCode::Char(value), KeyModifiers::NONE));
        }
        assert!(reduce(&mut active, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            active.status,
            "Model switching is disabled while work or a request is active"
        );

        let thread_id = current.selected_thread().unwrap().thread_id;
        current.pending_model_switch = Some(PendingModelSwitch {
            switch_id: 4,
            thread_id,
            provider_name: "other-provider".into(),
            model: "other-model".into(),
        });
        reduce(
            &mut current,
            ThreadUiInput::ModelSwitchError {
                switch_id: 3,
                error: "stale".into(),
            },
        );
        assert!(current.pending_model_switch.is_some());
        reduce(
            &mut current,
            ThreadUiInput::ModelSwitchError {
                switch_id: 4,
                error: "lease lost".into(),
            },
        );
        assert!(current.pending_model_switch.is_none());
        assert_eq!(current.status, "Model switch rejected: lease lost");

        let mut empty = ThreadUiModel::default();
        open_model_picker(&mut empty);
        assert_eq!(empty.status, PROVIDER_SETUP_GUIDANCE);
        assert!(filtered_model_options(&empty).is_empty());
        assert!(
            reduce_model_picker_key(&mut empty, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                .is_empty()
        );

        let assigned = snapshot(ThreadLifecycle::Ready);
        let assigned_id = assigned.thread_id;
        let mut materializing = ThreadUiModel::with_startup(test_startup());
        materializing.pending_submission = Some(PendingSubmission {
            submission_id: 12,
            prompt: "materialize".into(),
            thread_id: Some(assigned_id),
            after_sequence: 0,
        });
        reduce(
            &mut materializing,
            ThreadUiInput::Snapshot(vec![assigned.clone()]),
        );
        assert_eq!(
            materializing.active_conversation,
            Some(ActiveConversation::Session(assigned_id))
        );
        let event = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            thread_id: assigned_id,
            revision: assigned.revision + 1,
            sequence: assigned.sequence + 1,
            event: ThreadEvent::BindingChanged {
                provider_name: "other".into(),
                model: "other-model".into(),
            },
        };
        assert_eq!(
            reduce(&mut materializing, ThreadUiInput::Event(event)),
            vec![ThreadUiAction::RefreshSnapshots]
        );

        let mut blocked = ThreadUiModel::with_startup(test_startup());
        blocked.pending_model_switch = Some(PendingModelSwitch {
            switch_id: 1,
            thread_id: assigned_id,
            provider_name: "other".into(),
            model: "other-model".into(),
        });
        blocked.composer = "/new".into();
        assert!(submit_composer(&mut blocked).is_empty());
        assert_eq!(blocked.composer, "/new");
        assert_eq!(
            blocked.status,
            "Wait for the model switch to finish before submitting"
        );
        restore_pending_submission(&mut blocked);
        let mut editor = String::new();
        append_editor_text(&mut editor, "a\u{1b}[31mb\u{7}c", 2);
        assert_eq!(editor, "ab");

        let mut slash_blocked = ThreadUiModel::with_startup(test_startup());
        reduce(
            &mut slash_blocked,
            ThreadUiInput::SessionOpened(Box::new(running_snapshot())),
        );
        slash_blocked.composer = "/model".into();
        assert!(submit_composer(&mut slash_blocked).is_empty());
        assert_eq!(slash_blocked.composer, "/model");
        assert_eq!(
            slash_blocked.status,
            "Session switching is disabled while work or a request is active"
        );

        let mut slash_popup = ThreadUiModel {
            composer: "/".into(),
            ..Default::default()
        };
        assert_eq!(
            reduce_slash_popup_key(
                &mut slash_popup,
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(Vec::new())
        );
        open_model_picker(&mut draft);
        assert!(
            reduce_model_picker_key(&mut draft, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
                .is_empty()
        );
        assert!(
            reduce_composer_key(
                &mut ThreadUiModel::default(),
                KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)
            )
            .is_empty()
        );
        assert_eq!(presentation_text("abcd", 2), "ab…");
        let mut metadata_snapshot = snapshot(ThreadLifecycle::Ready);
        let entry = metadata_snapshot.transcript.entries.remove(0);
        assert!(tool_metadata(&entry).is_empty());
    }

    #[test]
    fn slash_popup_filters_navigates_executes_and_preserves_composer_ownership() {
        let mut model = ThreadUiModel::default();
        assert!(reduce(&mut model, key(KeyCode::Char('/'), KeyModifiers::NONE)).is_empty());
        let all = rendered(&model, 100, 32);
        assert!(all.contains("Suggestions"));
        assert!(all.contains("Start a new conversation draft"));
        assert!(all.contains("Find and resume a saved session"));
        assert!(all.contains("↑/↓ select · Enter run · Esc close"));

        reduce(&mut model, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::ShowSessions { query: None }]
        );
        assert!(model.composer.is_empty());

        model.composer = "/ref".into();
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        model.composer = "/resu".into();
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::ShowSessions { query: None }]
        );

        model.composer = "/h".into();
        assert!(reduce(&mut model, key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
        assert_eq!(model.focus, ThreadFocus::Composer);
        assert_eq!(model.composer, "/h");
        assert!(!rendered(&model, 100, 32).contains("Suggestions"));
        reduce(&mut model, key(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(model.composer, "/he");
        assert!(rendered(&model, 100, 32).contains("Show keyboard shortcuts"));

        reduce(&mut model, key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(model.command_palette);
        let palette = rendered(&model, 100, 32);
        assert!(palette.contains("Commands"));
        assert!(!palette.contains("Suggestions"));
    }

    #[test]
    fn slash_popup_stays_hidden_for_unknown_prompts_and_blocking_requests() {
        let mut prompt = ThreadUiModel {
            composer: "/tmp/file".into(),
            ..Default::default()
        };
        assert!(!rendered(&prompt, 100, 32).contains("Suggestions"));
        assert_eq!(
            reduce(&mut prompt, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::Start {
                submission_id: 1,
                prompt: "/tmp/file".into(),
            }]
        );

        let mut blocked = permission_model();
        blocked.composer = "/".into();
        assert!(!rendered(&blocked, 100, 32).contains("Suggestions"));
        assert!(reduce(&mut blocked, key(KeyCode::Down, KeyModifiers::NONE)).is_empty());
        assert_eq!(blocked.composer, "/");
    }

    #[test]
    fn startup_catalog_keeps_a_fresh_draft_until_resume_is_explicit() {
        let existing = snapshot(ThreadLifecycle::Ready);
        let summary = ThreadSessionSummary {
            thread_id: existing.thread_id,
            title: "Saved session".into(),
            workspace_root: "/workspace".into(),
            parent_thread_id: None,
            lifecycle: ThreadLifecycle::Ready,
            provider_name: "provider".into(),
            model: "model".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        let mut startup = ThreadUiModel::with_startup(test_startup());
        assert!(
            reduce(
                &mut startup,
                ThreadUiInput::SessionCatalog(vec![summary.clone()])
            )
            .is_empty()
        );
        assert_eq!(
            startup.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );
        assert!(startup.sessions.is_empty());

        let mut explicit_new = ThreadUiModel::with_startup(test_startup());
        explicit_new.composer = "/new".into();
        assert!(reduce(&mut explicit_new, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert!(
            reduce(
                &mut explicit_new,
                ThreadUiInput::SessionCatalog(vec![summary])
            )
            .is_empty()
        );
        assert_eq!(
            explicit_new.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_in_every_focus_mode_and_saturates() {
        let mut model = ThreadUiModel::default();
        let mouse = |kind| {
            ThreadUiInput::Mouse(MouseEvent {
                kind,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })
        };

        assert!(reduce(&mut model, mouse(MouseEventKind::ScrollUp)).is_empty());
        assert_eq!(model.scroll, 3);
        model.focus = ThreadFocus::Navigation;
        reduce(&mut model, mouse(MouseEventKind::ScrollUp));
        assert_eq!(model.scroll, 6);
        reduce(&mut model, mouse(MouseEventKind::ScrollDown));
        reduce(&mut model, mouse(MouseEventKind::ScrollDown));
        reduce(&mut model, mouse(MouseEventKind::ScrollDown));
        assert_eq!(model.scroll, 0);
        reduce(&mut model, mouse(MouseEventKind::Moved));
        assert_eq!(model.scroll, 0);
    }

    #[test]
    fn slash_commands_keep_local_session_typed_and_prompt_paths_distinct() {
        let mut model = ThreadUiModel::with_startup(test_startup());
        let existing = snapshot(ThreadLifecycle::Ready);
        let summary = ThreadSessionSummary {
            thread_id: existing.thread_id,
            title: "Resume me".into(),
            workspace_root: "/workspace".into(),
            parent_thread_id: None,
            lifecycle: ThreadLifecycle::Ready,
            provider_name: "p".into(),
            model: "m".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        model.composer = "/new".into();
        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(
            model.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );
        assert!(model.sessions.is_empty());

        model.composer = format!("/resume {}", summary.thread_id);
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::ShowSessions {
                query: Some(summary.thread_id.to_string()),
            }]
        );
        assert!(model.composer.is_empty());
        assert_eq!(
            reduce(
                &mut model,
                ThreadUiInput::SessionCatalogReady {
                    sessions: vec![summary],
                    query: Some(existing.thread_id.to_string()),
                }
            ),
            vec![ThreadUiAction::OpenSession {
                thread_id: existing.thread_id,
            }]
        );
        assert!(
            reduce(
                &mut model,
                ThreadUiInput::SessionOpened(Box::new(existing.clone()))
            )
            .is_empty()
        );
        assert_eq!(
            model.active_conversation,
            Some(ActiveConversation::Session(existing.thread_id))
        );

        model.active_conversation = Some(ActiveConversation::NewSessionDraft);
        model.sessions.clear();
        model.composer = "/tmp/file".into();
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::Start {
                submission_id: 1,
                prompt: "/tmp/file".into(),
            }]
        );

        let mut running = snapshot(ThreadLifecycle::Running);
        running.active_run_id = Some(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        let mut blocked = ThreadUiModel::default();
        reduce(&mut blocked, ThreadUiInput::Snapshot(vec![running]));
        blocked.composer = "/new".into();
        assert!(reduce(&mut blocked, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());
        assert_eq!(blocked.composer, "/new");
        assert!(blocked.status.contains("Session switching is disabled"));

        for lifecycle in [ThreadLifecycle::Failed, ThreadLifecycle::Interrupted] {
            assert_terminal_session_switching_available(lifecycle);
        }

        let mut reconciliation = ThreadUiModel {
            sessions: vec![snapshot(ThreadLifecycle::ReconciliationRequired)],
            ..Default::default()
        };
        assert!(
            dispatch_builtin(&mut reconciliation, BuiltinCommand::Sessions, String::new())
                .is_empty()
        );
        assert!(
            reconciliation
                .status
                .contains("Session switching is disabled")
        );
    }

    #[test]
    fn session_management_commands_are_typed_and_workspace_scoped() {
        let mut model = ThreadUiModel::with_startup(test_startup());
        let current = snapshot(ThreadLifecycle::Ready);
        let thread_id = current.thread_id;
        reduce(&mut model, ThreadUiInput::SessionOpened(Box::new(current)));
        assert_eq!(
            dispatch_builtin(&mut model, BuiltinCommand::Rename, "new title".into()),
            vec![ThreadUiAction::RenameSession {
                thread_id,
                title: "new title".into(),
            }]
        );
        assert_eq!(
            dispatch_builtin(&mut model, BuiltinCommand::Fork, "branch title".into()),
            vec![ThreadUiAction::ForkSession {
                thread_id,
                title: Some("branch title".into()),
            }]
        );
        assert_eq!(
            dispatch_builtin(&mut model, BuiltinCommand::Sessions, "durable".into()),
            vec![ThreadUiAction::SearchSessions {
                query: "durable".into(),
            }]
        );
        assert!(
            dispatch_builtin(&mut model, BuiltinCommand::Sessions, "--all durable".into())
                .is_empty()
        );
        assert!(model.status.contains("current-workspace"));
    }

    #[test]
    fn session_management_feedback_refreshes_and_opens_forks() {
        let ids = SystemIdSource::default();
        let current = ThreadId::from_uuid(ids.next_uuid_v7());
        let fork = ThreadId::from_uuid(ids.next_uuid_v7());
        let (feedback_tx, feedback_rx) = std::sync::mpsc::channel();
        let (_progress_tx, progress_rx) = std::sync::mpsc::channel();
        let mut model = ThreadUiModel {
            active_conversation: Some(ActiveConversation::Session(current)),
            ..ThreadUiModel::default()
        };

        feedback_tx
            .send(ThreadUiFeedback::session_management(Ok(
                SessionManagementOutcome::Updated("renamed".into()),
            )))
            .unwrap();
        feedback_tx
            .send(ThreadUiFeedback::session_management(Ok(
                SessionManagementOutcome::Forked(fork),
            )))
            .unwrap();
        let (changed, actions) = drain_runtime_updates(&mut model, &feedback_rx, &progress_rx);
        assert!(changed);
        assert!(actions.contains(&ThreadUiAction::RefreshSnapshots));
        assert!(actions.contains(&ThreadUiAction::OpenSession { thread_id: fork }));
        feedback_tx
            .send(ThreadUiFeedback::session_management(Err("rejected".into())))
            .unwrap();
        let (changed, actions) = drain_runtime_updates(&mut model, &feedback_rx, &progress_rx);
        assert!(changed);
        assert!(actions.is_empty());
        assert!(model.status.contains("rejected"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn projection_defaults_and_action_adapter_cover_session_discovery_and_refresh() {
        let saved = snapshot(ThreadLifecycle::Ready);
        let mut untitled = snapshot(ThreadLifecycle::Ready);
        untitled.transcript.entries.clear();

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone(), untitled.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let catalog = projection.session_catalog().unwrap();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].title, "hello");
        assert_eq!(catalog[0].created_at_ms, 1);
        assert_eq!(catalog[0].updated_at_ms, 1);
        assert_eq!(catalog[1].title, "Untitled session");
        assert_eq!(catalog[1].created_at_ms, 0);
        assert_eq!(catalog[1].updated_at_ms, 0);
        assert!(
            catalog
                .iter()
                .all(|session| session.workspace_root.is_empty())
        );

        let missing = snapshot(ThreadLifecycle::Ready).thread_id;
        let mut missing_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert_eq!(
            missing_projection.session(missing).unwrap_err(),
            format!("session {missing} was not found")
        );

        let mut exact_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()], vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut exact_model = ThreadUiModel::default();
        let mut dispatched = Vec::new();
        assert!(
            !apply_thread_actions(
                &mut exact_projection,
                &mut exact_model,
                &mut |action| {
                    dispatched.push(action);
                    Ok(())
                },
                vec![ThreadUiAction::ShowSessions {
                    query: Some(saved.thread_id.to_string()),
                }],
            )
            .unwrap()
        );
        assert!(dispatched.is_empty());
        assert_eq!(
            exact_model.active_conversation,
            Some(ActiveConversation::Session(saved.thread_id))
        );

        let mut picker_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut picker_model = ThreadUiModel::default();
        assert!(
            !apply_thread_actions(
                &mut picker_projection,
                &mut picker_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::ShowSessions { query: None }],
            )
            .unwrap()
        );
        assert!(picker_model.session_picker);
        assert_eq!(picker_model.status, "Select a session to resume");

        let mut search_projection = ScriptedProjection {
            snapshots: VecDeque::from([
                vec![saved.clone()],
                vec![saved.clone()],
                vec![saved.clone()],
            ]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert_eq!(
            search_projection
                .search_session_catalog("hello")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_projection.search_session_catalog("").unwrap().len(),
            1
        );
        let mut search_model = ThreadUiModel::default();
        assert!(
            !apply_thread_actions(
                &mut search_projection,
                &mut search_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::SearchSessions {
                    query: "hello".into(),
                }],
            )
            .unwrap()
        );
        assert_eq!(search_model.session_catalog[0].thread_id, saved.thread_id);

        let mut missing_open_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut missing_open_model = ThreadUiModel::default();
        assert!(
            !apply_thread_actions(
                &mut missing_open_projection,
                &mut missing_open_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::OpenSession { thread_id: missing }],
            )
            .unwrap()
        );
        assert!(missing_open_model.status.contains("was not found"));

        let mut active_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        assert!(
            !apply_thread_actions(
                &mut active_projection,
                &mut exact_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::RefreshSnapshots],
            )
            .unwrap()
        );
        assert_eq!(exact_model.sessions[0].thread_id, saved.thread_id);

        let mut draft_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut draft_model = ThreadUiModel {
            active_conversation: Some(ActiveConversation::NewSessionDraft),
            ..Default::default()
        };
        assert!(
            !apply_thread_actions(
                &mut draft_projection,
                &mut draft_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::RefreshSnapshots],
            )
            .unwrap()
        );
        assert_eq!(draft_model.session_catalog[0].thread_id, saved.thread_id);

        let mut snapshot_projection = ScriptedProjection {
            snapshots: VecDeque::from([vec![saved.clone()]]),
            poll: ThreadProjectionPoll::Empty,
        };
        let mut snapshot_model = ThreadUiModel::default();
        assert!(
            !apply_thread_actions(
                &mut snapshot_projection,
                &mut snapshot_model,
                &mut |_| Ok(()),
                vec![ThreadUiAction::RefreshSnapshots],
            )
            .unwrap()
        );
        assert_eq!(snapshot_model.sessions[0].thread_id, saved.thread_id);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_catalog_resolution_picker_keys_and_draft_materialization_are_total() {
        let mut first = snapshot(ThreadLifecycle::Ready);
        let second = snapshot(ThreadLifecycle::Ready);
        let first_summary = session_summary(&first, "same title", "/workspace/one");
        let second_summary = session_summary(&second, "same title", "/workspace/two");

        let mut startup_empty = ThreadUiModel::with_startup(test_startup());
        assert!(
            reduce(
                &mut startup_empty,
                ThreadUiInput::SessionCatalog(Vec::new())
            )
            .is_empty()
        );
        assert_eq!(
            startup_empty.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );

        let mut empty = ThreadUiModel::default();
        assert!(
            reduce(
                &mut empty,
                ThreadUiInput::SessionCatalogReady {
                    sessions: Vec::new(),
                    query: Some(String::new()),
                },
            )
            .is_empty()
        );
        assert!(empty.session_picker);
        assert_eq!(empty.status, "No saved sessions");

        let mut missing = ThreadUiModel::default();
        assert!(
            reduce(
                &mut missing,
                ThreadUiInput::SessionCatalogReady {
                    sessions: Vec::new(),
                    query: Some("missing".into()),
                },
            )
            .is_empty()
        );
        assert!(missing.status.contains("No exact session match"));

        let mut ambiguous = ThreadUiModel::default();
        assert!(
            reduce(
                &mut ambiguous,
                ThreadUiInput::SessionCatalogReady {
                    sessions: vec![first_summary.clone(), second_summary.clone()],
                    query: Some("same title".into()),
                },
            )
            .is_empty()
        );
        assert!(ambiguous.session_picker);
        assert!(ambiguous.status.contains("Multiple sessions match"));

        ambiguous.session_index = 0;
        assert!(reduce(&mut ambiguous, key(KeyCode::Down, KeyModifiers::NONE)).is_empty());
        assert_eq!(ambiguous.session_index, 1);
        assert!(reduce(&mut ambiguous, key(KeyCode::Char('k'), KeyModifiers::NONE)).is_empty());
        assert_eq!(ambiguous.session_index, 0);
        assert!(reduce(&mut ambiguous, key(KeyCode::Char('j'), KeyModifiers::NONE)).is_empty());
        assert_eq!(ambiguous.session_index, 1);
        assert!(reduce(&mut ambiguous, key(KeyCode::Up, KeyModifiers::NONE)).is_empty());
        assert_eq!(ambiguous.session_index, 0);
        assert!(reduce(&mut ambiguous, key(KeyCode::Char('x'), KeyModifiers::NONE)).is_empty());
        assert_eq!(
            reduce(&mut ambiguous, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::OpenSession {
                thread_id: first.thread_id,
            }]
        );
        assert!(reduce(&mut ambiguous, key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
        assert!(!ambiguous.session_picker);

        let mut no_rows = ThreadUiModel {
            session_picker: true,
            ..Default::default()
        };
        assert!(reduce(&mut no_rows, key(KeyCode::Enter, KeyModifiers::NONE)).is_empty());

        let mut materializing = ThreadUiModel::with_startup(test_startup());
        first.transcript.entries[0].source_key = "thread:create:user".into();
        materializing.pending_submission = Some(PendingSubmission {
            submission_id: 9,
            prompt: "hello".into(),
            thread_id: None,
            after_sequence: 0,
        });
        assert!(
            reduce(
                &mut materializing,
                ThreadUiInput::Snapshot(vec![first.clone()]),
            )
            .is_empty()
        );
        assert_eq!(
            materializing.active_conversation,
            Some(ActiveConversation::NewSessionDraft)
        );
        reduce(
            &mut materializing,
            ThreadUiInput::SubmissionAssigned {
                submission_id: 9,
                thread_id: first.thread_id,
            },
        );
        assert_eq!(
            materializing.active_conversation,
            Some(ActiveConversation::Session(first.thread_id))
        );
        assert!(materializing.pending_submission.is_none());
        assert!(
            reduce(
                &mut materializing,
                ThreadUiInput::Snapshot(vec![first.clone(), second]),
            )
            .is_empty()
        );
        assert_eq!(
            materializing.selected_thread().unwrap().thread_id,
            first.thread_id
        );
    }

    #[test]
    fn command_failure_and_non_tty_adapter_boundaries_are_explicit() {
        let mut disabled = ThreadUiModel {
            pending_submission: Some(PendingSubmission {
                submission_id: 1,
                prompt: "busy".into(),
                thread_id: None,
                after_sequence: 0,
            }),
            ..Default::default()
        };
        assert!(dispatch_builtin(&mut disabled, BuiltinCommand::New, String::new()).is_empty());
        assert!(disabled.status.contains("Session switching is disabled"));

        let mut invalid = ThreadUiModel {
            composer: "/help unexpected".into(),
            ..Default::default()
        };
        assert!(submit_composer(&mut invalid).is_empty());
        assert_eq!(invalid.composer, "/help unexpected");
        assert!(invalid.status.contains("does not accept arguments"));

        let mut queued = ThreadUiModel::default();
        reduce(
            &mut queued,
            ThreadUiInput::Snapshot(vec![running_snapshot()]),
        );
        queued.queued_follow_up = Some("first".into());
        queued.composer = "second".into();
        assert!(submit_composer(&mut queued).is_empty());
        assert_eq!(queued.composer, "second");
        assert!(queued.pending_submission.is_none());
        assert_eq!(queued.status, "A follow-up is already queued");

        let mut projection = ScriptedProjection {
            snapshots: VecDeque::new(),
            poll: ThreadProjectionPoll::Empty,
        };
        let (_feedback_tx, feedback_rx) = std::sync::mpsc::channel();
        let (_progress_tx, progress_rx) = std::sync::mpsc::channel();
        assert!(matches!(
            run_with_feedback_and_progress(
                &mut projection,
                test_startup(),
                |_| Ok(()),
                &feedback_rx,
                &progress_rx,
            ),
            Err(TuiError::NonTty)
        ));
    }

    #[test]
    fn session_picker_rendering_covers_populated_and_empty_catalogs() {
        let first = snapshot(ThreadLifecycle::Ready);
        let second = snapshot(ThreadLifecycle::Failed);
        let populated = ThreadUiModel {
            session_picker: true,
            session_index: 1,
            session_catalog: vec![
                session_summary(&first, "First saved session", "/workspace/one"),
                session_summary(&second, "Second saved session", "/workspace/two"),
            ],
            ..Default::default()
        };
        let screen = rendered(&populated, 120, 32);
        assert!(screen.contains("Sessions"));
        assert!(screen.contains("First saved session"));
        assert!(screen.contains("Second saved session"));
        assert!(screen.contains("/workspace/one"));
        assert!(screen.contains("Enter resume"));

        let empty = ThreadUiModel {
            session_picker: true,
            ..Default::default()
        };
        let screen = rendered(&empty, 100, 28);
        assert!(screen.contains("No saved sessions"));
    }

    #[test]
    fn unicode_graphemes_wrap_and_place_the_composer_cursor_by_display_width() {
        assert_eq!(display_width("界"), 2);
        assert_eq!(display_width("👩‍💻"), 2);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(
            wrap_text("你好👩‍💻e\u{301}", 4),
            vec!["你好".to_owned(), "👩‍💻e\u{301}".to_owned()]
        );
        assert_eq!(wrapped_line_count(&"界".repeat(34), 66), 2);

        let model = ThreadUiModel {
            startup: Some(test_startup()),
            composer: "a界👩‍💻e\u{301}".into(),
            ..Default::default()
        };
        let backend = TestBackend::new(72, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (10, 20));
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains('界'));
        assert!(screen.contains("👩‍💻"));
        assert!(screen.contains("e\u{301}"));
    }

    #[test]
    fn exact_display_width_keeps_a_trailing_caret_row_for_every_grapheme_shape() {
        let ascii = "a".repeat(66);
        let cases = [
            ("ascii", ascii.clone()),
            ("cjk", "界".repeat(33)),
            ("emoji zwj", "👩‍💻".repeat(33)),
            ("combining", "e\u{301}".repeat(66)),
            ("tab", format!("{}\t{}", "a".repeat(60), "a".repeat(2))),
            ("trailing newline", format!("{ascii}\n")),
        ];

        for (name, text) in cases {
            let layout = composer_text_layout(&text, 66);
            assert_eq!(layout.rows.len(), 2, "{name}");
            assert_eq!(display_width(&layout.rows[0]), 66, "{name}");
            assert!(layout.rows[1].is_empty(), "{name}");
            assert_eq!((layout.caret_row, layout.caret_column), (1, 0), "{name}");

            let model = ThreadUiModel {
                startup: Some(test_startup()),
                composer: text,
                ..Default::default()
            };
            let viewport = viewport_layout(Rect::new(0, 0, 72, 24), visual_state(&model), &model);
            assert_eq!(viewport.composer, Rect::new(0, 18, 72, 6), "{name}");
            let backend = TestBackend::new(72, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &model)).unwrap();
            let cursor = terminal.backend().cursor_position();
            assert_eq!((cursor.x, cursor.y), (4, 20), "{name}");
        }

        let mut pasted = ThreadUiModel {
            startup: Some(test_startup()),
            ..Default::default()
        };
        reduce(&mut pasted, ThreadUiInput::Paste(ascii.clone()));
        assert_eq!(pasted.composer, ascii);
        let backend = TestBackend::new(72, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &pasted)).unwrap();
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (4, 20));

        pasted.composer.push('b');
        let layout = composer_text_layout(&pasted.composer, 66);
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.rows[1], "b");
        assert_eq!((layout.caret_row, layout.caret_column), (1, 1));
        terminal.draw(|frame| render(frame, &pasted)).unwrap();
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (5, 20));
    }

    #[test]
    fn pending_input_uses_the_same_exact_boundary_layout_and_caret() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::WaitingInput);
        thread.pending = Some(ThreadPendingRequest::Input {
            run_id,
            request_id: "boundary-input".into(),
            prompt: "value".into(),
            expected_run_revision: 2,
        });
        let model = ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![thread],
            input: "界".repeat(33),
            composer: "composer must not drive input layout".into(),
            ..Default::default()
        };

        assert_eq!(editor_text(&model), "界".repeat(33));
        let viewport = viewport_layout(Rect::new(0, 0, 72, 24), visual_state(&model), &model);
        assert_eq!(viewport.composer, Rect::new(0, 18, 72, 6));
        let backend = TestBackend::new(72, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &model)).unwrap();
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (4, 20));
    }

    #[test]
    fn ctrl_c_requires_a_second_press_and_first_press_cancels_active_work() {
        let thread = snapshot(ThreadLifecycle::Running);
        let thread_id = thread.thread_id;
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            ..Default::default()
        };
        let now = Instant::now();

        assert_eq!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                now,
            ),
            vec![ThreadUiAction::Cancel { thread_id }]
        );
        assert_eq!(status_line(&model), "Ctrl+C again to exit");
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                now + Duration::from_millis(50),
            )
            .is_empty()
        );
        assert_eq!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                now + Duration::from_millis(250),
            ),
            vec![ThreadUiAction::Quit]
        );
        assert!(model.ctrl_c_exit_armed_until.is_none());
    }

    #[test]
    fn ctrl_c_exit_confirmation_expires_and_other_input_disarms_it() {
        let mut model = ThreadUiModel::default();
        let now = Instant::now();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(reduce_key_at(&mut model, ctrl_c, now).is_empty());
        assert!(
            reduce_key_at(
                &mut model,
                ctrl_c,
                now + CTRL_C_EXIT_WINDOW + Duration::from_millis(1),
            )
            .is_empty()
        );
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                now + CTRL_C_EXIT_WINDOW + Duration::from_millis(2),
            )
            .is_empty()
        );
        assert_eq!(model.composer, "x");
        assert!(model.ctrl_c_exit_armed_until.is_none());

        assert!(
            reduce_key_at(
                &mut model,
                ctrl_c,
                now + CTRL_C_EXIT_WINDOW + Duration::from_millis(3),
            )
            .is_empty()
        );
    }

    #[test]
    fn permission_and_input_branches_consume_the_whole_key_event() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut permission = snapshot(ThreadLifecycle::WaitingPermission);
        permission.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "exact-permission".into(),
            description: "write one file".into(),
            expected_run_revision: 7,
        });
        let mut model = ThreadUiModel {
            sessions: vec![permission],
            composer: "kept".into(),
            input: "also kept".into(),
            ..Default::default()
        };
        for code in [
            KeyCode::Enter,
            KeyCode::Char('q'),
            KeyCode::Char('?'),
            KeyCode::Esc,
        ] {
            assert!(reduce(&mut model, key(code, KeyModifiers::NONE)).is_empty());
        }
        reduce(&mut model, ThreadUiInput::Paste("blocked paste".into()));
        assert_eq!(model.composer, "kept");
        assert_eq!(model.input, "also kept");
        assert_eq!(model.focus, ThreadFocus::Composer);
        reduce(&mut model, ThreadUiInput::FrameRendered);
        assert_eq!(
            reduce(&mut model, key(KeyCode::Char('d'), KeyModifiers::NONE)),
            vec![ThreadUiAction::ResolvePermission {
                thread_id: model.sessions[0].thread_id,
                request_id: "exact-permission".into(),
                allow: false,
            }]
        );

        model.sessions[0].lifecycle = ThreadLifecycle::WaitingInput;
        model.sessions[0].pending = Some(ThreadPendingRequest::Input {
            run_id,
            request_id: "exact-input".into(),
            prompt: "value".into(),
            expected_run_revision: 8,
        });
        model.input.clear();
        reduce(&mut model, key(KeyCode::Char('j'), KeyModifiers::NONE));
        reduce(&mut model, key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(model.input, "j\n");
        assert_eq!(model.composer, "kept");
        let input_submission_id = model.next_submission_id;
        assert_eq!(
            reduce(&mut model, key(KeyCode::Enter, KeyModifiers::NONE)),
            vec![ThreadUiAction::ProvideInput {
                submission_id: input_submission_id,
                thread_id: model.sessions[0].thread_id,
                request_id: "exact-input".into(),
                value: "j\n".into(),
            }]
        );
    }

    #[test]
    fn presentation_groups_runs_and_pairs_tool_results_without_private_payloads() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Ready);
        thread.latest_run_id = Some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 3,
            status: ThreadRunStatus::Completed,
            run_revision: 4,
            completed_at_ms: Some(4),
        });
        thread.transcript.entries = vec![
            transcript_entry(
                &ids,
                1,
                Some(run_id),
                TranscriptKind::User,
                "检查输入",
                None,
            ),
            transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::ToolCall,
                "Read crates/latte-tui/src/thread.rs",
                Some(serde_json::json!({
                    "descriptor": {
                        "tool_call_id": "call_read",
                        "effect_id": "effect-read",
                        "name": "read_file",
                        "input": {"private_checkpoint": "never render this"}
                    }
                })),
            ),
            transcript_entry(
                &ids,
                3,
                Some(run_id),
                TranscriptKind::ToolResult,
                "1,900 lines inspected",
                Some(serde_json::json!({
                    "tool_call_id": "call_read",
                    "name": "read_file",
                    "private_checkpoint": "never render this"
                })),
            ),
            transcript_entry(
                &ids,
                4,
                Some(run_id),
                TranscriptKind::ToolResult,
                "orphan result remains readable",
                Some(serde_json::json!({"unexpected": [1, 2, 3]})),
            ),
        ];

        let projection = project_transcript(&thread);
        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].heading, "Run 3 · Completed");
        assert!(matches!(
            &projection[0].items[1],
            PresentationItem::Action {
                state: ActivityState::Succeeded,
                result: Some(ActionResult { text, .. }),
                ..
            } if text == "1,900 lines inspected"
        ));
        assert!(matches!(
            &projection[0].items[2],
            PresentationItem::Action { name, summary, .. }
                if name == "Tool result" && summary == "orphan result remains readable"
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn render_is_single_transcript_with_nested_activity_and_bounded_disclosure() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.active_run_id = Some(run_id);
        thread.latest_run_id = Some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Running,
            run_revision: 2,
            completed_at_ms: None,
        });
        thread.transcript.entries = vec![
            transcript_entry(
                &ids,
                1,
                Some(run_id),
                TranscriptKind::User,
                "修复按键输入",
                None,
            ),
            transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::ToolCall,
                "Search reduce_key in crates/latte-tui",
                Some(serde_json::json!({
                    "descriptor": {
                        "tool_call_id": "call_search",
                        "effect_id": "effect-search",
                        "name": "search",
                        "private_checkpoint": "never render this"
                    }
                })),
            ),
            transcript_entry(
                &ids,
                3,
                Some(run_id),
                TranscriptKind::ToolResult,
                "18 matches",
                Some(serde_json::json!({
                    "tool_call_id": "call_search",
                    "name": "search",
                    "private_checkpoint": "never render this"
                })),
            ),
            transcript_entry(
                &ids,
                4,
                Some(run_id),
                TranscriptKind::Assistant,
                "Input path located.",
                Some(serde_json::json!({"private_checkpoint": "never render this"})),
            ),
        ];
        thread.transcript.has_more = true;
        let action_key = action_keys(Some(&thread))[0].clone();
        let mut model = ThreadUiModel {
            sessions: vec![thread],
            size: (100, 30),
            focus: ThreadFocus::Navigation,
            expanded_actions: BTreeSet::from([action_key]),
            progress: vec![ThreadTransientProgress::AssistantDelta {
                run_id,
                text: "Running tests...".into(),
            }],
            ..Default::default()
        };
        let screen = rendered(&model, 100, 30);
        assert!(screen.contains("Latte Code"));
        assert!(screen.contains("·  Running"));
        assert!(screen.contains("▎  ›"), "{screen}");
        assert!(screen.contains("Run 1 · Running"));
        assert!(screen.contains("search  Search reduce_key"));
        assert!(screen.contains("18 matches"));
        assert!(screen.contains("Input path located"));
        assert!(screen.contains("earlier transcript cards are omitted"));
        assert!(screen.contains("Running tests"));
        assert!(!screen.contains("Sessions"));
        assert!(!screen.contains("Transcript ·"));
        assert!(!screen.contains("Composer ·"));
        assert!(!screen.contains('┌'));
        assert!(!screen.contains('┐'));
        assert!(!screen.contains('┘'));
        assert!(!screen.contains("private_checkpoint"));

        model.help = true;
        let help = rendered(&model, 100, 30);
        assert!(help.contains("Single-session transcript"));

        let narrow = rendered(&model, 72, 24);
        assert!(narrow.contains("Latte Code"));
        assert!(!narrow.contains("Sessions"));

        model.help = false;
        let constrained = rendered(&model, 30, 8);
        assert!(constrained.contains("Latte Code"));
        assert!(!constrained.contains("Terminal too small"));
        assert!(is_submit(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)));
        assert!(is_submit(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));
        assert!(is_submit(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!is_submit(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(is_newline(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        let mut graphemes = "a👩‍💻".to_owned();
        pop_grapheme(&mut graphemes);
        assert_eq!(graphemes, "a");
        assert_eq!(
            centered(Rect::new(0, 0, 100, 100), 50, 50),
            Rect::new(25, 25, 50, 50)
        );
    }

    #[test]
    fn constrained_terminals_keep_rendering_the_product_layout() {
        let cases = [
            ("idle", idle_model()),
            ("working", working_model()),
            ("permission", permission_model()),
            ("reconciliation", reconciliation_model()),
            ("complete", complete_model()),
        ];
        for (name, model) in cases {
            for (width, height) in [(48, 24), (40, 16), (30, 8), (20, 5)] {
                let screen = rendered(&model, width, height);
                assert!(screen.contains("Latte Code"), "{name} {width}x{height}");
                assert!(screen.contains('›'), "{name} {width}x{height}");
                assert!(
                    !screen.contains("Terminal too small")
                        && !screen.contains("Resize to continue"),
                    "{name} {width}x{height}"
                );
            }
        }

        let permission = rendered(&permission_model(), 30, 8);
        assert!(permission.contains("Permission"));
        assert!(permission.contains("Ctrl+A"));

        let reconciliation = rendered(&reconciliation_model(), 30, 8);
        assert!(reconciliation.contains("Reconciliation"));
        assert!(reconciliation.contains("Ctrl+R"));

        let idle = idle_model();
        for (width, height) in [(48, 24), (40, 16), (30, 8), (20, 5)] {
            let screen = rendered(&idle, width, height);
            assert!(!screen.contains("Scoped repository changes, with evidence."));
            assert!(!screen.contains("Fix a failing test"));
            assert!(!screen.contains("Explain this codebase"));
            assert!(!screen.contains("Review my changes"));
            if width >= 40 {
                assert!(
                    screen.contains("Ctrl+Enter send · Ctrl+P commands"),
                    "{width}x{height}"
                );
                assert!(screen.contains("runtime-model"), "{width}x{height}");
                assert!(screen.contains("~/projects/latte-code"), "{width}x{height}");
                assert!(screen.contains("Ask mode"), "{width}x{height}");
            }
        }
    }

    #[test]
    fn test_backend_distinguishes_idle_permission_and_completed_states() {
        let idle = rendered(&idle_model(), 100, 30);
        assert!(idle.contains("╭────╮"));
        assert!(idle.contains("Latte Code"));
        assert!(!idle.contains("not bound"));
        assert!(idle.contains("Describe an outcome"));
        assert!(!idle.contains("Scoped repository changes, with evidence."));
        assert!(!idle.contains("Fix a failing test"));
        assert!(!idle.contains("Explain this codebase"));
        assert!(!idle.contains("Review my changes"));
        assert!(idle.contains("model:"));
        assert!(idle.contains("runtime-model"));
        assert!(idle.contains("directory:"));
        assert!(idle.contains("~/projects/latte-code"));
        assert!(idle.contains("permissions:"));
        assert!(idle.contains("Ask mode"));
        assert!(idle.contains("Ctrl+Enter send · Ctrl+P commands"));
        assert!(!idle.contains("Transcript ·"));
        assert!(!idle.contains("Composer ·"));

        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut permission = snapshot(ThreadLifecycle::WaitingPermission);
        permission.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-state".into(),
            description: "Edit one file".into(),
            expected_run_revision: 2,
        });
        let permission_screen = rendered(
            &ThreadUiModel {
                sessions: vec![permission],
                ..Default::default()
            },
            80,
            24,
        );
        assert!(permission_screen.contains("Waiting for approval"));
        assert!(permission_screen.contains("Permission required"));
        assert!(permission_screen.contains("Edit one file"));
        assert!(permission_screen.contains("Enter does nothing"));
        assert!(!permission_screen.contains("Transcript ·"));
        assert!(!permission_screen.contains("Composer ·"));

        let mut complete = snapshot(ThreadLifecycle::Ready);
        complete.transcript.entries.push(transcript_entry(
            &ids,
            2,
            Some(run_id),
            TranscriptKind::Completion,
            "Composer input handling fixed.",
            None,
        ));
        complete.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Completed,
            run_revision: 2,
            completed_at_ms: Some(2),
        });
        let complete_screen = rendered(
            &ThreadUiModel {
                sessions: vec![complete],
                ..Default::default()
            },
            80,
            18,
        );
        assert!(complete_screen.contains("Latte Code"));
        assert!(complete_screen.contains("·  Ready"));
        assert!(complete_screen.contains("● Completed"));
        assert!(complete_screen.contains("Composer input handling fixed"));
        assert!(complete_screen.contains("Ready for follow-up"));
        assert!(!complete_screen.contains("Changed"));
        assert!(!complete_screen.contains("Verified"));

        for lifecycle in [
            ThreadLifecycle::Running,
            ThreadLifecycle::WaitingInput,
            ThreadLifecycle::Interrupted,
            ThreadLifecycle::Failed,
            ThreadLifecycle::ReconciliationRequired,
        ] {
            assert!(!lifecycle_label(lifecycle).is_empty());
            assert_ne!(lifecycle_color(lifecycle), Color::Reset);
        }
    }

    #[test]
    fn prototype_palette_uses_exact_rgb_roles() {
        assert_eq!(TERMINAL, Color::Rgb(32, 44, 49));
        assert_eq!(TERMINAL_DEEP, Color::Rgb(24, 34, 38));
        assert_eq!(SURFACE, Color::Rgb(42, 55, 61));
        assert_eq!(SURFACE_STRONG, Color::Rgb(52, 67, 74));
        assert_eq!(LINE, Color::Rgb(83, 97, 104));
        assert_eq!(LINE_SOFT, Color::Rgb(57, 71, 77));
        assert_eq!(TEXT, Color::Rgb(244, 243, 239));
        assert_eq!(TEXT_SOFT, Color::Rgb(199, 200, 196));
        assert_eq!(MUTED, Color::Rgb(146, 153, 155));
        assert_eq!(FAINT, Color::Rgb(102, 113, 118));
        assert_eq!(LATTE, Color::Rgb(231, 187, 114));
        assert_eq!(LATTE_BRIGHT, Color::Rgb(244, 207, 142));
        assert_eq!(GREEN, Color::Rgb(113, 217, 154));
        assert_eq!(CYAN, Color::Rgb(123, 199, 232));
        assert_eq!(RED, Color::Rgb(240, 122, 120));
        assert_eq!(AMBER, Color::Rgb(239, 183, 99));
        assert_eq!(DIFF_ADD, Color::Rgb(168, 223, 183));
        assert_eq!(DIFF_REMOVE, Color::Rgb(220, 153, 150));
    }

    #[test]
    fn prototype_row_bands_and_insets_are_exact_at_all_three_tiers() {
        let idle = idle_model();
        for (width, height, header_height, composer_y, composer_height, idle_inset) in [
            (160, 80, 20, 75, 5, 7),
            (120, 40, 12, 36, 4, 7),
            (100, 30, 15, 26, 4, 4),
            (72, 24, 12, 19, 5, 2),
        ] {
            let layout = viewport_layout(Rect::new(0, 0, width, height), VisualState::Idle, &idle);
            assert_eq!(layout.header, Rect::new(0, 0, width, header_height));
            assert_eq!(layout.transcript.y, header_height);
            assert_eq!(layout.transcript.bottom(), composer_y);
            assert_eq!(
                layout.composer,
                Rect::new(0, composer_y, width, composer_height)
            );
            assert_eq!(layout.tier.idle_inset(), idle_inset);
        }

        let working = working_model();
        for (width, height, header_height, composer_y, composer_height, inset) in [
            (120, 40, 3, 36, 4, 4),
            (100, 30, 3, 26, 4, 4),
            (72, 24, 2, 19, 5, 2),
        ] {
            let layout = viewport_layout(
                Rect::new(0, 0, width, height),
                VisualState::Active,
                &working,
            );
            assert_eq!(layout.header, Rect::new(0, 0, width, header_height));
            assert_eq!(layout.transcript.y, header_height);
            assert_eq!(layout.transcript.bottom(), composer_y);
            assert_eq!(
                layout.composer,
                Rect::new(0, composer_y, width, composer_height)
            );
            assert_eq!(layout.transcript_inset, inset);
        }
    }

    #[test]
    fn test_backend_working_geometry_and_styles_match_the_prototype() {
        let model = working_model();
        let wide = rendered_buffer(&model, 120, 40);
        for y in 5..=7 {
            for x in 4..=115 {
                assert_eq!(wide[(x, y)].bg, SURFACE_STRONG, "cell ({x},{y})");
            }
            assert_eq!(wide[(4, y)].symbol(), "▎");
            assert_eq!(wide[(4, y)].fg, LATTE);
        }
        for x in 3..=116 {
            assert_eq!(wide[(x, 36)].symbol(), "─");
            assert_eq!(wide[(x, 36)].fg, LINE);
            assert_eq!(wide[(x, 38)].symbol(), "─");
            assert_eq!(wide[(x, 38)].fg, LINE_SOFT);
        }
        assert_ne!(wide[(3, 39)].symbol(), " ");

        let medium = rendered_buffer(&model, 100, 30);
        for y in 5..=7 {
            for x in 4..=95 {
                assert_eq!(medium[(x, y)].bg, SURFACE_STRONG, "cell ({x},{y})");
            }
        }
        assert_eq!(medium[(3, 26)].symbol(), "─");
        assert_eq!(medium[(96, 26)].symbol(), "─");

        let narrow = rendered_buffer(&model, 72, 24);
        assert_eq!(narrow[(2, 19)].symbol(), "─");
        assert_eq!(narrow[(69, 19)].symbol(), "─");
        assert_ne!(narrow[(2, 22)].symbol(), " ");
        assert_ne!(narrow[(2, 23)].symbol(), " ");
        for y in 2..19 {
            assert_eq!(narrow[(0, y)].symbol(), " ");
            assert_eq!(narrow[(1, y)].symbol(), " ");
        }
    }

    #[test]
    fn scrolling_only_changes_the_transcript_band() {
        let mut model = working_model();
        let ids = SystemIdSource::default();
        let run_id = model.sessions[0].active_run_id.unwrap();
        for sequence in 4..=48 {
            model.sessions[0].transcript.entries.push(transcript_entry(
                &ids,
                sequence,
                Some(run_id),
                TranscriptKind::Assistant,
                &format!("scroll fixture row {sequence}"),
                None,
            ));
        }
        let before = rendered_buffer(&model, 120, 40);
        let after = rendered_buffer(&ThreadUiModel { scroll: 8, ..model }, 120, 40);
        for y in 0..3 {
            for x in 0..120 {
                assert_eq!(before[(x, y)], after[(x, y)], "header ({x},{y})");
            }
        }
        for y in 36..40 {
            for x in 0..120 {
                assert_eq!(before[(x, y)], after[(x, y)], "composer ({x},{y})");
            }
        }
        assert!((3..36).any(|y| (0..120).any(|x| before[(x, y)] != after[(x, y)])));
    }

    #[test]
    fn idle_brand_environment_and_minimal_content_reflow_with_authoritative_values() {
        let model = idle_model();
        for (width, height, divider_y, composer_y, card_x, card_y, card_right) in [
            (160, 80, 19, 75, 86, 4, 152),
            (120, 40, 11, 36, 63, 2, 112),
            (100, 30, 14, 26, 4, 7, 95),
            (72, 24, 11, 19, 2, 5, 69),
        ] {
            let buffer = rendered_buffer(&model, width, height);
            let inset = ViewportTier::for_width(width).idle_inset();
            assert_eq!(buffer[(inset, divider_y)].symbol(), "─");
            assert_eq!(buffer[(inset, divider_y)].fg, LINE_SOFT);
            assert_eq!(buffer[(card_x, card_y)].symbol(), "┌");
            assert_eq!(buffer[(card_x, card_y)].fg, LINE);
            assert_eq!(buffer[(card_x, card_y)].bg, TERMINAL_DEEP);
            assert_eq!(buffer[(card_right, card_y)].symbol(), "┐");
            assert_eq!(buffer[(inset, composer_y)].symbol(), "─");
            assert_eq!(buffer[(inset, composer_y)].fg, LINE);
            let screen = buffer_text(&buffer);
            assert!(!screen.contains("Scoped repository changes, with evidence."));
            assert!(!screen.contains("Fix a failing test"));
            assert!(!screen.contains("Explain this codebase"));
            assert!(!screen.contains("Review my changes"));
            assert!(screen.contains("Ctrl+Enter send · Ctrl+P commands"));
        }
        let expanded = rendered_buffer(&model, 160, 80);
        let (title_x, title_y) = find_text_row(&expanded, "Latte Code").expect("brand title");
        assert_eq!(title_x, 17);
        assert_eq!(title_y, 6);
        let (tip_x, tip_y) = find_text_row(&expanded, "Tip:").expect("tip");
        assert_eq!(tip_x, 7);
        assert!(tip_y >= 24);
        let screen = buffer_text(&expanded);
        assert!(screen.contains("Build · runtime-model · Ask"));
        assert!(!screen.contains("xhigh"));
        assert!(!screen.contains("context"));
        assert!(!screen.contains("cost"));
        assert!(!screen.contains("branch"));
        assert!(!screen.contains("credential"));
    }

    #[test]
    fn idle_expanded_brand_cup_silhouette_is_complete() {
        let model = idle_model();
        let expanded = rendered_buffer(&model, 160, 80);
        for (x, y) in [(8, 5), (10, 5), (12, 5)] {
            assert_mark_cell(&expanded, x, y, "╱");
        }
        assert_mark_cell(&expanded, 7, 6, "╭");
        for x in 8..=11 {
            assert_mark_cell(&expanded, x, 6, "─");
        }
        assert_mark_cell(&expanded, 12, 6, "╮");
        assert_mark_cell(&expanded, 7, 7, "│");
        assert_mark_cell(&expanded, 12, 7, "├");
        assert_mark_cell(&expanded, 13, 7, "╮");
        assert_mark_cell(&expanded, 7, 8, "╰");
        for x in 8..=11 {
            assert_mark_cell(&expanded, x, 8, "─");
        }
        assert_mark_cell(&expanded, 12, 8, "╯");
        assert_mark_cell(&expanded, 13, 8, "╯");
        let (expanded_title_x, _) = find_text_row(&expanded, "Latte Code").expect("brand title");
        assert_eq!(expanded_title_x, 17);
    }

    #[test]
    fn idle_wide_brand_cup_silhouette_is_complete() {
        let model = idle_model();
        let wide = rendered_buffer(&model, 120, 40);
        for (x, y) in [(8, 3), (10, 3), (12, 3)] {
            assert_mark_cell(&wide, x, y, "╱");
        }
        assert_mark_cell(&wide, 7, 4, "╭");
        for x in 8..=11 {
            assert_mark_cell(&wide, x, 4, "─");
        }
        assert_mark_cell(&wide, 12, 4, "╮");
        assert_mark_cell(&wide, 7, 5, "│");
        assert_mark_cell(&wide, 12, 5, "├");
        assert_mark_cell(&wide, 13, 5, "╮");
        assert_mark_cell(&wide, 7, 6, "╰");
        for x in 8..=11 {
            assert_mark_cell(&wide, x, 6, "─");
        }
        assert_mark_cell(&wide, 12, 6, "╯");
        assert_mark_cell(&wide, 13, 6, "╯");
        let (wide_title_x, _) = find_text_row(&wide, "Latte Code").expect("brand title");
        assert_eq!(wide_title_x, 17);
    }

    #[test]
    fn idle_stacked_and_compact_brand_cups_remain_complete() {
        let model = idle_model();
        let stacked = rendered_buffer(&model, 100, 30);
        for (x, y) in [(5, 1), (7, 1), (9, 1)] {
            assert_mark_cell(&stacked, x, y, "╱");
        }
        assert_mark_cell(&stacked, 4, 2, "╭");
        for x in 5..=8 {
            assert_mark_cell(&stacked, x, 2, "─");
        }
        assert_mark_cell(&stacked, 9, 2, "╮");
        assert_mark_cell(&stacked, 4, 3, "│");
        assert_mark_cell(&stacked, 9, 3, "├");
        assert_mark_cell(&stacked, 10, 3, "╮");
        assert_mark_cell(&stacked, 4, 4, "╰");
        for x in 5..=8 {
            assert_mark_cell(&stacked, x, 4, "─");
        }
        assert_mark_cell(&stacked, 9, 4, "╯");
        assert_mark_cell(&stacked, 10, 4, "╯");

        let compact = rendered_buffer(&model, 72, 24);
        assert_mark_cell(&compact, 3, 1, "╱");
        assert_mark_cell(&compact, 5, 1, "╱");
        assert_mark_cell(&compact, 7, 1, "╱");
        assert_mark_cell(&compact, 2, 2, "╭");
        for x in 3..=6 {
            assert_mark_cell(&compact, x, 2, "─");
        }
        assert_mark_cell(&compact, 7, 2, "╮");
        assert_mark_cell(&compact, 2, 3, "│");
        assert_mark_cell(&compact, 7, 3, "├");
        assert_mark_cell(&compact, 8, 3, "╮");
        assert_mark_cell(&compact, 2, 4, "╰");
        for x in 3..=6 {
            assert_mark_cell(&compact, x, 4, "─");
        }
        assert_mark_cell(&compact, 7, 4, "╯");
        assert_mark_cell(&compact, 8, 4, "╯");

        let very_narrow = rendered_buffer(&model, 48, 24);
        assert_mark_cell(&very_narrow, 3, 1, "╱");
        assert_mark_cell(&very_narrow, 5, 1, "╱");
        assert_eq!(very_narrow[(7, 1)].symbol(), " ");
        assert_mark_cell(&very_narrow, 2, 2, "╭");
        for x in 3..=5 {
            assert_mark_cell(&very_narrow, x, 2, "─");
        }
        assert_mark_cell(&very_narrow, 6, 2, "╮");
        assert_mark_cell(&very_narrow, 2, 3, "│");
        assert_mark_cell(&very_narrow, 6, 3, "├");
        assert_mark_cell(&very_narrow, 7, 3, "╮");
        assert_mark_cell(&very_narrow, 2, 4, "╰");
        for x in 3..=5 {
            assert_mark_cell(&very_narrow, x, 4, "─");
        }
        assert_mark_cell(&very_narrow, 6, 4, "╯");
        assert_mark_cell(&very_narrow, 7, 4, "╯");
        let (very_narrow_title_x, _) =
            find_text_row(&very_narrow, "Latte Code").expect("very narrow brand title");
        assert_eq!(very_narrow_title_x, 10);

        let constrained = rendered(&model, 40, 16);
        assert!(constrained.contains("● Latte Code"));
        assert!(!constrained.contains(" ╱ ╱ ╱"));
    }

    #[test]
    fn permission_reconciliation_and_completion_keep_distinct_hierarchy() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut permission = snapshot(ThreadLifecycle::WaitingPermission);
        permission.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-geometry".into(),
            description: "Edit src/lib.rs".into(),
            expected_run_revision: 2,
        });
        let permission_buffer = rendered_buffer(
            &ThreadUiModel {
                sessions: vec![permission],
                ..Default::default()
            },
            120,
            40,
        );
        let (permission_x, permission_y) =
            find_symbol(&permission_buffer, "┌").expect("permission card");
        assert_eq!(permission_x, 5);
        assert_eq!(
            permission_buffer[(permission_x + 75, permission_y)].symbol(),
            "┐"
        );
        assert_eq!(permission_buffer[(permission_x, permission_y)].fg, AMBER);
        assert_eq!(permission_buffer[(permission_x, permission_y)].bg, SURFACE);

        let effect_id = "effect-authoritative-7";
        let mut reconciliation = snapshot(ThreadLifecycle::ReconciliationRequired);
        reconciliation.transcript.entries.push(transcript_entry(
            &ids,
            2,
            None,
            TranscriptKind::Failure,
            "effect outcome unknown",
            Some(serde_json::json!({"effect_id": effect_id, "status": "unknown"})),
        ));
        let reconciliation_buffer = rendered_buffer(
            &ThreadUiModel {
                sessions: vec![reconciliation],
                ..Default::default()
            },
            120,
            40,
        );
        let (reconciliation_x, reconciliation_y) =
            find_symbol(&reconciliation_buffer, "┌").expect("reconciliation card");
        assert_eq!(reconciliation_x, 5);
        assert_eq!(
            reconciliation_buffer[(reconciliation_x + 75, reconciliation_y)].symbol(),
            "┐"
        );

        let mut complete = snapshot(ThreadLifecycle::Ready);
        complete.latest_run_id = Some(run_id);
        complete.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Completed,
            run_revision: 2,
            completed_at_ms: Some(2),
        });
        complete.transcript.entries.push(transcript_entry(
            &ids,
            2,
            Some(run_id),
            TranscriptKind::Completion,
            "The change is complete.",
            None,
        ));
        let complete_buffer = rendered_buffer(
            &ThreadUiModel {
                sessions: vec![complete],
                ..Default::default()
            },
            120,
            40,
        );
        let (_, completed_y) = find_text_row(&complete_buffer, "Completed").expect("completed");
        let (bullet_x, bullet_y) = find_symbol_from_row(&complete_buffer, "•", completed_y + 1)
            .expect("completion response bullet");
        assert_eq!(complete_buffer[(bullet_x, bullet_y)].fg, TEXT);
        assert_eq!(complete_buffer[(bullet_x, bullet_y)].bg, TERMINAL);
        let complete_screen = buffer_text(&complete_buffer);
        assert!(!complete_screen.contains("Changed"));
        assert!(!complete_screen.contains("Verified"));
    }

    #[test]
    fn authoritative_repository_permission_tool_and_handoff_data_are_projected() {
        let working = rendered(&working_model(), 120, 40);
        assert!(working.contains("~/projects/latte-code"));
        assert!(working.contains("Query: reduce_key"));

        let permission = rendered(&permission_model(), 120, 40);
        assert!(permission.contains("Operation  Edit file"));
        assert!(permission.contains("Target     src/lib.rs"));
        assert!(permission.contains("Scope      Edit src/lib.rs"));

        let complete = rendered(&complete_model(), 120, 40);
        assert!(complete.contains("CHANGED"));
        assert!(complete.contains("crates/latte-tui/src/thread.rs"));
        assert!(complete.contains("VERIFIED"));
        assert!(complete.contains("cargo test -p latte-tui"));
        assert!(complete.contains("all focused tests passed"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_backend_covers_every_prototype_state_at_all_required_sizes() {
        let cases = [
            ("idle", idle_model()),
            ("working", working_model()),
            ("permission", permission_model()),
            ("reconciliation", reconciliation_model()),
            ("complete", complete_model()),
        ];
        for (name, model) in cases {
            let state = visual_state(&model);
            for (width, height) in [(160, 80), (120, 40), (100, 30), (72, 24)] {
                let (composer_y, composer_height) = match (state, width, height) {
                    (VisualState::Idle, 160, 80) => (75, 5),
                    (_, 160, 80) => (76, 4),
                    (_, 120, 40) => (36, 4),
                    (_, 100, 30) => (26, 4),
                    (_, 72, 24) => (19, 5),
                    _ => unreachable!(),
                };
                let tier = ViewportTier::for_width(width);
                let header_height = if state == VisualState::Idle {
                    IdleComposition::for_area(Rect::new(0, 0, width, height), tier).header_height()
                } else if tier == ViewportTier::Narrow {
                    2
                } else {
                    3
                };
                let layout = viewport_layout(Rect::new(0, 0, width, height), state, &model);
                assert_eq!(
                    layout.header,
                    Rect::new(0, 0, width, header_height),
                    "{name}"
                );
                assert_eq!(
                    layout.transcript,
                    Rect::new(0, header_height, width, composer_y - header_height),
                    "{name}"
                );
                assert_eq!(
                    layout.composer,
                    Rect::new(0, composer_y, width, composer_height),
                    "{name}"
                );

                let buffer = rendered_buffer(&model, width, height);
                let screen = buffer_text(&buffer);
                assert!(
                    screen.contains("~/projects/latte-code"),
                    "{name} {width}x{height}"
                );
                let header_inset = if state == VisualState::Idle {
                    tier.idle_inset()
                } else {
                    tier.compact_inset()
                };
                let header_rule_y = header_height - 1;
                assert_eq!(
                    buffer[(header_inset, header_rule_y)].symbol(),
                    "─",
                    "{name}"
                );
                assert_eq!(
                    buffer[(header_inset, header_rule_y)].fg,
                    LINE_SOFT,
                    "{name}"
                );

                let composer_inset = if state == VisualState::Idle {
                    tier.idle_composer_inset()
                } else {
                    tier.compact_inset()
                };
                let composer_right = width - composer_inset - 1;
                assert_eq!(buffer[(composer_inset, composer_y)].symbol(), "─", "{name}");
                assert_eq!(buffer[(composer_inset, composer_y)].fg, LINE, "{name}");
                assert_eq!(buffer[(composer_right, composer_y)].symbol(), "─", "{name}");
                let divider_y = if tier == ViewportTier::Narrow {
                    height - 3
                } else {
                    height - 2
                };
                assert_eq!(buffer[(composer_inset, divider_y)].symbol(), "─", "{name}");
                assert_eq!(buffer[(composer_inset, divider_y)].fg, LINE_SOFT, "{name}");

                match state {
                    VisualState::Idle => {
                        assert!(screen.contains("Latte Code"), "{name} {width}x{height}");
                        assert!(!screen.contains("Connected"), "{name} {width}x{height}");
                        assert!(screen.contains("directory:"), "{name} {width}x{height}");
                        assert!(screen.contains("permissions:"), "{name} {width}x{height}");
                        assert!(
                            screen.contains("Ctrl+Enter send · Ctrl+P commands"),
                            "{name} {width}x{height}"
                        );
                        assert!(!screen.contains("Scoped repository changes, with evidence."));
                        assert!(!screen.contains("Fix a failing test"));
                        assert!(!screen.contains("Explain this codebase"));
                        assert!(!screen.contains("Review my changes"));
                    }
                    VisualState::Active => {
                        let transcript_x = tier.transcript_inset();
                        let user_y = header_height + 2;
                        for y in user_y..user_y + 3 {
                            for x in transcript_x..width - transcript_x {
                                assert_eq!(buffer[(x, y)].bg, SURFACE_STRONG, "{name} ({x},{y})");
                            }
                            assert_eq!(buffer[(transcript_x, y)].symbol(), "▎", "{name}");
                            assert_eq!(buffer[(transcript_x, y)].fg, LATTE, "{name}");
                        }
                    }
                    VisualState::Permission | VisualState::Reconciliation => {
                        let transcript_x = tier.transcript_inset();
                        let card_x = transcript_x + 1;
                        let card_height = if state == VisualState::Permission {
                            8
                        } else {
                            7
                        };
                        let card_y = layout
                            .transcript
                            .bottom()
                            .saturating_sub(card_height)
                            .saturating_sub(1);
                        let card_width = width
                            .saturating_sub(transcript_x * 2)
                            .saturating_sub(2)
                            .clamp(20, 76);
                        assert_eq!(buffer[(card_x, card_y)].symbol(), "┌", "{name}");
                        assert_eq!(buffer[(card_x, card_y)].fg, AMBER, "{name}");
                        assert_eq!(buffer[(card_x, card_y)].bg, SURFACE, "{name}");
                        assert_eq!(
                            buffer[(card_x + card_width - 1, card_y)].symbol(),
                            "┐",
                            "{name}"
                        );
                        assert!(card_x + card_width <= width - transcript_x, "{name}");
                    }
                    VisualState::Complete => {
                        let (heading_x, heading_y) =
                            find_text_row(&buffer, "Completed").expect("completed heading");
                        assert_eq!(buffer[(heading_x, heading_y)].fg, TEXT, "{name}");
                        let (bullet_x, bullet_y) =
                            find_symbol_from_row(&buffer, "•", heading_y + 1)
                                .expect("completion response bullet");
                        assert_eq!(buffer[(bullet_x, bullet_y)].fg, TEXT, "{name}");
                        assert_eq!(buffer[(bullet_x, bullet_y)].bg, TERMINAL, "{name}");
                    }
                }
            }
        }
    }

    #[test]
    fn blocking_cards_ignore_transcript_scroll_and_keep_fixed_chrome() {
        for model in [permission_model(), reconciliation_model()] {
            for (width, height) in [(120, 40), (100, 30), (72, 24)] {
                let before = rendered_buffer(&model, width, height);
                let after = rendered_buffer(
                    &ThreadUiModel {
                        scroll: u16::MAX,
                        ..model.clone()
                    },
                    width,
                    height,
                );
                let (card_x, card_y) =
                    find_symbol(&before, "┌").expect("blocking card before scroll");
                let (after_x, after_y) =
                    find_symbol(&after, "┌").expect("blocking card after scroll");
                let card_right = card_x
                    + width
                        .saturating_sub(ViewportTier::for_width(width).transcript_inset() * 2)
                        .saturating_sub(2)
                        .clamp(20, 76)
                    - 1;
                assert_eq!((after_x, after_y), (card_x, card_y));
                assert_eq!(before[(card_x, card_y)].symbol(), "┌");
                assert_eq!(after[(card_x, card_y)].symbol(), "┌");
                assert_eq!(after[(card_right, card_y)].symbol(), "┐");
                let card_height = if visual_state(&model) == VisualState::Permission {
                    8
                } else {
                    7
                };
                for y in card_y..card_y + card_height {
                    for x in card_x..=card_right {
                        assert_eq!(before[(x, y)], after[(x, y)], "card ({x},{y})");
                    }
                }
                let state = visual_state(&model);
                let layout = viewport_layout(Rect::new(0, 0, width, height), state, &model);
                for y in layout.header.y..layout.header.bottom() {
                    for x in 0..width {
                        assert_eq!(before[(x, y)], after[(x, y)], "header ({x},{y})");
                    }
                }
                for y in layout.composer.y..layout.composer.bottom() {
                    for x in 0..width {
                        assert_eq!(before[(x, y)], after[(x, y)], "composer ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn rendered_status_roles_use_only_the_prototype_rgb_palette() {
        for (lifecycle, expected) in [
            (ThreadLifecycle::Ready, GREEN),
            (ThreadLifecycle::Running, CYAN),
            (ThreadLifecycle::WaitingPermission, AMBER),
            (ThreadLifecycle::WaitingInput, AMBER),
            (ThreadLifecycle::Interrupted, AMBER),
            (ThreadLifecycle::Failed, RED),
            (ThreadLifecycle::ReconciliationRequired, AMBER),
        ] {
            let model = lifecycle_model(lifecycle);
            let buffer = rendered_buffer(&model, 120, 40);
            assert_eq!(buffer[(3, 0)].symbol(), "●");
            assert_eq!(buffer[(3, 0)].fg, expected, "{lifecycle:?}");
        }

        for (status, expected) in [
            (ThreadRunStatus::Queued, CYAN),
            (ThreadRunStatus::Running, CYAN),
            (ThreadRunStatus::Cancelling, CYAN),
            (ThreadRunStatus::WaitingPermission, AMBER),
            (ThreadRunStatus::WaitingInput, AMBER),
            (ThreadRunStatus::Interrupted, AMBER),
            (ThreadRunStatus::Failed, RED),
            (ThreadRunStatus::Completed, GREEN),
        ] {
            let model = run_status_model(status);
            let buffer = rendered_buffer(&model, 120, 40);
            let (x, y) = find_symbol_from_row(&buffer, "●", 3).expect("phase status dot");
            assert_eq!(buffer[(x, y)].fg, expected, "{status:?}");
        }

        for (state, symbol, expected) in [
            (ActivityState::Recorded, "·", MUTED),
            (ActivityState::Running, "◌", CYAN),
            (ActivityState::Waiting, "!", AMBER),
            (ActivityState::Succeeded, "✓", GREEN),
            (ActivityState::Failed, "×", RED),
        ] {
            assert_eq!(activity_style(state), (symbol, expected));
        }
        for (model, symbol, expected) in [
            (activity_model(ActivityState::Recorded), "·", MUTED),
            (activity_model(ActivityState::Running), "◌", CYAN),
            (activity_model(ActivityState::Waiting), "!", AMBER),
            (activity_model(ActivityState::Succeeded), "✓", GREEN),
            (activity_model(ActivityState::Failed), "×", RED),
        ] {
            let buffer = rendered_buffer(&model, 120, 40);
            let (name_x, y) =
                find_text_row(&buffer, "Inspect  Inspect the repository").expect("activity row");
            let x = name_x.saturating_sub(2);
            assert_eq!(buffer[(x, y)].symbol(), symbol);
            assert_eq!(buffer[(x, y)].fg, expected, "{symbol}");
        }
    }

    #[test]
    fn snapshot_refresh_invalidates_stale_reconciliation_and_dispatches_one_queued_follow_up() {
        let mut model = ThreadUiModel::default();
        let mut ready = snapshot(ThreadLifecycle::Ready);
        ready.revision = 9;
        let thread_id = ready.thread_id;
        model.sessions = vec![snapshot(ThreadLifecycle::ReconciliationRequired)];
        model.reconciliation_confirmation = Some((model.sessions[0].thread_id, "stale".into()));
        model.queued_follow_up = Some("continue from durable state".into());
        model.pending_submission = Some(PendingSubmission {
            submission_id: 41,
            prompt: "continue from durable state".into(),
            thread_id: Some(thread_id),
            after_sequence: u64::MAX,
        });

        let actions = reduce(&mut model, ThreadUiInput::Snapshot(vec![ready]));
        assert_eq!(
            actions,
            vec![ThreadUiAction::FollowUp {
                submission_id: 41,
                thread_id,
                expected_thread_revision: 9,
                prompt: "continue from durable state".into(),
            }]
        );
        assert!(model.reconciliation_confirmation.is_none());
        assert!(model.queued_follow_up.is_none());
        assert_eq!(model.selected_thread().unwrap().thread_id, thread_id);

        let mut reconciling = snapshot(ThreadLifecycle::ReconciliationRequired);
        reconciling.transcript.entries.push(transcript_entry(
            &SystemIdSource::default(),
            2,
            None,
            TranscriptKind::Failure,
            "unknown",
            Some(serde_json::json!({"status":"unknown","effect_id":"effect-current"})),
        ));
        let reconciling_id = reconciling.thread_id;
        model.reconciliation_confirmation = Some((reconciling_id, "effect-current".into()));
        assert!(reduce(&mut model, ThreadUiInput::Snapshot(vec![reconciling])).is_empty());
        assert_eq!(
            model.reconciliation_confirmation,
            Some((reconciling_id, "effect-current".into()))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reducer_key_matrix_keeps_every_mode_scoped_and_non_authoritative_keys_inert() {
        let now = Instant::now();
        let mut model = ThreadUiModel {
            sessions: vec![snapshot(ThreadLifecycle::Running)],
            connection: ConnectionState::Disconnected,
            ..ThreadUiModel::default()
        };
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                now,
            )
            .is_empty()
        );
        assert_eq!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                now + Duration::from_secs(1),
            ),
            vec![ThreadUiAction::RefreshSnapshots]
        );
        assert_eq!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE),
                now + Duration::from_secs(2),
            ),
            vec![ThreadUiAction::Quit]
        );
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new_with_kind(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                now,
            )
            .is_empty()
        );

        model.connection = ConnectionState::Connected;
        model.reconciliation_confirmation = Some((model.sessions[0].thread_id, "effect".into()));
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                now,
            )
            .is_empty()
        );
        assert!(model.reconciliation_confirmation.is_none());
        assert!(model.status.contains("cancelled"));
        model.reconciliation_confirmation = Some((model.sessions[0].thread_id, "effect".into()));
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                now,
            )
            .is_empty()
        );
        assert!(model.reconciliation_confirmation.is_none());

        let mut waiting = snapshot(ThreadLifecycle::WaitingInput);
        waiting.pending = Some(ThreadPendingRequest::Input {
            run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            request_id: "input-matrix".into(),
            prompt: "value?".into(),
            expected_run_revision: 2,
        });
        model.sessions = vec![waiting];
        model.input.clear();
        assert!(reduce(&mut model, ThreadUiInput::Paste("pasted\nvalue".into())).is_empty());
        assert_eq!(model.input, "pasted\nvalue");
        model.input = "é".into();
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                now,
            )
            .is_empty()
        );
        assert!(model.input.is_empty());
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                now,
            )
            .is_empty()
        );
        assert_eq!(model.input, "\n");
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('值'), KeyModifiers::NONE),
                now,
            )
            .is_empty()
        );
        assert_eq!(model.input, "\n值");
        assert!(
            reduce_key_at(
                &mut model,
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
                now,
            )
            .is_empty()
        );

        model.sessions.clear();
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            model.command_palette = true;
            let _ = reduce_palette_key(&mut model, key);
        }
        model.command_palette = true;
        model.command_index = BUILTINS
            .iter()
            .position(|item| item.id == BuiltinCommand::Quit)
            .expect("quit command");
        assert_eq!(
            reduce_palette_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            vec![ThreadUiAction::Quit]
        );

        model.pending_submission = Some(PendingSubmission {
            submission_id: 1,
            prompt: "locked".into(),
            thread_id: None,
            after_sequence: 0,
        });
        model.composer = "locked".into();
        assert!(
            reduce_composer_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
            )
            .is_empty()
        );
        assert_eq!(model.composer, "lockedx");
        assert!(
            reduce_composer_key(
                &mut model,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
            )
            .is_empty()
        );
        assert_eq!(model.composer, "lockedx\n");
        model.pending_submission = None;
        let _ = reduce_composer_key(
            &mut model,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(model.composer, "lockedx");
        let _ = reduce_composer_key(&mut model, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(model.focus, ThreadFocus::Navigation);

        model.sessions = vec![snapshot(ThreadLifecycle::Ready)];
        model.focus = ThreadFocus::Navigation;
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('?'),
            KeyCode::Char('x'),
        ] {
            let _ = reduce_navigation_key(&mut model, KeyEvent::new(code, KeyModifiers::NONE));
        }
        let mut actionable = working_model();
        actionable.focus = ThreadFocus::Navigation;
        actionable.expanded_actions.clear();
        let action_key = action_keys(actionable.selected_thread())[0].clone();
        let _ = reduce_navigation_key(
            &mut actionable,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(actionable.expanded_actions.contains(&action_key));
        let _ = reduce_navigation_key(
            &mut actionable,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(!actionable.expanded_actions.contains(&action_key));
        let _ = reduce_navigation_key(
            &mut actionable,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert!(actionable.expanded_actions.contains(&action_key));
        let _ = reduce_navigation_key(
            &mut actionable,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        assert!(!actionable.expanded_actions.contains(&action_key));
        assert_eq!(
            reduce_navigation_key(
                &mut model,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            vec![ThreadUiAction::Quit]
        );
        let _ = reduce_navigation_key(
            &mut model,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        );
        assert_eq!(model.focus, ThreadFocus::Composer);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn layout_state_and_text_helpers_are_total_at_zero_narrow_and_unicode_boundaries() {
        assert_eq!(ThreadPermissionMode::Ask.label(), "Ask");
        assert_eq!(ThreadPermissionMode::Ask.card_label(), "Ask mode");
        let startup = test_startup();
        let model = ThreadUiModel::with_startup(startup.clone());
        assert_eq!(model.startup.as_ref(), Some(&startup));
        assert!(model.selected_thread().is_none());
        assert!(model.authority_enabled());

        assert_eq!(app_rect(Rect::new(10, 2, 200, 5)), Rect::new(30, 2, 160, 5));
        assert_eq!(bounded_inset(0, 9), 0);
        assert_eq!(bounded_inset(5, 9), 2);
        for (width, tier) in [
            (110, ViewportTier::Wide),
            (78, ViewportTier::Medium),
            (20, ViewportTier::Narrow),
        ] {
            assert_eq!(ViewportTier::for_width(width), tier);
            assert!(tier.compact_inset() > 0);
            assert!(tier.transcript_inset() > 0);
            assert!(tier.idle_inset() > 0);
            assert!(tier.idle_composer_inset() > 0);
        }
        for (area, tier, expected) in [
            (
                Rect::new(0, 0, 120, 52),
                ViewportTier::Wide,
                IdleComposition::Expanded,
            ),
            (
                Rect::new(0, 0, 120, 20),
                ViewportTier::Wide,
                IdleComposition::Wide,
            ),
            (
                Rect::new(0, 0, 80, 20),
                ViewportTier::Medium,
                IdleComposition::Stacked,
            ),
            (
                Rect::new(0, 0, 20, 20),
                ViewportTier::Narrow,
                IdleComposition::Compact,
            ),
        ] {
            assert_eq!(IdleComposition::for_area(area, tier), expected);
            assert!(expected.header_height() > 0);
        }
        for state in [
            VisualState::Idle,
            VisualState::Active,
            VisualState::Permission,
            VisualState::Reconciliation,
            VisualState::Complete,
        ] {
            for area in [
                Rect::new(0, 0, 0, 0),
                Rect::new(0, 0, 20, 2),
                Rect::new(0, 0, 40, 8),
                Rect::new(0, 0, 120, 40),
            ] {
                let layout = viewport_layout(area, state, &model);
                assert_eq!(layout.app, area);
                assert_eq!(
                    layout.header.height + layout.transcript.height + layout.composer.height,
                    area.height
                );
            }
        }

        assert_eq!(wrap_text("", 0), vec![String::new()]);
        assert_eq!(wrap_text("a\tb", 4), vec!["a   ", "b"]);
        assert_eq!(clip_to_width("ignored", 0), "");
        assert_eq!(clip_to_width("a\tb", 4), "a   ");
        assert_eq!(clip_to_width("first\nsecond", 20), "first");
        assert_eq!(display_width("wide界"), 6);
        assert_eq!(display_width("reset\n界"), 2);
        assert_eq!(grapheme_width_at("\t", 1), 3);
        assert_eq!(wrapped_line_count("", 0), 1);
        assert_eq!(composer_text_layout("1234", 4).caret_row, 1);

        for (status, label) in [
            (ThreadRunStatus::Queued, "Queued"),
            (ThreadRunStatus::Running, "Running"),
            (ThreadRunStatus::Cancelling, "Cancelling"),
            (ThreadRunStatus::WaitingPermission, "Waiting permission"),
            (ThreadRunStatus::WaitingInput, "Waiting input"),
            (ThreadRunStatus::Interrupted, "Interrupted"),
            (ThreadRunStatus::Failed, "Failed"),
            (ThreadRunStatus::Completed, "Completed"),
        ] {
            assert_eq!(run_status_label(status), label);
            let _ = run_status_color(status);
        }
        for lifecycle in [
            ThreadLifecycle::Ready,
            ThreadLifecycle::Running,
            ThreadLifecycle::WaitingPermission,
            ThreadLifecycle::WaitingInput,
            ThreadLifecycle::Interrupted,
            ThreadLifecycle::Failed,
            ThreadLifecycle::ReconciliationRequired,
        ] {
            assert!(!lifecycle_label(lifecycle).is_empty());
            let _ = lifecycle_color(lifecycle);
        }
        for state in [
            ActivityState::Recorded,
            ActivityState::Running,
            ActivityState::Waiting,
            ActivityState::Succeeded,
            ActivityState::Failed,
        ] {
            assert!(!activity_style(state).0.is_empty());
        }
        for connection in [
            ConnectionState::Connected,
            ConnectionState::Disconnected,
            ConnectionState::SnapshotRequired,
        ] {
            assert!(!connection_label(connection).is_empty());
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn permission_progress_visual_state_and_tiny_rendering_matrix_remains_secret_safe() {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        for (name, input, operation, target) in [
            (
                "write_file",
                serde_json::json!({"path":"a.txt"}),
                "Write file",
                "a.txt",
            ),
            (
                "edit_file",
                serde_json::json!({"path":"b.txt"}),
                "Edit file",
                "b.txt",
            ),
            (
                "process",
                serde_json::json!({"cwd":"crates"}),
                "Run process",
                "crates",
            ),
            (
                "read_file",
                serde_json::json!({"path":"c.txt"}),
                "Read file",
                "c.txt",
            ),
            (
                "list_directory",
                serde_json::json!({"path":"src"}),
                "List directory",
                "src",
            ),
            (
                "search",
                serde_json::json!({"query":"needle"}),
                "Search workspace",
                "needle",
            ),
            (
                "custom_tool",
                serde_json::json!({}),
                "custom tool",
                "Not exposed by runtime",
            ),
        ] {
            let request_id = format!("request-{name}");
            let mut thread = snapshot(ThreadLifecycle::WaitingPermission);
            thread.transcript.entries.push(transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::ToolCall,
                "operation",
                Some(serde_json::json!({"descriptor":{
                    "effect_id":request_id,"name":name,"input":input
                }})),
            ));
            let presentation = permission_presentation(
                &thread,
                &format!("request-{name}"),
                "scope\napi_key=live-secret-value",
            );
            assert_eq!(presentation.operation, operation);
            assert_eq!(presentation.target, target);
            assert!(!presentation.scope.contains("live-secret-value"));
            assert!(!presentation.scope.chars().any(char::is_control));
        }

        for progress in [
            ThreadTransientProgress::ProviderAttempt { run_id, number: 3 },
            ThreadTransientProgress::AssistantDelta {
                run_id,
                text: "answer".into(),
            },
            ThreadTransientProgress::ToolProgress {
                run_id,
                name: "read_file".into(),
                detail: "reading".into(),
            },
        ] {
            assert_eq!(progress_run_id(&progress), run_id);
            assert!(progress_text(&progress).starts_with('…'));
        }

        let mut idle = ThreadUiModel::default();
        assert_eq!(visual_state(&idle), VisualState::Idle);
        idle.pending_submission = Some(PendingSubmission {
            submission_id: 1,
            prompt: "pending".into(),
            thread_id: None,
            after_sequence: 0,
        });
        assert_eq!(visual_state(&idle), VisualState::Active);
        for (lifecycle, expected) in [
            (ThreadLifecycle::Running, VisualState::Active),
            (ThreadLifecycle::WaitingPermission, VisualState::Permission),
            (
                ThreadLifecycle::ReconciliationRequired,
                VisualState::Reconciliation,
            ),
        ] {
            let model = ThreadUiModel {
                sessions: vec![snapshot(lifecycle)],
                ..ThreadUiModel::default()
            };
            assert_eq!(visual_state(&model), expected);
        }
        let mut complete = snapshot(ThreadLifecycle::Ready);
        complete.transcript.entries.push(transcript_entry(
            &ids,
            2,
            Some(run_id),
            TranscriptKind::Completion,
            "done",
            None,
        ));
        let complete_model = ThreadUiModel {
            sessions: vec![complete],
            help: true,
            command_palette: true,
            ..ThreadUiModel::default()
        };
        assert_eq!(visual_state(&complete_model), VisualState::Complete);
        for (width, height) in [(1, 1), (2, 2), (8, 3), (40, 8)] {
            let buffer = rendered_buffer(&complete_model, width, height);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
        let none =
            permission_presentation(&snapshot(ThreadLifecycle::WaitingPermission), "none", "");
        assert_eq!(none.operation, "Repository operation");
        assert_eq!(none.target, "Not exposed by runtime");
        assert_eq!(none.scope, "[operation summary unavailable]");
    }

    #[test]
    fn standalone_transcript_rendering_distinguishes_roles_evidence_and_connection_state() {
        let lines_text = |lines: &[Line<'static>]| {
            lines.iter().fold(String::new(), |mut text, line| {
                for span in &line.spans {
                    text.push_str(span.content.as_ref());
                }
                text.push('\n');
                text
            })
        };

        let mut lines = Vec::new();
        render_message_lines(
            &mut lines,
            TranscriptKind::Completion,
            "completed response wraps",
            12,
        );
        let completion = lines_text(&lines);
        assert!(completion.contains(" • completed"));
        assert!(completion.contains("   response"));

        lines.clear();
        for (kind, expected) in [
            (TranscriptKind::Permission, " ! Permission · "),
            (TranscriptKind::Input, " ? Input · "),
            (TranscriptKind::System, " · "),
            (TranscriptKind::ToolCall, " · "),
            (TranscriptKind::ToolResult, " · "),
        ] {
            render_message_lines(&mut lines, kind, "detail", 40);
            assert!(lines_text(&lines).contains(expected));
            lines.clear();
        }

        let empty_handoff = latte_core::Handoff {
            summary: "done".into(),
            files_changed: vec![],
            evidence: vec![],
        };
        render_completion_lines(&mut lines, "done", Some(&empty_handoff), 40);
        assert!(!lines_text(&lines).contains("VERIFIED"));
        lines.clear();
        let nonpassing_handoff = latte_core::Handoff {
            summary: "not yet verified".into(),
            files_changed: vec![],
            evidence: vec![
                latte_core::Evidence {
                    name: "cargo test".into(),
                    status: latte_core::VerificationStatus::Failed,
                    summary: String::new(),
                },
                latte_core::Evidence {
                    name: "cargo clippy".into(),
                    status: latte_core::VerificationStatus::NotRun,
                    summary: "blocked".into(),
                },
            ],
        };
        render_completion_lines(
            &mut lines,
            "not yet verified",
            Some(&nonpassing_handoff),
            40,
        );
        let evidence = lines_text(&lines);
        assert!(evidence.contains("× cargo test"));
        assert!(evidence.contains("· cargo clippy · blocked"));

        lines.clear();
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        render_progress(
            &mut lines,
            &ThreadTransientProgress::ProviderAttempt { run_id, number: 2 },
        );
        assert!(lines_text(&lines).contains("provider attempt 2"));
        assert_eq!(surface_line("x", 4, TERMINAL, None).spans[0].content, " ");
        assert_eq!(wrap_text("abcdef", 3), ["abc", "def"]);

        let mut disconnected = idle_model();
        disconnected.connection = ConnectionState::Disconnected;
        assert!(
            rendered(&disconnected, 120, 40)
                .contains("actions disabled until the transcript can be refreshed")
        );
        let mut active = working_model();
        active.connection = ConnectionState::Disconnected;
        assert_eq!(
            composer_meta(&active, visual_state(&active)).1,
            "Ctrl+R refresh"
        );
        let mut reconciliation = reconciliation_model();
        reconciliation.reconciliation_confirmation = Some((
            reconciliation.sessions[0].thread_id,
            "effect-authoritative-matrix".into(),
        ));
        assert!(
            composer_meta(&reconciliation, VisualState::Reconciliation)
                .1
                .starts_with("Ctrl+A confirm failed")
        );
    }

    #[test]
    fn projection_boundaries_reject_malformed_reconciliation_and_keep_editors_scoped() {
        let ids = SystemIdSource::default();
        let malformed_effect = |payload| {
            let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
            thread.transcript.entries.push(transcript_entry(
                &ids,
                1,
                None,
                TranscriptKind::Failure,
                "unknown outcome",
                payload,
            ));
            reconciliation_effect_from_snapshot(&thread)
        };
        assert!(malformed_effect(None).is_none());
        assert!(
            malformed_effect(Some(serde_json::json!({
                "status": "failed",
                "effect_id": "effect-1"
            })))
            .is_none()
        );
        assert!(malformed_effect(Some(serde_json::json!({"status": "unknown"}))).is_none());

        let mut permission = snapshot(ThreadLifecycle::WaitingPermission);
        permission.transcript.entries.push(transcript_entry(
            &ids,
            2,
            None,
            TranscriptKind::ToolCall,
            "opaque operation",
            None,
        ));
        let presentation = permission_presentation(&permission, "missing", "safe scope");
        assert_eq!(presentation.operation, "Repository operation");
        assert_eq!(presentation.target, "Not exposed by runtime");

        let mut reconciliation = reconciliation_model();
        reconciliation.reconciliation_confirmation = Some((
            reconciliation.sessions[0].thread_id,
            "effect-authoritative-matrix".into(),
        ));
        assert!(rendered(&reconciliation, 120, 40).contains("Ctrl+A confirm failed"));

        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut waiting = snapshot(ThreadLifecycle::WaitingInput);
        waiting.pending = Some(ThreadPendingRequest::Input {
            run_id,
            request_id: "input-prompt".into(),
            prompt: "enter the durable value".into(),
            expected_run_revision: 2,
        });
        let input_model = ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![waiting],
            ..Default::default()
        };
        assert!(rendered(&input_model, 120, 40).contains("enter the durable value"));

        let active = ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![snapshot(ThreadLifecycle::Running)],
            composer: "scoped follow-up draft".into(),
            ..Default::default()
        };
        assert!(rendered(&active, 120, 40).contains("scoped follow-up draft"));
        assert_eq!(wrap_text("ab界", 3), ["ab", "界"]);

        let mut submitting = ThreadUiModel {
            pending_submission: Some(PendingSubmission {
                submission_id: 9,
                prompt: "pending".into(),
                thread_id: None,
                after_sequence: 0,
            }),
            ..Default::default()
        };
        assert!(
            reduce(
                &mut submitting,
                ThreadUiInput::SubmissionCompleted { submission_id: 9 }
            )
            .is_empty()
        );
        assert_eq!(
            submitting.status,
            "Submission accepted; synchronizing transcript"
        );
    }

    fn permission_model() -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::WaitingPermission);
        thread.pending = Some(ThreadPendingRequest::Permission {
            run_id,
            request_id: "permission-matrix".into(),
            description: "Edit src/lib.rs".into(),
            expected_run_revision: 2,
        });
        thread.transcript.entries.push(transcript_entry(
            &ids,
            2,
            Some(run_id),
            TranscriptKind::ToolCall,
            "Edit src/lib.rs",
            Some(serde_json::json!({
                "descriptor": {
                    "effect_id": "permission-matrix",
                    "tool_call_id": "permission-call",
                    "name": "edit_file",
                    "input": {"path": "src/lib.rs"}
                }
            })),
        ));
        ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![thread],
            ..Default::default()
        }
    }

    fn reconciliation_model() -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let mut thread = snapshot(ThreadLifecycle::ReconciliationRequired);
        thread.transcript.entries.push(transcript_entry(
            &ids,
            1,
            None,
            TranscriptKind::Failure,
            "effect outcome unknown",
            Some(serde_json::json!({
                "effect_id": "effect-authoritative-matrix",
                "status": "unknown"
            })),
        ));
        ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![thread],
            ..Default::default()
        }
    }

    fn complete_model() -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Ready);
        thread.latest_run_id = Some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Completed,
            run_revision: 2,
            completed_at_ms: Some(2),
        });
        thread.transcript.entries.push(transcript_entry(
            &ids,
            1,
            Some(run_id),
            TranscriptKind::Completion,
            "The requested repository change is complete.",
            Some(serde_json::json!({
                "handoff": {
                    "summary": "The requested repository change is complete.",
                    "files_changed": ["crates/latte-tui/src/thread.rs"],
                    "evidence": [{
                        "name": "cargo test -p latte-tui",
                        "status": "passed",
                        "summary": "all focused tests passed"
                    }]
                }
            })),
        ));
        ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![thread],
            ..Default::default()
        }
    }

    fn lifecycle_model(lifecycle: ThreadLifecycle) -> ThreadUiModel {
        ThreadUiModel {
            sessions: vec![snapshot(lifecycle)],
            ..Default::default()
        }
    }

    fn run_status_model(status: ThreadRunStatus) -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status,
            run_revision: 2,
            completed_at_ms: (status == ThreadRunStatus::Completed).then_some(2),
        });
        thread.transcript.entries.push(transcript_entry(
            &ids,
            1,
            Some(run_id),
            TranscriptKind::ToolCall,
            "Inspect the repository",
            Some(serde_json::json!({
                "descriptor": {"tool_call_id": "run-status-call", "name": "Inspect"}
            })),
        ));
        ThreadUiModel {
            sessions: vec![thread],
            ..Default::default()
        }
    }

    fn activity_model(state: ActivityState) -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let lifecycle = if state == ActivityState::Waiting {
            ThreadLifecycle::WaitingPermission
        } else {
            ThreadLifecycle::Running
        };
        let mut thread = snapshot(lifecycle);
        thread.active_run_id = (state != ActivityState::Recorded).then_some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: if state == ActivityState::Waiting {
                ThreadRunStatus::WaitingPermission
            } else {
                ThreadRunStatus::Running
            },
            run_revision: 2,
            completed_at_ms: None,
        });
        thread.transcript.entries.push(transcript_entry(
            &ids,
            1,
            Some(run_id),
            TranscriptKind::ToolCall,
            "Inspect the repository",
            Some(serde_json::json!({
                "descriptor": {"tool_call_id": "activity-call", "name": "Inspect"}
            })),
        ));
        if matches!(state, ActivityState::Succeeded | ActivityState::Failed) {
            thread.transcript.entries.push(transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::ToolResult,
                if state == ActivityState::Failed {
                    "Inspection failed"
                } else {
                    "Inspection complete"
                },
                Some(if state == ActivityState::Failed {
                    serde_json::json!({"tool_call_id": "activity-call", "error": "redacted"})
                } else {
                    serde_json::json!({"tool_call_id": "activity-call"})
                }),
            ));
        }
        ThreadUiModel {
            sessions: vec![thread],
            ..Default::default()
        }
    }

    fn idle_model() -> ThreadUiModel {
        ThreadUiModel::with_startup(test_startup())
    }

    fn test_startup() -> ThreadStartupPresentation {
        ThreadStartupPresentation {
            default_provider: "runtime-provider".into(),
            default_model: "runtime-model".into(),
            model_catalog: vec![
                ThreadModelOption {
                    provider_name: "runtime-provider".into(),
                    model: "runtime-model".into(),
                    name: Some("Runtime Stable".into()),
                    is_default: true,
                },
                ThreadModelOption {
                    provider_name: "runtime-provider".into(),
                    model: "runtime-model-fast".into(),
                    name: Some("Runtime Fast".into()),
                    is_default: false,
                },
                ThreadModelOption {
                    provider_name: "other-provider".into(),
                    model: "other-model".into(),
                    name: None,
                    is_default: true,
                },
            ],
            workspace_display: "~/projects/latte-code".into(),
            permission_mode: ThreadPermissionMode::Ask,
        }
    }

    #[test]
    fn missing_provider_keeps_tui_usable_and_prompt_local_until_configured() {
        let startup = ThreadStartupPresentation {
            default_provider: String::new(),
            default_model: String::new(),
            model_catalog: Vec::new(),
            workspace_display: "~/projects/latte-code".into(),
            permission_mode: ThreadPermissionMode::Ask,
        };
        let mut model = ThreadUiModel::with_startup(startup);

        assert!(model.draft_model.is_none());
        assert_eq!(model.status, PROVIDER_SETUP_GUIDANCE);
        assert!(rendered(&model, 100, 32).contains(MODEL_NOT_CONFIGURED));

        model.composer = "keep this prompt".into();
        assert!(submit_composer(&mut model).is_empty());
        assert_eq!(model.composer, "keep this prompt");
        assert!(model.pending_submission.is_none());
        assert_eq!(model.status, PROVIDER_SETUP_GUIDANCE);

        assert!(reduce(&mut model, key(KeyCode::Enter, KeyModifiers::SHIFT)).is_empty());
        assert!(reduce(&mut model, key(KeyCode::Char('x'), KeyModifiers::NONE)).is_empty());
        assert_eq!(model.composer, "keep this prompt\nx");

        open_model_picker(&mut model);
        assert!(model.model_picker.is_none());
        assert_eq!(model.status, PROVIDER_SETUP_GUIDANCE);
    }

    fn working_model() -> ThreadUiModel {
        let ids = SystemIdSource::default();
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let mut thread = snapshot(ThreadLifecycle::Running);
        thread.active_run_id = Some(run_id);
        thread.latest_run_id = Some(run_id);
        thread.runs.push(latte_core::ThreadRunSummary {
            run_id,
            parent_run_id: None,
            ordinal: 1,
            status: ThreadRunStatus::Running,
            run_revision: 2,
            completed_at_ms: None,
        });
        thread.transcript.entries = vec![
            transcript_entry(
                &ids,
                1,
                Some(run_id),
                TranscriptKind::User,
                "Fix the composer input path.",
                None,
            ),
            transcript_entry(
                &ids,
                2,
                Some(run_id),
                TranscriptKind::ToolCall,
                "Search reducer input handling",
                Some(serde_json::json!({
                    "descriptor": {
                        "tool_call_id": "call-search",
                        "name": "Search",
                        "input": {"query": "reduce_key"}
                    }
                })),
            ),
            transcript_entry(
                &ids,
                3,
                Some(run_id),
                TranscriptKind::ToolResult,
                "Located the reducer.",
                Some(serde_json::json!({"tool_call_id": "call-search"})),
            ),
        ];
        let action_key = action_keys(Some(&thread))[0].clone();
        ThreadUiModel {
            startup: Some(test_startup()),
            sessions: vec![thread],
            expanded_actions: BTreeSet::from([action_key]),
            ..Default::default()
        }
    }

    fn transcript_entry(
        ids: &SystemIdSource,
        sequence: u64,
        run_id: Option<RunId>,
        kind: TranscriptKind,
        text: &str,
        payload: Option<serde_json::Value>,
    ) -> TranscriptEntry {
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence,
            run_id,
            kind,
            text: text.into(),
            payload,
            source_key: format!("entry-{sequence}"),
            created_at_ms: sequence,
        }
    }

    fn rendered(model: &ThreadUiModel, width: u16, height: u16) -> String {
        buffer_text(&rendered_buffer(model, width, height))
    }

    fn rendered_buffer(model: &ThreadUiModel, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, model)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn assert_mark_cell(buffer: &Buffer, x: u16, y: u16, symbol: &str) {
        assert_eq!(buffer[(x, y)].symbol(), symbol, "symbol at ({x},{y})");
        assert_eq!(buffer[(x, y)].fg, LATTE, "foreground at ({x},{y})");
    }

    fn find_symbol(buffer: &Buffer, symbol: &str) -> Option<(u16, u16)> {
        find_symbol_from_row(buffer, symbol, buffer.area.y)
    }

    fn find_symbol_from_row(buffer: &Buffer, symbol: &str, start_y: u16) -> Option<(u16, u16)> {
        for y in start_y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                if buffer[(x, y)].symbol() == symbol {
                    return Some((x, y));
                }
            }
        }
        None
    }

    fn find_text_row(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
        for y in buffer.area.y..buffer.area.bottom() {
            let row = (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            if let Some(byte_index) = row.find(text) {
                let x = u16::try_from(row[..byte_index].graphemes(true).count())
                    .unwrap_or_default()
                    + buffer.area.x;
                return Some((x, y));
            }
        }
        None
    }
}
