//! HTTP server with message bus architecture.

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use latte_core::ThreadId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

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
        .with_state(state)
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

    // TODO: implement session creation
    let session_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7());
    let accepted_revision = 0;

    Ok((
        StatusCode::ACCEPTED,
        Json(SessionCreatedResponse {
            session_id: session_id.to_string(),
            accepted_revision,
        }),
    ))
}

async fn list_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(_pagination): Query<PaginationQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "sessions": [] })))
}

async fn search_sessions(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
    Query(_pagination): Query<PaginationQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "sessions": [] })))
}

async fn get_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<FollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "accepted_revision": 0 }))))
}

async fn switch_model(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<SwitchModelRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn cancel_session(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn queue_follow_up(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<QueueFollowUpRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "position": 0 }))))
}

async fn resolve_permission(
    State(state): State<Arc<ServerState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(req): Json<ResolvePermissionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn provide_input(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    Json(req): Json<ProvideInputRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn reconcile_effect(
    State(state): State<Arc<ServerState>>,
    Path((id, effect_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: implement
    Ok(Json(serde_json::json!({ "snapshot": null })))
}

async fn workspace_events(
    State(state): State<Arc<ServerState>>,
    Path(workspace_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
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

    Sse::new(stream)
}

/// Run the HTTP server.
pub async fn run(state: Arc<ServerState>, port: u16) -> Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    info!("server listening on 127.0.0.1:{}", port);
    axum::serve(listener, app).await?;
    Ok(())
}
