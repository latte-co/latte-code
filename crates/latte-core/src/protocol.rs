use crate::{EventId, TurnId};
use serde::{Deserialize, Serialize};

/// A versioned turn-event message persisted in the turn event ledger.
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

/// Durable turn-level events persisted alongside `TurnState` transitions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// The durable state changed.
    StateChanged { status: TurnStatus },
    /// A handoff was produced.
    HandoffProduced { handoff: Handoff },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdSource, PROTOCOL_VERSION, SystemIdSource};

    #[test]
    fn event_envelope_round_trips_turn_transitions_and_handoffs() {
        let ids = SystemIdSource::default();
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(ids.next_uuid_v7()),
            turn_id: TurnId::from_uuid(ids.next_uuid_v7()),
            revision: 1,
            event: RuntimeEvent::HandoffProduced {
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec!["src/lib.rs".into()],
                    evidence: vec![Evidence {
                        name: "tests".into(),
                        status: VerificationStatus::Passed,
                        summary: "ok".into(),
                    }],
                },
            },
        };
        let encoded = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            serde_json::from_str::<EventEnvelope>(&encoded).unwrap(),
            envelope
        );
    }
}
