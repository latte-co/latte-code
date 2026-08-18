//! HTTP client for TUI projection and actions.
//!
//! Phase 1 of the server-client integration (see
//! `docs/design/server-client-integration.md` §5): the TUI talks to the server
//! over HTTP+SSE instead of an in-process engine. The TUI loop runs on a
//! `spawn_blocking` thread; all network I/O uses `reqwest::blocking` on
//! dedicated OS threads so no Tokio async worker is ever blocked.
//!
//! Ownership shape (§5.5): [`ClientWorkersOwner::start`] returns
//! `(Owner, Inputs)`. The owner (cancel flag + join handles) stays on the async
//! side and shuts down the workers; the inputs (wake state, action queue,
//! feedback/progress receivers) move into the TUI closure.

use crate::thread::{
    SessionManagementOutcome, ThreadProjectionClient, ThreadProjectionPoll, ThreadUiAction,
    ThreadUiFeedback,
};
use latte_core::{
    IdSource, SystemIdSource, ThreadCommandId, ThreadId, ThreadSessionSummary, ThreadSnapshot,
    ThreadTransientProgress,
};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Wake state (§5.2): sticky 5-state atomic, poll() swaps it back to idle.
// ---------------------------------------------------------------------------

const WAKE_IDLE: u8 = 0;
const WAKE_DIRTY: u8 = 1;
const WAKE_LAGGED: u8 = 2;
const WAKE_CLOSED: u8 = 3;
const WAKE_ERROR: u8 = 4;

/// Sticky wake bits shared between the SSE worker (writer) and the projection
/// client (reader). Event-driven states use `fetch_max` so a higher severity is
/// never lost; reconnect success uses `store` to *replace* the state, because
/// LAGGED implies a full resync that covers whatever was pending (the
/// `fetch_max` alternative lets a stale `ERROR(4)` swallow a reconnect's
/// `LAGGED(2)`).
pub(crate) struct WakeState {
    state: AtomicU8,
    error_msg: Mutex<Option<String>>,
}

impl WakeState {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(WAKE_IDLE),
            error_msg: Mutex::new(None),
        }
    }

    fn dirty(&self) {
        self.state.fetch_max(WAKE_DIRTY, Ordering::Release);
    }

    fn lagged(&self) {
        self.state.fetch_max(WAKE_LAGGED, Ordering::Release);
    }

    fn closed(&self) {
        self.state.fetch_max(WAKE_CLOSED, Ordering::Release);
    }

    /// Stores the error message *before* the state, so a poll that observes
    /// ERROR always finds the text ready.
    fn error(&self, msg: impl Into<String>) {
        *self.error_msg.lock().expect("wake mutex poisoned") = Some(msg.into());
        self.state.store(WAKE_ERROR, Ordering::Release);
    }

    /// Replaces the current state with LAGGED on a successful reconnect. Also
    /// clears any stale error message.
    fn reconnect_success(&self) {
        *self.error_msg.lock().expect("wake mutex poisoned") = None;
        self.state.store(WAKE_LAGGED, Ordering::Release);
    }

    fn poll(&self) -> ThreadProjectionPoll {
        match self.state.swap(WAKE_IDLE, Ordering::Acquire) {
            WAKE_DIRTY => ThreadProjectionPoll::Event,
            WAKE_LAGGED => ThreadProjectionPoll::Lagged(0),
            WAKE_CLOSED => ThreadProjectionPoll::Closed,
            WAKE_ERROR => {
                let msg = self
                    .error_msg
                    .lock()
                    .expect("wake mutex poisoned")
                    .take()
                    .unwrap_or_else(|| "connection error".into());
                ThreadProjectionPoll::Error(msg)
            }
            _ => ThreadProjectionPoll::Empty,
        }
    }
}

// ---------------------------------------------------------------------------
// Active-thread slot: progress demux (§5.4 item 6).
// ---------------------------------------------------------------------------

/// Tracks the session the TUI is currently viewing so the SSE worker can demux
/// progress events: only the active thread's transient progress is forwarded
/// to the TUI; other sessions' progress is dropped (it is ephemeral and would
/// otherwise leak into the wrong session's view).
pub(crate) struct ActiveThread(Mutex<Option<ThreadId>>);

impl ActiveThread {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn set(&self, thread_id: ThreadId) {
        *self.0.lock().expect("active thread mutex poisoned") = Some(thread_id);
    }

    fn is_active(&self, thread_id: ThreadId) -> bool {
        *self.0.lock().expect("active thread mutex poisoned") == Some(thread_id)
    }
}

// ---------------------------------------------------------------------------
// HttpProjectionClient (§5.2)
// ---------------------------------------------------------------------------

/// HTTP-based projection client. Reads go through `reqwest::blocking` (the
/// client lives on a `spawn_blocking` thread); change notifications arrive via
/// the shared [`WakeState`] written by the SSE worker.
pub struct HttpProjectionClient {
    http: reqwest::blocking::Client,
    base_url: String,
    token: String,
    workspace_id: String,
    wake: Arc<WakeState>,
    active: Arc<ActiveThread>,
}

impl HttpProjectionClient {
    /// Creates a projection client. `wake` and `active` are shared with the
    /// SSE worker started by [`ClientWorkersOwner::start`].
    #[must_use]
    pub fn new(
        base_url: String,
        token: String,
        workspace_id: String,
        wake: Arc<WakeState>,
        active: Arc<ActiveThread>,
    ) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url,
            token,
            workspace_id,
            wake,
            active,
        }
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .expect("token is a valid header value"),
        );
        headers
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .headers(self.auth_headers())
            .send()
            .map_err(|error| format!("request failed: {error}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status} on GET {path}"));
        }
        resp.json()
            .map_err(|error| format!("invalid JSON on GET {path}: {error}"))
    }

    fn get_snapshots(&self) -> Result<Vec<ThreadSnapshot>, String> {
        let body = self.get_json(&format!(
            "/v1/workspaces/{}/sessions",
            self.workspace_id
        ))?;
        serde_json::from_value(body["sessions"].clone())
            .map_err(|error| format!("invalid sessions payload: {error}"))
    }

    fn get_session(&self, thread_id: ThreadId) -> Result<ThreadSnapshot, String> {
        let body = self.get_json(&format!("/v1/sessions/{thread_id}"))?;
        serde_json::from_value(body["snapshot"].clone())
            .map_err(|error| format!("invalid session payload: {error}"))
    }

    fn search_sessions(&self, query: &str, limit: u32) -> Result<Vec<ThreadSessionSummary>, String> {
        let body = self.get_json(&format!(
            "/v1/workspaces/{}/sessions/search?q={}&limit={}",
            self.workspace_id,
            serde_json::to_string(query).map_err(|e| e.to_string())?,
            limit
        ))?;
        serde_json::from_value(body["sessions"].clone())
            .map_err(|error| format!("invalid search payload: {error}"))
    }
}

impl ThreadProjectionClient for HttpProjectionClient {
    fn snapshots(&mut self) -> Result<Vec<ThreadSnapshot>, String> {
        self.get_snapshots()
    }

    fn session_catalog(&mut self) -> Result<Vec<ThreadSessionSummary>, String> {
        // The search endpoint returns stored session metadata (title,
        // workspace_root, timestamps), which stays correct after a rename;
        // the trait default derives the title from the transcript and would
        // go stale. An empty query matches every session in the workspace.
        self.search_sessions("", 200)
    }

    fn exact_session_catalog(
        &mut self,
        query: &str,
    ) -> Result<Vec<ThreadSessionSummary>, String> {
        let all = self.search_sessions(query, 200)?;
        Ok(all
            .into_iter()
            .filter(|summary| summary.thread_id.to_string() == query || summary.title == query)
            .collect())
    }

    fn exact_session(&mut self, query: &str) -> Result<Option<ThreadSnapshot>, String> {
        let matches = self.exact_session_catalog(query)?;
        let [metadata] = matches.as_slice() else {
            return Ok(None);
        };
        self.get_session(metadata.thread_id).map(Some)
    }

    fn search_session_catalog(
        &mut self,
        query: &str,
    ) -> Result<Vec<ThreadSessionSummary>, String> {
        self.search_sessions(query, 200)
    }

    fn session(&mut self, thread_id: ThreadId) -> Result<ThreadSnapshot, String> {
        self.active.set(thread_id);
        self.get_session(thread_id)
    }

    fn poll(&mut self) -> ThreadProjectionPoll {
        self.wake.poll()
    }
}

// ---------------------------------------------------------------------------
// SSE worker (§5.4): dedicated OS thread, reconnect with backoff.
// ---------------------------------------------------------------------------

/// Spawns the SSE worker thread. The worker connects to the workspace event
/// stream, maps events to wake bits / progress, and reconnects with
/// exponential backoff (1s initial, 30s cap). It exits when `cancel` is set
/// (checked between stream lines; the server's 2s keepalive comment bounds the
/// wait) or when the stream ends and cancellation is requested during backoff.
fn spawn_sse_thread(
    base_url: String,
    token: String,
    workspace_id: String,
    wake: Arc<WakeState>,
    active: Arc<ActiveThread>,
    progress_tx: Sender<ThreadTransientProgress>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build SSE client");
        let mut backoff = Duration::from_secs(1);
        let mut ever_connected = false;
        while !cancel.load(Ordering::Acquire) {
            let url = format!("{base_url}/v1/workspaces/{workspace_id}/events");
            let resp = match client.get(&url).bearer_auth(&token).send() {
                Ok(resp) if resp.status().is_success() => {
                    if ever_connected {
                        // Reconnected after a gap: full resync to cover it.
                        wake.reconnect_success();
                    }
                    ever_connected = true;
                    backoff = Duration::from_secs(1);
                    resp
                }
                Ok(resp) => {
                    wake.error(format!("SSE connect failed: HTTP {}", resp.status()));
                    if sleep_with_cancel(&cancel, backoff) {
                        return;
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
                Err(error) => {
                    wake.error(format!("SSE connect failed: {error}"));
                    if sleep_with_cancel(&cancel, backoff) {
                        return;
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
            };
            // Read the SSE stream line by line. The server sends a keepalive
            // comment every 2s, so a blocked read returns at least that often
            // and the cancel check stays responsive.
            let reader = BufReader::new(resp);
            let mut event_name = String::new();
            let mut data_lines: Vec<String> = Vec::new();
            for line in reader.lines() {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let Ok(line) = line else { break };
                if line.is_empty() {
                    dispatch_sse_event(&event_name, &data_lines, &wake, &active, &progress_tx);
                    event_name.clear();
                    data_lines.clear();
                    continue;
                }
                if line.starts_with(':') {
                    continue; // comment / keepalive
                }
                if let Some((field, value)) = line.split_once(':') {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => event_name = value.to_string(),
                        "data" => data_lines.push(value.to_string()),
                        _ => {}
                    }
                }
            }
            // Stream ended unexpectedly.
            if cancel.load(Ordering::Acquire) {
                return;
            }
            wake.closed();
            if sleep_with_cancel(&cancel, backoff) {
                return;
            }
            backoff = next_backoff(backoff);
        }
    })
}

/// Dispatches one SSE event. `ServerEvent` serializes as externally tagged:
/// `{"ThreadChanged":{...}}`, `{"Progress":{...}}`, `"ResyncRequired"`.
fn dispatch_sse_event(
    event: &str,
    data_lines: &[String],
    wake: &WakeState,
    active: &ActiveThread,
    progress_tx: &Sender<ThreadTransientProgress>,
) {
    let data = data_lines.join("\n");
    match event {
        "thread_changed" => wake.dirty(),
        "resync_required" => wake.lagged(),
        "progress" => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
                return;
            };
            let Some(progress) = value.get("Progress") else {
                return;
            };
            let Some(session_id) = progress["session_id"].as_str() else {
                return;
            };
            let Ok(thread_id) =
                serde_json::from_value::<ThreadId>(serde_json::Value::String(session_id.into()))
            else {
                return;
            };
            if !active.is_active(thread_id) {
                return;
            }
            let Ok(update) =
                serde_json::from_value::<ThreadTransientProgress>(progress["progress"].clone())
            else {
                return;
            };
            let _ = progress_tx.send(update);
        }
        _ => {}
    }
}

/// Sleeps for `duration`, returning `true` early if cancellation is requested.
fn sleep_with_cancel(cancel: &AtomicBool, duration: Duration) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while !cancel.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(std::cmp::min(remaining, Duration::from_millis(50)));
    }
    true
}

fn next_backoff(current: Duration) -> Duration {
    std::cmp::min(current.saturating_mul(2), Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// Action sink + worker (§5.3): non-blocking sink, bounded queue, OS thread.
// ---------------------------------------------------------------------------

/// Capacity of the bounded action queue. The TUI enqueues without blocking;
/// when full, the sink immediately sends a failure feedback instead.
const ACTION_QUEUE_CAPACITY: usize = 64;

/// Creates the action sink closure. **The sink always returns `Ok(())`**: a
/// failed or full queue is surfaced through the feedback channel, never as an
/// error (an error from the sink exits the whole TUI).
#[must_use]
pub fn http_action_sink(
    queue: SyncSender<ThreadUiAction>,
    feedback_tx: Sender<ThreadUiFeedback>,
) -> impl FnMut(ThreadUiAction) -> Result<(), String> {
    move |action| {
        if is_ui_internal(&action) {
            return Ok(());
        }
        match queue.try_send(action) {
            Ok(()) => {}
            Err(TrySendError::Full(action)) => {
                queue_full_feedback(&feedback_tx, &action);
            }
            // The worker is gone (shutting down); the feedback channel is
            // likely also closed, so there is nowhere to report.
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(())
    }
}

fn is_ui_internal(action: &ThreadUiAction) -> bool {
    matches!(
        action,
        ThreadUiAction::RefreshSnapshots
            | ThreadUiAction::ShowSessions { .. }
            | ThreadUiAction::SearchSessions { .. }
            | ThreadUiAction::OpenSession { .. }
            | ThreadUiAction::Quit
    )
}

fn queue_full_feedback(tx: &Sender<ThreadUiFeedback>, action: &ThreadUiAction) {
    const MSG: &str = "action queue full";
    let _ = match action {
        ThreadUiAction::Start { submission_id, .. }
        | ThreadUiAction::StartWithModel { submission_id, .. }
        | ThreadUiAction::FollowUp { submission_id, .. }
        | ThreadUiAction::QueueFollowUp { submission_id, .. } => {
            tx.send(ThreadUiFeedback::submission(*submission_id, Err(MSG.into())))
        }
        ThreadUiAction::ProvideInput { submission_id, .. } => {
            tx.send(ThreadUiFeedback::input_submission(
                *submission_id,
                Err(MSG.into()),
            ))
        }
        ThreadUiAction::SwitchModel { switch_id, .. } => {
            tx.send(ThreadUiFeedback::model_switch(*switch_id, Err(MSG.into())))
        }
        ThreadUiAction::Cancel { .. }
        | ThreadUiAction::ResolvePermission { .. }
        | ThreadUiAction::ReconcileUnknown { .. } => {
            tx.send(ThreadUiFeedback::command(Err(MSG.into())))
        }
        ThreadUiAction::RenameSession { .. } | ThreadUiAction::ForkSession { .. } => {
            tx.send(ThreadUiFeedback::session_management(Err(MSG.into())))
        }
        _ => Ok(()),
    };
}

/// Spawns the action worker thread. A single worker guarantees per-session
/// ordering (actions for the same session are never executed concurrently);
/// TUI actions are user-driven and infrequent, so one worker is sufficient.
fn spawn_action_worker(
    base_url: String,
    token: String,
    workspace_id: String,
    wake: Arc<WakeState>,
    rx: Receiver<ThreadUiAction>,
    feedback_tx: Sender<ThreadUiFeedback>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build action client");
        let http = ActionHttpClient {
            http: &client,
            base_url: &base_url,
            token: &token,
            workspace_id: &workspace_id,
            wake: &wake,
        };
        while let Ok(action) = rx.recv() {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            execute_action(&http, &feedback_tx, action);
        }
    })
}

struct ActionHttpClient<'a> {
    http: &'a reqwest::blocking::Client,
    base_url: &'a str,
    token: &'a str,
    workspace_id: &'a str,
    wake: &'a WakeState,
}

impl ActionHttpClient<'_> {
    fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        self.request(reqwest::Method::GET, path, None, None)
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        self.request(reqwest::Method::POST, path, Some(body), None)
    }

    fn patch(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        self.request(reqwest::Method::PATCH, path, Some(body), None)
    }

    /// Like [`Self::post`] but sends an `Idempotency-Key` header. The create
    /// endpoint requires the header to equal the body's `command_id`.
    fn post_idempotent(
        &self,
        path: &str,
        body: serde_json::Value,
        idempotency_key: &str,
    ) -> Result<serde_json::Value, String> {
        self.request(
            reqwest::Method::POST,
            path,
            Some(body),
            Some(idempotency_key),
        )
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        idempotency_key: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self
            .http
            .request(method, &url)
            .bearer_auth(self.token);
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let resp = builder
            .send()
            .map_err(|error| format!("request failed: {error}"))?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .json()
                .map_err(|error| format!("invalid JSON on {path}: {error}"));
        }
        // A 409 revision-fence conflict means the server has newer state; set
        // the dirty bit so the TUI refreshes its snapshot on the next poll.
        if status == reqwest::StatusCode::CONFLICT {
            self.wake.dirty();
        }
        let detail = resp.text().unwrap_or_default();
        Err(format!("HTTP {status} on {path}: {detail}"))
    }

    /// Fetches the bindings catalog and returns the default binding's full
    /// `ThreadProviderBindingV2` value.
    fn default_binding(&self) -> Result<serde_json::Value, String> {
        let catalog = self.bindings_catalog()?;
        catalog
            .into_iter()
            .find(|entry| entry["is_default"].as_bool() == Some(true))
            .map(|entry| entry["binding"].clone())
            .ok_or_else(|| {
                "configure default_model and providers in ~/.latte/latte-code.jsonc, then restart Latte Code"
                    .into()
            })
    }

    /// Fetches the bindings catalog and returns the binding for the given
    /// provider/model pair.
    fn binding_for_model(
        &self,
        provider_name: &str,
        model: &str,
    ) -> Result<serde_json::Value, String> {
        let catalog = self.bindings_catalog()?;
        catalog
            .into_iter()
            .find(|entry| {
                entry["provider_name"].as_str() == Some(provider_name)
                    && entry["model"].as_str() == Some(model)
            })
            .map(|entry| entry["binding"].clone())
            .ok_or_else(|| format!("model {provider_name}/{model} not found in bindings catalog"))
    }

    fn bindings_catalog(&self) -> Result<Vec<serde_json::Value>, String> {
        let body = self.get(&format!("/v1/workspaces/{}/bindings", self.workspace_id))?;
        body["bindings"]
            .as_array()
            .cloned()
            .ok_or_else(|| "invalid bindings payload".to_string())
    }

    /// Fetches the current snapshot and extracts the revision fences needed
    /// for cancel/input/permission requests.
    fn current_revisions(&self, thread_id: ThreadId) -> Result<(u64, u64), String> {
        let body = self.get(&format!("/v1/sessions/{thread_id}"))?;
        let snapshot = &body["snapshot"];
        let thread_revision = snapshot["revision"].as_u64().unwrap_or(0);
        let run_revision = snapshot["active_run_id"]
            .as_str()
            .and_then(|active| {
                snapshot["runs"]
                    .as_array()?
                    .iter()
                    .find(|run| run["run_id"].as_str() == Some(active))
            })
            .and_then(|run| run["run_revision"].as_u64())
            .unwrap_or(0);
        Ok((thread_revision, run_revision))
    }
}

/// Executes one action and sends the matching feedback. All failures are
/// reported as feedback; this function never panics on network errors.
fn execute_action(http: &ActionHttpClient, feedback_tx: &Sender<ThreadUiFeedback>, action: ThreadUiAction) {
    use ThreadUiAction::*;
    match action {
        Start {
            submission_id,
            prompt,
        } => {
            let result = start_session(http, feedback_tx, submission_id, &prompt, None, None);
            send_submission(feedback_tx, submission_id, result);
        }
        StartWithModel {
            submission_id,
            prompt,
            provider_name,
            model,
        } => {
            let result = http
                .binding_for_model(&provider_name, &model)
                .and_then(|binding| {
                    start_session(http, feedback_tx, submission_id, &prompt, Some(binding), None)
                });
            send_submission(feedback_tx, submission_id, result);
        }
        FollowUp {
            submission_id,
            thread_id,
            expected_thread_revision,
            prompt,
        } => {
            let result = http
                .post(
                    &format!("/v1/sessions/{thread_id}/follow-up"),
                    serde_json::json!({
                        "prompt": prompt,
                        "expected_thread_revision": expected_thread_revision,
                    }),
                )
                .map(|_| "follow-up completed".into());
            send_submission(feedback_tx, submission_id, result);
        }
        QueueFollowUp {
            submission_id,
            thread_id,
            prompt,
        } => {
            let result = http
                .post(
                    &format!("/v1/sessions/{thread_id}/queue"),
                    serde_json::json!({ "prompt": prompt }),
                )
                .map(|body| {
                    format!(
                        "follow-up queued at position {}",
                        body["position"].as_u64().unwrap_or(0)
                    )
                });
            send_submission(feedback_tx, submission_id, result);
        }
        Cancel { thread_id } => {
            let result = http
                .current_revisions(thread_id)
                .and_then(|(thread_revision, run_revision)| {
                    http.post(
                        &format!("/v1/sessions/{thread_id}/cancel"),
                        serde_json::json!({
                            "expected_thread_revision": thread_revision,
                            "expected_run_revision": run_revision,
                        }),
                    )
                })
                .map(|_| "interruption requested".into());
            let _ = feedback_tx.send(ThreadUiFeedback::command(result));
        }
        ProvideInput {
            submission_id,
            thread_id,
            request_id,
            value,
        } => {
            let result = http
                .current_revisions(thread_id)
                .and_then(|(thread_revision, run_revision)| {
                    http.post(
                        &format!("/v1/sessions/{thread_id}/input"),
                        serde_json::json!({
                            "request_id": request_id,
                            "value": value,
                            "expected_thread_revision": thread_revision,
                            "expected_run_revision": run_revision,
                        }),
                    )
                })
                .map(|_| "input accepted".into());
            let _ = feedback_tx.send(ThreadUiFeedback::input_submission(
                submission_id,
                result,
            ));
        }
        ResolvePermission {
            thread_id,
            request_id,
            allow,
        } => {
            let result = http
                .current_revisions(thread_id)
                .and_then(|(thread_revision, run_revision)| {
                    http.post(
                        &format!("/v1/sessions/{thread_id}/permissions/{request_id}"),
                        serde_json::json!({
                            "allow": allow,
                            "expected_thread_revision": thread_revision,
                            "expected_run_revision": run_revision,
                        }),
                    )
                })
                .map(|_| {
                    if allow {
                        "permission allowed".into()
                    } else {
                        "permission denied".into()
                    }
                });
            let _ = feedback_tx.send(ThreadUiFeedback::command(result));
        }
        SwitchModel {
            switch_id,
            thread_id,
            expected_thread_revision,
            provider_name,
            model,
        } => {
            let result = http
                .binding_for_model(&provider_name, &model)
                .and_then(|binding| {
                    http.post(
                        &format!("/v1/sessions/{thread_id}/model"),
                        serde_json::json!({
                            "binding": binding,
                            "expected_thread_revision": expected_thread_revision,
                        }),
                    )
                })
                .map(|_| format!("Model switched to {provider_name}/{model}"));
            let _ = feedback_tx.send(ThreadUiFeedback::model_switch(switch_id, result));
        }
        ReconcileUnknown {
            thread_id,
            effect_id,
        } => {
            let result = http
                .post(
                    &format!("/v1/sessions/{thread_id}/effects/{effect_id}/reconcile"),
                    serde_json::json!({}),
                )
                .map(|_| "unknown effect acknowledged; child aborted".into());
            let _ = feedback_tx.send(ThreadUiFeedback::command(result));
        }
        RenameSession { thread_id, title } => {
            let result = http
                .patch(
                    &format!("/v1/sessions/{thread_id}"),
                    serde_json::json!({ "title": title }),
                )
                .map(|_| SessionManagementOutcome::Updated(format!("Session renamed to {title}")));
            let _ = feedback_tx.send(ThreadUiFeedback::session_management(result));
        }
        ForkSession { thread_id, title } => {
            let result = http
                .post(
                    &format!("/v1/sessions/{thread_id}/fork"),
                    serde_json::json!({ "title": title }),
                )
                .and_then(|body| {
                    let id = body["snapshot"]["thread_id"]
                        .as_str()
                        .ok_or_else(|| "fork response missing thread_id".to_string())?;
                    let thread_id = serde_json::from_value::<ThreadId>(
                        serde_json::Value::String(id.to_string()),
                    )
                    .map_err(|error| format!("invalid fork thread_id: {error}"))?;
                    Ok(SessionManagementOutcome::Forked(thread_id))
                });
            let _ = feedback_tx.send(ThreadUiFeedback::session_management(result));
        }
        // UI-internal actions are filtered by the sink; this is defensive.
        RefreshSnapshots | ShowSessions { .. } | SearchSessions { .. } | OpenSession { .. }
        | Quit => {}
    }
}

/// Starts a new session. Sends `SubmissionAssigned` immediately (the client
/// generates the thread id, so the TUI can track the pending submission
/// without waiting for the server), then POSTs the create request and returns
/// the result for `SubmissionResult`.
fn start_session(
    http: &ActionHttpClient,
    feedback_tx: &Sender<ThreadUiFeedback>,
    submission_id: u64,
    prompt: &str,
    binding: Option<serde_json::Value>,
    focus: Option<String>,
) -> Result<String, String> {
    let binding = match binding {
        Some(binding) => binding,
        None => http.default_binding()?,
    };
    let ids = SystemIdSource::default();
    let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
    let command_id = ThreadCommandId::from_uuid(ids.next_uuid_v7());
    let _ = feedback_tx.send(ThreadUiFeedback::assigned(submission_id, thread_id));
    http.post_idempotent(
        &format!("/v1/workspaces/{}/sessions", http.workspace_id),
        serde_json::json!({
            "thread_id": thread_id,
            "command_id": command_id,
            "prompt": prompt,
            "binding": binding,
            "focus": focus,
        }),
        &command_id.to_string(),
    )
    .map(|_| "conversation started".into())
}

fn send_submission(
    tx: &Sender<ThreadUiFeedback>,
    submission_id: u64,
    result: Result<String, String>,
) {
    let _ = tx.send(ThreadUiFeedback::submission(submission_id, result));
}

// ---------------------------------------------------------------------------
// ClientWorkersOwner / ClientWorkerInputs (§5.5)
// ---------------------------------------------------------------------------

/// Inputs that move into the TUI closure. The receivers are exclusive (moved,
/// not cloned — `std::sync::mpsc::Receiver` is not `Clone`).
pub struct ClientWorkerInputs {
    pub wake_state: Arc<WakeState>,
    pub active_thread: Arc<ActiveThread>,
    pub action_queue: SyncSender<ThreadUiAction>,
    pub feedback_rx: Receiver<ThreadUiFeedback>,
    pub progress_rx: Receiver<ThreadTransientProgress>,
}

/// RAII owner of the client worker threads. Stays on the async side; the TUI
/// closure only receives [`ClientWorkerInputs`].
pub struct ClientWorkersOwner {
    cancel: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl ClientWorkersOwner {
    /// Starts the SSE worker and the action worker, returning the owner and
    /// the inputs for the TUI closure.
    #[must_use]
    pub fn start(base_url: &str, token: &str, workspace_id: &str) -> (Self, ClientWorkerInputs) {
        let wake_state = Arc::new(WakeState::new());
        let active_thread = Arc::new(ActiveThread::new());
        let cancel = Arc::new(AtomicBool::new(false));
        let (feedback_tx, feedback_rx) = mpsc::channel::<ThreadUiFeedback>();
        let (progress_tx, progress_rx) = mpsc::channel::<ThreadTransientProgress>();
        let (action_queue, action_rx) = mpsc::sync_channel::<ThreadUiAction>(ACTION_QUEUE_CAPACITY);

        let sse_handle = spawn_sse_thread(
            base_url.to_string(),
            token.to_string(),
            workspace_id.to_string(),
            Arc::clone(&wake_state),
            Arc::clone(&active_thread),
            progress_tx,
            Arc::clone(&cancel),
        );
        let action_handle = spawn_action_worker(
            base_url.to_string(),
            token.to_string(),
            workspace_id.to_string(),
            Arc::clone(&wake_state),
            action_rx,
            feedback_tx,
            Arc::clone(&cancel),
        );

        let owner = Self {
            cancel,
            handles: vec![sse_handle, action_handle],
        };
        let inputs = ClientWorkerInputs {
            wake_state,
            active_thread,
            action_queue,
            feedback_rx,
            progress_rx,
        };
        (owner, inputs)
    }

    /// Cancels and joins all client workers. The SSE thread checks the cancel
    /// flag between stream lines (bounded by the server's 2s keepalive); the
    /// action thread exits when its queue sender is dropped.
    pub fn shutdown(self) {
        self.cancel.store(true, Ordering::Release);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_state_dirty_lagged_closed_transitions() {
        let wake = WakeState::new();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Empty));
        wake.dirty();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Event));
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Empty));
        wake.lagged();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Lagged(0)));
        wake.closed();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Closed));
    }

    #[test]
    fn wake_state_error_carries_message() {
        let wake = WakeState::new();
        wake.error("boom");
        match wake.poll() {
            ThreadProjectionPoll::Error(msg) => assert_eq!(msg, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
        // Error is consumed by poll.
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Empty));
    }

    /// Regression: `fetch_max(LAGGED)` after an `ERROR` would leave the state
    /// at 4, so the reconnect's resync signal would be lost. Reconnect must
    /// *replace* the state with LAGGED.
    #[test]
    fn wake_state_reconnect_success_replaces_error_with_lagged() {
        let wake = WakeState::new();
        wake.error("transient");
        wake.reconnect_success();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Lagged(0)));
    }

    #[test]
    fn wake_state_reconnect_success_replaces_closed_with_lagged() {
        let wake = WakeState::new();
        wake.closed();
        wake.reconnect_success();
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Lagged(0)));
    }

    #[test]
    fn wake_state_dirty_does_not_override_higher_severity() {
        let wake = WakeState::new();
        wake.error("boom");
        wake.dirty(); // fetch_max(1) must not lower ERROR(4)
        assert!(matches!(wake.poll(), ThreadProjectionPoll::Error(_)));
    }

    #[test]
    fn active_thread_demux() {
        let active = ActiveThread::new();
        let id1 = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let id2 = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        assert!(!active.is_active(id1));
        active.set(id1);
        assert!(active.is_active(id1));
        assert!(!active.is_active(id2));
        active.set(id2);
        assert!(active.is_active(id2));
        assert!(!active.is_active(id1));
    }

    #[test]
    fn sink_returns_ok_when_queue_full_and_sends_failure_feedback() {
        let (queue_tx, queue_rx) = mpsc::sync_channel::<ThreadUiAction>(1);
        let (feedback_tx, feedback_rx) = mpsc::channel::<ThreadUiFeedback>();
        let mut sink = http_action_sink(queue_tx, feedback_tx);

        // Fill the single slot.
        sink(ThreadUiAction::RefreshSnapshots).unwrap(); // UI-internal, not queued
        let action = ThreadUiAction::Cancel {
            thread_id: ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        };
        sink(action.clone()).unwrap(); // occupies the slot
        // Now the queue is full; a second action must not block and must
        // produce a failure feedback.
        let action2 = ThreadUiAction::Cancel {
            thread_id: ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        };
        sink(action2).unwrap(); // must return Ok, not block
        match feedback_rx.try_recv() {
            Ok(ThreadUiFeedback::Command(Err(msg))) => assert_eq!(msg, "action queue full"),
            other => panic!("expected Command(Err), got {other:?}"),
        }
        // The first action is still queued (not dropped).
        assert!(queue_rx.try_recv().is_ok());
    }

    #[test]
    fn sink_filters_ui_internal_actions() {
        let (queue_tx, queue_rx) = mpsc::sync_channel::<ThreadUiAction>(4);
        let (feedback_tx, _feedback_rx) = mpsc::channel::<ThreadUiFeedback>();
        let mut sink = http_action_sink(queue_tx, feedback_tx);
        sink(ThreadUiAction::RefreshSnapshots).unwrap();
        sink(ThreadUiAction::Quit).unwrap();
        sink(ThreadUiAction::ShowSessions {
            query: Some("x".into()),
        })
        .unwrap();
        assert!(queue_rx.try_recv().is_err()); // nothing queued
    }

    #[test]
    fn queue_full_feedback_covers_all_action_kinds() {
        let (queue_tx, _queue_rx) = mpsc::sync_channel::<ThreadUiAction>(0); // zero capacity
        let (feedback_tx, feedback_rx) = mpsc::channel::<ThreadUiFeedback>();
        let mut sink = http_action_sink(queue_tx, feedback_tx);
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());

        sink(ThreadUiAction::Start {
            submission_id: 1,
            prompt: "p".into(),
        })
        .unwrap();
        assert!(matches!(
            feedback_rx.try_recv(),
            Ok(ThreadUiFeedback::SubmissionResult { .. })
        ));

        sink(ThreadUiAction::ProvideInput {
            submission_id: 2,
            thread_id,
            request_id: "r".into(),
            value: "v".into(),
        })
        .unwrap();
        assert!(matches!(
            feedback_rx.try_recv(),
            Ok(ThreadUiFeedback::InputSubmissionResult { .. })
        ));

        sink(ThreadUiAction::SwitchModel {
            switch_id: 3,
            thread_id,
            expected_thread_revision: 1,
            provider_name: "p".into(),
            model: "m".into(),
        })
        .unwrap();
        assert!(matches!(
            feedback_rx.try_recv(),
            Ok(ThreadUiFeedback::ModelSwitchResult { .. })
        ));

        sink(ThreadUiAction::RenameSession {
            thread_id,
            title: "t".into(),
        })
        .unwrap();
        assert!(matches!(
            feedback_rx.try_recv(),
            Ok(ThreadUiFeedback::SessionManagement(Err(_)))
        ));
    }

    #[test]
    fn next_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(16)), Duration::from_secs(30));
        assert_eq!(next_backoff(Duration::from_secs(30)), Duration::from_secs(30));
    }

    #[test]
    fn sleep_with_cancel_returns_immediately_when_cancelled() {
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(sleep_with_cancel(&cancel, Duration::from_secs(60)));
    }
}
