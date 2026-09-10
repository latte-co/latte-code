use crate::{CommandId, EventId, PROTOCOL_VERSION, TurnId};
use serde::{Deserialize, Serialize};

/// A versioned command message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    /// Protocol version; currently always one.
    pub protocol_version: u16,
    /// Unique command identifier.
    pub command_id: CommandId,
    /// Command payload.
    pub command: RuntimeCommand,
}

impl CommandEnvelope {
    /// Wraps a command in the current protocol version.
    #[must_use]
    pub const fn new(command_id: CommandId, command: RuntimeCommand) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            command_id,
            command,
        }
    }
}

/// A versioned event message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Protocol version.
    pub protocol_version: u16,
    /// Unique event identifier.
    pub event_id: EventId,
    /// Related turn.
    pub turn_id: TurnId,
    /// Monotonic turn revision.
    pub revision: u64,
    /// Event payload.
    pub event: RuntimeEvent,
}

/// A versioned read-model snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadModelEnvelope {
    /// Protocol version.
    pub protocol_version: u16,
    /// Snapshot payload.
    #[serde(alias = "run")]
    pub turn: TurnState,
}

/// Commands accepted by the runtime boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommand {
    /// Start a new turn.
    Run { prompt: String },
    /// Resume an interrupted or retryable failed turn.
    Resume {
        turn_id: TurnId,
        expected_revision: u64,
    },
    /// Fetch one turn.
    Show { turn_id: TurnId },
    /// List known turns.
    List,
    /// Resolve a permission request.
    ResolvePermission {
        turn_id: TurnId,
        request_id: String,
        expected_revision: u64,
        decision: PermissionDecision,
    },
    /// Supply requested input.
    ProvideInput {
        turn_id: TurnId,
        request_id: String,
        expected_revision: u64,
        value: String,
    },
    /// Cancel a turn.
    Cancel {
        turn_id: TurnId,
        expected_revision: u64,
    },
    /// Stop the engine without mutating a turn.
    Shutdown,
}

/// Events emitted by the runtime boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// The durable state changed.
    StateChanged { status: TurnStatus },
    /// A tool started.
    ToolStarted { name: String },
    /// A tool completed.
    ToolCompleted { name: String, success: bool },
    /// Verification evidence was recorded.
    EvidenceRecorded { evidence: Evidence },
    /// A handoff was produced.
    HandoffProduced { handoff: Handoff },
}

/// Permission resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
}

/// Durable turn status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    WaitingPermission,
    WaitingInput,
    Cancelling,
    Interrupted,
    Failed,
    Completed,
}

/// A pending permission request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPermission {
    pub request_id: String,
    pub operation_digest: String,
    pub description: String,
}

/// A pending user input request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInput {
    pub request_id: String,
    pub prompt: String,
}

/// Whether a failure permits resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    Retryable,
    Terminal,
}

/// Typed runtime failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFailure {
    pub code: FailureCode,
    pub message: String,
    pub retryability: Retryability,
}

/// Stable machine-readable failure classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    Cancelled,
    PermissionDenied,
    VerificationFailed,
    RuntimeFailed,
}

/// Verification result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    NotRun,
}

/// Evidence attached to a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub name: String,
    pub status: VerificationStatus,
    pub summary: String,
}

/// Reviewable final handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub summary: String,
    pub files_changed: Vec<String>,
    pub evidence: Vec<Evidence>,
}

use crate::TurnState;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdSource, SystemIdSource};

    #[test]
    fn command_envelope_pins_the_current_version_without_changing_payload() {
        let ids = SystemIdSource::default();
        let command_id = CommandId::from_uuid(ids.next_uuid_v7());
        let envelope = CommandEnvelope::new(command_id, RuntimeCommand::List);
        assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
        assert_eq!(envelope.command_id, command_id);
        assert_eq!(envelope.command, RuntimeCommand::List);
    }
}
