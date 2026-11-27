use latte_core::*;
use uuid::Uuid;

fn id() -> TurnId {
    TurnId::from_uuid(Uuid::nil())
}
fn failure(retryability: Retryability) -> TurnFailure {
    TurnFailure {
        code: FailureCode::RuntimeFailed,
        message: "crash".into(),
        retryability,
    }
}
fn handoff() -> Handoff {
    Handoff {
        summary: "done".into(),
        files_changed: vec![],
        evidence: vec![],
    }
}

fn handoff_with(status: VerificationStatus) -> Handoff {
    Handoff {
        summary: "verified".into(),
        files_changed: vec![],
        evidence: vec![Evidence {
            name: "tests".into(),
            status,
            summary: "result".into(),
        }],
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn transition_table_covers_lifecycle_and_revision_guards() {
    let queued = TurnState::queued(id());
    assert_eq!(
        queued.transition(0, Transition::Cancel).unwrap().status,
        TurnStatus::Cancelling
    );
    assert!(matches!(
        queued.transition(1, Transition::Start),
        Err(TransitionError::StaleRevision { .. })
    ));
    let running = queued.transition(0, Transition::Start).unwrap();
    let permission = running
        .transition(
            1,
            Transition::RequestPermission(PendingPermission {
                request_id: "p1".into(),
                operation_digest: "digest".into(),
                description: "write".into(),
            }),
        )
        .unwrap();
    assert_eq!(permission.status, TurnStatus::WaitingPermission);
    assert_eq!(
        permission.transition(
            2,
            Transition::ResolvePermission {
                request_id: "wrong".into(),
                allowed: true
            }
        ),
        Err(TransitionError::MismatchedRequest)
    );
    let denied = permission
        .transition(
            2,
            Transition::ResolvePermission {
                request_id: "p1".into(),
                allowed: false,
            },
        )
        .unwrap();
    assert_eq!(denied.failure.unwrap().retryability, Retryability::Terminal);

    let input = running
        .transition(
            1,
            Transition::RequestInput(PendingInput {
                request_id: "i1".into(),
                prompt: "value?".into(),
            }),
        )
        .unwrap();
    assert!(matches!(
        input.transition(
            2,
            Transition::ProvideInput {
                request_id: "other".into()
            }
        ),
        Err(TransitionError::MismatchedRequest)
    ));
    assert_eq!(
        input
            .transition(
                2,
                Transition::ProvideInput {
                    request_id: "i1".into()
                }
            )
            .unwrap()
            .status,
        TurnStatus::Running
    );
    assert_eq!(
        input.transition(2, Transition::Cancel).unwrap().status,
        TurnStatus::Cancelling
    );
    assert_eq!(
        running
            .transition(1, Transition::Cancel)
            .unwrap()
            .transition(2, Transition::Interrupt)
            .unwrap()
            .status,
        TurnStatus::Interrupted
    );
    let cancelling = running.transition(1, Transition::Cancel).unwrap();
    assert_eq!(
        cancelling
            .transition(2, Transition::Fail(failure(Retryability::Terminal)))
            .unwrap()
            .status,
        TurnStatus::Failed
    );

    let crashed = running
        .transition(1, Transition::Fail(failure(Retryability::Retryable)))
        .unwrap();
    assert_eq!(
        crashed.transition(2, Transition::Resume).unwrap().status,
        TurnStatus::Queued
    );
    let terminal = running
        .transition(1, Transition::Fail(failure(Retryability::Terminal)))
        .unwrap();
    assert!(matches!(
        terminal.transition(2, Transition::Resume),
        Err(TransitionError::Invalid { .. })
    ));
    let completed = running
        .transition(
            1,
            Transition::Complete {
                handoff: handoff(),
                policy: CompletionPolicy::VerificationNotRequired,
            },
        )
        .unwrap();
    assert!(matches!(
        completed.transition(2, Transition::Cancel),
        Err(TransitionError::CompletedImmutable)
    ));
}

#[test]
fn every_status_has_expected_legal_or_rejected_transitions() {
    let states = [
        TurnStatus::Queued,
        TurnStatus::Running,
        TurnStatus::WaitingPermission,
        TurnStatus::WaitingInput,
        TurnStatus::Cancelling,
        TurnStatus::Interrupted,
        TurnStatus::Failed,
        TurnStatus::Completed,
    ];
    for status in states {
        let mut state = TurnState::queued(id());
        state.status = status;
        let result = state.transition(0, Transition::Start);
        assert_eq!(result.is_ok(), status == TurnStatus::Queued);
    }
}

#[test]
fn completion_policy_prevents_false_success() {
    let running = TurnState::queued(id())
        .transition(0, Transition::Start)
        .unwrap();

    let passed = running
        .transition(
            1,
            Transition::Complete {
                handoff: handoff_with(VerificationStatus::Passed),
                policy: CompletionPolicy::VerificationRequired,
            },
        )
        .unwrap();
    assert_eq!(passed.status, TurnStatus::Completed);

    for handoff in [
        handoff_with(VerificationStatus::Failed),
        handoff_with(VerificationStatus::NotRun),
        handoff(),
    ] {
        let failed = running
            .transition(
                1,
                Transition::Complete {
                    handoff,
                    policy: CompletionPolicy::VerificationRequired,
                },
            )
            .unwrap();
        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.revision, 2);
        assert_eq!(
            failed.failure.unwrap().code,
            FailureCode::VerificationFailed
        );
    }

    let unverified = running
        .transition(
            1,
            Transition::Complete {
                handoff: handoff(),
                policy: CompletionPolicy::VerificationNotRequired,
            },
        )
        .unwrap();
    assert_eq!(unverified.status, TurnStatus::Completed);
    assert!(matches!(
        unverified.transition(2, Transition::Cancel),
        Err(TransitionError::CompletedImmutable)
    ));
}

#[test]
fn protocol_json_is_exact() {
    let event = EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        event_id: EventId::from_uuid(Uuid::nil()),
        turn_id: id(),
        revision: 3,
        event: RuntimeEvent::StateChanged {
            status: TurnStatus::Running,
        },
    };
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"protocol_version":1,"event_id":"00000000-0000-0000-0000-000000000000","turn_id":"00000000-0000-0000-0000-000000000000","revision":3,"event":{"type":"state_changed","status":"running"}}"#
    );
    assert_eq!(
        serde_json::from_str::<EventEnvelope>(&serde_json::to_string(&event).unwrap()).unwrap(),
        event
    );

    let state = TurnState::queued(id());
    assert_eq!(
        serde_json::to_string(&state).unwrap(),
        r#"{"turn_id":"00000000-0000-0000-0000-000000000000","revision":0,"status":"queued","pending_permission":null,"pending_input":null,"failure":null,"handoff":null}"#
    );
    // Read-only compatibility: a pre-schema-15 payload keyed "run_id" must
    // still deserialize into the renamed `TurnState`.
    let legacy_json = r#"{"run_id":"00000000-0000-0000-0000-000000000000","revision":0,"status":"queued","pending_permission":null,"pending_input":null,"failure":null,"handoff":null}"#;
    assert_eq!(
        serde_json::from_str::<TurnState>(legacy_json).unwrap(),
        state
    );

    // Read-only compatibility for Batch 3 renamed keys (`revision`, `ordinal`, `version`).
    let turn_summary: SessionTurnSummary = serde_json::from_str(
        r#"{"run_id":"00000000-0000-0000-0000-000000000000","parent_run_id":null,"ordinal":2,"status":"completed","run_revision":4,"completed_at_ms":10}"#,
    )
    .unwrap();
    assert_eq!(turn_summary.turn_ordinal, 2);
    assert_eq!(turn_summary.turn_revision, 4);

    let compact_state: ManualCompactionState =
        serde_json::from_str(r#"{"compacted":{"tier":"summarized","revision":9}}"#).unwrap();
    assert_eq!(
        compact_state,
        ManualCompactionState::Compacted {
            tier: ManualCompactionTier::Summarized,
            session_revision: 9,
        }
    );
}

#[derive(Debug)]
struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

struct FixedIds(Uuid);
impl IdSource for FixedIds {
    fn next_uuid_v7(&self) -> Uuid {
        self.0
    }
}

#[test]
fn clock_and_id_generation_are_deterministic_and_v7() {
    let generated = SystemIdSource::new(FixedClock(1_700_000_000_123)).next_uuid_v7();
    assert_eq!(generated.get_version_num(), 7);
    assert_eq!(
        generated.get_timestamp().unwrap().to_unix().0,
        1_700_000_000
    );
    let source = FixedIds(generated);
    assert_eq!(source.next_uuid_v7(), source.next_uuid_v7());
}
