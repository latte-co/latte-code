//! latte-code server: accepts client connections and routes commands to the runtime.
mod transport;

use anyhow::{Context, Result};
use latte_core::{
    ServerCommand, ServerCommandPayload, ServerError, ServerEvent, ServerFrame, ServerResponse,
    ServerResponsePayload, ThreadCommand,
};
use latte_headless::thread::ThreadRuntimeService;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};

use crate::transport::Listener;

/// The latte-code server.
pub struct Server {
    runtime: Arc<ThreadRuntimeService>,
    event_tx: broadcast::Sender<ServerEvent>,
    /// Per-connection subscriptions: connection_id -> set of thread_ids
    subscriptions: Arc<Mutex<HashMap<u64, Vec<latte_core::ThreadId>>>>,
    next_conn_id: Arc<Mutex<u64>>,
}

impl Server {
    /// Create a new server with the given runtime.
    pub fn new(runtime: ThreadRuntimeService) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            runtime: Arc::new(runtime),
            event_tx,
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
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

            let runtime = self.runtime.clone();
            let event_tx = self.event_tx.clone();
            let subscriptions = self.subscriptions.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(conn_id, conn, runtime, event_tx, subscriptions).await {
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

async fn handle_connection(
    conn_id: u64,
    mut conn: crate::transport::Connection,
    runtime: Arc<ThreadRuntimeService>,
    event_tx: broadcast::Sender<ServerEvent>,
    subscriptions: Arc<Mutex<HashMap<u64, Vec<latte_core::ThreadId>>>>,
) -> Result<()> {
    info!("connection {} established", conn_id);
    let mut event_rx = event_tx.subscribe();

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
                        let response = handle_command(&runtime, cmd).await;
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
                        // Only send events for threads this connection subscribes to
                        let should_send = match &event {
                            ServerEvent::ThreadChanged { thread_id, .. } => {
                                let subs = subscriptions.lock().await;
                                subs.get(&conn_id)
                                    .map(|threads| threads.contains(thread_id))
                                    .unwrap_or(false)
                            }
                            ServerEvent::Progress { thread_id, .. } => {
                                let subs = subscriptions.lock().await;
                                subs.get(&conn_id)
                                    .map(|threads| threads.contains(thread_id))
                                    .unwrap_or(false)
                            }
                        };
                        if should_send {
                            let frame = ServerFrame::Event(event);
                            let json = serde_json::to_vec(&frame)?;
                            if let Err(e) = conn.send(&json).await {
                                warn!("failed to send event: {}", e);
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client is lagging; it should resync via snapshot
                        warn!("connection {} lagged", conn_id);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Clean up subscriptions
    subscriptions.lock().await.remove(&conn_id);
    info!("connection {} closed", conn_id);
    Ok(())
}

async fn handle_command(
    runtime: &ThreadRuntimeService,
    cmd: ServerCommand,
) -> ServerResponse {
    let command_id = cmd.command_id;
    let result = match cmd.payload {
        ServerCommandPayload::Thread(thread_cmd) => {
            handle_thread_command(runtime, thread_cmd).await
        }
        ServerCommandPayload::QueueFollowUp { thread_id, prompt } => {
            runtime.queue_follow_up(thread_id, prompt)
                .map(|pos| ServerResponsePayload::Received)
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ServerCommandPayload::ReconcileUnknown { thread_id, effect_id } => {
            runtime.reconcile_unknown_effect(thread_id, &effect_id)
                .map(|_| ServerResponsePayload::Received)
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ServerCommandPayload::ListSessions => {
            // TODO: implement session listing
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::SearchSessions { query } => {
            // TODO: implement session search
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::GetSession { thread_id } => {
            // TODO: implement session get
            Err(ServerError::Failed {
                message: "not implemented".to_string(),
            })
        }
        ServerCommandPayload::Subscribe { thread_id } => {
            // Subscription is handled at connection level
            Ok(ServerResponsePayload::Received)
        }
        ServerCommandPayload::Unsubscribe { thread_id } => {
            Ok(ServerResponsePayload::Received)
        }
    };

    match result {
        Ok(payload) => ServerResponse {
            command_id,
            payload,
        },
        Err(error) => ServerResponse {
            command_id,
            payload: ServerResponsePayload::Error { error },
        },
    }
}

async fn handle_thread_command(
    runtime: &ThreadRuntimeService,
    cmd: ThreadCommand,
) -> Result<ServerResponsePayload, ServerError> {
    match cmd {
        ThreadCommand::Start { thread_id, prompt, binding } => {
            runtime.start(thread_id, prompt, binding)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::FollowUp { thread_id, expected_thread_revision, prompt } => {
            runtime.follow_up(thread_id, expected_thread_revision, prompt)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::SwitchModel { thread_id, expected_thread_revision, binding } => {
            runtime.switch_model(thread_id, expected_thread_revision, &binding)
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::ProvideInput { thread_id, request_id, expected_thread_revision, expected_run_revision, value } => {
            runtime.provide_input(thread_id, expected_thread_revision, request_id, value)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::ResolvePermission { thread_id, request_id, expected_thread_revision, expected_run_revision, allow } => {
            runtime.resolve_permission(thread_id, expected_thread_revision, request_id, allow)
                .await
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
        ThreadCommand::Cancel { thread_id, expected_thread_revision, expected_run_revision } => {
            runtime.cancel_durable(thread_id)
                .map(|snapshot| ServerResponsePayload::Completed { snapshot })
                .map_err(|e| ServerError::Failed { message: e.to_string() })
        }
    }
}
