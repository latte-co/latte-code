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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Evidence, IdSource, SystemIdSource};

    fn queued() -> RunState {
        RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()))
    }

    fn permission(id: &str) -> PendingPermission {
        PendingPermission {
            request_id: id.into(),
            operation_digest: "digest".into(),
            description: "write a file".into(),
        }
    }

    fn input(id: &str) -> PendingInput {
        PendingInput {
            request_id: id.into(),
            prompt: "value".into(),
        }
    }

    fn failure(retryability: Retryability) -> RunFailure {
        RunFailure {
            code: FailureCode::RuntimeFailed,
            message: "failed".into(),
            retryability,
        }
    }

    fn handoff(statuses: &[VerificationStatus]) -> Handoff {
        Handoff {
            summary: "done".into(),
            files_changed: vec!["src/lib.rs".into()],
            evidence: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| Evidence {
                    name: format!("check-{index}"),
                    status: *status,
                    summary: "result".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn permission_and_input_transitions_require_exact_durable_identity() {
        let running = queued().transition(0, Transition::Start).unwrap();
        let waiting = running
            .transition(
                running.revision,
                Transition::RequestPermission(permission("permission-1")),
            )
            .unwrap();
        assert_eq!(waiting.status, RunStatus::WaitingPermission);
        assert_eq!(
            waiting.transition(
                waiting.revision,
                Transition::ResolvePermission {
                    request_id: "wrong".into(),
                    allowed: true,
                }
            ),
            Err(TransitionError::MismatchedRequest)
        );
        let refreshed = waiting
            .transition(
                waiting.revision,
                Transition::RefreshPermission(permission("permission-2")),
            )
            .unwrap();
        assert_eq!(
            refreshed.pending_permission.as_ref().unwrap().request_id,
            "permission-2"
        );
        let allowed = refreshed
            .transition(
                refreshed.revision,
                Transition::ResolvePermission {
                    request_id: "permission-2".into(),
                    allowed: true,
                },
            )
            .unwrap();
        assert_eq!(allowed.status, RunStatus::Running);
        assert!(allowed.pending_permission.is_none());

        let waiting_input = allowed
            .transition(allowed.revision, Transition::RequestInput(input("input-1")))
            .unwrap();
        assert_eq!(
            waiting_input.transition(
                waiting_input.revision,
                Transition::ProvideInput {
                    request_id: "wrong".into()
                }
            ),
            Err(TransitionError::MismatchedRequest)
        );
        let resumed = waiting_input
            .transition(
                waiting_input.revision,
                Transition::ProvideInput {
                    request_id: "input-1".into(),
                },
            )
            .unwrap();
        assert_eq!(resumed.status, RunStatus::Running);
        assert!(resumed.pending_input.is_none());

        let denied_waiting = running
            .transition(
                running.revision,
                Transition::RequestPermission(permission("deny")),
            )
            .unwrap();
        let denied = denied_waiting
            .transition(
                denied_waiting.revision,
                Transition::ResolvePermission {
                    request_id: "deny".into(),
                    allowed: false,
                },
            )
            .unwrap();
        assert_eq!(denied.status, RunStatus::Failed);
        assert_eq!(denied.failure.unwrap().code, FailureCode::PermissionDenied);
    }

    #[test]
    fn cancellation_failure_resume_and_revision_guards_are_total() {
        let initial = queued();
        assert_eq!(
            initial.transition(9, Transition::Start),
            Err(TransitionError::StaleRevision {
                expected: 9,
                actual: 0
            })
        );
        let cancelling = initial.transition(0, Transition::Cancel).unwrap();
        assert_eq!(cancelling.status, RunStatus::Cancelling);
        let interrupted = cancelling
            .transition(cancelling.revision, Transition::Interrupt)
            .unwrap();
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        let resumed = interrupted
            .transition(interrupted.revision, Transition::Resume)
            .unwrap();
        assert_eq!(resumed.status, RunStatus::Queued);

        for state in [
            queued().transition(0, Transition::Start).unwrap(),
            queued()
                .transition(0, Transition::Start)
                .unwrap()
                .transition(1, Transition::RequestPermission(permission("p")))
                .unwrap(),
            queued()
                .transition(0, Transition::Start)
                .unwrap()
                .transition(1, Transition::RequestInput(input("i")))
                .unwrap(),
        ] {
            let cancelled = state
                .transition(state.revision, Transition::Cancel)
                .unwrap();
            assert_eq!(cancelled.status, RunStatus::Cancelling);
            assert!(cancelled.pending_permission.is_none());
            assert!(cancelled.pending_input.is_none());
        }

        let running = queued().transition(0, Transition::Start).unwrap();
        let failed = running
            .transition(
                running.revision,
                Transition::Fail(failure(Retryability::Retryable)),
            )
            .unwrap();
        let retried = failed
            .transition(failed.revision, Transition::Resume)
            .unwrap();
        assert_eq!(retried.status, RunStatus::Queued);
        assert!(retried.failure.is_none());

        let terminal = queued()
            .transition(0, Transition::Fail(failure(Retryability::Terminal)))
            .unwrap();
        assert_eq!(
            terminal.transition(terminal.revision, Transition::Resume),
            Err(TransitionError::Invalid {
                from: RunStatus::Failed
            })
        );

        let mut overflow = queued();
        overflow.revision = u64::MAX;
        assert_eq!(
            overflow.transition(u64::MAX, Transition::Start),
            Err(TransitionError::RevisionOverflow)
        );
    }

    #[test]
    fn completion_policy_fails_closed_and_completed_runs_are_immutable() {
        let running = queued().transition(0, Transition::Start).unwrap();
        let no_verification = running
            .transition(
                running.revision,
                Transition::Complete {
                    handoff: handoff(&[]),
                    policy: CompletionPolicy::VerificationNotRequired,
                },
            )
            .unwrap();
        assert_eq!(no_verification.status, RunStatus::Completed);
        assert!(no_verification.handoff.is_some());
        assert_eq!(
            no_verification.transition(no_verification.revision, Transition::Cancel),
            Err(TransitionError::CompletedImmutable)
        );

        for statuses in [
            Vec::new(),
            vec![VerificationStatus::NotRun],
            vec![VerificationStatus::Passed, VerificationStatus::Failed],
        ] {
            let running = queued().transition(0, Transition::Start).unwrap();
            let failed = running
                .transition(
                    running.revision,
                    Transition::Complete {
                        handoff: handoff(&statuses),
                        policy: CompletionPolicy::VerificationRequired,
                    },
                )
                .unwrap();
            assert_eq!(failed.status, RunStatus::Failed);
            assert_eq!(
                failed.failure.unwrap().code,
                FailureCode::VerificationFailed
            );
        }

        let running = queued().transition(0, Transition::Start).unwrap();
        let completed = running
            .transition(
                running.revision,
                Transition::Complete {
                    handoff: handoff(&[VerificationStatus::Passed]),
                    policy: CompletionPolicy::VerificationRequired,
                },
            )
            .unwrap();
        assert_eq!(completed.status, RunStatus::Completed);
    }

    #[test]
    fn malformed_waiting_states_and_invalid_transitions_are_rejected() {
        let mut missing_permission = queued();
        missing_permission.status = RunStatus::WaitingPermission;
        assert_eq!(
            missing_permission.transition(
                0,
                Transition::ResolvePermission {
                    request_id: "p".into(),
                    allowed: true,
                }
            ),
            Err(TransitionError::MissingPending)
        );
        let mut missing_input = queued();
        missing_input.status = RunStatus::WaitingInput;
        assert_eq!(
            missing_input.transition(
                0,
                Transition::ProvideInput {
                    request_id: "i".into()
                }
            ),
            Err(TransitionError::MissingPending)
        );
        assert_eq!(
            queued().transition(0, Transition::Interrupt),
            Err(TransitionError::Invalid {
                from: RunStatus::Queued
            })
        );
    }

    #[test]
    fn all_headless_outcomes_have_stable_process_exit_codes() {
        assert_eq!(HeadlessOutcome::Success.exit_code(), 0);
        assert_eq!(HeadlessOutcome::Failed.exit_code(), 1);
        assert_eq!(HeadlessOutcome::UsageError.exit_code(), 2);
        assert_eq!(HeadlessOutcome::Interrupted.exit_code(), 130);
        assert_eq!(HeadlessOutcome::Cancelled.exit_code(), 130);
        assert_eq!(HeadlessOutcome::InternalError.exit_code(), 70);
    }
}
