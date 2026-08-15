//! Server protocol types for client-server communication.
//!
//! This module defines the wire protocol between latte-code clients (CLI/TUI)
//! and the latte-code server. The protocol is JSON over unix socket with
//! length-prefixed frames.

use crate::thread::{
    ThreadCommand, ThreadSnapshot, ThreadTransientProgress,
};
use crate::ThreadId;
use serde::{Deserialize, Serialize};

/// Protocol version for the server protocol.
pub const SERVER_PROTOCOL_VERSION: u16 = 1;

/// A command from client to server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerCommand {
    /// Unique command identifier for idempotency.
    pub command_id: String,
    /// The command payload.
    #[serde(flatten)]
    pub payload: ServerCommandPayload,
}

/// Command payloads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerCommandPayload {
    /// Thread commands (Start, FollowUp, etc.)
    Thread(ThreadCommand),
    /// Queue a follow-up message.
    QueueFollowUp {
        thread_id: ThreadId,
        prompt: String,
    },
    /// Reconcile an unknown effect.
    ReconcileUnknown {
        thread_id: ThreadId,
        effect_id: String,
    },
    /// Query commands.
    ListSessions,
    SearchSessions { query: String },
    GetSession { thread_id: ThreadId },
    /// Subscribe to thread events.
    Subscribe { thread_id: ThreadId },
    /// Unsubscribe from thread events.
    Unsubscribe { thread_id: ThreadId },
}

/// A response from server to client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerResponse {
    /// The command_id this response is for.
    pub command_id: String,
    /// The response payload.
    #[serde(flatten)]
    pub payload: ServerResponsePayload,
}

/// Response payloads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerResponsePayload {
    /// Command received (not necessarily durable).
    Received,
    /// Command completed with a snapshot.
    Completed { snapshot: ThreadSnapshot },
    /// Command completed with a list of sessions.
    Sessions { sessions: Vec<ThreadSnapshot> },
    /// Command failed.
    Error { error: ServerError },
}

/// Server error types.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerError {
    /// Command was rejected (invalid arguments, state conflict).
    Rejected { message: String },
    /// Client is not authorized.
    Unauthorized { message: String },
    /// Command execution failed.
    Failed { message: String },
    /// Server is temporarily unavailable.
    Unavailable { message: String },
}

/// An event from server to client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Thread state changed (wake-up signal; client should fetch snapshot).
    ThreadChanged {
        thread_id: ThreadId,
        revision: u64,
    },
    /// Transient progress (best-effort, may be lost).
    Progress {
        thread_id: ThreadId,
        progress: ThreadTransientProgress,
    },
}

/// A frame in the protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Command(ServerCommand),
    Response(ServerResponse),
    Event(ServerEvent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trip() {
        let cmd = ServerCommand {
            command_id: "test-1".to_string(),
            payload: ServerCommandPayload::ListSessions,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: ServerCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.command_id, "test-1");
    }

    #[test]
    fn frame_round_trip() {
        let frame = ServerFrame::Event(ServerEvent::ThreadChanged {
            thread_id: ThreadId::from_uuid(uuid::Uuid::now_v7()),
            revision: 42,
        });
        let json = serde_json::to_string(&frame).unwrap();
        let parsed: ServerFrame = serde_json::from_str(&json).unwrap();
        match parsed {
            ServerFrame::Event(ServerEvent::ThreadChanged { revision, .. }) => {
                assert_eq!(revision, 42);
            }
            _ => panic!("wrong frame type"),
        }
    }
}
