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
use latte_core::{ThreadId, ThreadSnapshot};
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
}

/// Server state shared across handlers.
pub struct ServerState {
    pub workspaces: Arc<WorkspaceManager>,
    pub event_tx: broadcast::Sender<ServerEvent>,
    pub token: String,
    /// Deduplicates durable mutations by `(token, idempotency_key)`. A retry
    /// after a timeout returns the original accepted result instead of
    /// starting a second session or duplicating provider/effect work.
    idempotency: Mutex<HashMap<String, IdempotentRecord>>,
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

    /// Returns a previously accepted result for this idempotency key, if any.
    fn idempotent_replay(&self, key: &str) -> Option<IdempotentRecord> {
        self.idempotency
            .lock()
            .expect("idempotency mutex poisoned")
            .get(key)
            .cloned()
    }

    /// Records the accepted result for an idempotency key for later replay.
    fn idempotent_store(&self, key: String, record: IdempotentRecord) {
        self.idempotency
            .lock()
            .expect("idempotency mutex poisoned")
            .insert(key, record);
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
    let idempotency = namespaced_idempotency_key(&state, &headers);
    if let Some(key) = &idempotency
        && let Some(record) = state.idempotent_replay(key)
    {
        return Ok((record.status, Json(record.body)));
    }

    let workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    // Parse and validate the complete non-secret binding before acceptance.
    let binding: latte_core::ThreadProviderBindingV2 = serde_json::from_value(req.binding)
        .map_err(|e| bad_request(&format!("invalid binding: {e}")))?;
    binding
        .validate()
        .map_err(|e| bad_request(&format!("invalid binding: {e}")))?;

    // Allocate the session id and register it synchronously so read routes and
    // SSE routing can observe it before the turn completes.
    let thread_id = ThreadId::from_uuid(uuid::Uuid::now_v7());
    state
        .workspaces
        .register_session(thread_id, workspace.path.clone())
        .await;

    // Run the provider turn under supervised background execution. The engine
    // persists the accepted user submission before provider construction, so a
    // credential/model failure becomes an observable child failure rather than
    // a lost prompt; the workspace event bridge forwards completion and error.
    let runtime = workspace.runtime.clone();
    let prompt = req.prompt;
    tokio::spawn(async move {
        if let Err(error) = runtime.start(thread_id, prompt, binding).await {
            warn!("session {thread_id} background turn failed: {error}");
        }
    });

    // Wake subscribers so they fetch the new session snapshot.
    let _ = workspace.event_tx.send(ServerEvent::ThreadChanged {
        session_id: thread_id.to_string(),
        revision: 0,
    });

    let body = serde_json::to_value(SessionCreatedResponse {
        session_id: thread_id.to_string(),
        accepted_revision: 0,
    })
    .expect("session response serializes");
    if let Some(key) = idempotency {
        state.idempotent_store(
            key,
            IdempotentRecord {
                status: StatusCode::ACCEPTED,
                body: body.clone(),
            },
        );
    }
    Ok((StatusCode::ACCEPTED, Json(body)))
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

/// Continues a session with a new user turn. Like create, this validates the
/// revision fence, returns 202, and runs the turn in the background; a durable
/// `Idempotency-Key` retry replays the original acceptance.
async fn follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), HandlerError> {
    let idempotency = namespaced_idempotency_key(&state, &headers);
    if let Some(key) = &idempotency
        && let Some(record) = state.idempotent_replay(key)
    {
        return Ok((record.status, Json(record.body)));
    }

    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    validate_fences(&snapshot, req.expected_thread_revision, None)?;

    let accepted_revision = snapshot.revision;
    let runtime = workspace.runtime.clone();
    let prompt = req.prompt;
    let expected = req.expected_thread_revision;
    tokio::spawn(async move {
        if let Err(error) = runtime.follow_up(thread_id, expected, prompt).await {
            warn!("session {thread_id} background follow-up failed: {error}");
        }
    });

    let body = serde_json::json!({ "accepted_revision": accepted_revision });
    if let Some(key) = idempotency {
        state.idempotent_store(
            key,
            IdempotentRecord {
                status: StatusCode::ACCEPTED,
                body: body.clone(),
            },
        );
    }
    Ok((StatusCode::ACCEPTED, Json(body)))
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
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    validate_fences(&snapshot, req.expected_thread_revision, None)?;

    match workspace
        .runtime
        .switch_model(thread_id, req.expected_thread_revision, &binding)
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => Err(map_runtime_error(&error, snapshot.revision)),
    }
}

/// Cancels an active session. Both advertised revision fences are validated
/// before the authority-changing mutation so a stale client cannot cancel a
/// newer run; a mismatch returns 409 with the current revision.
async fn cancel_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    validate_fences(
        &snapshot,
        req.expected_thread_revision,
        Some(req.expected_run_revision),
    )?;

    match workspace.runtime.cancel_durable(thread_id) {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => Err(map_runtime_error(&error, snapshot.revision)),
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

/// Resolves a permission request. Both revision fences are validated before the
/// approval consumes the prepared effect.
async fn resolve_permission(
    State(state): State<Arc<ServerState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(req): Json<ResolvePermissionRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    validate_fences(
        &snapshot,
        req.expected_thread_revision,
        Some(req.expected_run_revision),
    )?;

    match workspace
        .runtime
        .resolve_permission(
            thread_id,
            req.expected_thread_revision,
            request_id,
            req.allow,
        )
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => Err(map_runtime_error(&error, snapshot.revision)),
    }
}

/// Provides a requested non-secret input value. Both revision fences are
/// validated before the session continues the same child.
async fn provide_input(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<ProvideInputRequest>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let thread_id = parse_thread_id(&id)?;
    let workspace = lookup_workspace(&state, thread_id).await?;
    let snapshot = workspace
        .snapshot(thread_id)
        .map_err(|_| not_found("session not found"))?;
    validate_fences(
        &snapshot,
        req.expected_thread_revision,
        Some(req.expected_run_revision),
    )?;

    match workspace
        .runtime
        .provide_input(
            thread_id,
            req.expected_thread_revision,
            req.request_id,
            req.value,
        )
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(error) => Err(map_runtime_error(&error, snapshot.revision)),
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

/// Reads the optional `Idempotency-Key` header namespaced by the server token,
/// matching the documented `(token, idempotency_key)` dedup identity.
fn namespaced_idempotency_key(state: &ServerState, headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("{}:{value}", state.token))
}

/// Validates the client's revision fences against the current snapshot. A
/// mismatch returns 409 with the current thread revision so the client can
/// re-fetch and retry.
fn validate_fences(
    snapshot: &ThreadSnapshot,
    expected_thread_revision: u64,
    expected_run_revision: Option<u64>,
) -> Result<(), HandlerError> {
    if snapshot.revision != expected_thread_revision {
        return Err(conflict(
            "thread revision mismatch",
            Some(snapshot.revision),
        ));
    }
    if let Some(expected_run) = expected_run_revision {
        let current_run = snapshot
            .active_run_id
            .and_then(|run_id| snapshot.runs.iter().find(|run| run.run_id == run_id))
            .map(|run| run.run_revision);
        match current_run {
            Some(current) if current == expected_run => {}
            _ => return Err(conflict("run revision mismatch", Some(snapshot.revision))),
        }
    }
    Ok(())
}

/// Maps a runtime error to an HTTP response. A state/revision conflict becomes
/// 409 with the current revision; other failures become 500.
fn map_runtime_error(error: &ThreadRuntimeError, current_revision: u64) -> HandlerError {
    match error {
        ThreadRuntimeError::InvalidState => {
            conflict("session state changed", Some(current_revision))
        }
        other => failed(&format!("operation failed: {other}")),
    }
}

fn parse_thread_id(id: &str) -> Result<ThreadId, HandlerError> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| bad_request("invalid session id"))?;
    Ok(ThreadId::from_uuid(uuid))
}

fn bad_request(message: &str) -> HandlerError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: ErrorBody {
                error_type: "rejected".to_string(),
                message: message.to_string(),
                current_revision: None,
            },
        }),
    )
}

fn not_found(message: &str) -> HandlerError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: ErrorBody {
                error_type: "not_found".to_string(),
                message: message.to_string(),
                current_revision: None,
            },
        }),
    )
}

fn conflict(message: &str, current_revision: Option<u64>) -> HandlerError {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse {
            error: ErrorBody {
                error_type: "conflict".to_string(),
                message: message.to_string(),
                current_revision,
            },
        }),
    )
}

fn failed(message: &str) -> HandlerError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: ErrorBody {
                error_type: "failed".to_string(),
                message: message.to_string(),
                current_revision: None,
            },
        }),
    )
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

/// Serve the HTTP API on an already-bound listener.
///
/// Accepting a bound listener lets the caller discover the actual local
/// address (for example when binding to port 0) before serving begins.
/// Serving stops gracefully on Ctrl-C or (on Unix) SIGTERM, letting the
/// process flush and exit cleanly instead of being force-killed.
pub async fn serve(state: Arc<ServerState>, listener: tokio::net::TcpListener) -> Result<()> {
    let app = router(state);
    info!("server listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves when the process receives Ctrl-C or, on Unix, SIGTERM.
async fn shutdown_signal() {
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

    fn state() -> Arc<ServerState> {
        new_state(
            "test-token".to_string(),
            std::sync::Arc::new(|_| Err("test".to_string())),
        )
    }

    /// Server state whose provider factory returns a `FakeProvider` that
    /// completes the turn in one step, so a created session reaches a durable
    /// idle state and the follow-up/switch/read success paths are reachable.
    fn completing_state() -> Arc<ServerState> {
        use latte_headless::provider::{FakeProvider, ProviderResponse, ProviderUsage};
        use latte_headless::registry::{ProviderBinding, ResolvedProvider};

        let factory: crate::workspace::ProviderFactory =
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
        new_state("test-token".to_string(), factory)
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

        let factory: crate::workspace::ProviderFactory =
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
        new_state("test-token".to_string(), factory)
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

        let factory: crate::workspace::ProviderFactory =
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
        new_state("test-token".to_string(), factory)
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
        assert_eq!(body["accepted_revision"], 0);

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
    fn idempotency_key_is_namespaced_by_token_and_trimmed() {
        let state = state();
        let mut headers = HeaderMap::new();
        assert!(namespaced_idempotency_key(&state, &headers).is_none());

        headers.insert("idempotency-key", "  key-1  ".parse().unwrap());
        assert_eq!(
            namespaced_idempotency_key(&state, &headers).unwrap(),
            "test-token:key-1"
        );

        // An all-whitespace key is treated as absent.
        headers.insert("idempotency-key", "   ".parse().unwrap());
        assert!(namespaced_idempotency_key(&state, &headers).is_none());
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

        let (status, body) = call(
            &state,
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["accepted_revision"].as_u64().unwrap(), revision);
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
        assert!(body["error"]["current_revision"].is_u64());
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
    async fn serve_binds_a_listener_and_answers_health() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = crate::serve_on(state(), listener).await;
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

        server.abort();
    }
}
