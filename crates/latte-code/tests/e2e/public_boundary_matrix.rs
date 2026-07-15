use super::support::{Scenario, json};
use latte_core::{
    CompletionPolicy, FailureCode, Handoff, PendingPermission, RunId, RunStatus, Transition,
    wall_time_ms,
};
use latte_engine::{EngineHandle, ToolError, ToolInvocation};

fn run_id() -> RunId {
    RunId::from_uuid(uuid::Uuid::now_v7())
}

fn build_engine(scenario: &Scenario) -> EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn final_show(scenario: &Scenario, run_id: RunId) -> serde_json::Value {
    let output = scenario.output(&["--json", "show", &run_id.to_string()], |_| {});
    assert!(
        !output.stdout.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    json(&output)["data"]["run"].clone()
}

#[test]
#[allow(clippy::too_many_lines)]
fn stale_revision_and_foreign_authority_leave_final_projection_unchanged() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("stable.txt"), "stable boundary\n").unwrap();
    let engine = build_engine(&scenario);
    let now = wall_time_ms();
    let lease = engine
        .acquire_lease("boundary-owner", now, 120_000)
        .unwrap();
    let run_id = run_id();
    engine.create_run(run_id, now + 1).unwrap();
    let running = engine
        .apply_transition(run_id, 0, Transition::Start, now + 2, &lease)
        .unwrap();
    engine
        .persist_runtime_checkpoint(
            run_id,
            running.revision,
            &lease,
            r#"{"boundary":"stable","attempt":1}"#,
            now + 3,
        )
        .unwrap();
    assert!(
        engine
            .persist_runtime_checkpoint(run_id, running.revision, &lease, "{", now + 4)
            .is_err()
    );
    assert!(matches!(
        engine.persist_runtime_checkpoint(
            run_id,
            running.revision - 1,
            &lease,
            r#"{"boundary":"stale"}"#,
            now + 5,
        ),
        Err(latte_engine::StorageError::LeaseLost)
    ));

    let stale = engine.apply_transition(
        run_id,
        0,
        Transition::Fail(latte_core::RunFailure {
            code: FailureCode::RuntimeFailed,
            message: "must not commit".into(),
            retryability: latte_core::Retryability::Terminal,
        }),
        now + 6,
        &lease,
    );
    assert!(matches!(
        stale,
        Err(latte_engine::StorageError::StaleRevision {
            expected: 0,
            actual: 1
        })
    ));
    assert!(matches!(
        engine.acquire_lease("competing-owner", now + 7, 120_000),
        Err(latte_engine::StorageError::EngineUnavailable)
    ));

    let foreign_scenario = Scenario::new();
    let foreign_engine = build_engine(&foreign_scenario);
    let foreign = foreign_engine
        .acquire_lease("foreign-owner", now + 8, 120_000)
        .unwrap();
    assert!(matches!(
        engine.renew_lease(&foreign, now + 9, 120_000),
        Err(latte_engine::StorageError::LeaseLost)
    ));
    assert!(matches!(
        engine.persist_runtime_checkpoint(
            run_id,
            running.revision,
            &foreign,
            r#"{"boundary":"foreign"}"#,
            now + 10,
        ),
        Err(latte_engine::StorageError::LeaseLost)
    ));
    assert!(matches!(
        engine.apply_transition(
            run_id,
            running.revision,
            Transition::Cancel,
            now + 11,
            &foreign,
        ),
        Err(latte_engine::StorageError::LeaseLost)
    ));

    let input = serde_json::json!({"path":"stable.txt"});
    let wrong_authority = ToolInvocation {
        name: "read_file",
        input: &input,
        run_revision: running.revision,
        effect_id: "foreign-read-effect",
        attempt: 1,
        precondition: None,
        timeout_ms: 2_000,
        output_cap: 4_096,
        approval_digest: None,
        lease_owner: foreign.owner(),
        lease_token: foreign.fencing_token(),
    };
    assert!(matches!(
        engine.execute_tool(run_id, &lease, now + 12, &wrong_authority),
        Err(ToolError::InvalidApproval)
    ));
    assert!(engine.effect_status("foreign-read-effect").is_err());
    assert!(
        engine
            .apply_transition(
                run_id,
                running.revision,
                Transition::Resume,
                now + 13,
                &lease,
            )
            .is_err()
    );
    assert!(
        engine
            .apply_transition(
                run_id,
                running.revision,
                Transition::Complete {
                    handoff: Handoff {
                        summary: "completion bypass".into(),
                        files_changed: Vec::new(),
                        evidence: Vec::new(),
                    },
                    policy: CompletionPolicy::VerificationNotRequired,
                },
                now + 14,
                &lease,
            )
            .is_err()
    );
    assert!(
        engine
            .complete_verified_run(
                run_id,
                running.revision,
                &lease,
                "missing evidence must fail".into(),
                now + 15,
            )
            .is_err()
    );

    assert_eq!(engine.show(run_id).unwrap(), running);
    assert_eq!(
        engine.runtime_checkpoint(run_id).unwrap().as_deref(),
        Some(r#"{"boundary":"stable","attempt":1}"#)
    );
    let shown = final_show(&scenario, run_id);
    assert_eq!(shown["status"], "running");
    assert_eq!(shown["revision"], 1);
    assert!(shown["failure"].is_null());
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let projected = json(&listed)["data"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["run_id"] == run_id.to_string())
        .unwrap()
        .clone();
    assert_eq!(projected["status"], "running");
    assert_eq!(projected["revision"], 1);

    foreign_engine.release_lease(&foreign).unwrap();
    engine.release_lease(&lease).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn prepared_write_wrong_digest_then_public_deny_is_visible_and_never_mutates() {
    let scenario = Scenario::new();
    let engine = build_engine(&scenario);
    let now = wall_time_ms();
    let lease = engine
        .acquire_lease("permission-boundary", now, 120_000)
        .unwrap();
    let run_id = run_id();
    engine.create_run(run_id, now + 1).unwrap();
    let running = engine
        .apply_transition(run_id, 0, Transition::Start, now + 2, &lease)
        .unwrap();
    let input = serde_json::json!({
        "path":"must-not-exist.txt",
        "content":"boundary mutation must never happen\n",
        "create_intent":true
    });
    let initial = ToolInvocation {
        name: "write_file",
        input: &input,
        run_revision: running.revision + 2,
        effect_id: "boundary-write",
        attempt: 1,
        precondition: None,
        timeout_ms: 2_000,
        output_cap: 4_096,
        approval_digest: None,
        lease_owner: lease.owner(),
        lease_token: lease.fencing_token(),
    };
    let digest = match engine.execute_tool(run_id, &lease, now + 3, &initial) {
        Err(ToolError::PermissionRequired { digest, .. }) => digest,
        other => panic!("expected prepared permission, got {other:?}"),
    };
    assert!(!scenario.root().join("must-not-exist.txt").exists());

    let waiting = engine
        .apply_transition(
            run_id,
            running.revision,
            Transition::RequestPermission(PendingPermission {
                request_id: "boundary-write".into(),
                operation_digest: digest.clone(),
                description: "allow boundary write".into(),
            }),
            now + 4,
            &lease,
        )
        .unwrap();
    assert!(
        engine
            .permission_matches(
                "boundary-write",
                run_id,
                waiting.revision + 1,
                &lease,
                &digest,
                now + 5,
            )
            .unwrap()
    );
    assert!(
        !engine
            .permission_matches(
                "boundary-write",
                run_id,
                waiting.revision + 1,
                &lease,
                "wrong-digest",
                now + 6,
            )
            .unwrap()
    );

    let wrong_digest = ToolInvocation {
        approval_digest: Some("0"),
        ..initial
    };
    assert!(matches!(
        engine.execute_tool(run_id, &lease, now + 7, &wrong_digest),
        Err(ToolError::InvalidApproval)
    ));
    assert!(!scenario.root().join("must-not-exist.txt").exists());
    assert_eq!(engine.show(run_id).unwrap(), waiting);

    let denied = engine
        .deny_waiting_permission(run_id, waiting.revision, &lease, now + 8)
        .unwrap();
    assert_eq!(denied.status, RunStatus::Failed);
    assert_eq!(
        denied.failure.as_ref().unwrap().code,
        FailureCode::PermissionDenied
    );
    assert!(denied.pending_permission.is_none());
    assert_eq!(
        engine.effect_status("boundary-write").unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
    assert!(!scenario.root().join("must-not-exist.txt").exists());
    assert_eq!(
        engine
            .deny_waiting_permission(run_id, denied.revision, &lease, now + 9)
            .unwrap(),
        denied
    );
    assert_eq!(
        engine
            .cancel_waiting_run(run_id, denied.revision, &lease, now + 10)
            .unwrap(),
        denied
    );
    engine.release_lease(&lease).unwrap();

    let redundant = scenario.output(&["--json", "resume", &run_id.to_string(), "--deny"], |_| {});
    assert_eq!(redundant.status.code(), Some(1));

    let shown = final_show(&scenario, run_id);
    assert_eq!(shown["status"], "failed");
    assert_eq!(shown["failure"]["code"], "permission_denied");
    assert!(shown["pending_permission"].is_null());
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["run_id"] == run_id.to_string() && value["status"] == "failed")
    );
}

#[test]
fn expired_owner_reopens_as_interrupted_without_losing_checkpoint_or_history() {
    let scenario = Scenario::new();
    let engine = build_engine(&scenario);
    let real_now = wall_time_ms();
    let logical_now = real_now.saturating_sub(10_000);
    let lease = engine
        .acquire_lease("expired-boundary", logical_now, 100)
        .unwrap();
    let run_id = run_id();
    engine.create_run(run_id, logical_now + 1).unwrap();
    let running = engine
        .apply_transition(run_id, 0, Transition::Start, logical_now + 2, &lease)
        .unwrap();
    engine
        .persist_runtime_checkpoint(
            run_id,
            running.revision,
            &lease,
            r#"{"boundary":"reopen","history":["first","second"]}"#,
            logical_now + 3,
        )
        .unwrap();
    assert!(matches!(
        engine.apply_transition(
            run_id,
            running.revision,
            Transition::Cancel,
            logical_now + 100,
            &lease,
        ),
        Err(latte_engine::StorageError::LeaseLost)
    ));
    assert_eq!(engine.show(run_id).unwrap(), running);
    drop(engine);

    let shown = final_show(&scenario, run_id);
    assert_eq!(shown["status"], "interrupted");
    assert_eq!(shown["revision"], 2);
    assert!(shown["failure"].is_null());
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| {
                value["run_id"] == run_id.to_string()
                    && value["status"] == "interrupted"
                    && value["revision"] == 2
            })
    );

    let reopened = build_engine(&scenario);
    assert_eq!(
        reopened.runtime_checkpoint(run_id).unwrap().as_deref(),
        Some(r#"{"boundary":"reopen","history":["first","second"]}"#)
    );
    assert_eq!(
        reopened.show(run_id).unwrap().status,
        RunStatus::Interrupted
    );
    assert!(matches!(
        reopened
            .interrupt_after_lease_loss(run_id, &lease, 2, real_now + 1)
            .unwrap(),
        latte_engine::LeaseLossRecovery::AlreadyTerminal(ref state)
            if state.status == RunStatus::Interrupted
    ));
    let fresh = reopened
        .acquire_lease("reopen-boundary", real_now + 2, 120_000)
        .unwrap();
    assert_eq!(
        reopened
            .cancel_waiting_run(run_id, 2, &fresh, real_now + 3)
            .unwrap()
            .status,
        RunStatus::Interrupted
    );
    let shown_again = final_show(&scenario, run_id);
    assert_eq!(shown_again["status"], "interrupted");
    assert_eq!(shown_again["revision"], 2);
    reopened.release_lease(&fresh).unwrap();
}
