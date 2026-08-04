use super::support::{Scenario, json};
use latte_core::{
    FailureCode, PendingInput, PendingPermission, Retryability, RunFailure, RunId, RunStatus,
    Transition,
};
use latte_engine::{CancellationToken, EngineHandle, ProcessInvocation};
use latte_headless::{
    provider::FakeProvider,
    runtime::{AgentRuntime, VerificationPlan},
};
use std::{collections::BTreeMap, time::SystemTime};

fn wall_now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn new_run_id() -> RunId {
    RunId::from_uuid(uuid::Uuid::now_v7())
}

fn fixture_engine(scenario: &Scenario) -> EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .enabled_tools(["read_file", "search"])
        .deny_globs(["private/**"])
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

fn assert_final_status(scenario: &Scenario, run_id: RunId, status: RunStatus) {
    assert_eq!(
        final_show(scenario, run_id)["status"],
        serde_json::to_value(status).unwrap()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn public_legacy_lifecycle_matrix_is_visible_through_final_list_and_show() {
    let scenario = Scenario::new();
    let engine = fixture_engine(&scenario);
    let now = wall_now_ms();
    let lease = engine.acquire_lease("legacy-matrix", now, 120_000).unwrap();

    let queued = new_run_id();
    engine.create_run(queued, now + 1).unwrap();

    let running = new_run_id();
    engine.create_run(running, now + 2).unwrap();
    let running_state = engine
        .apply_transition(running, 0, Transition::Start, now + 3, &lease)
        .unwrap();
    engine
        .persist_runtime_checkpoint(
            running,
            running_state.revision,
            &lease,
            r#"{"phase":"provider_wait","attempt":2}"#,
            now + 4,
        )
        .unwrap();
    assert_eq!(
        engine.runtime_checkpoint(running).unwrap().as_deref(),
        Some(r#"{"phase":"provider_wait","attempt":2}"#)
    );

    let waiting_input = new_run_id();
    engine.create_run(waiting_input, now + 5).unwrap();
    let waiting_input_state = engine
        .apply_transition(waiting_input, 0, Transition::Start, now + 6, &lease)
        .unwrap();
    engine
        .apply_transition(
            waiting_input,
            waiting_input_state.revision,
            Transition::RequestInput(PendingInput {
                request_id: "matrix-input".into(),
                prompt: "matrix value".into(),
            }),
            now + 7,
            &lease,
        )
        .unwrap();

    let cancelled_input = new_run_id();
    engine.create_run(cancelled_input, now + 8).unwrap();
    let state = engine
        .apply_transition(cancelled_input, 0, Transition::Start, now + 9, &lease)
        .unwrap();
    let state = engine
        .apply_transition(
            cancelled_input,
            state.revision,
            Transition::RequestInput(PendingInput {
                request_id: "cancel-input".into(),
                prompt: "cancel me".into(),
            }),
            now + 10,
            &lease,
        )
        .unwrap();
    let cancelled = engine
        .cancel_waiting_run(cancelled_input, state.revision, &lease, now + 11)
        .unwrap();
    assert_eq!(cancelled.failure.unwrap().code, FailureCode::Cancelled);

    let waiting_permission = new_run_id();
    engine.create_run(waiting_permission, now + 12).unwrap();
    let state = engine
        .apply_transition(waiting_permission, 0, Transition::Start, now + 13, &lease)
        .unwrap();
    engine
        .apply_transition(
            waiting_permission,
            state.revision,
            Transition::RequestPermission(PendingPermission {
                request_id: "fixture-permission".into(),
                operation_digest: "fixture-digest".into(),
                description: "review fixture operation".into(),
            }),
            now + 14,
            &lease,
        )
        .unwrap();

    let retried = new_run_id();
    engine.create_run(retried, now + 15).unwrap();
    let state = engine
        .apply_transition(retried, 0, Transition::Start, now + 16, &lease)
        .unwrap();
    let state = engine
        .apply_transition(
            retried,
            state.revision,
            Transition::Fail(RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "retry the fixture".into(),
                retryability: Retryability::Retryable,
            }),
            now + 17,
            &lease,
        )
        .unwrap();
    engine
        .apply_transition(
            retried,
            state.revision,
            Transition::Resume,
            now + 18,
            &lease,
        )
        .unwrap();

    let failed = new_run_id();
    engine.create_run(failed, now + 19).unwrap();
    let state = engine
        .apply_transition(failed, 0, Transition::Start, now + 20, &lease)
        .unwrap();
    engine
        .apply_transition(
            failed,
            state.revision,
            Transition::Fail(RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "terminal fixture failure".into(),
                retryability: Retryability::Terminal,
            }),
            now + 21,
            &lease,
        )
        .unwrap();

    let interrupted = new_run_id();
    engine.create_run(interrupted, now + 22).unwrap();
    let state = engine
        .apply_transition(interrupted, 0, Transition::Start, now + 23, &lease)
        .unwrap();
    let state = engine
        .apply_transition(
            interrupted,
            state.revision,
            Transition::Cancel,
            now + 24,
            &lease,
        )
        .unwrap();
    engine
        .apply_transition(
            interrupted,
            state.revision,
            Transition::Interrupt,
            now + 25,
            &lease,
        )
        .unwrap();

    let completed = new_run_id();
    engine.create_run(completed, now + 26).unwrap();
    let state = engine
        .apply_transition(completed, 0, Transition::Start, now + 27, &lease)
        .unwrap();
    let argv = vec!["/bin/pwd".to_owned()];
    let env = BTreeMap::new();
    let verification = ProcessInvocation {
        argv: &argv,
        shell: None,
        cwd: ".",
        env: &env,
        timeout_ms: 2_000,
        grace_ms: 50,
        stdout_cap: 1_024,
        stderr_cap: 1_024,
        run_revision: state.revision,
        effect_id: "legacy-matrix-verification",
        attempt: 1,
        approval_digest: None,
        lease_owner: lease.owner(),
        lease_token: lease.fencing_token(),
    };
    let verification_output = engine
        .execute_verification(
            completed,
            state.revision,
            &lease,
            now + 28,
            &verification,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(verification_output.command_succeeded());
    engine
        .complete_verified_run(
            completed,
            state.revision,
            &lease,
            "public fixture verified".into(),
            now + 29,
        )
        .unwrap();

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_json = json(&listed);
    let listed_runs = listed_json["data"]["runs"].as_array().unwrap();
    for run_id in [
        queued,
        running,
        waiting_input,
        cancelled_input,
        waiting_permission,
        retried,
        failed,
        interrupted,
        completed,
    ] {
        assert!(
            listed_runs
                .iter()
                .any(|run| run["run_id"] == run_id.to_string()),
            "final list omitted {run_id}"
        );
    }

    for (run_id, status) in [
        (queued, RunStatus::Queued),
        (running, RunStatus::Running),
        (waiting_input, RunStatus::WaitingInput),
        (cancelled_input, RunStatus::Failed),
        (waiting_permission, RunStatus::WaitingPermission),
        (retried, RunStatus::Queued),
        (failed, RunStatus::Failed),
        (interrupted, RunStatus::Interrupted),
        (completed, RunStatus::Completed),
    ] {
        assert_final_status(&scenario, run_id, status);
    }
    let completed_show = final_show(&scenario, completed);
    assert_eq!(
        completed_show["handoff"]["summary"],
        "public fixture verified"
    );
    assert_eq!(completed_show["handoff"]["evidence"][0]["status"], "passed");
    engine.release_lease(&lease).unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_unknown_effect_reconciliation_is_terminal_in_final_binary() {
    let scenario = Scenario::new();
    let engine = fixture_engine(&scenario);
    let now = wall_now_ms();
    let lease = engine
        .acquire_lease("unknown-matrix", now, 120_000)
        .unwrap();
    let run_id = new_run_id();
    engine.create_run(run_id, now + 1).unwrap();
    let state = engine
        .apply_transition(run_id, 0, Transition::Start, now + 2, &lease)
        .unwrap();

    let argv = vec!["/bin/pwd".to_owned()];
    let env = BTreeMap::new();
    let invocation = ProcessInvocation {
        argv: &argv,
        shell: None,
        cwd: ".",
        env: &env,
        timeout_ms: 2_000,
        grace_ms: 50,
        stdout_cap: 1_024,
        stderr_cap: 1_024,
        run_revision: state.revision,
        effect_id: "cancelled-public-effect",
        attempt: 1,
        approval_digest: None,
        lease_owner: lease.owner(),
        lease_token: lease.fencing_token(),
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = engine
        .execute_process(run_id, &lease, now + 3, &invocation, &cancellation)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(
        engine.unknown_effects_for_run(run_id).unwrap(),
        vec!["cancelled-public-effect"]
    );
    engine.release_lease(&lease).unwrap();
    let runtime = AgentRuntime::new(
        engine.clone(),
        FakeProvider::default(),
        scenario.root(),
        VerificationPlan {
            argv: vec!["/bin/pwd".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 50,
            stdout_cap: 1_024,
            stderr_cap: 1_024,
        },
    );
    let failed = runtime
        .reconcile_unknown_and_abort(run_id, "cancelled-public-effect")
        .unwrap();
    assert_eq!(failed.status, RunStatus::Failed);

    let shown = final_show(&scenario, run_id);
    assert_eq!(shown["status"], "failed");
    assert!(
        shown["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("acknowledged failed")
    );
}

#[test]
fn public_lease_loss_fencing_is_observed_as_interrupted_by_final_binary() {
    let scenario = Scenario::new();
    let engine = fixture_engine(&scenario);
    let run_id = new_run_id();
    engine.create_run(run_id, 10).unwrap();
    let stale = engine.acquire_lease("stale-owner", 10, 5).unwrap();
    let running = engine
        .apply_transition(run_id, 0, Transition::Start, 11, &stale)
        .unwrap();
    assert!(
        engine
            .interrupt_after_lease_loss(run_id, &stale, running.revision, 14)
            .is_err(),
        "a live lease must not be fenced"
    );
    let recovered = engine
        .interrupt_after_lease_loss(run_id, &stale, running.revision, 15)
        .unwrap();
    assert!(matches!(
        recovered,
        latte_engine::LeaseLossRecovery::Interrupted(ref snapshot)
            if snapshot.status == RunStatus::Interrupted
    ));
    assert!(matches!(
        engine
            .interrupt_after_lease_loss(run_id, &stale, running.revision, 16)
            .unwrap(),
        latte_engine::LeaseLossRecovery::FencedNoop
    ));
    assert_final_status(&scenario, run_id, RunStatus::Interrupted);
}
