use crate::{
    FailureCode, Handoff, PendingInput, PendingPermission, Retryability, RunFailure, RunId,
    RunStatus, VerificationStatus,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Complete durable state for one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// Run identifier.
    pub run_id: RunId,
    /// Revision incremented exactly once per accepted transition.
    pub revision: u64,
    /// Current status.
    pub status: RunStatus,
    /// Outstanding permission, if any.
    pub pending_permission: Option<PendingPermission>,
    /// Outstanding input, if any.
    pub pending_input: Option<PendingInput>,
    /// Failure details when failed.
    pub failure: Option<RunFailure>,
    /// Final handoff when completed.
    pub handoff: Option<Handoff>,
}

impl RunState {
    /// Creates a queued run at revision zero.
    #[must_use]
    pub const fn queued(run_id: RunId) -> Self {
        Self {
            run_id,
            revision: 0,
            status: RunStatus::Queued,
            pending_permission: None,
            pending_input: None,
            failure: None,
            handoff: None,
        }
    }

    /// Applies a checked transition and returns the next immutable value.
    ///
    /// # Errors
    ///
    /// Returns a typed rejection when the revision, request identity, or state
    /// does not permit the requested transition.
    #[allow(clippy::too_many_lines)]
    pub fn transition(
        &self,
        expected_revision: u64,
        transition: Transition,
    ) -> Result<Self, TransitionError> {
        if self.revision != expected_revision {
            return Err(TransitionError::StaleRevision {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        if self.status == RunStatus::Completed {
            return Err(TransitionError::CompletedImmutable);
        }
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(TransitionError::RevisionOverflow)?;
        match transition {
            Transition::Start if self.status == RunStatus::Queued => {
                next.status = RunStatus::Running;
            }
            Transition::RequestPermission(request) if self.status == RunStatus::Running => {
                next.status = RunStatus::WaitingPermission;
                next.pending_permission = Some(request);
            }
            Transition::ResolvePermission {
                request_id,
                allowed,
            } if self.status == RunStatus::WaitingPermission => {
                let pending = self
                    .pending_permission
                    .as_ref()
                    .ok_or(TransitionError::MissingPending)?;
                if pending.request_id != request_id {
                    return Err(TransitionError::MismatchedRequest);
                }
                next.pending_permission = None;
                if allowed {
                    next.status = RunStatus::Running;
                } else {
                    next.status = RunStatus::Failed;
                    next.failure = Some(RunFailure {
                        code: FailureCode::PermissionDenied,
                        message: "permission was denied".into(),
                        retryability: Retryability::Terminal,
                    });
                }
            }
            Transition::RefreshPermission(request)
                if self.status == RunStatus::WaitingPermission =>
            {
                next.status = RunStatus::WaitingPermission;
                next.pending_permission = Some(request);
            }
            Transition::RequestInput(request) if self.status == RunStatus::Running => {
                next.status = RunStatus::WaitingInput;
                next.pending_input = Some(request);
            }
            Transition::ProvideInput { request_id } if self.status == RunStatus::WaitingInput => {
                let pending = self
                    .pending_input
                    .as_ref()
                    .ok_or(TransitionError::MissingPending)?;
                if pending.request_id != request_id {
                    return Err(TransitionError::MismatchedRequest);
                }
                next.pending_input = None;
                next.status = RunStatus::Running;
            }
            Transition::Cancel
                if matches!(
                    self.status,
                    RunStatus::Queued
                        | RunStatus::Running
                        | RunStatus::WaitingPermission
                        | RunStatus::WaitingInput
                ) =>
            {
                next.status = RunStatus::Cancelling;
                next.pending_input = None;
                next.pending_permission = None;
            }
            Transition::Interrupt
                if matches!(self.status, RunStatus::Running | RunStatus::Cancelling) =>
            {
                next.status = RunStatus::Interrupted;
            }
            Transition::Fail(failure)
                if matches!(
                    self.status,
                    RunStatus::Queued | RunStatus::Running | RunStatus::Cancelling
                ) =>
            {
                next.status = RunStatus::Failed;
                next.failure = Some(failure);
            }
            Transition::Resume
                if self.status == RunStatus::Interrupted
                    || (self.status == RunStatus::Failed
                        && self.failure.as_ref().is_some_and(|failure| {
                            failure.retryability == Retryability::Retryable
                        })) =>
            {
                next.status = RunStatus::Queued;
                next.failure = None;
            }
            Transition::Complete { handoff, policy } if self.status == RunStatus::Running => {
                let verification_passed = match policy {
                    CompletionPolicy::VerificationNotRequired => true,
                    CompletionPolicy::VerificationRequired => {
                        !handoff.evidence.is_empty()
                            && handoff
                                .evidence
                                .iter()
                                .all(|evidence| evidence.status == VerificationStatus::Passed)
                    }
                };
                if verification_passed {
                    next.status = RunStatus::Completed;
                    next.handoff = Some(handoff);
                } else {
                    next.status = RunStatus::Failed;
                    next.failure = Some(RunFailure {
                        code: FailureCode::VerificationFailed,
                        message: "required verification did not pass".into(),
                        retryability: Retryability::Terminal,
                    });
                }
            }
            _ => return Err(TransitionError::Invalid { from: self.status }),
        }
        Ok(next)
    }
}

/// Every legal state mutation is represented here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    Start,
    RequestPermission(PendingPermission),
    ResolvePermission {
        request_id: String,
        allowed: bool,
    },
    /// Replaces an expired permission without executing its effect.
    RefreshPermission(PendingPermission),
    RequestInput(PendingInput),
    ProvideInput {
        request_id: String,
    },
    Cancel,
    Interrupt,
    Fail(RunFailure),
    Resume,
    Complete {
        /// Final reviewable output.
        handoff: Handoff,
        /// Explicit verification precondition.
        policy: CompletionPolicy,
    },
}

/// Verification precondition for completing a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// Completion is permitted without verification evidence.
    VerificationNotRequired,
    /// At least one evidence item must exist and every item must have passed.
    VerificationRequired,
}

/// Rejection from the centralized transition API.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("stale revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("transition is invalid from {from:?}")]
    Invalid { from: RunStatus },
    #[error("request id does not match the pending request")]
    MismatchedRequest,
    #[error("state is missing its pending request")]
    MissingPending,
    #[error("completed runs are immutable")]
    CompletedImmutable,
    #[error("revision overflow")]
    RevisionOverflow,
}

/// Stable process exit classifications for headless callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessOutcome {
    Success,
    Failed,
    Interrupted,
    Cancelled,
    UsageError,
    InternalError,
}

impl HeadlessOutcome {
    /// Maps outcomes to stable process exit codes.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::UsageError => 2,
            Self::Interrupted | Self::Cancelled => 130,
            Self::InternalError => 70,
        }
    }
}
