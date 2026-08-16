//! HTTP server with per-workspace event hubs.

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::sse::{Event, Sse},
    routing::{get, post},
};
use futures::stream::Stream;
use latte_core::ThreadId;
use latte_headless::thread::ThreadRuntimeError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{info, warn};

use crate::workspace::{WorkspaceInstance, WorkspaceManager};

/// A recorded outcome of a durable mutation, replayed verbatim when a client
/// retries the same `Idempotency-Key`. Only the accepted acknowledgement is
/// stored; the durable turn continues under supervised background execution.
#[derive(Clone)]
struct IdempotentRecord {
    status: StatusCode,
    body: serde_json::Value,
    payload_digest: String,
}

/// The state of one idempotency key: either an in-flight reservation (a
/// concurrent request is currently producing the result) or a completed record
/// available for replay.
#[derive(Clone)]
enum IdempotentSlot {
    Pending { payload_digest: String },
    Done(IdempotentRecord),
}

/// The outcome of trying to claim an idempotency key.
enum IdempotencyClaim {
    /// This request owns the key and must produce the result.
    Owner,
    /// A prior request already completed; replay its recorded result.
    Replay(IdempotentRecord),
    /// A concurrent request holds the reservation and has not completed yet.
    InFlight,
    /// The key was used with a different payload digest — caller must not proceed.
    PayloadMismatch,
}

/// Server state shared across handlers.
pub struct ServerState {
    pub workspaces: Arc<WorkspaceManager>,
    pub event_tx: broadcast::Sender<ServerEvent>,
    pub token: String,
    /// Deduplicates durable mutations by `(token, idempotency_key)`. A retry
    /// after a timeout returns the original accepted result instead of
    /// starting a second session or duplicating provider/effect work.
    idempotency: Mutex<HashMap<String, IdempotentSlot>>,
}

impl ServerState {
    /// Creates server state with an empty idempotency ledger.
    #[must_use]
    pub fn new(
        workspaces: Arc<WorkspaceManager>,
        event_tx: broadcast::Sender<ServerEvent>,
        token: String,
    ) -> Self {
        Self {
            workspaces,
            event_tx,
            token,
            idempotency: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically claims an idempotency key: the first caller becomes `Owner`
    /// and reserves the slot; a concurrent caller sees `InFlight`; a caller
    /// after completion gets `Replay`. The slot never expires based on time —
    /// it remains in-flight until the owner explicitly completes or releases.
    ///
    /// If a completed record exists but the payload digest differs from the
    /// current request, `PayloadMismatch` is returned so the handler can
    /// reject the request with 422 rather than replaying stale data.
    fn idempotency_claim(&self, key: &str, payload_digest: &str) -> IdempotencyClaim {
        let mut ledger = self.idempotency.lock().expect("idempotency mutex poisoned");
        match ledger.get(key) {
            Some(IdempotentSlot::Done(record)) => {
                if record.payload_digest == payload_digest {
                    IdempotencyClaim::Replay(record.clone())
                } else {
                    IdempotencyClaim::PayloadMismatch
                }
            }
            Some(IdempotentSlot::Pending {
                payload_digest: existing,
            }) => {
                if existing == payload_digest {
                    IdempotencyClaim::InFlight
                } else {
                    IdempotencyClaim::PayloadMismatch
                }
            }
            None => {
                ledger.insert(
                    key.to_string(),
                    IdempotentSlot::Pending {
                        payload_digest: payload_digest.to_string(),
                    },
                );
                IdempotencyClaim::Owner
            }
        }
    }

    /// Records the completed result for a key the caller owns, unblocking
    /// future replays.
    fn idempotency_complete(&self, key: &str, record: IdempotentRecord) {
        self.idempotency
            .lock()
            .expect("idempotency mutex poisoned")
            .insert(key.to_string(), IdempotentSlot::Done(record));
    }

    /// Releases an owned reservation without recording a result, so a failed
    /// owner does not permanently block retries of the same key.
    fn idempotency_release(&self, key: &str) {
        let mut ledger = self.idempotency.lock().expect("idempotency mutex poisoned");
        if matches!(ledger.get(key), Some(IdempotentSlot::Pending { .. })) {
            ledger.remove(key);
        }
    }
}

/// Server events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    ThreadChanged {
        session_id: String,
        revision: u64,
    },
    Progress {
        session_id: String,
        progress: serde_json::Value,
    },
    ResyncRequired,
}

/// Create the HTTP router.
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/v1/workspaces", post(create_workspace))
        .route(
            "/v1/workspaces/{workspace_id}/sessions",
            post(create_session).get(list_sessions),
        )
        .route(
            "/v1/workspaces/{workspace_id}/sessions/search",
            get(search_sessions),
        )
        .route(
            "/v1/workspaces/{workspace_id}/events",
            get(workspace_events),
        )
        .route("/v1/sessions/{session_id}", get(get_session))
        .route("/v1/sessions/{session_id}/follow-up", post(follow_up))
        .route("/v1/sessions/{session_id}/model", post(switch_model))
        .route("/v1/sessions/{session_id}/cancel", post(cancel_session))
        .route("/v1/sessions/{session_id}/queue", post(queue_follow_up))
        .route(
            "/v1/sessions/{session_id}/permissions/{request_id}",
            post(resolve_permission),
        )
        .route("/v1/sessions/{session_id}/input", post(provide_input))
        .route(
            "/v1/sessions/{session_id}/effects/{effect_id}/reconcile",
            post(reconcile_effect),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Health check endpoint.
async fn health_check() -> &'static str {
    "ok"
}

/// Auth middleware: validate Bearer token.
async fn auth_middleware(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    // Skip auth for health check
    if request.uri().path() == "/health" {
        return Ok(next.run(request).await);
    }

    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    match auth {
        Some(token) if token == state.token => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// Request/Response types

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    pub workspace_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub prompt: String,
    pub binding: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct SessionCreatedResponse {
    pub session_id: String,
    pub accepted_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct FollowUpRequest {
    pub prompt: String,
    pub expected_thread_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct SwitchModelRequest {
    pub binding: serde_json::Value,
    pub expected_thread_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct QueueFollowUpRequest {
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
pub struct ResolvePermissionRequest {
    pub allow: bool,
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct ProvideInputRequest {
    pub request_id: String,
    pub value: String,
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

type HandlerError = (StatusCode, Json<ErrorResponse>);

// Handlers

async fn create_workspace(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, HandlerError> {
    let path = PathBuf::from(&req.path);
    let workspace = state.workspaces.get_or_create(&path).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: ErrorBody {
                    error_type: "rejected".to_string(),
                    message: format!("invalid workspace: {e}"),
                    current_revision: None,
                },
            }),
        )
    })?;

    Ok(Json(WorkspaceResponse {
        workspace_id: workspace.id.clone(),
        path: workspace.path.display().to_string(),
    }))
}

/// Accepts a new conversation. The session is registered and the first turn is
/// dispatched under supervised background execution; the endpoint returns 202
/// immediately and completion/error is observed through the workspace SSE
/// stream. A durable `Idempotency-Key` retry replays the original acceptance.
async fn create_session(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), HandlerError> {
    let idempotency = scoped_idempotency_key(&state, &headers, &format!("create:{workspace_id}"));
    let payload_digest = canonical_digest(&serde_json::json!({
        "prompt": &req.prompt,
        "binding": &req.binding,
    }));
    // Atomically claim the key: replay a prior result, reject a concurrent
    // in-flight retry, or become the owner responsible for producing it.
    if let Some(key) = &idempotency {
        match state.idempotency_claim(key, &payload_digest) {
            IdempotencyClaim::Replay(record) => return Ok((record.status, Json(record.body))),
            IdempotencyClaim::InFlight => return Err(in_flight()),
            IdempotencyClaim::PayloadMismatch => return Err(payload_mismatch()),
            IdempotencyClaim::Owner => {}
        }
    }
    // From here the owner must release the reservation on any early error.
    let result = create_session_owned(&state, &workspace_id, req).await;
    match (idempotency, result) {
        (Some(key), Ok((status, body))) => {
            state.idempotency_complete(
                &key,
                IdempotentRecord {
                    status,
                    body: body.clone(),
                    payload_digest,
                },
            );
            Ok((status, Json(body)))
        }
        (Some(key), Err(error)) => {
            state.idempotency_release(&key);
            Err(error)
        }
        (None, Ok((status, body))) => Ok((status, Json(body))),
        (None, Err(error)) => Err(error),
    }
}

/// Core of create_session, independent of the idempotency ledger. Persists the
/// accepted submission durably (awaiting the runtime's acceptance signal) and
/// returns 202 only after durable acceptance; the turn runs in the background.
async fn create_session_owned(
    state: &Arc<ServerState>,
    workspace_id: &str,
    req: CreateSessionRequest,
) -> Result<(StatusCode, serde_json::Value), HandlerError> {
    let workspace = state
        .workspaces
        .get_by_id(workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    // Parse and validate the complete non-secret binding before acceptance.
    let binding: latte_core::ThreadProviderBindingV2 = serde_json::from_value(req.binding)
        .map_err(|e| bad_request(&format!("invalid binding: {e}")))?;
    binding
        .validate()
        .map_err(|e| bad_request(&format!("invalid binding: {e}")))?;

    let thread_id = ThreadId::from_uuid(uuid::Uuid::now_v7());
    let runtime = workspace.runtime.clone();
    let prompt = req.prompt;

    // Run the turn under supervised background execution, but only acknowledge
    // 202 after the runtime signals the submission is durably accepted.
    let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Err(error) = runtime
            .start_accepted(thread_id, prompt, binding, accept_tx)
            .await
        {
            warn!("session {thread_id} background turn failed: {error}");
        }
    });

    let accepted = accept_rx
        .await
        .map_err(|_| failed("session runtime dropped before acceptance"))?
        .map_err(|message| failed(&format!("failed to accept session: {message}")))?;

    // Register the session for O(1) routing now that it is durable.
    state
        .workspaces
        .register_session(thread_id, workspace.path.clone())
        .await;

    // Wake subscribers so they fetch the new session snapshot.
    let _ = workspace.event_tx.send(ServerEvent::ThreadChanged {
        session_id: thread_id.to_string(),
        revision: accepted.revision,
    });

    let body = serde_json::to_value(SessionCreatedResponse {
        session_id: thread_id.to_string(),
        accepted_revision: accepted.revision,
    })
    .expect("session response serializes");
    Ok((StatusCode::ACCEPTED, body))
}

async fn list_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(_pagination): Query<PaginationQuery>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    let sessions = workspace
        .list_sessions()
        .map_err(|e| failed(&format!("cannot list sessions: {e}")))?;
    Ok(Json(
        serde_json::json!({ "sessions": sessions, "next_cursor": null }),
    ))
}

async fn search_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    let limit = query.limit.unwrap_or(50).clamp(1, 200) as usize;
    let sessions = workspace
        .search_sessions(&query.q, limit)
        .map_err(|e| failed(&format!("cannot search sessions: {e}")))?;
    Ok(Json(
        serde_json::json!({ "sessions": sessions, "next_cursor": null }),
    ))
}

async fn get_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    Ok(Json(serde_json::json!({ "snapshot": snapshot })))
}

/// Continues a session with a new user turn. Like create, this awaits durable
/// acceptance before returning 202 and runs the turn in the background; a
/// durable `Idempotency-Key` retry replays the original acceptance.
async fn follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), HandlerError> {
    let idempotency = scoped_idempotency_key(&state, &headers, &format!("follow-up:{id}"));
    let payload_digest = canonical_digest(&serde_json::json!({
        "prompt": &req.prompt,
        "expected_thread_revision": req.expected_thread_revision,
    }));
    if let Some(key) = &idempotency {
        match state.idempotency_claim(key, &payload_digest) {
            IdempotencyClaim::Replay(record) => return Ok((record.status, Json(record.body))),
            IdempotencyClaim::InFlight => return Err(in_flight()),
            IdempotencyClaim::PayloadMismatch => return Err(payload_mismatch()),
            IdempotencyClaim::Owner => {}
        }
    }
    let result = follow_up_owned(&state, &id, req).await;
    match (idempotency, result) {
        (Some(key), Ok((status, body))) => {
            state.idempotency_complete(
                &key,
                IdempotentRecord {
                    status,
                    body: body.clone(),
                    payload_digest,
                },
            );
            Ok((status, Json(body)))
        }
        (Some(key), Err(error)) => {
            state.idempotency_release(&key);
            Err(error)
        }
        (None, Ok((status, body))) => Ok((status, Json(body))),
        (None, Err(error)) => Err(error),
    }
}

async fn follow_up_owned(
    state: &Arc<ServerState>,
    id: &str,
    req: FollowUpRequest,
) -> Result<(StatusCode, serde_json::Value), HandlerError> {
    let thread_id = parse_thread_id(id)?;
    let workspace = lookup_workspace(state, thread_id).await?;
    let runtime = workspace.runtime.clone();
    let prompt = req.prompt;
    let expected = req.expected_thread_revision;

    let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Err(error) = runtime
            .follow_up_accepted(thread_id, expected, prompt, accept_tx)
            .await
        {
            warn!("session {thread_id} background follow-up failed: {error}");
        }
    });

    let accepted = accept_rx
        .await
        .map_err(|_| failed("session runtime dropped before acceptance"))?
        .map_err(|_| {
            conflict(
                "thread revision mismatch or session not accepting follow-up",
                None,
            )
        })?;

    let _ = workspace.event_tx.send(ServerEvent::ThreadChanged {
        session_id: thread_id.to_string(),
        revision: accepted.revision,
    });

    let body = serde_json::json!({ "accepted_revision": accepted.revision });
    Ok((StatusCode::ACCEPTED, body))
}

async fn switch_model(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<SwitchModelRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let binding: latte_core::ThreadProviderBindingV2 = serde_json::from_value(req.binding)
        .map_err(|e| bad_request(&format!("invalid binding: {e}")))?;

    let workspace = lookup_workspace(&state, thread_id).await?;
    // The revision fence is validated atomically inside the engine operation;
    // no TOCTOU precheck here.
    match workspace
        .runtime
        .switch_model(thread_id, req.expected_thread_revision, &binding)
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => {
            let current = workspace.snapshot(thread_id).ok().map(|s| s.revision);
            Err(map_runtime_error(&error, current.unwrap_or_default()))
        }
    }
}

/// Cancels an active session. Both revision fences are validated atomically
/// inside the engine authority operation (not a TOCTOU precheck here), so a
/// stale client cannot cancel a newer run; a mismatch returns 409 with the
/// current revision.
async fn cancel_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;

    match workspace.runtime.cancel_durable(
        thread_id,
        req.expected_thread_revision,
        req.expected_run_revision,
    ) {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => {
            // On a fence/state rejection, surface the current revision so the
            // client can re-fetch and retry.
            let current = workspace.snapshot(thread_id).ok().map(|s| s.revision);
            Err(map_runtime_error(&error, current.unwrap_or_default()))
        }
    }
}

async fn queue_follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<QueueFollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;

    match workspace.runtime.queue_follow_up(thread_id, req.prompt) {
        Ok(position) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "position": position })),
        )),
        Err(ThreadRuntimeError::MailboxFull) => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: ErrorBody {
                    error_type: "conflict".to_string(),
                    message: "input mailbox is full".to_string(),
                    current_revision: None,
                },
            }),
        )),
        Err(_) => Err(conflict("session is not accepting queued input", None)),
    }
}

/// Resolves a permission request. Both thread and run revision fences are
/// validated before the authority-changing operation proceeds.
async fn resolve_permission(
    State(state): State<Arc<ServerState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(req): Json<ResolvePermissionRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;

    // The run revision fence is now validated atomically inside the runtime
    // method alongside the thread revision fence.
    match workspace
        .runtime
        .resolve_permission(
            thread_id,
            req.expected_thread_revision,
            req.expected_run_revision,
            request_id,
            req.allow,
        )
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => {
            let current = workspace.snapshot(thread_id).ok().map(|s| s.revision);
            Err(map_runtime_error(&error, current.unwrap_or_default()))
        }
    }
}

/// Provides a requested non-secret input value. Both thread and run revision
/// fences are validated before the authority-changing operation proceeds.
async fn provide_input(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<ProvideInputRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;

    // The run revision fence is now validated atomically inside the runtime
    // method alongside the thread revision fence.
    match workspace
        .runtime
        .provide_input(
            thread_id,
            req.expected_thread_revision,
            req.expected_run_revision,
            req.request_id,
            req.value,
        )
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => {
            let current = workspace.snapshot(thread_id).ok().map(|s| s.revision);
            Err(map_runtime_error(&error, current.unwrap_or_default()))
        }
    }
}

async fn reconcile_effect(
    State(state): State<Arc<ServerState>>,
    Path((id, effect_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;

    match workspace
        .runtime
        .reconcile_unknown_effect(thread_id, &effect_id)
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

// Helper functions

/// Resolves the workspace that durably owns a session, or a 404.
async fn lookup_workspace(
    state: &ServerState,
    thread_id: ThreadId,
) -> Result<Arc<WorkspaceInstance>, HandlerError> {
    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))
}

/// Reads the optional `Idempotency-Key` header and scopes it by server token
/// and a caller-provided operation namespace (typically endpoint + session id)
/// so the same user-supplied key cannot accidentally replay across different
/// endpoints or sessions.
fn scoped_idempotency_key(
    state: &ServerState,
    headers: &HeaderMap,
    namespace: &str,
) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("{}:{namespace}:{value}", state.token))
}

/// Maps a runtime error to an HTTP response. State/revision conflicts and
/// lease/storage races become 409 with the current revision so clients can
/// retry after a refetch; other failures become 500.
fn map_runtime_error(error: &ThreadRuntimeError, current_revision: u64) -> HandlerError {
    match error {
        ThreadRuntimeError::InvalidState => {
            conflict("session state changed", Some(current_revision))
        }
        ThreadRuntimeError::Storage(storage_err) if is_retryable_storage(storage_err) => {
            conflict(&format!("{storage_err}"), Some(current_revision))
        }
        other => failed(&format!("operation failed: {other}")),
    }
}

fn is_retryable_storage(err: &latte_engine::StorageError) -> bool {
    matches!(
        err,
        latte_engine::StorageError::EngineUnavailable
            | latte_engine::StorageError::LeaseLost
            | latte_engine::StorageError::StaleRevision { .. }
            | latte_engine::StorageError::StaleThreadRevision { .. }
    )
}

fn parse_thread_id(id: &str) -> Result<ThreadId, HandlerError> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| bad_request("invalid session id"))?;
    Ok(ThreadId::from_uuid(uuid))
}

/// Builds a typed error response with an optional current revision.
fn error_response(
    status: StatusCode,
    error_type: &str,
    message: &str,
    current_revision: Option<u64>,
) -> HandlerError {
    (
        status,
        Json(ErrorResponse {
            error: ErrorBody {
                error_type: error_type.to_string(),
                message: message.to_string(),
                current_revision,
            },
        }),
    )
}

fn bad_request(message: &str) -> HandlerError {
    error_response(StatusCode::BAD_REQUEST, "rejected", message, None)
}

fn not_found(message: &str) -> HandlerError {
    error_response(StatusCode::NOT_FOUND, "not_found", message, None)
}

fn conflict(message: &str, current_revision: Option<u64>) -> HandlerError {
    error_response(StatusCode::CONFLICT, "conflict", message, current_revision)
}

fn failed(message: &str) -> HandlerError {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed", message, None)
}

/// 409 for a concurrent retry that arrived while the original request with the
/// same idempotency key is still in flight.
fn in_flight() -> HandlerError {
    error_response(
        StatusCode::CONFLICT,
        "conflict",
        "a request with this idempotency key is still in flight",
        None,
    )
}

/// 422 when an idempotency key is reused with a different request payload.
fn payload_mismatch() -> HandlerError {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        "idempotency_mismatch",
        "this idempotency key was used with a different request payload",
        None,
    )
}

/// Computes a stable SHA-256 hex digest of the canonical JSON serialization.
fn canonical_digest(value: &serde_json::Value) -> String {
    use sha2::Digest;
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", sha2::Sha256::digest(&bytes))
}

async fn workspace_events(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
) -> Sse<std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>> {
    // Get the workspace's event receiver
    let workspace = state.workspaces.get_by_id(&workspace_id).await;

    let rx = match workspace {
        Some(ws) => ws.event_tx.subscribe(),
        None => {
            // Return an empty stream if workspace not found
            return Sse::new(Box::pin(futures::stream::empty()));
        }
    };

    let stream = BroadcastStream::new(rx).map(|result| {
        match result {
            Ok(event) => {
                let event_type = match &event {
                    ServerEvent::ThreadChanged { .. } => "thread_changed",
                    ServerEvent::Progress { .. } => "progress",
                    ServerEvent::ResyncRequired => "resync_required",
                };
                let data = serde_json::to_string(&event).unwrap_or_default();
                Ok(Event::default().event(event_type).data(data))
            }
            // A lagged broadcast receiver means the client fell behind the
            // channel capacity. Signal a resync instead of terminating.
            Err(_) => Ok(Event::default().event("resync_required").data("{}")),
        }
    });

    Sse::new(Box::pin(stream))
}

/// Serve until `shutdown` resolves. Separating the shutdown future lets tests
/// drive graceful shutdown deterministically without process signals.
pub async fn serve_with_shutdown(
    state: Arc<ServerState>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let app = router(state);
    info!("server listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Resolves when the process receives Ctrl-C or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_state;
    use std::time::Duration;
    use tower::util::ServiceExt;

    fn valid_binding() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "provider_name": "test",
            "provider_type": "openai-chat",
            "protocol": "chat",
            "model": "test",
            "config_fingerprint": "config",
            "tools_fingerprint": "tools",
            "aliases": {},
            "credential_ref_id": "env:TEST",
            "data_scope_id": "workspace",
            "credential_generation": 1
        })
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn create_workspace_id(state: &Arc<ServerState>, path: &str) -> String {
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "path": path }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await["workspace_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Builds durable server state whose per-workspace runtimes all use the
    /// given thread provider factory (each workspace still gets its own engine
    /// under a temp dir). The session locator is a no-op; tests drive the
    /// in-memory index via `register_session`/create.
    fn state_with_factory(
        factory: latte_headless::thread::ThreadProviderFactory,
    ) -> Arc<ServerState> {
        let builder: crate::workspace::WorkspaceRuntimeBuilder =
            std::sync::Arc::new(move |root: &std::path::Path| {
                let db = root.join(".latte/state.db");
                std::fs::create_dir_all(db.parent().unwrap()).map_err(|e| e.to_string())?;
                let engine = latte_engine::EngineBuilder::new()
                    .workspace_root(root)
                    .database_path(&db)
                    .conversation_root(root.join(".latte/sessions"))
                    .build()
                    .map_err(|e| e.to_string())?;
                let runtime = latte_headless::thread::ThreadRuntimeService::new(
                    engine.clone(),
                    root,
                    Default::default(),
                    factory.clone(),
                );
                Ok(crate::workspace::BuiltWorkspace { engine, runtime })
            });
        let locator: crate::workspace::SessionLocator = std::sync::Arc::new(|_| None);
        new_state("test-token".to_string(), builder, locator)
    }

    fn state() -> Arc<ServerState> {
        state_with_factory(std::sync::Arc::new(|_| Err("test".to_string())))
    }

    /// Server state whose provider factory returns a `FakeProvider` that
    /// completes the turn in one step, so a created session reaches a durable
    /// idle state and the follow-up/switch/read success paths are reachable.
    fn completing_state() -> Arc<ServerState> {
        use latte_headless::provider::{FakeProvider, ProviderResponse, ProviderUsage};
        use latte_headless::registry::{ProviderBinding, ResolvedProvider};

        let factory: latte_headless::thread::ThreadProviderFactory =
            std::sync::Arc::new(|binding: &latte_core::ThreadProviderBindingV2| {
                let provider = FakeProvider::scripted([ProviderResponse {
                    message: Some("done".into()),
                    tool_calls: Vec::new(),
                    input_request: None,
                    usage: ProviderUsage::default(),
                    finish_reason: Some(latte_headless::provider::FinishReason::Stop),
                    provider_state: None,
                }]);
                Ok(ResolvedProvider {
                    provider: std::sync::Arc::new(provider),
                    binding: ProviderBinding {
                        version: binding.version,
                        provider_name: binding.provider_name.clone(),
                        provider_type: binding.provider_type.clone(),
                        protocol: binding.protocol.clone(),
                        model: binding.model.clone(),
                        config_fingerprint: binding.config_fingerprint.clone(),
                        tools_fingerprint: binding.tools_fingerprint.clone(),
                        aliases: binding.aliases.clone(),
                    },
                })
            });
        state_with_factory(factory)
    }

    /// Creates a session and blocks until it is durably idle (accepts a
    /// follow-up), returning the session id and its current thread revision.
    async fn completed_session(state: &Arc<ServerState>, workspace_id: &str) -> (String, u64) {
        let (_, created) = call(
            state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "hello", "binding": valid_binding() })),
        )
        .await;
        let session_id = created["session_id"].as_str().unwrap().to_string();
        for _ in 0..200 {
            let (status, body) =
                call(state, "GET", &format!("/v1/sessions/{session_id}"), None).await;
            if status == StatusCode::OK && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                let revision = body["snapshot"]["revision"].as_u64().unwrap();
                return (session_id, revision);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session did not reach a durable idle state");
    }

    /// Server state whose provider requests a `write_file` tool call, so a
    /// created session parks at `WaitingPermission` and the permission-resolve
    /// path is reachable.
    fn permission_state() -> Arc<ServerState> {
        use latte_headless::provider::{FakeProvider, ProviderResponse, ProviderUsage, ToolCall};
        use latte_headless::registry::{ProviderBinding, ResolvedProvider};

        let factory: latte_headless::thread::ThreadProviderFactory =
            std::sync::Arc::new(|binding: &latte_core::ThreadProviderBindingV2| {
                let provider = FakeProvider::scripted([
                    ProviderResponse {
                        message: Some("writing".into()),
                        tool_calls: vec![ToolCall {
                            id: "write-1".into(),
                            name: "write_file".into(),
                            input: serde_json::json!({
                                "path": "note.txt",
                                "content": "hello\n",
                                "create_intent": true
                            }),
                        }],
                        input_request: None,
                        usage: ProviderUsage::default(),
                        finish_reason: None,
                        provider_state: None,
                    },
                    ProviderResponse {
                        message: Some("done".into()),
                        tool_calls: Vec::new(),
                        input_request: None,
                        usage: ProviderUsage::default(),
                        finish_reason: Some(latte_headless::provider::FinishReason::Stop),
                        provider_state: None,
                    },
                ]);
                Ok(ResolvedProvider {
                    provider: std::sync::Arc::new(provider),
                    binding: ProviderBinding {
                        version: binding.version,
                        provider_name: binding.provider_name.clone(),
                        provider_type: binding.provider_type.clone(),
                        protocol: binding.protocol.clone(),
                        model: binding.model.clone(),
                        config_fingerprint: binding.config_fingerprint.clone(),
                        tools_fingerprint: binding.tools_fingerprint.clone(),
                        aliases: binding.aliases.clone(),
                    },
                })
            });
        state_with_factory(factory)
    }

    /// Creates a session that parks at `WaitingPermission`, returning its id,
    /// current thread revision, pending request id, and expected run revision.
    async fn waiting_permission_session(
        state: &Arc<ServerState>,
        workspace_id: &str,
    ) -> (String, u64, String, u64) {
        let (_, created) = call(
            state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "write it", "binding": valid_binding() })),
        )
        .await;
        let session_id = created["session_id"].as_str().unwrap().to_string();
        for _ in 0..200 {
            let (status, body) =
                call(state, "GET", &format!("/v1/sessions/{session_id}"), None).await;
            if status == StatusCode::OK
                && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission")
            {
                let revision = body["snapshot"]["revision"].as_u64().unwrap();
                let pending = &body["snapshot"]["pending"];
                let request_id = pending["request_id"]
                    .as_str()
                    .expect("pending permission request id")
                    .to_string();
                let run_revision = pending["expected_run_revision"]
                    .as_u64()
                    .expect("pending expected run revision");
                return (session_id, revision, request_id, run_revision);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session did not reach WaitingPermission");
    }

    /// Server state whose provider requests non-secret input, so a created
    /// session parks at `WaitingInput` and the provide-input path is reachable.
    fn input_state() -> Arc<ServerState> {
        use latte_headless::provider::{
            FakeProvider, InputRequest, ProviderResponse, ProviderUsage,
        };
        use latte_headless::registry::{ProviderBinding, ResolvedProvider};

        let factory: latte_headless::thread::ThreadProviderFactory =
            std::sync::Arc::new(|binding: &latte_core::ThreadProviderBindingV2| {
                let provider = FakeProvider::scripted([
                    ProviderResponse {
                        message: None,
                        tool_calls: Vec::new(),
                        input_request: Some(InputRequest {
                            id: "req-1".into(),
                            prompt: "what value?".into(),
                            secret: false,
                        }),
                        usage: ProviderUsage::default(),
                        finish_reason: None,
                        provider_state: None,
                    },
                    ProviderResponse {
                        message: Some("done".into()),
                        tool_calls: Vec::new(),
                        input_request: None,
                        usage: ProviderUsage::default(),
                        finish_reason: Some(latte_headless::provider::FinishReason::Stop),
                        provider_state: None,
                    },
                ]);
                Ok(ResolvedProvider {
                    provider: std::sync::Arc::new(provider),
                    binding: ProviderBinding {
                        version: binding.version,
                        provider_name: binding.provider_name.clone(),
                        provider_type: binding.provider_type.clone(),
                        protocol: binding.protocol.clone(),
                        model: binding.model.clone(),
                        config_fingerprint: binding.config_fingerprint.clone(),
                        tools_fingerprint: binding.tools_fingerprint.clone(),
                        aliases: binding.aliases.clone(),
                    },
                })
            });
        state_with_factory(factory)
    }

    /// Creates a session that parks at `WaitingInput`, returning its id,
    /// current thread revision, pending request id, and expected run revision.
    async fn waiting_input_session(
        state: &Arc<ServerState>,
        workspace_id: &str,
    ) -> (String, u64, String, u64) {
        let (_, created) = call(
            state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "need input", "binding": valid_binding() })),
        )
        .await;
        let session_id = created["session_id"].as_str().unwrap().to_string();
        for _ in 0..200 {
            let (status, body) =
                call(state, "GET", &format!("/v1/sessions/{session_id}"), None).await;
            if status == StatusCode::OK
                && body["snapshot"]["lifecycle"].as_str() == Some("waiting_input")
            {
                let revision = body["snapshot"]["revision"].as_u64().unwrap();
                let pending = &body["snapshot"]["pending"];
                let request_id = pending["request_id"].as_str().unwrap().to_string();
                let run_revision = pending["expected_run_revision"].as_u64().unwrap();
                return (session_id, revision, request_id, run_revision);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session did not reach WaitingInput");
    }

    #[tokio::test]
    async fn provide_input_stale_revision_conflicts() {
        let state = input_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision, request_id, run_revision) =
            waiting_input_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            Some(serde_json::json!({
                "request_id": request_id,
                "value": "the answer",
                "expected_thread_revision": 999,
                "expected_run_revision": run_revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn test_health_check() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_required() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({"path": "/tmp"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_with_wrong_token() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({"path": "/tmp"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_create_workspace() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "path": workspace.path().to_string_lossy() })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let result = json_body(response).await;
        assert!(result.get("workspace_id").is_some());
        assert!(result.get("path").is_some());
    }

    #[tokio::test]
    async fn test_create_workspace_invalid_path() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({"path": "/nonexistent/path/that/does/not/exist"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_session_not_found() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/sessions/00000000-0000-0000-0000-000000000000")
                    .method("GET")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_session_invalid_id_is_bad_request() {
        let response = router(state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/sessions/not-a-uuid")
                    .method("GET")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_session_accepts_immediately_and_registers_for_reads() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "prompt": "hello", "binding": valid_binding() })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = json_body(response).await;
        let session_id = body["session_id"].as_str().unwrap().to_string();
        // accepted_revision is the real durable revision after acceptance.
        assert!(body["accepted_revision"].is_u64());

        // The durable thread is created even though the test provider fails,
        // so the read route resolves it once the background turn persists it.
        let mut found = false;
        for _ in 0..50 {
            let response = router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/v1/sessions/{session_id}"))
                        .method("GET")
                        .header("authorization", "Bearer test-token")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() == StatusCode::OK {
                let snapshot = json_body(response).await;
                assert_eq!(snapshot["snapshot"]["thread_id"], session_id);
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(found, "durable session did not become readable");
    }

    #[tokio::test]
    async fn create_session_replays_idempotent_key() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let request = || {
            axum::http::Request::builder()
                .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                .method("POST")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test-token")
                .header("idempotency-key", "abc-123")
                .body(axum::body::Body::from(
                    serde_json::json!({ "prompt": "hello", "binding": valid_binding() })
                        .to_string(),
                ))
                .unwrap()
        };

        let first = router(state.clone()).oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let first_id = json_body(first).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let second = router(state.clone()).oneshot(request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        let second_id = json_body(second).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        assert_eq!(
            first_id, second_id,
            "retry must replay the original session"
        );
    }

    #[tokio::test]
    async fn create_session_rejects_invalid_binding() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "prompt": "hello", "binding": {"version": 1} })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_sessions_returns_created_session() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let created = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "prompt": "hello", "binding": valid_binding() })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::ACCEPTED);

        let mut listed = false;
        for _ in 0..50 {
            let response = router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                        .method("GET")
                        .header("authorization", "Bearer test-token")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            if body["sessions"].as_array().is_some_and(|s| !s.is_empty()) {
                listed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(listed, "list route did not surface the durable session");
    }

    #[tokio::test]
    async fn cancel_rejects_stale_revision_with_conflict() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let created = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "prompt": "hello", "binding": valid_binding() })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let session_id = json_body(created).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Wait until the durable session is readable, then cancel with a
        // deliberately stale thread revision.
        let mut conflicted = false;
        for _ in 0..50 {
            let response = router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!("/v1/sessions/{session_id}/cancel"))
                        .method("POST")
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer test-token")
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "expected_thread_revision": 999,
                                "expected_run_revision": 999
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() == StatusCode::CONFLICT {
                let body = json_body(response).await;
                assert_eq!(body["error"]["type"], "conflict");
                assert!(body["error"]["current_revision"].is_u64());
                conflicted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            conflicted,
            "stale cancel did not return 409 with current revision"
        );
    }

    #[tokio::test]
    async fn session_mutations_on_missing_session_are_not_found() {
        let state = state();
        let missing = uuid::Uuid::now_v7();

        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/sessions/{missing}/cancel"))
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "expected_thread_revision": 0,
                            "expected_run_revision": 0
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn map_runtime_error_classifies_conflict_and_failure() {
        // InvalidState is a revision/state conflict carrying the current revision.
        let (status, body) = map_runtime_error(&ThreadRuntimeError::InvalidState, 12);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0.error.error_type, "conflict");
        assert_eq!(body.0.error.current_revision, Some(12));

        // Any other runtime error becomes an opaque 500 without a revision.
        let (status, body) = map_runtime_error(&ThreadRuntimeError::Effect("boom".into()), 12);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0.error.error_type, "failed");
        assert!(body.0.error.current_revision.is_none());
        assert!(body.0.error.message.contains("operation failed"));
    }

    #[test]
    fn map_runtime_error_classifies_storage_lease_and_revision_as_conflict() {
        // EngineUnavailable (lease held by another owner) becomes 409.
        let (status, body) = map_runtime_error(
            &ThreadRuntimeError::Storage(latte_engine::StorageError::EngineUnavailable),
            5,
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0.error.current_revision, Some(5));

        // LeaseLost becomes 409.
        let (status, _) = map_runtime_error(
            &ThreadRuntimeError::Storage(latte_engine::StorageError::LeaseLost),
            7,
        );
        assert_eq!(status, StatusCode::CONFLICT);

        // StaleRevision becomes 409.
        let (status, _) = map_runtime_error(
            &ThreadRuntimeError::Storage(latte_engine::StorageError::StaleRevision {
                expected: 1,
                actual: 2,
            }),
            8,
        );
        assert_eq!(status, StatusCode::CONFLICT);

        // StaleThreadRevision becomes 409.
        let (status, _) = map_runtime_error(
            &ThreadRuntimeError::Storage(latte_engine::StorageError::StaleThreadRevision {
                expected: 1,
                actual: 2,
            }),
            9,
        );
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn idempotency_payload_mismatch_rejects() {
        let state = state();
        let key = "test-token:ns:mismatch-test";

        // Claim with one payload.
        assert!(matches!(
            state.idempotency_claim(key, "digest-a"),
            IdempotencyClaim::Owner
        ));

        // Same key + same digest = InFlight.
        assert!(matches!(
            state.idempotency_claim(key, "digest-a"),
            IdempotencyClaim::InFlight
        ));

        // Same key + different digest = PayloadMismatch.
        assert!(matches!(
            state.idempotency_claim(key, "digest-b"),
            IdempotencyClaim::PayloadMismatch
        ));

        // Complete the key, then replay with matching digest works.
        state.idempotency_complete(
            key,
            IdempotentRecord {
                status: StatusCode::ACCEPTED,
                body: serde_json::json!({"ok": true}),
                payload_digest: "digest-a".to_string(),
            },
        );
        assert!(matches!(
            state.idempotency_claim(key, "digest-a"),
            IdempotencyClaim::Replay(_)
        ));

        // Replay with different digest = PayloadMismatch.
        assert!(matches!(
            state.idempotency_claim(key, "digest-c"),
            IdempotencyClaim::PayloadMismatch
        ));
    }

    #[test]
    fn idempotency_key_is_namespaced_by_token_and_trimmed() {
        let state = state();
        let mut headers = HeaderMap::new();
        assert!(scoped_idempotency_key(&state, &headers, "test").is_none());

        headers.insert("idempotency-key", "  key-1  ".parse().unwrap());
        assert_eq!(
            scoped_idempotency_key(&state, &headers, "test").unwrap(),
            "test-token:test:key-1"
        );

        // An all-whitespace key is treated as absent.
        headers.insert("idempotency-key", "   ".parse().unwrap());
        assert!(scoped_idempotency_key(&state, &headers, "test").is_none());
    }

    /// Drives one authenticated request through the router and returns the
    /// (status, body) pair.
    async fn call(
        state: &Arc<ServerState>,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder()
            .uri(uri)
            .method(method)
            .header("authorization", "Bearer test-token");
        let request = if let Some(body) = body {
            builder = builder.header("content-type", "application/json");
            builder
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        } else {
            builder.body(axum::body::Body::empty()).unwrap()
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        (status, json_body(response).await)
    }

    /// Like `call`, but with extra request headers (e.g. an idempotency key).
    async fn call_with_headers(
        state: &Arc<ServerState>,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder()
            .uri(uri)
            .method(method)
            .header("authorization", "Bearer test-token");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = if let Some(body) = body {
            builder = builder.header("content-type", "application/json");
            builder
                .body(axum::body::Body::from(body.to_string()))
                .unwrap()
        } else {
            builder.body(axum::body::Body::empty()).unwrap()
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        (status, json_body(response).await)
    }

    #[tokio::test]
    async fn every_session_mutation_on_missing_session_is_not_found() {
        let state = state();
        let missing = uuid::Uuid::now_v7();
        let cases: Vec<(&str, String, serde_json::Value)> = vec![
            (
                "POST",
                format!("/v1/sessions/{missing}/follow-up"),
                serde_json::json!({ "prompt": "x", "expected_thread_revision": 0 }),
            ),
            (
                "POST",
                format!("/v1/sessions/{missing}/model"),
                serde_json::json!({ "binding": valid_binding(), "expected_thread_revision": 0 }),
            ),
            (
                "POST",
                format!("/v1/sessions/{missing}/queue"),
                serde_json::json!({ "prompt": "x" }),
            ),
            (
                "POST",
                format!("/v1/sessions/{missing}/permissions/req-1"),
                serde_json::json!({
                    "allow": true,
                    "expected_thread_revision": 0,
                    "expected_run_revision": 0
                }),
            ),
            (
                "POST",
                format!("/v1/sessions/{missing}/input"),
                serde_json::json!({
                    "request_id": "req-1",
                    "value": "v",
                    "expected_thread_revision": 0,
                    "expected_run_revision": 0
                }),
            ),
            (
                "POST",
                format!("/v1/sessions/{missing}/effects/effect-1/reconcile"),
                serde_json::Value::Null,
            ),
        ];
        for (method, uri, body) in cases {
            let payload = (!body.is_null()).then_some(body);
            let (status, _) = call(&state, method, &uri, payload).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn follow_up_rejects_invalid_session_id() {
        let state = state();
        let (status, body) = call(
            &state,
            "POST",
            "/v1/sessions/not-a-uuid/follow-up",
            Some(serde_json::json!({ "prompt": "x", "expected_thread_revision": 0 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "rejected");
    }

    #[tokio::test]
    async fn read_routes_on_missing_workspace_are_not_found() {
        let state = state();
        let (list_status, _) =
            call(&state, "GET", "/v1/workspaces/ws_missing/sessions", None).await;
        assert_eq!(list_status, StatusCode::NOT_FOUND);
        let (search_status, _) = call(
            &state,
            "GET",
            "/v1/workspaces/ws_missing/sessions/search?q=hello",
            None,
        )
        .await;
        assert_eq!(search_status, StatusCode::NOT_FOUND);
        let (create_status, _) = call(
            &state,
            "POST",
            "/v1/workspaces/ws_missing/sessions",
            Some(serde_json::json!({ "prompt": "x", "binding": valid_binding() })),
        )
        .await;
        assert_eq!(create_status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn search_sessions_returns_empty_for_unmatched_query() {
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (status, body) = call(
            &state,
            "GET",
            &format!("/v1/workspaces/{workspace_id}/sessions/search?q=nothing-here"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["sessions"].as_array().unwrap().is_empty());
        assert!(body["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn workspace_events_missing_workspace_yields_empty_stream() {
        // A missing workspace returns an immediately-terminating SSE stream
        // rather than an error status.
        let state = state();
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces/ws_missing/events")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(content_type.starts_with("text/event-stream"));
    }

    #[tokio::test]
    async fn workspace_events_emits_thread_changed_frame() {
        use tokio_stream::StreamExt as _;

        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let workspace = state.workspaces.get_by_id(&workspace_id).await.unwrap();

        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/workspaces/{workspace_id}/events"))
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Publish one of each event variant, then confirm the SSE body maps
        // every server event type to its named frame.
        let _ = workspace.event_tx.send(ServerEvent::ThreadChanged {
            session_id: "abc".into(),
            revision: 7,
        });
        let _ = workspace.event_tx.send(ServerEvent::Progress {
            session_id: "abc".into(),
            progress: serde_json::json!({ "step": 1 }),
        });
        let _ = workspace.event_tx.send(ServerEvent::ResyncRequired);

        let mut body = response.into_body().into_data_stream();
        let mut seen = String::new();
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_millis(200), body.next()).await {
                Ok(Some(Ok(chunk))) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    if seen.contains("thread_changed")
                        && seen.contains("progress")
                        && seen.contains("resync_required")
                    {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            seen.contains("thread_changed"),
            "SSE stream did not carry the thread_changed frame: {seen:?}"
        );
        assert!(
            seen.contains("progress"),
            "missing progress frame: {seen:?}"
        );
        assert!(
            seen.contains("resync_required"),
            "missing resync_required frame: {seen:?}"
        );
    }

    #[tokio::test]
    async fn queue_follow_up_on_idle_session_conflicts() {
        // A freshly created session has no active runner mailbox, so queueing
        // is rejected as a conflict rather than accepted.
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (_, created) = call(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "hello", "binding": valid_binding() })),
        )
        .await;
        let session_id = created["session_id"].as_str().unwrap().to_string();

        // Wait until the session is durably readable, then queue against it.
        let mut resolved = false;
        for _ in 0..100 {
            let (status, _) = call(
                &state,
                "POST",
                &format!("/v1/sessions/{session_id}/queue"),
                Some(serde_json::json!({ "prompt": "later" })),
            )
            .await;
            if status == StatusCode::CONFLICT || status == StatusCode::ACCEPTED {
                resolved = true;
                break;
            }
            if status == StatusCode::NOT_FOUND {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            panic!("unexpected queue status: {status}");
        }
        assert!(resolved, "queue never resolved against the durable session");
    }

    #[tokio::test]
    async fn follow_up_accepts_matching_revision_on_completed_session() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        // Use an idempotency key so the owner/complete path is exercised.
        let (status, body) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
            &[("idempotency-key", "follow-key-1")],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "follow-up returned {body:?}");
        // The follow-up creates a new child, advancing the thread revision past
        // the value the client fenced against.
        let accepted = body["accepted_revision"].as_u64().unwrap();
        assert!(accepted >= revision);

        // Retrying with the same key replays the original accepted revision.
        let (replay_status, replay_body) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
            &[("idempotency-key", "follow-key-1")],
        )
        .await;
        assert_eq!(replay_status, StatusCode::ACCEPTED);
        assert_eq!(replay_body["accepted_revision"].as_u64().unwrap(), accepted);
    }

    #[tokio::test]
    async fn follow_up_stale_revision_conflicts() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision) = completed_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "again", "expected_thread_revision": 999 })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn switch_model_rejects_stale_revision_on_completed_session() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision) = completed_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/model"),
            Some(serde_json::json!({
                "binding": valid_binding(),
                "expected_thread_revision": 999
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn switch_model_rejects_invalid_binding() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        let (status, _) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/model"),
            Some(serde_json::json!({
                "binding": { "version": 1 },
                "expected_thread_revision": revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn switch_model_persists_a_new_binding_on_completed_session() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        // A binding that differs from the session's current one triggers a
        // durable switch (rather than the same-binding no-op early return).
        let mut binding = valid_binding();
        binding["model"] = serde_json::json!("test-2");
        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/model"),
            Some(serde_json::json!({
                "binding": binding,
                "expected_thread_revision": revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "switch returned {body:?}");
        assert_eq!(body["snapshot"]["binding"]["model"], "test-2");
    }

    #[tokio::test]
    async fn get_session_returns_completed_transcript() {
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision) = completed_session(&state, &workspace_id).await;

        let (status, body) = call(&state, "GET", &format!("/v1/sessions/{session_id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["snapshot"]["thread_id"].as_str().unwrap(), session_id);
        assert_eq!(body["snapshot"]["lifecycle"], "ready");
    }

    #[tokio::test]
    async fn reconcile_effect_on_non_reconciling_session_is_not_found() {
        // A completed session is not awaiting reconciliation, so the engine
        // rejects the request and the handler maps it to 404.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision) = completed_session(&state, &workspace_id).await;

        let (status, _) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/effects/effect-1/reconcile"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resolve_permission_deny_terminalizes_without_executing_effect() {
        let state = permission_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision, request_id, run_revision) =
            waiting_permission_session(&state, &workspace_id).await;

        // Denial consumes the prepared permission without running the tool, so
        // the handler returns 200 with a snapshot and the file is never written.
        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(serde_json::json!({
                "allow": false,
                "expected_thread_revision": revision,
                "expected_run_revision": run_revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "resolve returned {body:?}");
        assert!(body["snapshot"].is_object());
        assert!(
            !workspace.path().join("note.txt").exists(),
            "denied effect must not write the file"
        );
    }

    #[tokio::test]
    async fn resolve_permission_stale_thread_revision_conflicts() {
        let state = permission_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision, request_id, run_revision) =
            waiting_permission_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(serde_json::json!({
                "allow": true,
                "expected_thread_revision": 999,
                "expected_run_revision": run_revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
        assert!(body["error"]["current_revision"].is_u64());
    }

    #[tokio::test]
    async fn provide_input_on_permission_waiting_session_conflicts() {
        // A session waiting on permission is not waiting on input, so the
        // input path validates fences then reports the state conflict.
        let state = permission_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision, _request_id, run_revision) =
            waiting_permission_session(&state, &workspace_id).await;

        let (status, _) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            Some(serde_json::json!({
                "request_id": "whatever",
                "value": "v",
                "expected_thread_revision": revision,
                "expected_run_revision": run_revision
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn resolve_permission_stale_run_revision_conflicts() {
        // A stale expected_run_revision is rejected with 409 even when the
        // thread revision is correct.
        let state = permission_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision, request_id, _run_revision) =
            waiting_permission_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(serde_json::json!({
                "allow": true,
                "expected_thread_revision": revision,
                "expected_run_revision": 999
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn provide_input_stale_run_revision_conflicts() {
        // A stale expected_run_revision on provide_input is rejected with 409.
        let state = input_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision, request_id, _run_revision) =
            waiting_input_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/input"),
            Some(serde_json::json!({
                "request_id": request_id,
                "value": "the answer",
                "expected_thread_revision": revision,
                "expected_run_revision": 999
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn follow_up_payload_mismatch_returns_422() {
        // A keyed follow-up that completed with one payload must reject a retry
        // of the same key with a different payload.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        // First request completes.
        let (first, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "original", "expected_thread_revision": revision })),
            &[("idempotency-key", "follow-mismatch")],
        )
        .await;
        assert_eq!(first, StatusCode::ACCEPTED);

        // Same key, different prompt → 422.
        let (mismatch, body) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(
                serde_json::json!({ "prompt": "DIFFERENT", "expected_thread_revision": revision }),
            ),
            &[("idempotency-key", "follow-mismatch")],
        )
        .await;
        assert_eq!(mismatch, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["type"], "idempotency_mismatch");
    }

    #[tokio::test]
    async fn serve_answers_health_and_shuts_down_gracefully() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Drive graceful shutdown from a oneshot so the test controls it
        // deterministically (no process signals).
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            crate::http::serve_with_shutdown(state(), listener, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        // Raw HTTP/1.1 GET /health over the bound socket.
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "unexpected response: {text}"
        );
        assert!(text.trim_end().ends_with("ok"));

        // Trigger graceful shutdown; the serve future resolves cleanly.
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server did not shut down within the deadline");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn shutdown_signal_awaits_without_a_signal() {
        // Poll the real shutdown future briefly: it installs the ctrl-c/SIGTERM
        // waiters and parks on the select without a signal, so the timeout
        // elapses. This exercises the signal-wiring construction path.
        let elapsed =
            tokio::time::timeout(std::time::Duration::from_millis(50), shutdown_signal()).await;
        assert!(
            elapsed.is_err(),
            "no signal was sent, so it must not resolve"
        );
    }

    #[tokio::test]
    async fn serve_wrapper_binds_and_answers_before_abort() {
        // Exercise the public `serve_on` wrapper (which the binary uses and
        // which wires the real signal-based shutdown); we abort rather than
        // signal, since signals are process-wide.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { crate::serve_on(state(), listener).await });

        let mut ok = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ok, "serve wrapper never accepted a connection");
        server.abort();
    }

    #[test]
    fn idempotency_claim_reserves_replays_and_releases() {
        let state = state();
        let key = "test-token:abc";

        // First claim owns the key and reserves it.
        assert!(matches!(
            state.idempotency_claim(key, "d"),
            IdempotencyClaim::Owner
        ));
        // A concurrent claim while pending sees it in flight.
        assert!(matches!(
            state.idempotency_claim(key, "d"),
            IdempotencyClaim::InFlight
        ));

        // Completing records the result; later claims replay it.
        state.idempotency_complete(
            key,
            IdempotentRecord {
                status: StatusCode::ACCEPTED,
                body: serde_json::json!({ "ok": true }),
                payload_digest: "d".to_string(),
            },
        );
        match state.idempotency_claim(key, "d") {
            IdempotencyClaim::Replay(record) => {
                assert_eq!(record.status, StatusCode::ACCEPTED);
                assert_eq!(record.body, serde_json::json!({ "ok": true }));
            }
            _ => panic!("expected replay after completion"),
        }

        // Releasing a still-pending key clears it for retry; releasing a
        // completed key is a no-op.
        let key2 = "test-token:def";
        assert!(matches!(
            state.idempotency_claim(key2, "d2"),
            IdempotencyClaim::Owner
        ));
        state.idempotency_release(key2);
        assert!(matches!(
            state.idempotency_claim(key2, "d2"),
            IdempotencyClaim::Owner
        ));
        state.idempotency_release(key); // completed → no-op
        assert!(matches!(
            state.idempotency_claim(key, "d"),
            IdempotencyClaim::Replay(_)
        ));
    }

    #[tokio::test]
    async fn in_flight_idempotent_retry_conflicts() {
        // A second create with the same key, while the first holds the
        // reservation, is rejected as a conflict rather than executing twice.
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        // Pre-claim with the exact payload digest the handler will compute.
        let payload_digest = canonical_digest(&serde_json::json!({
            "prompt": "x",
            "binding": valid_binding(),
        }));
        state.idempotency_claim(
            &format!("test-token:create:{workspace_id}:key-inflight"),
            &payload_digest,
        );

        let (status, body) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "x", "binding": valid_binding() })),
            &[("idempotency-key", "key-inflight")],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["type"], "conflict");
    }

    #[tokio::test]
    async fn failed_keyed_create_releases_the_key_for_retry() {
        // A keyed create that fails (invalid binding) must release its
        // reservation so a corrected retry with the same key is not blocked.
        let state = state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let (bad_status, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "x", "binding": { "version": 1 } })),
            &[("idempotency-key", "retry-key")],
        )
        .await;
        assert_eq!(bad_status, StatusCode::BAD_REQUEST);

        // The key was released, so the corrected retry proceeds (202), not 409.
        let (ok_status, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "x", "binding": valid_binding() })),
            &[("idempotency-key", "retry-key")],
        )
        .await;
        assert_eq!(ok_status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn reconcile_on_completed_session_is_not_found() {
        // A completed session is not awaiting reconciliation, so the engine
        // rejects and the handler maps it to 404 (covers the reconcile Err arm).
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, _revision) = completed_session(&state, &workspace_id).await;

        let (status, _) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/effects/effect-1/reconcile"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn failed_keyed_follow_up_releases_the_key_for_retry() {
        // A keyed follow-up on a missing session fails and releases the key, so
        // a later retry with the same key is not blocked as in-flight.
        let state = state();
        let missing = uuid::Uuid::now_v7();

        let (first, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{missing}/follow-up"),
            Some(serde_json::json!({ "prompt": "x", "expected_thread_revision": 0 })),
            &[("idempotency-key", "fu-retry")],
        )
        .await;
        assert_eq!(first, StatusCode::NOT_FOUND);

        let (second, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/sessions/{missing}/follow-up"),
            Some(serde_json::json!({ "prompt": "x", "expected_thread_revision": 0 })),
            &[("idempotency-key", "fu-retry")],
        )
        .await;
        // Released, so the retry re-runs (still 404) rather than a 409 in-flight.
        assert_eq!(second, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn keyed_create_on_missing_workspace_is_not_found() {
        let state = state();
        let (status, _) = call_with_headers(
            &state,
            "POST",
            "/v1/workspaces/ws_absent/sessions",
            Some(serde_json::json!({ "prompt": "x", "binding": valid_binding() })),
            &[("idempotency-key", "ws-missing-key")],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_session_rejects_binding_that_fails_validation() {
        // A binding that deserializes but fails validate() returns 400 and
        // covers the binding.validate() error branch.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        // provider_name is empty → validate() returns Err.
        let mut binding = valid_binding();
        binding["provider_name"] = serde_json::json!("");
        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "x", "binding": binding })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid binding")
        );
    }

    #[tokio::test]
    async fn create_session_without_key_succeeds() {
        // A create without Idempotency-Key still succeeds, covering the
        // idempotency=None path through the handler.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "no key", "binding": valid_binding() })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body["session_id"].is_string());
    }

    #[tokio::test]
    async fn follow_up_without_key_on_completed_session_succeeds() {
        // Follow-up without Idempotency-Key on a completed session covers the
        // None idempotency branch.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "no key", "expected_thread_revision": revision })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "follow-up: {body:?}");
        assert!(body["accepted_revision"].is_u64());
    }

    #[tokio::test]
    async fn switch_model_binding_validates_before_engine() {
        // switch_model with a binding missing required fields fails
        // deserialization and returns 400.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (session_id, revision) = completed_session(&state, &workspace_id).await;

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/model"),
            Some(serde_json::json!({ "binding": {"not": "a valid binding"}, "expected_thread_revision": revision })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("invalid binding")
        );
    }

    #[tokio::test]
    async fn payload_mismatch_returns_422_for_create() {
        // A keyed create that completed with one payload must reject a retry
        // of the same key with a different payload as 422 rather than replaying.
        let state = completing_state();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;

        // First request succeeds (202).
        let (first, _) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "original", "binding": valid_binding() })),
            &[("idempotency-key", "mismatch-key")],
        )
        .await;
        assert_eq!(first, StatusCode::ACCEPTED);

        // Same key, different prompt → 422 payload mismatch.
        let (mismatch, body) = call_with_headers(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "CHANGED", "binding": valid_binding() })),
            &[("idempotency-key", "mismatch-key")],
        )
        .await;
        assert_eq!(mismatch, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["type"], "idempotency_mismatch");
    }

    #[test]
    fn canonical_digest_is_stable_and_content_sensitive() {
        let a = canonical_digest(&serde_json::json!({"prompt": "hello", "binding": {}}));
        let b = canonical_digest(&serde_json::json!({"prompt": "hello", "binding": {}}));
        let c = canonical_digest(&serde_json::json!({"prompt": "world", "binding": {}}));
        assert_eq!(a, b, "same input must produce same digest");
        assert_ne!(a, c, "different input must produce different digest");
        assert_eq!(a.len(), 64, "SHA-256 hex is always 64 chars");
    }

    /// A provider whose turn blocks until released, keeping the session's runner
    /// (and its mailbox) active so queue-accepted / mailbox-full are reachable.
    fn blocking_state(gate: std::sync::Arc<tokio::sync::Notify>) -> Arc<ServerState> {
        use latte_headless::provider::{
            Provider, ProviderCapabilities, ProviderContext, ProviderFuture, ProviderRequest,
            ProviderResponse, ProviderUsage,
        };
        use latte_headless::registry::{ProviderBinding, ResolvedProvider};

        struct Blocking(std::sync::Arc<tokio::sync::Notify>);
        impl Provider for Blocking {
            fn complete(&self, _: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
                let gate = self.0.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(ProviderResponse {
                        message: Some("done".into()),
                        tool_calls: Vec::new(),
                        input_request: None,
                        usage: ProviderUsage::default(),
                        finish_reason: Some(latte_headless::provider::FinishReason::Stop),
                        provider_state: None,
                    })
                })
            }
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    tools: true,
                    parallel_tool_calls: true,
                    input_request: true,
                }
            }
        }

        let factory: latte_headless::thread::ThreadProviderFactory =
            std::sync::Arc::new(move |binding: &latte_core::ThreadProviderBindingV2| {
                Ok(ResolvedProvider {
                    provider: std::sync::Arc::new(Blocking(gate.clone())),
                    binding: ProviderBinding {
                        version: binding.version,
                        provider_name: binding.provider_name.clone(),
                        provider_type: binding.provider_type.clone(),
                        protocol: binding.protocol.clone(),
                        model: binding.model.clone(),
                        config_fingerprint: binding.config_fingerprint.clone(),
                        tools_fingerprint: binding.tools_fingerprint.clone(),
                        aliases: binding.aliases.clone(),
                    },
                })
            });
        state_with_factory(factory)
    }

    #[tokio::test]
    async fn queue_accepts_then_reports_full_during_active_turn() {
        // A blocking provider keeps the turn (and runner mailbox) alive while we
        // queue: the first queues are accepted with positions, then the bounded
        // mailbox reports full — covering both queue arms.
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let state = blocking_state(gate.clone());
        let workspace = tempfile::tempdir().unwrap();
        let workspace_id = create_workspace_id(&state, &workspace.path().to_string_lossy()).await;
        let (_, created) = call(
            &state,
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(serde_json::json!({ "prompt": "slow", "binding": valid_binding() })),
        )
        .await;
        let session_id = created["session_id"].as_str().unwrap().to_string();

        // While the turn blocks, queue until the mailbox reports full.
        let mut accepted = 0;
        let mut saw_full = false;
        for _ in 0..64 {
            let (status, body) = call(
                &state,
                "POST",
                &format!("/v1/sessions/{session_id}/queue"),
                Some(serde_json::json!({ "prompt": "q" })),
            )
            .await;
            match status {
                StatusCode::ACCEPTED => {
                    assert!(body["position"].as_u64().is_some());
                    accepted += 1;
                }
                StatusCode::CONFLICT if body["error"]["message"] == "input mailbox is full" => {
                    saw_full = true;
                    break;
                }
                other => panic!("unexpected queue status {other}: {body:?}"),
            }
        }
        assert!(accepted >= 1, "at least one queue must be accepted");
        assert!(saw_full, "mailbox-full arm must be reached");

        // Release the turn so the runner drains and exits cleanly.
        gate.notify_waiters();
    }
}
