//! latte-code server: HTTP API with per-workspace event hubs.

pub mod http;
pub mod workspace;

use anyhow::Result;
use std::sync::Arc;

pub use crate::http::{ServerState, serve};
pub use crate::workspace::{
    BuiltWorkspace, SessionLocator, WorkspaceInstance, WorkspaceManager, WorkspaceRuntimeBuilder,
};

/// Create a new server state from a durable per-workspace runtime builder and a
/// durable session locator.
pub fn new_state(
    token: String,
    builder: WorkspaceRuntimeBuilder,
    session_locator: SessionLocator,
) -> Arc<ServerState> {
    Arc::new(ServerState::new(
        Arc::new(WorkspaceManager::new(builder, session_locator)),
        tokio::sync::broadcast::channel(256).0,
        token,
    ))
}

/// Run the HTTP server on an already-bound listener, allowing the caller to
/// discover the actual local address before serving begins.
pub async fn serve_on(state: Arc<ServerState>, listener: tokio::net::TcpListener) -> Result<()> {
    http::serve(state, listener).await
}
