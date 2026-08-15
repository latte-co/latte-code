//! latte-code server: accepts client connections and routes commands to workspaces.

mod transport;
mod workspace;

use anyhow::{Context, Result};
use latte_core::{
    ServerCommand, ServerCommandPayload, ServerError, ServerEvent, ServerFrame, ServerResponse,
    ServerResponsePayload, ThreadCommand,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};

use crate::transport::Listener;
use crate::workspace::WorkspaceManager;

/// The latte-code server.
pub struct Server {
    workspaces: Arc<WorkspaceManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    /// Per-connection state: connection_id -> workspace path
    connections: Arc<Mutex<HashMap<u64, Option<PathBuf>>>>,
    next_conn_id: Arc<Mutex<u64>>,
}

impl Server {
    /// Create a new server.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            workspaces: Arc::new(WorkspaceManager::new()),
            event_tx,
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_conn_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Run the server, listening on the given socket path.
    pub async fn run(&self, socket_path: impl AsRef<std::path::Path>) -> Result<()> {
        let listener = Listener::bind(socket_path).await?;
        info!("server listening");

        loop {
            let conn = listener.accept().await?;
            let conn_id = {
                let mut next = self.next_conn_id.lock().await;
                let id = *next;
                *next += 1;
                id
            };

            let workspaces = self.workspaces.clone();
            let event_tx = self.event_tx.clone();
            let connections = self.connections.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(conn_id, conn, workspaces, event_tx, connections).await {
                    error!("connection {} error: {}", conn_id, e);
                }
            });
        }
    }

    /// Get a receiver for server events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

async fn handle_connection(
    conn_id: u64,
    mut conn: crate::transport::Connection,
    workspaces: Arc<WorkspaceManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    connections: Arc<Mutex<HashMap<u64, Option<PathBuf>>>>,
) -> Result<()> {
    info!("connection {} established", conn_id);
    let mut event_rx = event_tx.subscribe();

    // Register connection
    connections.lock().await.insert(conn_id, None);

    loop {
        tokio::select! {
            frame = conn.recv() => {
                let frame = match frame? {
                    Some(f) => f,
                    None => break, // EOF
                };
                let frame: ServerFrame = serde_json::from_slice(&frame)
                    .context("invalid frame")?;

                match frame {
                    ServerFrame::Command(cmd) => {
                        let response = match handle_command(conn_id, &workspaces, &connections, cmd).await {
                            Ok(resp) => resp,
                            Err(e) => ServerResponse {
                                command_id: String::new(),
                                payload: ServerResponsePayload::Error { error: e },
                            },
                        };
                        let response_frame = ServerFrame::Response(response);
                        let json = serde_json::to_vec(&response_frame)?;
                        conn.send(&json).await?;
                    }
                    ServerFrame::Response(_) | ServerFrame::Event(_) => {
                        warn!("server received unexpected frame type");
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        // TODO: implement per-connection subscription tracking
                        let frame = ServerFrame::Event(event);
                        let json = serde_json::to_vec(&frame)?;
                        if let Err(e) = conn.send(&json).await {
                            warn!("failed to send event: {}", e);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        warn!("connection {} lagged", conn_id);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Clean up
    connections.lock().await.remove(&conn_id);
    info!("connection {} closed", conn_id);
    Ok(())
}

async fn handle_command(
    conn_id: u64,
    workspaces: &WorkspaceManager,
    connections: &Mutex<HashMap<u64, Option<PathBuf>>>,
    cmd: ServerCommand,
) -> Result<ServerResponse, ServerError> {
    let command_id = cmd.command_id;
    let result = match cmd.payload {
        ServerCommandPayload::SelectWorkspace { path } => {
            let workspace_path = PathBuf::from(path);
            match workspaces.get_or_create(&workspace_path).await {
                Ok(_) => {
                    connections.lock().await.insert(conn_id, Some(workspace_path));
                    Ok(ServerResponsePayload::Received)
                }
                Err(e) => Err(ServerError::Rejected {
                    message: format!("invalid workspace: {}", e),
                }),
            }
        }
        ServerCommandPayload::Thread(thread_cmd) => {
            let runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            handle_thread_command(&runtime, thread_cmd).await
        }
        ServerCommandPayload::QueueFollowUp { thread_id, prompt } => {
            let runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            runtime.queue_follow_up(thread_id, prompt)
                .map(|_| ServerResponsePayload::Received)
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ServerCommandPayload::ReconcileUnknown { thread_id, effect_id } => {
            let runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            runtime.reconcile_unknown_effect(thread_id, &effect_id)
                .map(|_| ServerResponsePayload::Received)
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ServerCommandPayload::ListSessions => {
            let _runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            // TODO: implement session listing
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::SearchSessions { query } => {
            let _runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            // TODO: implement session search
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::GetSession { thread_id } => {
            let _runtime = get_workspace_runtime(conn_id, connections, workspaces).await?;
            // TODO: implement session get
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::Subscribe { thread_id } => {
            // TODO: implement per-connection subscription tracking
            Ok(ServerResponsePayload::Received)
        }
        ServerCommandPayload::Unsubscribe { thread_id } => {
            // TODO: implement per-connection subscription tracking
            Ok(ServerResponsePayload::Received)
        }
    };

    match result {
        Ok(payload) => Ok(ServerResponse {
            command_id,
            payload,
        }),
        Err(error) => Ok(ServerResponse {
            command_id,
            payload: ServerResponsePayload::Error { error },
        }),
    }
}

async fn get_workspace_runtime(
    conn_id: u64,
    connections: &Mutex<HashMap<u64, Option<PathBuf>>>,
    workspaces: &WorkspaceManager,
) -> Result<std::sync::Arc<latte_headless::thread::ThreadRuntimeService>, ServerError> {
    let workspace_path = {
        let conns = connections.lock().await;
        conns
            .get(&conn_id)
            .and_then(|p| p.clone())
            .ok_or_else(|| ServerError::Rejected {
                message: "no workspace selected".to_string(),
            })?
    };

    let instance = workspaces
        .get_or_create(&workspace_path)
        .await
        .map_err(|e| ServerError::Failed {
            message: e.to_string(),
        })?;

    Ok(instance.runtime.clone())
}

async fn handle_thread_command(
    runtime: &latte_headless::thread::ThreadRuntimeService,
    cmd: ThreadCommand,
) -> Result<ServerResponsePayload, ServerError> {
    match cmd {
        ThreadCommand::Start { thread_id, prompt, binding } => {
            runtime
                .start(thread_id, prompt, binding)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::FollowUp { thread_id, expected_thread_revision, prompt } => {
            runtime
                .follow_up(thread_id, expected_thread_revision, prompt)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::SwitchModel { thread_id, expected_thread_revision, binding } => {
            runtime
                .switch_model(thread_id, expected_thread_revision, &binding)
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::ProvideInput { thread_id, request_id, expected_thread_revision, expected_run_revision, value } => {
            runtime
                .provide_input(thread_id, expected_thread_revision, request_id, value)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::ResolvePermission { thread_id, request_id, expected_thread_revision, expected_run_revision, allow } => {
            runtime
                .resolve_permission(thread_id, expected_thread_revision, request_id, allow)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::Cancel { thread_id, expected_thread_revision, expected_run_revision } => {
            runtime
                .cancel_durable(thread_id)
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
    }
}
