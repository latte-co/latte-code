//! HTTP server with per-workspace event hubs.

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::info;

use crate::workspace::WorkspaceManager;

/// Server state shared across handlers.
pub struct ServerState {
    pub workspaces: Arc<WorkspaceManager>,
    pub event_tx: broadcast::Sender<ServerEvent>,
    pub token: String,
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
        .route("/v1/workspaces/{workspace_id}/sessions", post(create_session))
        .route("/v1/workspaces/{workspace_id}/sessions", get(list_sessions))
        .route("/v1/workspaces/{workspace_id}/sessions/search", get(search_sessions))
        .route("/v1/workspaces/{workspace_id}/events", get(workspace_events))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/sessions/{id}/follow-up", post(follow_up))
        .route("/v1/sessions/{id}/model", post(switch_model))
        .route("/v1/sessions/{id}/cancel", post(cancel_session))
        .route("/v1/sessions/{id}/queue", post(queue_follow_up))
        .route("/v1/sessions/{id}/permissions/{request_id}", post(resolve_permission))
        .route("/v1/sessions/{id}/input", post(provide_input))
        .route("/v1/sessions/{id}/effects/{effect_id}/reconcile", post(reconcile_effect))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
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

// Handlers

async fn create_workspace(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let path = PathBuf::from(&req.path);
    let workspace = state.workspaces.get_or_create(&path).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: ErrorBody {
                    error_type: "rejected".to_string(),
                    message: format!("invalid workspace: {}", e),
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

async fn create_session(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionCreatedResponse>), (StatusCode, Json<ErrorResponse>)> {
    let workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "not_found".to_string(),
                        message: "workspace not found".to_string(),
                        current_revision: None,
                    },
                }),
            )
        })?;

    // Parse binding
    let binding: latte_core::ThreadProviderBindingV2 = serde_json::from_value(req.binding)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "rejected".to_string(),
                        message: format!("invalid binding: {}", e),
                        current_revision: None,
                    },
                }),
            )
        })?;

    // Create thread
    let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
    let _run_id = latte_core::RunId::from_uuid(uuid::Uuid::now_v7());

    workspace
        .runtime
        .start(thread_id, req.prompt, binding)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "failed".to_string(),
                        message: format!("failed to start session: {}", e),
                        current_revision: None,
                    },
                }),
            )
        })?;

    // Register session in index
    state
        .workspaces
        .register_session(thread_id, workspace.path.clone())
        .await;

    // Emit event
    let _ = workspace.event_tx.send(ServerEvent::ThreadChanged {
        session_id: thread_id.to_string(),
        revision: 0,
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(SessionCreatedResponse {
            session_id: thread_id.to_string(),
            accepted_revision: 0,
        }),
    ))
}

async fn list_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(_pagination): Query<PaginationQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let _workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    // TODO: implement session listing from workspace
    // For now, return empty list
    Ok(Json(serde_json::json!({ "sessions": [], "next_cursor": null })))
}

async fn search_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(_query): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let _workspace = state
        .workspaces
        .get_by_id(&workspace_id)
        .await
        .ok_or_else(|| not_found("workspace not found"))?;

    // TODO: implement session search from workspace
    // For now, return empty list
    Ok(Json(serde_json::json!({ "sessions": [], "next_cursor": null })))
}

async fn get_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    // Look up workspace from index
    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;

    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    // Get session from workspace
    // TODO: implement actual session retrieval
    Err(not_found("session not found"))
}

async fn follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let thread_id = latte_core::ThreadId::from_uuid(
        uuid::Uuid::parse_str(&id).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "rejected".to_string(),
                        message: "invalid session id".to_string(),
                        current_revision: None,
                    },
                }),
            )
        })?,
    );

    let prompt = req.prompt.clone();
    let expected_revision = req.expected_thread_revision;

    // Look up workspace from index
    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace
        .runtime
        .follow_up(thread_id, expected_revision, prompt)
        .await
    {
        Ok(snapshot) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "snapshot": snapshot })),
        )),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn switch_model(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<SwitchModelRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = latte_core::ThreadId::from_uuid(
        uuid::Uuid::parse_str(&id).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "rejected".to_string(),
                        message: "invalid session id".to_string(),
                        current_revision: None,
                    },
                }),
            )
        })?,
    );

    let binding: latte_core::ThreadProviderBindingV2 = serde_json::from_value(req.binding)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorBody {
                        error_type: "rejected".to_string(),
                        message: format!("invalid binding: {}", e),
                        current_revision: None,
                    },
                }),
            )
        })?;

    // Look up workspace from index
    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace
        .runtime
        .switch_model(thread_id, req.expected_thread_revision, &binding)
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn cancel_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(_req): Json<CancelRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace.runtime.cancel_durable(thread_id) {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn queue_follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<QueueFollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace.runtime.queue_follow_up(thread_id, req.prompt) {
        Ok(position) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "position": position })),
        )),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn resolve_permission(
    State(state): State<Arc<ServerState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(req): Json<ResolvePermissionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace
        .runtime
        .resolve_permission(thread_id, req.expected_thread_revision, request_id, req.allow)
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn provide_input(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<ProvideInputRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace
        .runtime
        .provide_input(thread_id, req.expected_thread_revision, req.request_id, req.value)
        .await
    {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

async fn reconcile_effect(
    State(state): State<Arc<ServerState>>,
    Path((id, effect_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let thread_id = parse_thread_id(&id)?;

    let workspace_path = state
        .workspaces
        .get_session_workspace(&thread_id)
        .await
        .ok_or_else(|| not_found("session not found"))?;
    let workspace = state
        .workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|_| not_found("workspace not found"))?;

    match workspace.runtime.reconcile_unknown_effect(thread_id, &effect_id) {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(_) => Err(not_found("session not found")),
    }
}

// Helper functions

fn parse_thread_id(id: &str) -> Result<latte_core::ThreadId, (StatusCode, Json<ErrorResponse>)> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| bad_request("invalid session id"))?;
    Ok(latte_core::ThreadId::from_uuid(uuid))
}

fn bad_request(message: &str) -> (StatusCode, Json<ErrorResponse>) {
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

fn not_found(message: &str) -> (StatusCode, Json<ErrorResponse>) {
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

    let stream = BroadcastStream::new(rx)
        .map(|result| {
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
                Err(_) => Ok(Event::default().event("resync_required").data("{}")),
            }
        });

    Sse::new(Box::pin(stream))
}

/// Run the HTTP server.
pub async fn run(state: Arc<ServerState>, port: u16) -> Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    info!("server listening on 127.0.0.1:{}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_state;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn test_health_check() {
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        let response = app
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
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        // Request without auth should fail
        let response = app
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
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        // Request with wrong token should fail
        let response = app
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
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        // Create workspace
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workspaces")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::from(
                        serde_json::json!({"path": "/tmp"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(result.get("workspace_id").is_some());
        assert!(result.get("path").is_some());
    }

    #[tokio::test]
    async fn test_create_workspace_invalid_path() {
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        // Create workspace with invalid path
        let response = app
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
        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let app = router(state);

        // Get non-existent session (valid UUID format)
        let response = app
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
}
