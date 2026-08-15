//! latte-code server: HTTP API with per-workspace event hubs.

mod http;
mod workspace;

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

pub use crate::http::{run, ServerState};
pub use crate::workspace::{WorkspaceInstance, WorkspaceManager};

/// Create a new server state.
pub fn new_state(token: String) -> Arc<ServerState> {
    Arc::new(ServerState {
        workspaces: Arc::new(WorkspaceManager::new()),
        event_tx: tokio::sync::broadcast::channel(256).0,
        token,
    })
}

/// Run the HTTP server.
pub async fn run_http(state: Arc<ServerState>, port: u16) -> Result<()> {
    info!("starting HTTP server on 127.0.0.1:{}", port);
    http::run(state, port).await
}
