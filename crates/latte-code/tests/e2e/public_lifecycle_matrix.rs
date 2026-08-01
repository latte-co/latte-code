use super::support::{PtySession, Scenario, json, wait_until};
use latte_core::{
    FailureCode, IdSource, Retryability, RunFailure, RunId, SystemIdSource, ThreadCommandId,
    ThreadId, ThreadProviderBindingV2,
};
use latte_engine::{
    CancellationToken, CommitThreadRunUpdate, EngineHandle, Lease, ProcessOutput,
    ProcessTermination, StorageError, ThreadCommitRequest, ThreadEffectDescriptor,
    ThreadEffectObservedValue, ThreadEffectPolicy, ThreadEffectRequest, ThreadEffectStartRequest,
    ThreadLeaseLossRecovery,
};
use std::{collections::BTreeMap, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_A: &[u8] = b"\x1b[97;5u";
const CTRL_R: &[u8] = b"\x1b[114;5u";

fn run_id() -> RunId {
    RunId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn thread_id() -> ThreadId {
    ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn command_id() -> ThreadCommandId {
    ThreadCommandId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn binding() -> ThreadProviderBindingV2 {
    ThreadProviderBindingV2 {
        version: 1,
        provider_name: "public-fixture".into(),
        provider_type: "openai-chat".into(),
        protocol: "openai-chat-completions-v1".into(),
        model: "public-lifecycle-model".into(),
        config_fingerprint: "public-lifecycle-config".into(),
        tools_fingerprint: "public-lifecycle-tools".into(),
        aliases: BTreeMap::new(),
        credential_ref_id: "env:PUBLIC_FIXTURE_KEY".into(),
        data_scope_id: "workspace".into(),
        credential_generation: 1,
    }
}

fn build_engine(scenario: &Scenario) -> EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn active_run(snapshot: &latte_core::ThreadSnapshot) -> (RunId, u64) {
    let run_id = snapshot.active_run_id.unwrap();
    let revision = snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;
    (run_id, revision)
}

fn commit_thread(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    update: CommitThreadRunUpdate,
    now: u64,
) -> latte_core::ThreadSnapshot {
    let (run_id, run_revision) = active_run(snapshot);
    engine
        .commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: snapshot.thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: run_revision,
                command_id: command_id(),
                request_id: None,
                effect_id: None,
                update,
            },
            lease,
            now,
        )
        .unwrap()
        .snapshot
}

fn start_thread(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    source: &str,
    now: u64,
) -> latte_core::ThreadSnapshot {
    commit_thread(
        engine,
        lease,
        snapshot,
        CommitThreadRunUpdate::Start {
            source_key: source.into(),
        },
        now,
    )
}

fn prepare_read(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    effect_id: &str,
    path: &str,
    now: u64,
) -> latte_engine::ThreadEffectPrepared {
    let (run_id, run_revision) = active_run(snapshot);
    let prepared = engine
        .prepare_thread_effect(
            ThreadEffectRequest {
                thread_id: snapshot.thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: run_revision,
                command_id: command_id(),
                source_key: format!("public:{effect_id}:prepare"),
                descriptor: ThreadEffectDescriptor {
                    effect_id: effect_id.into(),
                    tool_call_id: format!("call-{effect_id}"),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":path}),
                    attempt: 1,
                },
            },
            lease,
            now,
        )
        .unwrap();
    assert_eq!(prepared.policy, ThreadEffectPolicy::Allow);
    prepared
}

fn start_effect(
    engine: &EngineHandle,
    lease: &Lease,
    prepared: &latte_engine::ThreadEffectPrepared,
    effect_id: &str,
    now: u64,
) -> latte_engine::ThreadEffectStarted {
    let (run_id, run_revision) = active_run(&prepared.snapshot);
    engine
        .start_thread_effect(
            ThreadEffectStartRequest {
                thread_id: prepared.snapshot.thread_id,
                run_id,
                expected_thread_revision: prepared.snapshot.revision,
                expected_run_revision: run_revision,
                command_id: command_id(),
                source_key: format!("public:{effect_id}:start"),
                effect_id: effect_id.into(),
            },
            prepared.operation_digest.clone(),
            lease,
            now,
        )
        .unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_thread_effects_verification_and_follow_up_render_through_final_binary() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(
        scenario.root().join("public-read.txt"),
        "public effect value\n",
    )
    .unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let success_thread_id = thread_id();
    let success_lease = engine
        .acquire_thread_lease(success_thread_id, now, 120_000)
        .unwrap();

    let success_parent_id = run_id();
    let initial = engine
        .create_thread_v2(
            success_thread_id,
            success_parent_id,
            binding(),
            "public effect and verification",
            now + 1,
        )
        .unwrap();
    let started_snapshot = start_thread(
        &engine,
        &success_lease,
        &initial,
        "public:success:start",
        now + 2,
    );
    let prepared = prepare_read(
        &engine,
        &success_lease,
        &started_snapshot,
        "public-success-effect",
        "public-read.txt",
        now + 3,
    );
    let started_effect = start_effect(
        &engine,
        &success_lease,
        &prepared,
        "public-success-effect",
        now + 4,
    );
    let value = engine
        .execute_started_thread_effect(&started_effect, &success_lease, &CancellationToken::new())
        .await
        .unwrap();
    assert!(value.success);
    assert!(value.result.contains("public effect value"));
    let observed = engine
        .observe_thread_effect(
            &started_effect,
            "public:success:observe".into(),
            command_id(),
            value,
            &success_lease,
            now + 5,
        )
        .unwrap();
    let (parent_run_id, parent_revision) = active_run(&observed.snapshot);
    engine
        .record_thread_verification(
            parent_run_id,
            parent_revision,
            "public-thread-verification",
            &ProcessOutput {
                exit_code: Some(0),
                stdout: "public verification passed".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                termination: ProcessTermination::Exited,
            },
            &success_lease,
            now + 6,
        )
        .unwrap();
    let completed_parent = engine
        .complete_thread_verified(
            &observed.snapshot,
            "public parent verified".into(),
            "public-thread-verification".into(),
            &success_lease,
            now + 7,
        )
        .unwrap();
    assert_eq!(
        completed_parent.lifecycle,
        latte_core::ThreadLifecycle::Ready
    );

    let follow_up_id = run_id();
    let follow_up = engine
        .create_thread_follow_up_v2(
            success_thread_id,
            follow_up_id,
            completed_parent.revision,
            "public immutable follow-up",
            now + 8,
        )
        .unwrap();
    let follow_up = start_thread(
        &engine,
        &success_lease,
        &follow_up,
        "public:follow-up:start",
        now + 9,
    );
    let follow_up = commit_thread(
        &engine,
        &success_lease,
        &follow_up,
        CommitThreadRunUpdate::Fail {
            source_key: "public:follow-up:fail".into(),
            failure: RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "public follow-up terminal failure".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 10,
    );
    assert_eq!(follow_up.lifecycle, latte_core::ThreadLifecycle::Failed);
    assert_eq!(follow_up.runs.len(), 2);

    let failed_thread_id = thread_id();
    let failed_lease = engine
        .acquire_thread_lease(failed_thread_id, now + 11, 120_000)
        .unwrap();
    let failed_run_id = run_id();
    let failed_initial = engine
        .create_thread_v2(
            failed_thread_id,
            failed_run_id,
            binding(),
            "public observed failure",
            now + 11,
        )
        .unwrap();
    let failed_started = start_thread(
        &engine,
        &failed_lease,
        &failed_initial,
        "public:failed:start",
        now + 12,
    );
    let failed_prepared = prepare_read(
        &engine,
        &failed_lease,
        &failed_started,
        "public-failed-effect",
        "public-read.txt",
        now + 13,
    );
    let failed_effect = start_effect(
        &engine,
        &failed_lease,
        &failed_prepared,
        "public-failed-effect",
        now + 14,
    );
    let observed_failure = engine
        .observe_thread_effect(
            &failed_effect,
            "public:failed:observe".into(),
            command_id(),
            ThreadEffectObservedValue {
                result: r#"{"error":"public certified failure"}"#.into(),
                payload: Some(serde_json::json!({
                    "tool_call_id":"call-public-failed-effect",
                    "name":"read_file",
                    "error":"public certified failure"
                })),
                success: false,
            },
            &failed_lease,
            now + 15,
        )
        .unwrap();
    let failed_thread = commit_thread(
        &engine,
        &failed_lease,
        &observed_failure.snapshot,
        CommitThreadRunUpdate::Fail {
            source_key: "public:failed:terminal".into(),
            failure: RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "public certified effect failed".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 16,
    );
    assert_eq!(failed_thread.lifecycle, latte_core::ThreadLifecycle::Failed);

    engine.release_lease(&success_lease).unwrap();
    engine.release_lease(&failed_lease).unwrap();
    drop(engine);
    for (id, status, summary) in [
        (
            success_parent_id,
            "completed",
            Some("public parent verified"),
        ),
        (follow_up_id, "failed", None),
        (failed_run_id, "failed", None),
    ] {
        let shown = scenario.output(&["--json", "show", &id.to_string()], |_| {});
        assert_eq!(json(&shown)["data"]["run"]["status"], status);
        if let Some(summary) = summary {
            assert_eq!(json(&shown)["data"]["run"]["handoff"]["summary"], summary);
        }
    }
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 3);

    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        pty.wait_for_output(b"public certified effect failed", Duration::from_secs(5)),
        "public failure was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn public_lease_takeover_recovers_unknown_and_final_tui_reconciles_it() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(scenario.root().join("lease-read.txt"), "lease fixture\n").unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let stale = engine.acquire_thread_lease(thread_id, now, 100).unwrap();
    let run_id = run_id();
    let initial = engine
        .create_thread_v2(
            thread_id,
            run_id,
            binding(),
            "public lease recovery",
            now + 1,
        )
        .unwrap();
    let running = start_thread(&engine, &stale, &initial, "public:lease:start", now + 2);
    let prepared = prepare_read(
        &engine,
        &stale,
        &running,
        "public-unknown-effect",
        "lease-read.txt",
        now + 3,
    );
    let started = start_effect(&engine, &stale, &prepared, "public-unknown-effect", now + 4);
    let (_, started_revision) = active_run(&started.snapshot);

    let fresh = engine
        .acquire_thread_lease(thread_id, now + 200, 120_000)
        .unwrap();
    let stale_commit = engine.commit_thread_run_update(
        ThreadCommitRequest {
            thread_id,
            run_id,
            expected_thread_revision: started.snapshot.revision,
            expected_run_revision: started_revision,
            command_id: command_id(),
            request_id: None,
            effect_id: None,
            update: CommitThreadRunUpdate::AppendTranscript {
                source_key: "public:stale:must-fail".into(),
                kind: latte_core::TranscriptKind::System,
                text: "stale owner must not commit".into(),
                payload: None,
            },
        },
        &stale,
        now + 201,
    );
    assert!(matches!(stale_commit, Err(StorageError::LeaseLost)));

    let recovered = engine
        .recover_thread_after_lease_loss(thread_id, run_id, &stale, started_revision, now + 202)
        .unwrap();
    let recovered = match recovered {
        ThreadLeaseLossRecovery::Recovered(response) => response.snapshot,
        other => panic!("expected recovered lifecycle, got {other:?}"),
    };
    assert_eq!(
        recovered.lifecycle,
        latte_core::ThreadLifecycle::ReconciliationRequired
    );
    assert_eq!(
        engine.effect_status("public-unknown-effect").unwrap(),
        latte_engine::EffectStatus::Unknown
    );
    engine.release_lease(&fresh).unwrap();
    drop(engine);

    let projection = build_engine(&scenario);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Reconciliation", Duration::from_secs(5)));
    pty.write(CTRL_R);
    assert!(pty.wait_for_output(b"Ctrl+A confirm failed", Duration::from_secs(5)));
    pty.write(CTRL_A);
    assert!(
        wait_until(Duration::from_secs(5), || {
            projection
                .effect_status("public-unknown-effect")
                .is_ok_and(|status| status == latte_engine::EffectStatus::ObservedFailed)
                && projection.list_threads_v2().is_ok_and(|threads| {
                    threads.len() == 1
                        && threads[0].lifecycle == latte_core::ThreadLifecycle::Failed
                        && threads[0].pending.is_none()
                })
        }),
        "final TUI did not reconcile public fixture: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());

    drop(projection);
    let shown = scenario.output(&["--json", "show", &run_id.to_string()], |_| {});
    assert_eq!(shown.status.code(), Some(1));
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert!(
        json(&shown)["data"]["run"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("acknowledged failed")
    );
}
