//! latte-code server: HTTP API with per-workspace event hubs.

pub mod http;
pub mod workspace;

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

pub use crate::http::{run, ServerState};
pub use crate::workspace::{ProviderFactory, WorkspaceInstance, WorkspaceManager};

/// Create a new server state.
pub fn new_state(token: String, provider_factory: ProviderFactory) -> Arc<ServerState> {
    Arc::new(ServerState {
        workspaces: Arc::new(WorkspaceManager::new(provider_factory)),
        event_tx: tokio::sync::broadcast::channel(256).0,
        token,
    })
}

/// Run the HTTP server.
pub async fn run_http(state: Arc<ServerState>, port: u16) -> Result<()> {
    info!("starting HTTP server on 127.0.0.1:{}", port);
    http::run(state, port).await
}
