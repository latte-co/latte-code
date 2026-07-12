use crate::{CommandId, EventId, PROTOCOL_VERSION, RunId};
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
    /// Related run.
    pub run_id: RunId,
    /// Monotonic run revision.
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
    pub run: RunState,
}

/// Commands accepted by the runtime boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommand {
    /// Start a new run.
    Run { prompt: String },
    /// Resume an interrupted or retryable failed run.
    Resume {
        run_id: RunId,
        expected_revision: u64,
    },
    /// Fetch one run.
    Show { run_id: RunId },
    /// List known runs.
    List,
    /// Resolve a permission request.
    ResolvePermission {
        run_id: RunId,
        request_id: String,
        expected_revision: u64,
        decision: PermissionDecision,
    },
    /// Supply requested input.
    ProvideInput {
        run_id: RunId,
        request_id: String,
        expected_revision: u64,
        value: String,
    },
    /// Cancel a run.
    Cancel {
        run_id: RunId,
        expected_revision: u64,
    },
    /// Stop the engine without mutating a run.
    Shutdown,
}

/// Events emitted by the runtime boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// The durable state changed.
    StateChanged { status: RunStatus },
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

/// Durable run status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
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
pub struct RunFailure {
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

/// Evidence attached to a run.
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

use crate::RunState;
