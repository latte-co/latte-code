//! HTTP+SSE client for the latte-code server.
//!
//! `run`/`list`/`show`/`resume` are thin entry points: every engine operation
//! goes through the server's HTTP API, whether the server is embedded in the
//! same process (default, random loopback port) or standalone (`--server`).
//! The server is the single engine host; there is no in-process shortcut.

use crate::prepare_server;
use futures::StreamExt;
use latte_core::{
    FailureCode, ThreadCommandId, ThreadId, ThreadLifecycle, ThreadRunStatus, ThreadSessionSummary,
    ThreadSnapshot, ThreadTransientProgress, TranscriptKind,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// SSE idle/read timeout: if no bytes arrive for this long, the stream is
/// considered dead and the observer reconnects (§8.1). This is NOT a total
/// request timeout — healthy long-lived streams are never forcibly closed.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A classified client failure. Each variant maps to a stable process exit
/// code and JSON error code (see `exit_code`/`code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The server could not be reached. Exit 71.
    Unreachable(String),
    /// Bad arguments or configuration. Exit 2.
    Usage(String),
    /// The session does not exist. Exit 4.
    NotFound(String),
    /// Authentication failed. Exit 70.
    Unauthorized(String),
    /// A revision fence conflict. Exit 1.
    Conflict(String),
    /// An internal server or infrastructure failure. Exit 70.
    Internal(String),
    /// Any other failure. Exit 1.
    Failed(String),
}

impl ClientError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Unreachable(_) => 71,
            Self::Usage(_) => 2,
            Self::NotFound(_) => 4,
            Self::Unauthorized(_) | Self::Internal(_) => 70,
            Self::Conflict(_) | Self::Failed(_) => 1,
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "server_unreachable",
            Self::Usage(_) => "usage",
            Self::NotFound(_) => "not_found",
            Self::Unauthorized(_) => "unauthorized",
            Self::Internal(_) => "internal",
            Self::Conflict(_) => "conflict",
            Self::Failed(_) => "failed",
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Unreachable(message)
            | Self::Usage(message)
            | Self::NotFound(message)
            | Self::Unauthorized(message)
            | Self::Internal(message)
            | Self::Conflict(message)
            | Self::Failed(message) => message,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for ClientError {}

#[allow(clippy::needless_pass_by_value)]
fn network(error: reqwest::Error) -> ClientError {
    if error.is_connect() || error.is_timeout() {
        ClientError::Unreachable(error.to_string())
    } else {
        ClientError::Failed(error.to_string())
    }
}

fn map_error_status(status: reqwest::StatusCode, body: &Value) -> ClientError {
    let message = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("server error")
        .to_string();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        ClientError::Unauthorized(message)
    } else if status == reqwest::StatusCode::NOT_FOUND {
        ClientError::NotFound(message)
    } else if status == reqwest::StatusCode::BAD_REQUEST {
        ClientError::Usage(message)
    } else if status == reqwest::StatusCode::CONFLICT {
        ClientError::Conflict(message)
    } else if status.is_server_error() {
        ClientError::Internal(format!("{status}: {message}"))
    } else {
        ClientError::Failed(format!("{status}: {message}"))
    }
}

// ---------------------------------------------------------------------------
// Command parsing
// ---------------------------------------------------------------------------

/// A parsed session command (v2 contract; the v1 run-id contract is removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// Create a session and stream it to a terminal state.
    Run {
        prompt: String,
        focus: Option<PathBuf>,
    },
    /// List sessions in the workspace.
    List,
    /// Show one session snapshot.
    Show { session_id: String },
    /// Append a follow-up turn to an existing session.
    Resume { session_id: String, prompt: String },
}

/// A parsed command plus its connection flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSessionCommand {
    pub command: SessionCommand,
    pub json: bool,
    pub server: Option<String>,
    pub token: Option<String>,
}

/// Parses `run`/`list`/`show`/`resume` plus the shared `--json`/`--server`/
/// `--token` flags (accepted in any position). `--focus` is only valid on
/// `run`.
///
/// # Errors
/// Returns a usage message for unknown commands/options, missing values, wrong
/// arity, or malformed session ids.
pub fn parse_session_command(args: &[String]) -> Result<ParsedSessionCommand, String> {
    let (name, rest) = args
        .split_first()
        .ok_or_else(|| "expected a command: run | list | show | resume".to_string())?;
    let mut json = false;
    let mut server = None;
    let mut token = None;
    let mut focus = None;
    let mut positional = Vec::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                // Everything after `--` is positional (prompt content),
                // even tokens that look like `--flag`.
                positional.extend(iter.cloned());
                break;
            }
            "--json" => json = true,
            "--server" => server = Some(next_value(&mut iter, "--server")?),
            "--token" => token = Some(next_value(&mut iter, "--token")?),
            "--focus" if name == "run" => {
                focus = Some(PathBuf::from(next_value(&mut iter, "--focus")?));
            }
            "--focus" => return Err("--focus is only valid with run".to_string()),
            // Removed v1 flags that must remain hard errors.
            flag @ ("--allow" | "--deny") => {
                return Err(format!(
                    "{flag} is no longer supported; use the permission API"
                ));
            }
            // For run/resume, unknown --flag tokens are prompt content
            // (e.g. `latte-code run cargo test --workspace`). For list/show,
            // reject them as unknown options since they take no prompt.
            other if other.starts_with("--") && (name == "run" || name == "resume") => {
                positional.push(other.to_string());
            }
            other if other.starts_with("--") => return Err(format!("unknown option: {other}")),
            other => positional.push(other.to_string()),
        }
    }
    let command = match name.as_str() {
        "run" => {
            if positional.is_empty() {
                return Err("run requires a prompt".to_string());
            }
            SessionCommand::Run {
                prompt: positional.join(" "),
                focus,
            }
        }
        "list" => {
            if !positional.is_empty() {
                return Err("list takes no arguments".to_string());
            }
            SessionCommand::List
        }
        "show" => {
            if positional.len() != 1 {
                return Err("show requires exactly one session id".to_string());
            }
            let session_id = positional.remove(0);
            parse_session_id(&session_id).map_err(|error| error.to_string())?;
            SessionCommand::Show { session_id }
        }
        "resume" => {
            if positional.len() < 2 {
                return Err("resume requires a session id and a prompt".to_string());
            }
            parse_session_id(&positional[0]).map_err(|error| error.to_string())?;
            let prompt = positional[1..].join(" ");
            SessionCommand::Resume {
                session_id: positional.remove(0),
                prompt,
            }
        }
        other => return Err(format!("unknown command: {other}")),
    };
    Ok(ParsedSessionCommand {
        command,
        json,
        server,
        token,
    })
}

fn next_value<'a, I: Iterator<Item = &'a String>>(
    iter: &mut I,
    flag: &str,
) -> Result<String, String> {
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

/// Parses a session id (UUID).
///
/// # Errors
/// Returns a usage error for malformed ids.
pub fn parse_session_id(value: &str) -> Result<ThreadId, ClientError> {
    Uuid::parse_str(value)
        .map(ThreadId::from_uuid)
        .map_err(|_| ClientError::Usage(format!("invalid session id: {value}")))
}

// ---------------------------------------------------------------------------
// Terminal classification (pure, mirrors design doc §6.4)
// ---------------------------------------------------------------------------

/// The terminal outcome of a session, derived from lifecycle + latest run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Completed,
    Failed,
    Denied,
    Waiting,
    Interrupted,
    ReconciliationRequired,
}

impl TerminalOutcome {
    #[must_use]
    pub const fn status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Waiting => "waiting",
            Self::Interrupted => "interrupted",
            Self::ReconciliationRequired => "reconciliation_required",
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Denied => 11,
            Self::Failed | Self::ReconciliationRequired => 1,
            Self::Waiting => 10,
            Self::Interrupted => 130,
        }
    }
}

/// Classifies a snapshot per the §6.4 exit-code table. Returns `None` while the
/// session is still running.
#[must_use]
pub fn classify(snapshot: &ThreadSnapshot) -> Option<TerminalOutcome> {
    match snapshot.lifecycle {
        ThreadLifecycle::Running => None,
        ThreadLifecycle::WaitingPermission | ThreadLifecycle::WaitingInput => {
            Some(TerminalOutcome::Waiting)
        }
        ThreadLifecycle::Interrupted => Some(TerminalOutcome::Interrupted),
        ThreadLifecycle::ReconciliationRequired => Some(TerminalOutcome::ReconciliationRequired),
        ThreadLifecycle::Failed => Some(TerminalOutcome::Failed),
        ThreadLifecycle::Ready => match latest_run(snapshot) {
            Some(run) if run.status == ThreadRunStatus::Completed => {
                Some(TerminalOutcome::Completed)
            }
            Some(run) if run.status == ThreadRunStatus::Failed => {
                if run.failure_code == Some(FailureCode::PermissionDenied) {
                    Some(TerminalOutcome::Denied)
                } else {
                    Some(TerminalOutcome::Failed)
                }
            }
            _ => Some(TerminalOutcome::Failed),
        },
    }
}

/// The newest child run by ordinal.
fn latest_run(snapshot: &ThreadSnapshot) -> Option<&latte_core::ThreadRunSummary> {
    snapshot.runs.iter().max_by_key(|run| run.ordinal)
}

/// The result of observing a session to completion.
#[derive(Debug)]
pub enum RunResult {
    /// The session reached a terminal state.
    Terminal {
        snapshot: ThreadSnapshot,
        outcome: TerminalOutcome,
    },
    /// The user interrupted locally (Ctrl+C); a best-effort cancel was sent.
    Cancelled { snapshot: Option<ThreadSnapshot> },
}

impl RunResult {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Terminal { outcome, .. } => outcome.exit_code(),
            Self::Cancelled { .. } => 130,
        }
    }

    #[must_use]
    pub const fn status(&self) -> &'static str {
        match self {
            Self::Terminal { outcome, .. } => outcome.status(),
            Self::Cancelled { .. } => "cancelled",
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<&ThreadSnapshot> {
        match self {
            Self::Terminal { snapshot, .. }
            | Self::Cancelled {
                snapshot: Some(snapshot),
            } => Some(snapshot),
            Self::Cancelled { snapshot: None } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// SSE events
// ---------------------------------------------------------------------------

/// A decoded workspace event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Durable session state changed; refetch the snapshot.
    ThreadChanged { session_id: String, revision: u64 },
    /// Transient provider progress.
    Progress {
        session_id: String,
        run_id: String,
        progress: Value,
    },
    /// The client fell behind or the server signalled a resync.
    ResyncRequired,
}

/// Decodes one SSE frame (event type + raw data payload).
#[must_use]
pub fn parse_sse_frame(event_type: Option<&str>, data: &str) -> Option<StreamEvent> {
    let value: Value = serde_json::from_str(data).ok()?;
    match event_type {
        Some("thread_changed") => Some(StreamEvent::ThreadChanged {
            session_id: value.get("session_id")?.as_str()?.to_string(),
            revision: value.get("revision")?.as_u64()?,
        }),
        Some("progress") => Some(StreamEvent::Progress {
            session_id: value.get("session_id")?.as_str()?.to_string(),
            run_id: value.get("run_id")?.as_str()?.to_string(),
            progress: value.get("progress")?.clone(),
        }),
        Some("resync_required") => Some(StreamEvent::ResyncRequired),
        _ => None,
    }
}

/// Incremental SSE line decoder: feed one line (without newline) at a time; a
/// blank line dispatches the accumulated frame.
#[derive(Debug, Default)]
struct SseDecoder {
    event_type: Option<String>,
    data: String,
}

impl SseDecoder {
    fn line(&mut self, line: &str) -> Option<StreamEvent> {
        if line.is_empty() {
            let event = parse_sse_frame(self.event_type.as_deref(), &self.data);
            self.event_type = None;
            self.data.clear();
            return event;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event_type = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.trim_start());
        }
        // Comments (`:`) and unknown fields are ignored.
        None
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders a progress payload for stderr streaming. Assistant deltas stream
/// through verbatim; tool progress becomes a compact line; provider attempts
/// are noise on the CLI and render as nothing.
#[must_use]
pub fn render_progress(progress: &Value) -> Option<String> {
    let progress: ThreadTransientProgress = serde_json::from_value(progress.clone()).ok()?;
    match progress {
        ThreadTransientProgress::AssistantDelta { text, .. } => Some(text),
        ThreadTransientProgress::ToolProgress { name, detail, .. } => {
            Some(format!("[tool] {name}: {detail}\n"))
        }
        ThreadTransientProgress::ProviderAttempt { .. } => None,
    }
}

/// The lifecycle's stable `snake_case` name (the serde representation).
#[must_use]
pub const fn lifecycle_name(lifecycle: ThreadLifecycle) -> &'static str {
    match lifecycle {
        ThreadLifecycle::Ready => "ready",
        ThreadLifecycle::Running => "running",
        ThreadLifecycle::WaitingPermission => "waiting_permission",
        ThreadLifecycle::WaitingInput => "waiting_input",
        ThreadLifecycle::Interrupted => "interrupted",
        ThreadLifecycle::Failed => "failed",
        ThreadLifecycle::ReconciliationRequired => "reconciliation_required",
    }
}

/// Renders the human-readable session summary: header line plus the last
/// assistant (or failure) message text.
#[must_use]
pub fn render_session_text(snapshot: &ThreadSnapshot) -> String {
    let status = classify(snapshot).map_or_else(
        || lifecycle_name(snapshot.lifecycle),
        TerminalOutcome::status,
    );
    let mut out = format!(
        "session {}: {} (revision {})",
        snapshot.thread_id, status, snapshot.revision
    );
    if let Some(text) = last_message_text(snapshot) {
        out.push('\n');
        out.push_str(&text);
    }
    out
}

/// Renders one session list row.
#[must_use]
pub fn render_session_row(snapshot: &ThreadSnapshot) -> String {
    format!(
        "{}\t{}\trev {}",
        snapshot.thread_id,
        lifecycle_name(snapshot.lifecycle),
        snapshot.revision
    )
}

fn last_message_text(snapshot: &ThreadSnapshot) -> Option<String> {
    snapshot
        .transcript
        .entries
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry.kind,
                TranscriptKind::Assistant | TranscriptKind::Failure
            )
        })
        .map(|entry| entry.text.clone())
}

#[must_use]
pub fn run_envelope(result: &RunResult) -> Value {
    json!({
        "version": 2,
        "status": result.status(),
        "data": { "session": result.snapshot() },
    })
}

#[must_use]
pub fn list_envelope(sessions: &[ThreadSnapshot]) -> Value {
    json!({
        "version": 2,
        "status": "completed",
        "data": { "sessions": sessions },
    })
}

#[must_use]
pub fn session_envelope(snapshot: &ThreadSnapshot) -> Value {
    json!({
        "version": 2,
        "status": "completed",
        "data": { "session": snapshot },
    })
}

#[must_use]
pub fn error_envelope(error: &ClientError) -> Value {
    json!({
        "version": 2,
        "status": "failed",
        "error": { "code": error.code(), "message": error.message() },
    })
}

// ---------------------------------------------------------------------------
// Server surface (trait: real HTTP impl below, scripted mock in tests)
// ---------------------------------------------------------------------------

/// The server surface the session commands drive. [`ServerClient`] is the
/// HTTP+SSE implementation; tests use a scripted mock.
#[allow(dead_code)] // TUI operations consumed by Phase 1 migration
pub trait SessionServer {
    /// Resolves (creating if needed) the workspace id for `root`.
    async fn resolve_workspace(&mut self, root: &Path) -> Result<String, ClientError>;
    /// Picks the default provider binding from the workspace catalog.
    async fn default_binding(&mut self, workspace_id: &str) -> Result<Value, ClientError>;
    /// Creates a session and starts its first turn. Returns the accepted
    /// revision.
    async fn create_session(
        &mut self,
        workspace_id: &str,
        thread_id: ThreadId,
        command_id: ThreadCommandId,
        prompt: &str,
        focus: Option<&Path>,
        binding: &Value,
    ) -> Result<u64, ClientError>;
    /// Appends a follow-up turn. Returns the accepted revision and the
    /// workspace id that owns the session (so the caller can subscribe to
    /// the correct event stream when the cwd workspace differs).
    async fn follow_up(
        &mut self,
        session_id: &ThreadId,
        expected_revision: u64,
        prompt: &str,
    ) -> Result<(u64, String), ClientError>;
    /// Fetches the authoritative session snapshot.
    async fn snapshot(&mut self, session_id: &ThreadId) -> Result<ThreadSnapshot, ClientError>;
    /// Lists the workspace's durable sessions.
    async fn list_sessions(
        &mut self,
        workspace_id: &str,
    ) -> Result<Vec<ThreadSnapshot>, ClientError>;
    /// Requests cancellation of the active run.
    async fn cancel(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
    ) -> Result<(), ClientError>;
    /// Opens the workspace event stream; subsequent
    /// [`next_event`](Self::next_event) calls read from it.
    async fn open_events(&mut self, workspace_id: &str) -> Result<(), ClientError>;
    /// Reads the next event, or `None` when the stream has ended.
    async fn next_event(&mut self) -> Result<Option<StreamEvent>, ClientError>;

    // -- TUI operations ------------------------------------------------------

    /// Renames a session.
    async fn rename_session(
        &mut self,
        session_id: &ThreadId,
        title: &str,
    ) -> Result<(), ClientError>;
    /// Forks a session. Returns the new fork's thread id.
    async fn fork_session(
        &mut self,
        session_id: &ThreadId,
        title: Option<&str>,
    ) -> Result<ThreadId, ClientError>;
    /// Switches the model binding for a session.
    async fn switch_model(
        &mut self,
        session_id: &ThreadId,
        expected_revision: u64,
        binding: &Value,
    ) -> Result<(), ClientError>;
    /// Queues a follow-up prompt for a busy session. Returns the queue position.
    async fn queue_follow_up(
        &mut self,
        session_id: &ThreadId,
        prompt: &str,
    ) -> Result<u64, ClientError>;
    /// Provides input for a waiting-input session.
    async fn provide_input(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        value: &str,
    ) -> Result<(), ClientError>;
    /// Resolves a pending permission request.
    async fn resolve_permission(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        allow: bool,
    ) -> Result<(), ClientError>;
    /// Reconciles an unknown effect (aborts the child process).
    async fn reconcile_effect(
        &mut self,
        session_id: &ThreadId,
        effect_id: &str,
    ) -> Result<(), ClientError>;
    /// Searches sessions by title or ID fragment.
    async fn search_sessions(
        &mut self,
        workspace_id: &str,
        query: &str,
    ) -> Result<Vec<ThreadSessionSummary>, ClientError>;
    /// Returns the full binding catalog for the workspace.
    async fn bindings_catalog(
        &mut self,
        workspace_id: &str,
    ) -> Result<Vec<Value>, ClientError>;
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Creates a session and observes it to a terminal state, streaming progress
/// through `on_progress`. `cancel` resolves on local Ctrl+C.
pub async fn run_session(
    server: &mut impl SessionServer,
    root: &Path,
    prompt: &str,
    focus: Option<&Path>,
    on_progress: &mut impl FnMut(&str),
    cancel: impl std::future::Future<Output = ()>,
) -> Result<RunResult, ClientError> {
    let workspace_id = server.resolve_workspace(root).await?;
    let binding = server.default_binding(&workspace_id).await?;
    let thread_id = ThreadId::from_uuid(Uuid::now_v7());
    let command_id = ThreadCommandId::from_uuid(Uuid::now_v7());
    server
        .create_session(
            &workspace_id,
            thread_id,
            command_id,
            prompt,
            focus,
            &binding,
        )
        .await?;
    observe_session(server, &workspace_id, &thread_id, on_progress, cancel).await
}

/// Appends a follow-up turn and observes the session to a terminal state.
pub async fn resume_session(
    server: &mut impl SessionServer,
    root: &Path,
    session_id: &str,
    prompt: &str,
    on_progress: &mut impl FnMut(&str),
    cancel: impl std::future::Future<Output = ()>,
) -> Result<RunResult, ClientError> {
    let _workspace_id = server.resolve_workspace(root).await?;
    let thread_id = parse_session_id(session_id)?;
    let snapshot = server.snapshot(&thread_id).await?;
    let (_, event_workspace_id) = server
        .follow_up(&thread_id, snapshot.revision, prompt)
        .await?;
    observe_session(server, &event_workspace_id, &thread_id, on_progress, cancel).await
}

/// Observes a session to a terminal state: unconditional snapshot resync on
/// (re)connect (§8.1), progress streamed, Ctrl+C mapped to cancel.
async fn observe_session(
    server: &mut impl SessionServer,
    workspace_id: &str,
    session_id: &ThreadId,
    on_progress: &mut impl FnMut(&str),
    cancel: impl std::future::Future<Output = ()>,
) -> Result<RunResult, ClientError> {
    // Resync before subscribing: a fast turn may already be terminal.
    if let Some(result) = check_terminal(server, session_id).await? {
        return Ok(result);
    }
    server.open_events(workspace_id).await?;
    // Unconditional resync after connecting (§8.1).
    if let Some(result) = check_terminal(server, session_id).await? {
        return Ok(result);
    }
    let mut cancel = std::pin::pin!(cancel);
    let session_id_str = session_id.to_string();
    loop {
        tokio::select! {
            () = &mut cancel => {
                return Ok(cancel_session(server, session_id).await);
            }
            event = server.next_event() => {
                // Both stream-end (Ok(None)) and read errors (Err) enter the
                // same reconnect path: resync, reconnect, resync (§8.1).
                // Errors from `open_events`/`snapshot` in the reconnect path
                // still propagate via `?`, so a down server exits cleanly.
                let event = event.unwrap_or_default();
                match event {
                    None => {
                        // Stream ended: resync, reconnect, resync.
                        if let Some(result) = check_terminal(server, session_id).await? {
                            return Ok(result);
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        server.open_events(workspace_id).await?;
                        if let Some(result) = check_terminal(server, session_id).await? {
                            return Ok(result);
                        }
                    }
                    Some(StreamEvent::Progress {
                        session_id,
                        progress,
                        ..
                    }) if session_id == session_id_str => {
                        if let Some(text) = render_progress(&progress) {
                            on_progress(&text);
                        }
                    }
                    Some(StreamEvent::Progress { .. }) => {
                        // Progress for another session in the same workspace.
                    }
                    Some(StreamEvent::ThreadChanged { .. } | StreamEvent::ResyncRequired) => {
                        if let Some(result) = check_terminal(server, session_id).await? {
                            return Ok(result);
                        }
                    }
                }
            }
        }
    }
}

async fn check_terminal(
    server: &mut impl SessionServer,
    session_id: &ThreadId,
) -> Result<Option<RunResult>, ClientError> {
    let snapshot = server.snapshot(session_id).await?;
    Ok(classify(&snapshot).map(|outcome| RunResult::Terminal { snapshot, outcome }))
}

async fn cancel_session(server: &mut impl SessionServer, session_id: &ThreadId) -> RunResult {
    let snapshot = server.snapshot(session_id).await.ok();
    if let Some(snapshot) = &snapshot {
        let run_revision = latest_run(snapshot).map_or(0, |run| run.run_revision);
        let _ = server
            .cancel(session_id, snapshot.revision, run_revision)
            .await;
    }
    RunResult::Cancelled { snapshot }
}

// ---------------------------------------------------------------------------
// Embedded server
// ---------------------------------------------------------------------------

/// A server running inside the client process. The bearer token stays in
/// memory (nothing is written to disk); the listener binds a random loopback
/// port.
pub struct EmbeddedServer {
    base_url: String,
    token: String,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    handle: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl EmbeddedServer {
    /// Builds the server state and starts serving on `127.0.0.1:0`.
    ///
    /// # Errors
    /// Propagates setup failures (configuration, storage, bind) as classified
    /// client errors.
    pub async fn start(root: &Path, storage_home: &Path) -> Result<Self, ClientError> {
        let (state, token, _token_path) = prepare_server(root, storage_home).map_err(|error| {
            // Configuration failures stay usage errors (exit 2); storage/engine
            // failures are internal (exit 70), matching the standalone `serve`
            // command's `exit_for_setup` mapping.
            if error.code == "usage" {
                ClientError::Usage(error.message)
            } else {
                ClientError::Internal(format!("server setup: {}", error.message))
            }
        })?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| {
                ClientError::Internal(format!("cannot bind loopback port: {error}"))
            })?;
        let port = listener
            .local_addr()
            .map_err(|error| ClientError::Internal(error.to_string()))?
            .port();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown = async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        };
        let handle = tokio::spawn(latte_server::serve_with_shutdown(state, listener, shutdown));
        Ok(Self {
            base_url: format!("http://127.0.0.1:{port}"),
            token,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Signals the embedded server to stop and waits for the port to release.
    pub async fn shutdown(mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }

    fn signal_shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(true);
        }
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        // Backstop for forgotten shutdowns: signal the server so it cannot
        // outlive the process. The explicit `shutdown` path additionally waits
        // for the task and port release.
        self.signal_shutdown();
    }
}

/// Connects to a standalone server or starts an embedded one, returning the
/// client and the embedded server handle (when embedded).
///
/// # Errors
/// Returns [`ClientError::Usage`] for missing tokens/configuration and
/// [`ClientError::Unreachable`] when the server cannot be contacted.
pub async fn connect(
    server: Option<String>,
    token: Option<String>,
    root: &Path,
    storage_home: &Path,
) -> Result<(ServerClient, Option<EmbeddedServer>), ClientError> {
    let Some(url) = server else {
        let embedded = EmbeddedServer::start(root, storage_home).await?;
        let client = ServerClient::new(
            embedded.base_url().to_string(),
            embedded.token().to_string(),
        );
        return Ok((client, Some(embedded)));
    };
    let token = resolve_remote_token(token, storage_home)?;
    let client = ServerClient::new(url.trim_end_matches('/').to_string(), token);
    client.health_check().await?;
    Ok((client, None))
}

fn resolve_remote_token(
    explicit: Option<String>,
    storage_home: &Path,
) -> Result<String, ClientError> {
    if let Some(token) = explicit {
        return Ok(token);
    }
    let path = storage_home.join("server.token");
    std::fs::read_to_string(&path)
        .map(|token| token.trim().to_string())
        .map_err(|_| {
            ClientError::Usage(format!(
                "no token: pass --token or start a server that writes {}",
                path.display()
            ))
        })
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

type ByteStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

struct EventStream {
    stream: ByteStream,
    buf: Vec<u8>,
    decoder: SseDecoder,
    done: bool,
}

impl EventStream {
    /// Drains all complete lines from the buffer, feeding the decoder.
    fn drain_lines(&mut self) -> Result<Option<StreamEvent>, ClientError> {
        loop {
            let Some(newline) = self.buf.iter().position(|&byte| byte == b'\n') else {
                return Ok(None);
            };
            let line_bytes: Vec<u8> = self.buf[..newline].to_vec();
            self.buf.drain(..=newline);
            let line = String::from_utf8(line_bytes)
                .map_err(|error| ClientError::Failed(format!("invalid UTF-8 in SSE: {error}")))?;
            if let Some(event) = self.decoder.line(line.trim_end_matches('\r')) {
                return Ok(Some(event));
            }
        }
    }
}

/// A cloneable handle for making HTTP calls to the server.
///
/// This is the lightweight, cloneable counterpart to [`ServerClient`].
/// It holds only the HTTP client + connection details (no SSE state),
/// so it can be shared across tasks (e.g. TUI action dispatch + projection).
#[derive(Clone)]
#[allow(dead_code)] // TUI operations consumed by Phase 1 migration
pub struct ServerHandle {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

#[allow(dead_code)] // TUI operations consumed by Phase 1 migration
impl ServerHandle {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn get(&self, path: &str) -> Result<Value, ClientError> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(network)?;
        Self::json(response).await
    }

    async fn post(
        &self,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut request = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(&body);
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().await.map_err(network)?;
        Self::json(response).await
    }

    async fn patch(&self, path: &str, body: Value) -> Result<Value, ClientError> {
        let response = self
            .http
            .patch(self.url(path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(network)?;
        Self::json(response).await
    }

    async fn json(response: reqwest::Response) -> Result<Value, ClientError> {
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ClientError::Failed(format!("cannot read response body: {error}")))?;
        if status.is_success() {
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&bytes).map_err(|error| {
                ClientError::Failed(format!("invalid JSON from server: {error}"))
            });
        }
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Err(map_error_status(status, &value))
    }

    // -- TUI operations (inherent async methods) -----------------------------
    // These will be consumed by the TUI HTTP+SSE migration (Phase 1).
    // #[allow(dead_code)] is removed once the TUI adapter lands.

    /// Resolves (creating if needed) the workspace id for `root`.
    pub async fn resolve_workspace_id(&self, root: &Path) -> Result<String, ClientError> {
        let value = self
            .post("/v1/workspaces", json!({ "path": root }), None)
            .await?;
        value
            .get("workspace_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ClientError::Failed("workspace response missing workspace_id".into()))
    }

    /// Renames a session.
    pub async fn rename_session(
        &self,
        session_id: &ThreadId,
        title: &str,
    ) -> Result<(), ClientError> {
        self.patch(
            &format!("/v1/sessions/{session_id}"),
            json!({ "title": title }),
        )
        .await?;
        Ok(())
    }

    /// Forks a session. Returns the new fork's thread id.
    pub async fn fork_session(
        &self,
        session_id: &ThreadId,
        title: Option<&str>,
    ) -> Result<ThreadId, ClientError> {
        let body = match title {
            Some(title) => json!({ "title": title }),
            None => json!({}),
        };
        let value = self
            .post(&format!("/v1/sessions/{session_id}/fork"), body, None)
            .await?;
        let snapshot = value
            .get("snapshot")
            .ok_or_else(|| ClientError::Failed("fork response missing snapshot".into()))?;
        let thread_id = snapshot
            .get("thread_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Failed("fork snapshot missing thread_id".into()))?;
        parse_session_id(thread_id)
    }

    /// Switches the model binding for a session.
    pub async fn switch_model(
        &self,
        session_id: &ThreadId,
        expected_revision: u64,
        binding: &Value,
    ) -> Result<(), ClientError> {
        let body = json!({
            "binding": binding,
            "expected_thread_revision": expected_revision,
        });
        self.post(&format!("/v1/sessions/{session_id}/model"), body, None)
            .await?;
        Ok(())
    }

    /// Queues a follow-up prompt for a busy session. Returns the queue position.
    pub async fn queue_follow_up(
        &self,
        session_id: &ThreadId,
        prompt: &str,
    ) -> Result<u64, ClientError> {
        let value = self
            .post(
                &format!("/v1/sessions/{session_id}/queue"),
                json!({ "prompt": prompt }),
                None,
            )
            .await?;
        value
            .get("position")
            .and_then(Value::as_u64)
            .ok_or_else(|| ClientError::Failed("queue response missing position".into()))
    }

    /// Provides input for a waiting-input session.
    pub async fn provide_input(
        &self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        value: &str,
    ) -> Result<(), ClientError> {
        let body = json!({
            "request_id": request_id,
            "value": value,
            "expected_thread_revision": thread_revision,
            "expected_run_revision": run_revision,
        });
        self.post(&format!("/v1/sessions/{session_id}/input"), body, None)
            .await?;
        Ok(())
    }

    /// Resolves a pending permission request.
    pub async fn resolve_permission(
        &self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        allow: bool,
    ) -> Result<(), ClientError> {
        let body = json!({
            "allow": allow,
            "expected_thread_revision": thread_revision,
            "expected_run_revision": run_revision,
        });
        self.post(
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            body,
            None,
        )
        .await?;
        Ok(())
    }

    /// Reconciles an unknown effect (aborts the child process).
    pub async fn reconcile_effect(
        &self,
        session_id: &ThreadId,
        effect_id: &str,
    ) -> Result<(), ClientError> {
        self.post(
            &format!("/v1/sessions/{session_id}/effects/{effect_id}/reconcile"),
            json!({}),
            None,
        )
        .await?;
        Ok(())
    }

    /// Searches sessions by title or ID fragment.
    pub async fn search_sessions(
        &self,
        workspace_id: &str,
        query: &str,
    ) -> Result<Vec<ThreadSessionSummary>, ClientError> {
        let value = self
            .get(&format!(
                "/v1/workspaces/{workspace_id}/sessions/search?q={}",
                urlencoding::encode(query)
            ))
            .await?;
        let sessions = value
            .get("sessions")
            .cloned()
            .unwrap_or_else(|| json!([]));
        serde_json::from_value(sessions)
            .map_err(|error| ClientError::Failed(format!("invalid search results: {error}")))
    }

    /// Returns the full binding catalog for the workspace.
    pub async fn bindings_catalog(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Value>, ClientError> {
        let value = self
            .get(&format!("/v1/workspaces/{workspace_id}/bindings"))
            .await?;
        let bindings = value
            .get("bindings")
            .and_then(Value::as_array)
            .ok_or_else(|| ClientError::Failed("bindings response missing bindings".into()))?;
        Ok(bindings.clone())
    }
}

/// The HTTP+SSE implementation of [`SessionServer`].
///
/// Two reqwest clients are used: `http` for REST calls (30s total timeout)
/// and `sse_http` for the long-lived SSE stream (connect timeout only, no
/// total timeout — healthy streams must not be forcibly closed; §8.3).
pub struct ServerClient {
    handle: ServerHandle,
    sse_http: reqwest::Client,
    workspace_id: Option<String>,
    events: Option<EventStream>,
}

impl ServerClient {
    #[must_use]
    pub fn new(base_url: String, token: String) -> Self {
        Self::with_rest_timeout(base_url, token, Duration::from_secs(30))
    }

    /// Constructs a client with a custom REST timeout. Production uses
    /// [`Self::new`] (30s); tests inject a smaller value to verify that the
    /// SSE stream is not subject to the REST client's total timeout.
    #[must_use]
    fn with_rest_timeout(base_url: String, token: String, rest_timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(rest_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // SSE client: connect timeout only, NO total request timeout. The
        // per-read idle timeout is enforced in `next_event` (§8.1).
        let sse_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            handle: ServerHandle {
                http,
                base_url,
                token,
            },
            sse_http,
            workspace_id: None,
            events: None,
        }
    }

    /// Returns a cloneable handle for making HTTP calls without SSE state.
    #[must_use]
    #[allow(dead_code)] // consumed by TUI Phase 1 migration
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    fn url(&self, path: &str) -> String {
        self.handle.url(path)
    }

    async fn health_check(&self) -> Result<(), ClientError> {
        let response = self
            .handle
            .http
            .get(self.url("/health"))
            .send()
            .await
            .map_err(network)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Unreachable(format!(
                "health check failed: {}",
                response.status()
            )))
        }
    }

    async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.handle.get(path).await
    }

    async fn post(
        &self,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ClientError> {
        self.handle.post(path, body, idempotency_key).await
    }
}

impl SessionServer for ServerClient {
    async fn resolve_workspace(&mut self, root: &Path) -> Result<String, ClientError> {
        if let Some(id) = &self.workspace_id {
            return Ok(id.clone());
        }
        let value = self
            .post("/v1/workspaces", json!({ "path": root }), None)
            .await?;
        let id = value
            .get("workspace_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Failed("workspace response missing workspace_id".into()))?
            .to_string();
        self.workspace_id = Some(id.clone());
        Ok(id)
    }

    async fn default_binding(&mut self, workspace_id: &str) -> Result<Value, ClientError> {
        let value = self
            .get(&format!("/v1/workspaces/{workspace_id}/bindings"))
            .await?;
        let bindings = value
            .get("bindings")
            .and_then(Value::as_array)
            .ok_or_else(|| ClientError::Failed("bindings response missing bindings".into()))?;
        let selected = bindings
            .iter()
            .find(|entry| {
                entry
                    .get("is_default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .or_else(|| bindings.first())
            .ok_or_else(|| {
                ClientError::Usage(
                    "no provider models configured; set default_model and at least one provider model"
                        .into(),
                )
            })?;
        selected
            .get("binding")
            .cloned()
            .ok_or_else(|| ClientError::Failed("binding entry missing binding".into()))
    }

    async fn create_session(
        &mut self,
        workspace_id: &str,
        thread_id: ThreadId,
        command_id: ThreadCommandId,
        prompt: &str,
        focus: Option<&Path>,
        binding: &Value,
    ) -> Result<u64, ClientError> {
        let body = json!({
            "thread_id": thread_id.to_string(),
            "command_id": command_id.to_string(),
            "prompt": prompt,
            "binding": binding,
            "focus": focus.map(|path| path.display().to_string()),
        });
        let value = self
            .post(
                &format!("/v1/workspaces/{workspace_id}/sessions"),
                body,
                Some(&command_id.to_string()),
            )
            .await?;
        value
            .get("accepted_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| ClientError::Failed("create response missing accepted_revision".into()))
    }

    async fn follow_up(
        &mut self,
        session_id: &ThreadId,
        expected_revision: u64,
        prompt: &str,
    ) -> Result<(u64, String), ClientError> {
        let body = json!({
            "prompt": prompt,
            "expected_thread_revision": expected_revision,
        });
        // Follow-up is a durable mutation: send an idempotency key so a
        // timeout/retry cannot append a duplicate turn.
        let idempotency_key = Uuid::now_v7().to_string();
        let value = self
            .post(
                &format!("/v1/sessions/{session_id}/follow-up"),
                body,
                Some(&idempotency_key),
            )
            .await?;
        let revision = value
            .get("accepted_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ClientError::Failed("follow-up response missing accepted_revision".into())
            })?;
        let workspace_id = value
            .get("workspace_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ClientError::Failed("follow-up response missing workspace_id".into()))?;
        Ok((revision, workspace_id))
    }

    async fn snapshot(&mut self, session_id: &ThreadId) -> Result<ThreadSnapshot, ClientError> {
        let value = self.get(&format!("/v1/sessions/{session_id}")).await?;
        let snapshot = value
            .get("snapshot")
            .cloned()
            .ok_or_else(|| ClientError::Failed("snapshot response missing snapshot".into()))?;
        serde_json::from_value(snapshot)
            .map_err(|error| ClientError::Failed(format!("invalid snapshot: {error}")))
    }

    async fn list_sessions(
        &mut self,
        workspace_id: &str,
    ) -> Result<Vec<ThreadSnapshot>, ClientError> {
        let value = self
            .get(&format!("/v1/workspaces/{workspace_id}/sessions"))
            .await?;
        let sessions = value.get("sessions").cloned().unwrap_or_else(|| json!([]));
        serde_json::from_value(sessions)
            .map_err(|error| ClientError::Failed(format!("invalid sessions list: {error}")))
    }

    async fn cancel(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
    ) -> Result<(), ClientError> {
        let body = json!({
            "expected_thread_revision": thread_revision,
            "expected_run_revision": run_revision,
        });
        self.post(&format!("/v1/sessions/{session_id}/cancel"), body, None)
            .await?;
        Ok(())
    }

    async fn open_events(&mut self, workspace_id: &str) -> Result<(), ClientError> {
        // Uses the SSE-specific client (no total timeout): the stream is
        // long-lived and the 2s keepalive comments bound read latency.
        // Idle/read timeout is enforced per-read in `next_event` (§8.1).
        let response = self
            .sse_http
            .get(self.url(&format!("/v1/workspaces/{workspace_id}/events")))
            .bearer_auth(&self.handle.token)
            .send()
            .await
            .map_err(network)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.json::<Value>().await.unwrap_or_default();
            return Err(map_error_status(status, &body));
        }
        self.events = Some(EventStream {
            stream: Box::pin(response.bytes_stream()),
            buf: Vec::new(),
            decoder: SseDecoder::default(),
            done: false,
        });
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<StreamEvent>, ClientError> {
        let Some(events) = self.events.as_mut() else {
            return Err(ClientError::Failed("event stream not open".into()));
        };
        if events.done {
            return Ok(None);
        }
        loop {
            match events.drain_lines() {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) => {}
                Err(_) => {
                    // Malformed SSE: treat as stream-end so the observer
                    // reconnects and resyncs (§8.1) instead of exiting.
                    events.done = true;
                    return Ok(None);
                }
            }
            // Per-read idle timeout: 30s without bytes means the stream is
            // dead. Read errors, idle timeouts, and graceful stream end all
            // enter the reconnect path (Ok(None)) rather than propagating.
            if let Ok(Some(Ok(bytes))) =
                tokio::time::timeout(SSE_IDLE_TIMEOUT, events.stream.next()).await
            {
                events.buf.extend_from_slice(&bytes);
            } else {
                events.done = true;
                return events.drain_lines().map_or(Ok(None), Ok);
            }
        }
    }

    // -- TUI operations ------------------------------------------------------

    async fn rename_session(
        &mut self,
        session_id: &ThreadId,
        title: &str,
    ) -> Result<(), ClientError> {
        self.handle.rename_session(session_id, title).await
    }

    async fn fork_session(
        &mut self,
        session_id: &ThreadId,
        title: Option<&str>,
    ) -> Result<ThreadId, ClientError> {
        self.handle.fork_session(session_id, title).await
    }

    async fn switch_model(
        &mut self,
        session_id: &ThreadId,
        expected_revision: u64,
        binding: &Value,
    ) -> Result<(), ClientError> {
        self.handle
            .switch_model(session_id, expected_revision, binding)
            .await
    }

    async fn queue_follow_up(
        &mut self,
        session_id: &ThreadId,
        prompt: &str,
    ) -> Result<u64, ClientError> {
        self.handle.queue_follow_up(session_id, prompt).await
    }

    async fn provide_input(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        value: &str,
    ) -> Result<(), ClientError> {
        self.handle
            .provide_input(session_id, thread_revision, run_revision, request_id, value)
            .await
    }

    async fn resolve_permission(
        &mut self,
        session_id: &ThreadId,
        thread_revision: u64,
        run_revision: u64,
        request_id: &str,
        allow: bool,
    ) -> Result<(), ClientError> {
        self.handle
            .resolve_permission(session_id, thread_revision, run_revision, request_id, allow)
            .await
    }

    async fn reconcile_effect(
        &mut self,
        session_id: &ThreadId,
        effect_id: &str,
    ) -> Result<(), ClientError> {
        self.handle.reconcile_effect(session_id, effect_id).await
    }

    async fn search_sessions(
        &mut self,
        workspace_id: &str,
        query: &str,
    ) -> Result<Vec<ThreadSessionSummary>, ClientError> {
        self.handle.search_sessions(workspace_id, query).await
    }

    async fn bindings_catalog(
        &mut self,
        workspace_id: &str,
    ) -> Result<Vec<Value>, ClientError> {
        self.handle.bindings_catalog(workspace_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{
        RunId, ThreadProviderBindingV2, ThreadRunSummary, TranscriptEntry, TranscriptEntryId,
        TranscriptPage,
    };
    use std::sync::{Arc, Mutex};

    fn binding() -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
            version: 1,
            provider_name: "main".into(),
            provider_type: "openai-chat".into(),
            protocol: "openai-chat".into(),
            model: "mock".into(),
            config_fingerprint: "cfg".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:TEST_OPENAI_KEY".into(),
            data_scope_id: "main/mock".into(),
            credential_generation: 0,
        }
    }

    fn snapshot(lifecycle: ThreadLifecycle, runs: Vec<ThreadRunSummary>) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: ThreadId::from_uuid(Uuid::now_v7()),
            revision: 1,
            sequence: 0,
            lifecycle,
            binding: binding(),
            latest_run_id: runs.last().map(|run| run.run_id),
            active_run_id: None,
            pending: None,
            runs,
            transcript: TranscriptPage {
                entries: Vec::new(),
                next_after: None,
                has_more: false,
            },
            focus: None,
        }
    }

    fn run_summary(status: ThreadRunStatus, failure_code: Option<FailureCode>) -> ThreadRunSummary {
        ThreadRunSummary {
            run_id: RunId::from_uuid(Uuid::now_v7()),
            parent_run_id: None,
            ordinal: 1,
            status,
            run_revision: 1,
            completed_at_ms: None,
            failure_code,
        }
    }

    // -- parsing -------------------------------------------------------------

    #[test]
    fn parses_every_command_shape() {
        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        let parsed = parse_session_command(&args(&["run", "fix", "it"])).unwrap();
        assert_eq!(
            parsed.command,
            SessionCommand::Run {
                prompt: "fix it".into(),
                focus: None
            }
        );
        assert!(!parsed.json);
        let parsed = parse_session_command(&args(&[
            "run",
            "--focus",
            "src/lib.rs",
            "--json",
            "--server",
            "http://127.0.0.1:9",
            "fix",
            "it",
        ]))
        .unwrap();
        assert_eq!(
            parsed.command,
            SessionCommand::Run {
                prompt: "fix it".into(),
                focus: Some(PathBuf::from("src/lib.rs"))
            }
        );
        assert!(parsed.json);
        assert_eq!(parsed.server.as_deref(), Some("http://127.0.0.1:9"));
        let parsed = parse_session_command(&args(&["list", "--json"])).unwrap();
        assert_eq!(parsed.command, SessionCommand::List);
        assert!(parsed.json);
        let id = "01900000-0000-7000-8000-000000000001";
        let parsed = parse_session_command(&args(&["show", id, "--token", "secret"])).unwrap();
        assert_eq!(
            parsed.command,
            SessionCommand::Show {
                session_id: id.into()
            }
        );
        assert_eq!(parsed.token.as_deref(), Some("secret"));
        let parsed = parse_session_command(&args(&["resume", id, "continue", "please"])).unwrap();
        assert_eq!(
            parsed.command,
            SessionCommand::Resume {
                session_id: id.into(),
                prompt: "continue please".into()
            }
        );
    }

    #[test]
    fn rejects_bad_command_shapes() {
        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(parse_session_command(&args(&[])).is_err());
        assert!(parse_session_command(&args(&["wat"])).is_err());
        assert!(parse_session_command(&args(&["run"])).is_err());
        assert!(parse_session_command(&args(&["run", "--focus"])).is_err());
        assert!(parse_session_command(&args(&["list", "extra"])).is_err());
        assert!(parse_session_command(&args(&["show"])).is_err());
        assert!(parse_session_command(&args(&["show", "a", "b"])).is_err());
        assert!(parse_session_command(&args(&["show", "not-a-uuid"])).is_err());
        assert!(parse_session_command(&args(&["resume", "not-a-uuid", "x"])).is_err());
        assert!(
            parse_session_command(&args(&["resume", "01900000-0000-7000-8000-000000000001"]))
                .is_err()
        );
        assert!(parse_session_command(&args(&["list", "--focus", "x"])).is_err());
        // run/resume accept unknown --flag tokens as prompt content.
        assert!(parse_session_command(&args(&["run", "--bogus"])).is_ok());
        assert!(parse_session_command(&args(&["run", "--server"])).is_err());
    }

    // -- classification ------------------------------------------------------

    #[test]
    fn classifies_every_terminal_state() {
        assert_eq!(classify(&snapshot(ThreadLifecycle::Running, vec![])), None);
        assert_eq!(
            classify(&snapshot(
                ThreadLifecycle::Ready,
                vec![run_summary(ThreadRunStatus::Completed, None)]
            )),
            Some(TerminalOutcome::Completed)
        );
        assert_eq!(
            classify(&snapshot(
                ThreadLifecycle::Ready,
                vec![run_summary(
                    ThreadRunStatus::Failed,
                    Some(FailureCode::PermissionDenied)
                )]
            )),
            Some(TerminalOutcome::Denied)
        );
        assert_eq!(
            classify(&snapshot(
                ThreadLifecycle::Ready,
                vec![run_summary(ThreadRunStatus::Failed, None)]
            )),
            Some(TerminalOutcome::Failed)
        );
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::WaitingPermission, vec![])),
            Some(TerminalOutcome::Waiting)
        );
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::WaitingInput, vec![])),
            Some(TerminalOutcome::Waiting)
        );
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::Interrupted, vec![])),
            Some(TerminalOutcome::Interrupted)
        );
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::ReconciliationRequired, vec![])),
            Some(TerminalOutcome::ReconciliationRequired)
        );
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::Failed, vec![])),
            Some(TerminalOutcome::Failed)
        );
        // Ready without a usable run still terminates as failed.
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::Ready, vec![])),
            Some(TerminalOutcome::Failed)
        );
        // The newest run by ordinal wins.
        let mut older = run_summary(ThreadRunStatus::Failed, None);
        older.ordinal = 1;
        let mut newer = run_summary(ThreadRunStatus::Completed, None);
        newer.ordinal = 2;
        assert_eq!(
            classify(&snapshot(ThreadLifecycle::Ready, vec![older, newer])),
            Some(TerminalOutcome::Completed)
        );
    }

    #[test]
    fn exit_codes_and_status_strings_match_the_contract() {
        assert_eq!(TerminalOutcome::Completed.exit_code(), 0);
        assert_eq!(TerminalOutcome::Denied.exit_code(), 11);
        assert_eq!(TerminalOutcome::Failed.exit_code(), 1);
        assert_eq!(TerminalOutcome::ReconciliationRequired.exit_code(), 1);
        assert_eq!(TerminalOutcome::Waiting.exit_code(), 10);
        assert_eq!(TerminalOutcome::Interrupted.exit_code(), 130);
        assert_eq!(TerminalOutcome::Completed.status(), "completed");
        assert_eq!(TerminalOutcome::Denied.status(), "denied");
        assert_eq!(TerminalOutcome::Waiting.status(), "waiting");
        assert_eq!(TerminalOutcome::Interrupted.status(), "interrupted");
        assert_eq!(
            TerminalOutcome::ReconciliationRequired.status(),
            "reconciliation_required"
        );
    }

    // -- SSE decoding --------------------------------------------------------

    #[test]
    fn decodes_sse_frames() {
        assert_eq!(
            parse_sse_frame(
                Some("thread_changed"),
                r#"{"session_id":"01900000-0000-7000-8000-000000000001","revision":7}"#
            ),
            Some(StreamEvent::ThreadChanged {
                session_id: "01900000-0000-7000-8000-000000000001".into(),
                revision: 7
            })
        );
        let progress = parse_sse_frame(
            Some("progress"),
            r#"{"session_id":"s","run_id":"r","progress":{"type":"assistant_delta","run_id":"01900000-0000-7000-8000-000000000001","text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(
            render_progress(&match progress {
                StreamEvent::Progress { progress, .. } => progress,
                other => panic!("{other:?}"),
            }),
            Some("hi".to_string())
        );
        assert_eq!(
            parse_sse_frame(Some("resync_required"), "{}"),
            Some(StreamEvent::ResyncRequired)
        );
        assert_eq!(parse_sse_frame(Some("unknown"), "{}"), None);
        assert_eq!(parse_sse_frame(Some("thread_changed"), "not json"), None);
    }

    #[test]
    fn sse_decoder_accumulates_frames() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.line("event: thread_changed").is_none());
        assert!(
            decoder
                .line("data: {\"session_id\":\"s\",\"revision\":1}")
                .is_none()
        );
        let event = decoder.line("").unwrap();
        assert_eq!(
            event,
            StreamEvent::ThreadChanged {
                session_id: "s".into(),
                revision: 1
            }
        );
        // Comments and keepalives are ignored.
        assert!(decoder.line(": keepalive").is_none());
        assert!(decoder.line("").is_none());
    }

    #[test]
    fn progress_rendering_covers_every_variant() {
        let run_id = "01900000-0000-7000-8000-000000000001";
        let delta = serde_json::json!({"type":"assistant_delta","run_id":run_id,"text":"hello"});
        assert_eq!(render_progress(&delta), Some("hello".into()));
        let tool = serde_json::json!({"type":"tool_progress","run_id":run_id,"name":"read","detail":"src/lib.rs"});
        assert_eq!(
            render_progress(&tool),
            Some("[tool] read: src/lib.rs\n".into())
        );
        let attempt = serde_json::json!({"type":"provider_attempt","run_id":run_id,"number":1});
        assert_eq!(render_progress(&attempt), None);
        assert_eq!(render_progress(&serde_json::json!({"type":"bogus"})), None);
    }

    // -- rendering -----------------------------------------------------------

    #[test]
    fn renders_session_text_and_rows() {
        let mut snapshot = snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        );
        snapshot.transcript.entries.push(TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: 0,
            run_id: None,
            kind: TranscriptKind::Assistant,
            text: "final answer".into(),
            payload: None,
            source_key: "test".into(),
            created_at_ms: 0,
        });
        let text = render_session_text(&snapshot);
        assert!(text.contains("session "));
        assert!(text.contains(": completed (revision 1)"));
        assert!(text.contains("final answer"));
        let row = render_session_row(&snapshot);
        assert!(row.contains("\tready\trev 1"));
        assert_eq!(
            lifecycle_name(ThreadLifecycle::WaitingPermission),
            "waiting_permission"
        );
    }

    #[test]
    fn envelopes_use_version_2() {
        let snapshot = snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        );
        let result = RunResult::Terminal {
            snapshot: snapshot.clone(),
            outcome: TerminalOutcome::Completed,
        };
        let envelope = run_envelope(&result);
        assert_eq!(envelope["version"], 2);
        assert_eq!(envelope["status"], "completed");
        assert!(envelope["data"]["session"].is_object());
        let cancelled = RunResult::Cancelled {
            snapshot: Some(snapshot.clone()),
        };
        assert_eq!(run_envelope(&cancelled)["status"], "cancelled");
        assert!(
            run_envelope(&RunResult::Cancelled { snapshot: None })["data"]["session"].is_null()
        );
        assert_eq!(
            list_envelope(std::slice::from_ref(&snapshot))["data"]["sessions"][0]["thread_id"],
            snapshot.thread_id.to_string()
        );
        assert_eq!(
            session_envelope(&snapshot)["data"]["session"]["thread_id"],
            snapshot.thread_id.to_string()
        );
        let error = ClientError::Unreachable("nope".into());
        assert_eq!(
            error_envelope(&error)["error"]["code"],
            "server_unreachable"
        );
        assert_eq!(error.exit_code(), 71);
        assert_eq!(ClientError::Usage("x".into()).exit_code(), 2);
        assert_eq!(ClientError::NotFound("x".into()).exit_code(), 4);
        assert_eq!(ClientError::Unauthorized("x".into()).exit_code(), 70);
        assert_eq!(ClientError::Conflict("x".into()).exit_code(), 1);
        assert_eq!(ClientError::Internal("x".into()).exit_code(), 70);
        assert_eq!(ClientError::Internal("x".into()).code(), "internal");
        assert_eq!(ClientError::Failed("x".into()).exit_code(), 1);
    }

    // -- mock server + orchestration ----------------------------------------

    #[derive(Default)]
    struct MockServer {
        workspace_id: String,
        binding: Value,
        snapshots: Mutex<std::collections::VecDeque<Result<ThreadSnapshot, ClientError>>>,
        events: Mutex<std::collections::VecDeque<Option<StreamEvent>>>,
        created: Mutex<Vec<(String, String, Option<String>)>>,
        followed_up: Mutex<Vec<(String, u64, String)>>,
        cancelled: Mutex<Vec<(u64, u64)>>,
        opened: Mutex<Vec<String>>,
        listed: Mutex<u32>,
        fail_resolve: bool,
        fail_binding: bool,
        read_error_once: Mutex<bool>,
    }

    impl MockServer {
        fn new() -> Self {
            Self {
                workspace_id: "ws-1".into(),
                binding: json!({"version":1}),
                ..Default::default()
            }
        }

        fn push_snapshot(&self, snapshot: ThreadSnapshot) {
            self.snapshots.lock().unwrap().push_back(Ok(snapshot));
        }

        fn push_event(&self, event: StreamEvent) {
            self.events.lock().unwrap().push_back(Some(event));
        }

        fn end_stream(&self) {
            self.events.lock().unwrap().push_back(None);
        }

        fn take_created(&self) -> Vec<(String, String, Option<String>)> {
            self.created.lock().unwrap().clone()
        }
    }

    impl SessionServer for MockServer {
        async fn resolve_workspace(&mut self, _root: &Path) -> Result<String, ClientError> {
            if self.fail_resolve {
                return Err(ClientError::Failed("resolve failed".into()));
            }
            Ok(self.workspace_id.clone())
        }

        async fn default_binding(&mut self, _workspace_id: &str) -> Result<Value, ClientError> {
            if self.fail_binding {
                return Err(ClientError::Usage("no binding".into()));
            }
            Ok(self.binding.clone())
        }

        async fn create_session(
            &mut self,
            _workspace_id: &str,
            thread_id: ThreadId,
            command_id: ThreadCommandId,
            prompt: &str,
            focus: Option<&Path>,
            _binding: &Value,
        ) -> Result<u64, ClientError> {
            self.created.lock().unwrap().push((
                thread_id.to_string(),
                prompt.to_string(),
                focus.map(|path| path.display().to_string()),
            ));
            let _ = command_id;
            Ok(1)
        }

        async fn follow_up(
            &mut self,
            session_id: &ThreadId,
            expected_revision: u64,
            prompt: &str,
        ) -> Result<(u64, String), ClientError> {
            self.followed_up.lock().unwrap().push((
                session_id.to_string(),
                expected_revision,
                prompt.to_string(),
            ));
            Ok((2, "mock-workspace".into()))
        }

        async fn snapshot(
            &mut self,
            _session_id: &ThreadId,
        ) -> Result<ThreadSnapshot, ClientError> {
            self.snapshots
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ClientError::Failed("snapshot queue empty".into())))
        }

        async fn list_sessions(
            &mut self,
            _workspace_id: &str,
        ) -> Result<Vec<ThreadSnapshot>, ClientError> {
            *self.listed.lock().unwrap() += 1;
            Ok(Vec::new())
        }

        async fn cancel(
            &mut self,
            _session_id: &ThreadId,
            thread_revision: u64,
            run_revision: u64,
        ) -> Result<(), ClientError> {
            self.cancelled
                .lock()
                .unwrap()
                .push((thread_revision, run_revision));
            Ok(())
        }

        async fn open_events(&mut self, workspace_id: &str) -> Result<(), ClientError> {
            self.opened.lock().unwrap().push(workspace_id.to_string());
            Ok(())
        }

        async fn next_event(&mut self) -> Result<Option<StreamEvent>, ClientError> {
            // Simulate a transient SSE read error once.
            if std::mem::replace(&mut *self.read_error_once.lock().unwrap(), false) {
                return Err(ClientError::Failed("simulated SSE read error".into()));
            }
            // Rewrite sentinel "self" session ids to the created session so
            // the observer's session filter sees them as its own; other ids
            // pass through unchanged (and must be filtered by the observer).
            let Some(event) = self.events.lock().unwrap().pop_front() else {
                // No queued event: stay pending so select! does not mistake an
                // idle stream for a closed one (the real SSE stream blocks).
                std::future::pending::<()>().await;
                unreachable!()
            };
            let created = self.created.lock().unwrap();
            let created_id = created.first().map(|(id, _, _)| id.clone());
            drop(created);
            Ok(event.map(|event| match (created_id, event) {
                (
                    Some(created),
                    StreamEvent::ThreadChanged {
                        session_id,
                        revision,
                    },
                ) if session_id == "self" => StreamEvent::ThreadChanged {
                    session_id: created,
                    revision,
                },
                (
                    Some(created),
                    StreamEvent::Progress {
                        session_id,
                        run_id,
                        progress,
                    },
                ) if session_id == "self" => StreamEvent::Progress {
                    session_id: created,
                    run_id,
                    progress,
                },
                (_, other) => other,
            }))
        }

        // -- TUI operations --------------------------------------------------

        async fn rename_session(
            &mut self,
            _session_id: &ThreadId,
            _title: &str,
        ) -> Result<(), ClientError> {
            Ok(())
        }

        async fn fork_session(
            &mut self,
            _session_id: &ThreadId,
            _title: Option<&str>,
        ) -> Result<ThreadId, ClientError> {
            Ok(ThreadId::from_uuid(Uuid::now_v7()))
        }

        async fn switch_model(
            &mut self,
            _session_id: &ThreadId,
            _expected_revision: u64,
            _binding: &Value,
        ) -> Result<(), ClientError> {
            Ok(())
        }

        async fn queue_follow_up(
            &mut self,
            _session_id: &ThreadId,
            _prompt: &str,
        ) -> Result<u64, ClientError> {
            Ok(1)
        }

        async fn provide_input(
            &mut self,
            _session_id: &ThreadId,
            _thread_revision: u64,
            _run_revision: u64,
            _request_id: &str,
            _value: &str,
        ) -> Result<(), ClientError> {
            Ok(())
        }

        async fn resolve_permission(
            &mut self,
            _session_id: &ThreadId,
            _thread_revision: u64,
            _run_revision: u64,
            _request_id: &str,
            _allow: bool,
        ) -> Result<(), ClientError> {
            Ok(())
        }

        async fn reconcile_effect(
            &mut self,
            _session_id: &ThreadId,
            _effect_id: &str,
        ) -> Result<(), ClientError> {
            Ok(())
        }

        async fn search_sessions(
            &mut self,
            _workspace_id: &str,
            _query: &str,
        ) -> Result<Vec<ThreadSessionSummary>, ClientError> {
            Ok(Vec::new())
        }

        async fn bindings_catalog(
            &mut self,
            _workspace_id: &str,
        ) -> Result<Vec<Value>, ClientError> {
            Ok(Vec::new())
        }
    }

    fn assistant_progress(session_id: &str) -> StreamEvent {
        StreamEvent::Progress {
            session_id: session_id.into(),
            run_id: "01900000-0000-7000-8000-000000000001".into(),
            progress: json!({"type":"assistant_delta","run_id":"01900000-0000-7000-8000-000000000001","text":"chunk"}),
        }
    }

    #[tokio::test]
    async fn run_session_streams_progress_and_completes() {
        let mut server = MockServer::new();
        // observe: resync (running), post-connect resync (running), then events.
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        // Another session's progress must not leak into this run's output.
        server.push_event(assistant_progress("other-session"));
        server.push_event(assistant_progress("self"));
        server.push_event(StreamEvent::ThreadChanged {
            session_id: "self".into(),
            revision: 2,
        });
        server.push_snapshot(snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        ));
        let mut printed = String::new();
        let result = run_session(
            &mut server,
            Path::new("/workspace"),
            "do it",
            None,
            &mut |text| printed.push_str(text),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 0);
        assert_eq!(result.status(), "completed");
        assert_eq!(printed, "chunk");
        let created = server.take_created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].1, "do it");
        assert_eq!(server.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_session_maps_every_terminal_outcome() {
        for (lifecycle, runs, expected_code, expected_status) in [
            (
                ThreadLifecycle::Ready,
                vec![run_summary(ThreadRunStatus::Completed, None)],
                0,
                "completed",
            ),
            (
                ThreadLifecycle::Ready,
                vec![run_summary(
                    ThreadRunStatus::Failed,
                    Some(FailureCode::PermissionDenied),
                )],
                11,
                "denied",
            ),
            (
                ThreadLifecycle::Ready,
                vec![run_summary(ThreadRunStatus::Failed, None)],
                1,
                "failed",
            ),
            (ThreadLifecycle::WaitingPermission, vec![], 10, "waiting"),
            (ThreadLifecycle::Interrupted, vec![], 130, "interrupted"),
            (
                ThreadLifecycle::ReconciliationRequired,
                vec![],
                1,
                "reconciliation_required",
            ),
            (ThreadLifecycle::Failed, vec![], 1, "failed"),
        ] {
            let mut server = MockServer::new();
            server.push_snapshot(snapshot(lifecycle, runs));
            let result = run_session(
                &mut server,
                Path::new("/workspace"),
                "x",
                None,
                &mut |_| {},
                std::future::pending(),
            )
            .await
            .unwrap();
            assert_eq!(result.exit_code(), expected_code);
            assert_eq!(result.status(), expected_status);
        }
    }

    #[tokio::test]
    async fn run_session_cancels_on_ctrl_c() {
        let mut server = MockServer::new();
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        let result = run_session(
            &mut server,
            Path::new("/workspace"),
            "x",
            None,
            &mut |_| {},
            std::future::ready(()),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 130);
        assert_eq!(result.status(), "cancelled");
        assert_eq!(server.cancelled.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_session_reconnects_after_stream_end() {
        let mut server = MockServer::new();
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.end_stream();
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        // second stream: a change event then terminal snapshot.
        server.push_event(StreamEvent::ThreadChanged {
            session_id: "s".into(),
            revision: 3,
        });
        server.push_snapshot(snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        ));
        let result = run_session(
            &mut server,
            Path::new("/workspace"),
            "x",
            None,
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 0);
        assert_eq!(server.opened.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn run_session_reconnects_after_sse_read_error() {
        let mut server = MockServer::new();
        // Initial check: running.
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        // Post-connect resync: still running.
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        // Simulate a transient SSE read error.
        *server.read_error_once.lock().unwrap() = true;
        // Reconnect resync: still running.
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        // Post-reconnect resync: terminal.
        server.push_snapshot(snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        ));
        let result = run_session(
            &mut server,
            Path::new("/workspace"),
            "x",
            None,
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 0);
        assert_eq!(server.opened.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn run_session_propagates_errors() {
        let mut server = MockServer::new();
        server.fail_resolve = true;
        let error = run_session(
            &mut server,
            Path::new("/workspace"),
            "x",
            None,
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, ClientError::Failed("resolve failed".into()));

        let mut server = MockServer::new();
        server.fail_binding = true;
        let error = run_session(
            &mut server,
            Path::new("/workspace"),
            "x",
            None,
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "usage");
    }

    #[tokio::test]
    async fn resume_session_follows_up_then_observes() {
        let mut server = MockServer::new();
        let id = "01900000-0000-7000-8000-000000000001";
        // Revision fetch (Ready), pre-subscribe resync, post-connect resync,
        // then the resync event's terminal snapshot.
        server.push_snapshot(snapshot(ThreadLifecycle::Ready, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_event(StreamEvent::ResyncRequired);
        server.push_snapshot(snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        ));
        let result = resume_session(
            &mut server,
            Path::new("/workspace"),
            id,
            "continue",
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 0);
        let followed = server.followed_up.lock().unwrap().clone();
        assert_eq!(followed.len(), 1);
        assert_eq!(followed[0].0, id);
        assert_eq!(followed[0].1, 1); // expected revision from the snapshot
        assert_eq!(followed[0].2, "continue");
    }

    #[tokio::test]
    async fn resume_session_subscribes_to_session_workspace_not_cwd_workspace() {
        // The cwd resolves to "ws-1" but the follow-up response returns
        // "mock-workspace" (the session's actual workspace). The SSE
        // subscription must use the session's workspace, not the cwd's.
        let mut server = MockServer::new();
        let id = "01900000-0000-7000-8000-000000000001";
        server.push_snapshot(snapshot(ThreadLifecycle::Ready, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_snapshot(snapshot(ThreadLifecycle::Running, vec![]));
        server.push_event(StreamEvent::ResyncRequired);
        server.push_snapshot(snapshot(
            ThreadLifecycle::Ready,
            vec![run_summary(ThreadRunStatus::Completed, None)],
        ));
        let result = resume_session(
            &mut server,
            Path::new("/different-workspace"),
            id,
            "continue",
            &mut |_| {},
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.exit_code(), 0);
        let opened = server.opened.lock().unwrap().clone();
        assert!(
            opened.iter().any(|ws| ws == "mock-workspace"),
            "expected open_events to use the session's workspace 'mock-workspace', got {opened:?}"
        );
        assert!(
            !opened.iter().any(|ws| ws == "ws-1"),
            "open_events must not use the cwd workspace 'ws-1', got {opened:?}"
        );
    }

    // -- real HTTP client against a mock axum server -------------------------

    struct MockHttp {
        base_url: String,
        _server: tokio::task::JoinHandle<()>,
        shutdown: tokio::sync::watch::Sender<bool>,
    }

    impl MockHttp {
        fn client(&self, token: &str) -> ServerClient {
            ServerClient::new(self.base_url.clone(), token.into())
        }

        fn client_with_rest_timeout(&self, token: &str, timeout: Duration) -> ServerClient {
            ServerClient::with_rest_timeout(self.base_url.clone(), token.into(), timeout)
        }
    }

    impl Drop for MockHttp {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
        }
    }

    fn auth(headers: &axum::http::HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some("Bearer token")
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn http_client_drives_the_full_rest_surface() {
        use axum::extract::{Path, State};
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicU64, Ordering};

        #[derive(Clone)]
        struct Api {
            created: Arc<AtomicU64>,
            cancelled: Arc<AtomicU64>,
        }

        async fn workspace(
            State(_api): State<Api>,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert!(body.get("path").is_some());
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "workspace_id": "ws-1", "path": body["path"] })),
            )
        }

        async fn bindings(State(_api): State<Api>, Path(ws): Path<String>) -> impl IntoResponse {
            assert_eq!(ws, "ws-1");
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "bindings": [
                    { "provider_name": "main", "model": "mock", "name": null,
                      "is_default": true, "binding": { "version": 1 } }
                ] })),
            )
        }

        async fn create(
            State(api): State<Api>,
            Path(ws): Path<String>,
            headers: axum::http::HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert_eq!(ws, "ws-1");
            assert!(auth(&headers));
            assert_eq!(
                headers.get("idempotency-key").unwrap().to_str().unwrap(),
                body["command_id"].as_str().unwrap()
            );
            api.created.fetch_add(1, Ordering::SeqCst);
            (
                axum::http::StatusCode::ACCEPTED,
                axum::Json(json!({ "session_id": body["thread_id"], "accepted_revision": 3 })),
            )
        }

        async fn snapshot(Path(id): Path<String>) -> impl IntoResponse {
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "snapshot": {
                    "thread_id": id,
                    "revision": 3,
                    "sequence": 0,
                    "lifecycle": "ready",
                    "binding": {
                        "version": 1, "provider_name": "main", "provider_type": "openai-chat",
                        "protocol": "openai-chat", "model": "mock",
                        "config_fingerprint": "c", "tools_fingerprint": "t", "aliases": {},
                        "credential_ref_id": "env:K", "data_scope_id": "main/mock",
                        "credential_generation": 0
                    },
                    "latest_run_id": null,
                    "active_run_id": null,
                    "runs": [],
                    "transcript": { "entries": [], "next_after": null, "has_more": false }
                } })),
            )
        }

        async fn list(Path(ws): Path<String>) -> impl IntoResponse {
            assert_eq!(ws, "ws-1");
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "sessions": [], "next_cursor": null })),
            )
        }

        async fn follow_up(
            Path(_id): Path<String>,
            headers: axum::http::HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert_eq!(body["expected_thread_revision"], 1);
            assert!(
                headers.get("idempotency-key").is_some(),
                "follow-up must send Idempotency-Key"
            );
            (
                axum::http::StatusCode::ACCEPTED,
                axum::Json(json!({ "accepted_revision": 2, "workspace_id": "mock-workspace" })),
            )
        }

        async fn cancel(State(api): State<Api>, Path(_id): Path<String>) -> impl IntoResponse {
            api.cancelled.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::OK
        }

        // -- TUI endpoint handlers -------------------------------------------

        async fn rename(Path(id): Path<String>, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
            assert!(body.get("title").is_some(), "rename requires title");
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "snapshot": { "thread_id": id, "revision": 1 } })),
            )
        }

        async fn fork(Path(_id): Path<String>) -> impl IntoResponse {
            let fork_id = Uuid::now_v7().to_string();
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "snapshot": { "thread_id": fork_id, "revision": 1 } })),
            )
        }

        async fn switch_model(
            Path(_id): Path<String>,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert!(body.get("binding").is_some(), "switch_model requires binding");
            assert!(
                body.get("expected_thread_revision").is_some(),
                "switch_model requires expected_thread_revision"
            );
            axum::http::StatusCode::OK
        }

        async fn queue(Path(_id): Path<String>, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
            assert!(body.get("prompt").is_some(), "queue requires prompt");
            (
                axum::http::StatusCode::ACCEPTED,
                axum::Json(json!({ "position": 1 })),
            )
        }

        async fn provide_input(
            Path(_id): Path<String>,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert!(body.get("request_id").is_some());
            assert!(body.get("value").is_some());
            axum::http::StatusCode::OK
        }

        async fn resolve_permission(
            Path((_id, _req_id)): Path<(String, String)>,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert!(body.get("allow").is_some());
            axum::http::StatusCode::OK
        }

        async fn reconcile(Path((_id, _effect_id)): Path<(String, String)>) -> impl IntoResponse {
            axum::http::StatusCode::OK
        }

        async fn search(
            Path(ws): Path<String>,
            axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
        ) -> impl IntoResponse {
            assert_eq!(ws, "ws-1");
            assert!(query.contains_key("q"), "search requires q param");
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "sessions": [], "next_cursor": null })),
            )
        }

        async fn events() -> impl IntoResponse {
            let stream = futures::stream::iter([Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .event("thread_changed")
                    .data(r#"{"session_id":"s","revision":9}"#),
            )]);
            axum::response::sse::Sse::new(stream)
        }

        let api = Api {
            created: Arc::new(AtomicU64::new(0)),
            cancelled: Arc::new(AtomicU64::new(0)),
        };
        let app = axum::Router::new()
            .route("/health", axum::routing::get(|| async { "ok" }))
            .route("/v1/workspaces", axum::routing::post(workspace))
            .route("/v1/workspaces/{ws}/bindings", axum::routing::get(bindings))
            .route(
                "/v1/workspaces/{ws}/sessions",
                axum::routing::post(create).get(list),
            )
            .route("/v1/workspaces/{ws}/events", axum::routing::get(events))
            .route("/v1/sessions/{id}", axum::routing::get(snapshot))
            .route(
                "/v1/sessions/{id}/follow-up",
                axum::routing::post(follow_up),
            )
            .route("/v1/sessions/{id}/cancel", axum::routing::post(cancel))
            .route("/v1/sessions/{id}", axum::routing::patch(rename))
            .route("/v1/sessions/{id}/fork", axum::routing::post(fork))
            .route("/v1/sessions/{id}/model", axum::routing::post(switch_model))
            .route("/v1/sessions/{id}/queue", axum::routing::post(queue))
            .route("/v1/sessions/{id}/input", axum::routing::post(provide_input))
            .route(
                "/v1/sessions/{id}/permissions/{req_id}",
                axum::routing::post(resolve_permission),
            )
            .route(
                "/v1/sessions/{id}/effects/{effect_id}/reconcile",
                axum::routing::post(reconcile),
            )
            .route(
                "/v1/workspaces/{ws}/sessions/search",
                axum::routing::get(search),
            )
            .with_state(api);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown = async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        };
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        });
        let mock = MockHttp {
            base_url: format!("http://127.0.0.1:{port}"),
            _server: server,
            shutdown: shutdown_tx,
        };

        let mut client = mock.client("token");
        let ws = client
            .resolve_workspace(std::path::Path::new("/tmp/ws"))
            .await
            .unwrap();
        assert_eq!(ws, "ws-1");
        // Cached on the second call.
        assert_eq!(
            client
                .resolve_workspace(std::path::Path::new("/tmp/ws"))
                .await
                .unwrap(),
            "ws-1"
        );
        let binding = client.default_binding(&ws).await.unwrap();
        assert_eq!(binding["version"], 1);
        let revision = client
            .create_session(
                &ws,
                ThreadId::from_uuid(Uuid::now_v7()),
                ThreadCommandId::from_uuid(Uuid::now_v7()),
                "hello",
                None,
                &binding,
            )
            .await
            .unwrap();
        assert_eq!(revision, 3);
        let snapshot = client
            .snapshot(&ThreadId::from_uuid(
                Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(snapshot.revision, 3);
        assert!(client.list_sessions(&ws).await.unwrap().is_empty());
        let revision = client
            .follow_up(
                &ThreadId::from_uuid(
                    Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
                ),
                1,
                "more",
            )
            .await
            .unwrap();
        let (revision, event_ws) = revision;
        assert_eq!(revision, 2);
        assert_eq!(event_ws, "mock-workspace");
        client.open_events(&ws).await.unwrap();
        let event = client.next_event().await.unwrap().unwrap();
        assert_eq!(
            event,
            StreamEvent::ThreadChanged {
                session_id: "s".into(),
                revision: 9
            }
        );
        assert!(client.next_event().await.unwrap().is_none());
        client
            .cancel(
                &ThreadId::from_uuid(
                    Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
                ),
                1,
                1,
            )
            .await
            .unwrap();
        // TUI operations through the SessionServer trait (delegates to ServerHandle).
        let session_id = ThreadId::from_uuid(
            Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
        );
        client.rename_session(&session_id, "new title").await.unwrap();
        let fork_id = client.fork_session(&session_id, None).await.unwrap();
        assert_ne!(fork_id, session_id);
        client
            .switch_model(&session_id, 1, &json!({"version": 1}))
            .await
            .unwrap();
        let position = client.queue_follow_up(&session_id, "queued").await.unwrap();
        assert_eq!(position, 1);
        client
            .provide_input(&session_id, 1, 1, "req-1", "answer")
            .await
            .unwrap();
        client
            .resolve_permission(&session_id, 1, 1, "req-1", true)
            .await
            .unwrap();
        client
            .reconcile_effect(&session_id, "effect-1")
            .await
            .unwrap();
        let results = client.search_sessions(&ws, "test").await.unwrap();
        assert!(results.is_empty());
        let catalog = client.bindings_catalog(&ws).await.unwrap();
        assert!(!catalog.is_empty());
        let _ = mock;
    }

    #[tokio::test]
    async fn mock_server_covers_tui_operations() {
        let mut server = MockServer::new();
        let session_id = ThreadId::from_uuid(Uuid::now_v7());
        // Exercise every TUI operation on the mock to cover its implementations.
        server.rename_session(&session_id, "title").await.unwrap();
        let fork_id = server.fork_session(&session_id, None).await.unwrap();
        assert_ne!(fork_id, session_id);
        server
            .switch_model(&session_id, 1, &json!({"version": 1}))
            .await
            .unwrap();
        let position = server.queue_follow_up(&session_id, "prompt").await.unwrap();
        assert_eq!(position, 1);
        server
            .provide_input(&session_id, 1, 1, "req-1", "value")
            .await
            .unwrap();
        server
            .resolve_permission(&session_id, 1, 1, "req-1", true)
            .await
            .unwrap();
        server
            .reconcile_effect(&session_id, "effect-1")
            .await
            .unwrap();
        let results = server.search_sessions("ws-1", "query").await.unwrap();
        assert!(results.is_empty());
        let catalog = server.bindings_catalog("ws-1").await.unwrap();
        assert!(catalog.is_empty());
    }

    #[tokio::test]
    async fn http_client_maps_error_statuses() {
        let app = axum::Router::new()
            .route(
                "/v1/sessions/{id}",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({ "error": { "type": "internal", "message": "boom" } })),
                    )
                }),
            )
            .fallback(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(json!({ "error": { "type": "not_found", "message": "missing" } })),
                )
            });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown = async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        };
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        });
        let mock = MockHttp {
            base_url: format!("http://127.0.0.1:{port}"),
            _server: server,
            shutdown: shutdown_tx,
        };
        let mut client = mock.client("token");
        let error = client
            .snapshot(&ThreadId::from_uuid(Uuid::now_v7()))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            ClientError::Internal("500 Internal Server Error: boom".into())
        );
        let error = client.list_sessions("ws").await.unwrap_err();
        assert_eq!(error, ClientError::NotFound("missing".into()));
        let _ = mock;
    }

    #[tokio::test]
    async fn http_client_reports_unreachable() {
        // Nothing listens on this port.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = ServerClient::new(format!("http://127.0.0.1:{port}"), "token".into());
        let error = client.health_check().await.unwrap_err();
        assert_eq!(error.code(), "server_unreachable");
    }

    #[tokio::test]
    async fn sse_stream_survives_rest_client_timeout() {
        // The SSE stream must NOT inherit the REST client's total timeout.
        // Use a 1s REST timeout and verify the SSE stream stays alive for
        // >2s with events arriving every 500ms.
        use axum::response::sse::{Event, Sse};
        use futures::stream;

        async fn events() -> Sse<impl stream::Stream<Item = Result<Event, std::convert::Infallible>>>
        {
            let s = stream::unfold(0u32, |i| async move {
                if i >= 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                let event = Event::default()
                    .event("thread_changed")
                    .data(format!(r#"{{"session_id":"s","revision":{i}}}"#));
                Some((Ok(event), i + 1))
            });
            Sse::new(s)
        }

        let app =
            axum::Router::new().route("/v1/workspaces/{ws}/events", axum::routing::get(events));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown = async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        };
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        });
        let mock = MockHttp {
            base_url: format!("http://127.0.0.1:{port}"),
            _server: server,
            shutdown: shutdown_tx,
        };
        // REST timeout = 1s; the SSE stream must survive past it.
        let mut client = mock.client_with_rest_timeout("token", Duration::from_secs(1));
        client.open_events("ws-1").await.unwrap();
        // Read 5 events over 2.5s. If the SSE stream inherited the 1s REST
        // timeout, next_event would return Ok(None) after ~1s and we'd only
        // get ~2 events.
        let mut count = 0;
        for _ in 0..5 {
            let event = client.next_event().await.unwrap();
            assert!(event.is_some(), "stream ended prematurely at event {count}");
            count += 1;
        }
        assert_eq!(count, 5);
        let _ = mock;
    }

    #[tokio::test]
    async fn embedded_server_starts_serves_and_shuts_down() {
        let temp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::create_dir_all(temp.path().join(".latte")).unwrap();
        std::fs::write(
            temp.path().join(".latte/latte-code.jsonc"),
            r#"{version:1,default_model:"main/mock",providers:{main:{type:"openai-chat",models:["mock"],base_url:"http://127.0.0.1:1",api_key:{source:"env",name:"TEST_OPENAI_KEY"}}},verification:{argv:["true"]}}"#,
        )
        .unwrap();
        let embedded = EmbeddedServer::start(temp.path(), home.path())
            .await
            .unwrap();
        let url = embedded.base_url().to_string();
        // Health check without auth.
        let health = reqwest::Client::new()
            .get(format!("{url}/health"))
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        // Authenticated workspace resolve.
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/workspaces"))
            .bearer_auth(embedded.token())
            .json(&json!({ "path": temp.path() }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        // Unauthenticated request is rejected.
        let unauthorized = reqwest::Client::new()
            .get(format!("{url}/v1/workspaces"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
        embedded.shutdown().await;
        // The port is released after shutdown.
        assert!(
            tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .is_ok()
        );
    }

    #[test]
    fn map_error_status_covers_all_branches() {
        let body = json!({"error": {"message": "test message"}});
        assert_eq!(
            map_error_status(reqwest::StatusCode::UNAUTHORIZED, &body),
            ClientError::Unauthorized("test message".into())
        );
        assert_eq!(
            map_error_status(reqwest::StatusCode::NOT_FOUND, &body),
            ClientError::NotFound("test message".into())
        );
        assert_eq!(
            map_error_status(reqwest::StatusCode::BAD_REQUEST, &body),
            ClientError::Usage("test message".into())
        );
        assert_eq!(
            map_error_status(reqwest::StatusCode::CONFLICT, &body),
            ClientError::Conflict("test message".into())
        );
        assert_eq!(
            map_error_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &body),
            ClientError::Internal("500 Internal Server Error: test message".into())
        );
        assert_eq!(
            map_error_status(reqwest::StatusCode::FORBIDDEN, &body),
            ClientError::Failed("403 Forbidden: test message".into())
        );
        // Missing error.message falls back to "server error".
        let empty = json!({});
        assert_eq!(
            map_error_status(reqwest::StatusCode::BAD_REQUEST, &empty),
            ClientError::Usage("server error".into())
        );
    }

    #[test]
    fn resolve_remote_token_uses_explicit_or_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Explicit token wins.
        assert_eq!(
            resolve_remote_token(Some("explicit".into()), tmp.path()).unwrap(),
            "explicit"
        );
        // No explicit token, no file → Usage error.
        assert!(resolve_remote_token(None, tmp.path()).is_err());
        // File token is trimmed.
        std::fs::write(tmp.path().join("server.token"), "  file-token  \n").unwrap();
        assert_eq!(
            resolve_remote_token(None, tmp.path()).unwrap(),
            "file-token"
        );
    }

    // -- TUI operation integration tests (embedded server) -------------------

    /// Sets up an embedded server with a workspace that has a provider config.
    /// Returns the server handle and the workspace id.
    async fn tui_test_server() -> (EmbeddedServer, String, ServerHandle) {
        let temp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        std::fs::create_dir_all(temp.path().join(".latte")).unwrap();
        std::fs::write(
            temp.path().join(".latte/latte-code.jsonc"),
            r#"{version:1,default_model:"main/mock",providers:{main:{type:"openai-chat",models:["mock"],base_url:"http://127.0.0.1:1",api_key:{source:"env",name:"TEST_OPENAI_KEY"}}},verification:{argv:["true"]}}"#,
        )
        .unwrap();
        let embedded = EmbeddedServer::start(temp.path(), home.path())
            .await
            .unwrap();
        let handle = ServerHandle {
            http: reqwest::Client::new(),
            base_url: embedded.base_url().to_string(),
            token: embedded.token().to_string(),
        };
        let workspace_id = handle
            .resolve_workspace_id(temp.path())
            .await
            .unwrap();
        // Leak temp dirs so the server can read the config for the test's lifetime.
        std::mem::forget(temp);
        std::mem::forget(home);
        (embedded, workspace_id, handle)
    }

    #[tokio::test]
    async fn tui_bindings_catalog_returns_configured_bindings() {
        let (embedded, workspace_id, handle) = tui_test_server().await;
        let bindings = handle.bindings_catalog(&workspace_id).await.unwrap();
        assert!(!bindings.is_empty(), "bindings catalog must not be empty");
        assert!(
            bindings[0].get("binding").is_some(),
            "binding entry must have binding field"
        );
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_search_sessions_returns_empty_for_unknown_query() {
        let (embedded, workspace_id, handle) = tui_test_server().await;
        let results = handle
            .search_sessions(&workspace_id, "nonexistent-session-xyz")
            .await
            .unwrap();
        assert!(results.is_empty());
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_rename_session_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle.rename_session(&unknown_id, "new title").await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_fork_session_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle.fork_session(&unknown_id, None).await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_switch_model_returns_error_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        // The server may reject the invalid binding (400) before checking
        // the session (404); either proves the HTTP round-trip works.
        let result = handle
            .switch_model(&unknown_id, 0, &json!({"version": 1}))
            .await;
        assert!(result.is_err(), "switch_model on unknown session must fail");
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_queue_follow_up_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle.queue_follow_up(&unknown_id, "prompt").await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_provide_input_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle
            .provide_input(&unknown_id, 0, 0, "req-1", "value")
            .await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_resolve_permission_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle
            .resolve_permission(&unknown_id, 0, 0, "req-1", true)
            .await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_reconcile_effect_returns_not_found_for_unknown_id() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let unknown_id = ThreadId::from_uuid(Uuid::now_v7());
        let result = handle.reconcile_effect(&unknown_id, "effect-1").await;
        assert!(matches!(result, Err(ClientError::NotFound(_))));
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_server_handle_is_cloneable() {
        let (embedded, _workspace_id, handle) = tui_test_server().await;
        let cloned = handle.clone();
        assert_eq!(handle.base_url, cloned.base_url);
        assert_eq!(handle.token, cloned.token);
        embedded.shutdown().await;
    }

    #[tokio::test]
    async fn tui_client_handle_returns_cloneable_handle() {
        let (embedded, workspace_id, _handle) = tui_test_server().await;
        let client = ServerClient::new(
            embedded.base_url().to_string(),
            embedded.token().to_string(),
        );
        let handle = client.handle();
        // The handle can make independent HTTP calls.
        let bindings = handle.bindings_catalog(&workspace_id).await.unwrap();
        assert!(!bindings.is_empty());
        embedded.shutdown().await;
    }
}
