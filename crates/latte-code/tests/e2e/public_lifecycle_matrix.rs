use super::support::{PtySession, Scenario, json, wait_until};
use latte_core::{
    FailureCode, IdSource, Retryability, SessionCommandId, SessionId, SessionProviderBinding,
    SystemIdSource, TurnFailure, TurnId,
};
use latte_engine::{
    CancellationToken, CommitSessionTurnUpdate, EngineHandle, Lease, ProcessOutput,
    ProcessTermination, SessionCommitRequest, SessionEffectDescriptor, SessionEffectObservedValue,
    SessionEffectPolicy, SessionEffectRequest, SessionEffectStartRequest, SessionLeaseLossRecovery,
    StorageError,
};
use std::{collections::BTreeMap, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_A: &[u8] = b"\x1b[97;5u";
const CTRL_R: &[u8] = b"\x1b[114;5u";

fn turn_id() -> TurnId {
    TurnId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn session_id() -> SessionId {
    SessionId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn command_id() -> SessionCommandId {
    SessionCommandId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn binding() -> SessionProviderBinding {
    SessionProviderBinding {
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

fn active_turn(snapshot: &latte_core::SessionSnapshot) -> (TurnId, u64) {
    let turn_id = snapshot.active_turn_id.unwrap();
    let revision = snapshot
        .turns
        .iter()
        .find(|run| run.turn_id == turn_id)
        .unwrap()
        .turn_revision;
    (turn_id, revision)
}

fn commit_session(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::SessionSnapshot,
    update: CommitSessionTurnUpdate,
    now: u64,
) -> latte_core::SessionSnapshot {
    let (turn_id, turn_revision) = active_turn(snapshot);
    engine
        .commit_session_turn_update(
            SessionCommitRequest {
                session_id: snapshot.session_id,
                turn_id,
                expected_session_revision: snapshot.revision,
                expected_turn_revision: turn_revision,
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

fn start_session(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::SessionSnapshot,
    source: &str,
    now: u64,
) -> latte_core::SessionSnapshot {
    commit_session(
        engine,
        lease,
        snapshot,
        CommitSessionTurnUpdate::Start {
            source_key: source.into(),
        },
        now,
    )
}

fn prepare_read(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::SessionSnapshot,
    effect_id: &str,
    path: &str,
    now: u64,
) -> latte_engine::SessionEffectPrepared {
    let (turn_id, turn_revision) = active_turn(snapshot);
    let prepared = engine
        .prepare_session_effect(
            SessionEffectRequest {
                session_id: snapshot.session_id,
                turn_id,
                expected_session_revision: snapshot.revision,
                expected_turn_revision: turn_revision,
                command_id: command_id(),
                source_key: format!("public:{effect_id}:prepare"),
                descriptor: SessionEffectDescriptor {
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
    assert_eq!(prepared.policy, SessionEffectPolicy::Allow);
    prepared
}

fn start_effect(
    engine: &EngineHandle,
    lease: &Lease,
    prepared: &latte_engine::SessionEffectPrepared,
    effect_id: &str,
    now: u64,
) -> latte_engine::SessionEffectStarted {
    let (turn_id, turn_revision) = active_turn(&prepared.snapshot);
    engine
        .start_session_effect(
            SessionEffectStartRequest {
                session_id: prepared.snapshot.session_id,
                turn_id,
                expected_session_revision: prepared.snapshot.revision,
                expected_turn_revision: turn_revision,
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
async fn public_session_effects_verification_and_follow_up_render_through_final_binary() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(
        scenario.root().join("public-read.txt"),
        "public effect value\n",
    )
    .unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let success_session_id = session_id();
    let success_lease = engine
        .acquire_session_lease(success_session_id, now, 120_000)
        .unwrap();

    let success_parent_id = turn_id();
    let initial = engine
        .create_session_v2(
            success_session_id,
            success_parent_id,
            binding(),
            "public effect and verification",
            now + 1,
        )
        .unwrap();
    let started_snapshot = start_session(
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
        .execute_started_session_effect(&started_effect, &success_lease, &CancellationToken::new())
        .await
        .unwrap();
    assert!(value.success);
    assert!(value.result.contains("public effect value"));
    let observed = engine
        .observe_session_effect(
            &started_effect,
            "public:success:observe".into(),
            command_id(),
            value,
            &success_lease,
            now + 5,
        )
        .unwrap();
    let (parent_turn_id, parent_revision) = active_turn(&observed.snapshot);
    engine
        .record_session_verification(
            parent_turn_id,
            parent_revision,
            "public-session-verification",
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
        .complete_session_verified(
            &observed.snapshot,
            "public parent verified".into(),
            "public-session-verification".into(),
            &success_lease,
            now + 7,
        )
        .unwrap();
    assert_eq!(
        completed_parent.lifecycle,
        latte_core::SessionLifecycle::Ready
    );

    // The verified parent Complete released `success_lease` in its terminal
    // commit; the follow-up turn runs under its own fresh coordinator lease.
    engine.release_lease(&success_lease).unwrap();
    let success_lease = engine
        .acquire_session_lease(success_session_id, now + 8, 120_000)
        .unwrap();

    let follow_up_id = turn_id();
    let follow_up = engine
        .create_session_follow_up_v2(
            success_session_id,
            follow_up_id,
            completed_parent.revision,
            "public immutable follow-up",
            now + 8,
        )
        .unwrap();
    let follow_up = start_session(
        &engine,
        &success_lease,
        &follow_up,
        "public:follow-up:start",
        now + 9,
    );
    let follow_up = commit_session(
        &engine,
        &success_lease,
        &follow_up,
        CommitSessionTurnUpdate::Fail {
            source_key: "public:follow-up:fail".into(),
            failure: TurnFailure {
                code: FailureCode::RuntimeFailed,
                message: "public follow-up terminal failure".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 10,
    );
    assert_eq!(follow_up.lifecycle, latte_core::SessionLifecycle::Failed);
    assert_eq!(follow_up.turns.len(), 2);

    let failed_session_id = session_id();
    let failed_lease = engine
        .acquire_session_lease(failed_session_id, now + 11, 120_000)
        .unwrap();
    let failed_turn_id = turn_id();
    let failed_initial = engine
        .create_session_v2(
            failed_session_id,
            failed_turn_id,
            binding(),
            "public observed failure",
            now + 11,
        )
        .unwrap();
    let failed_started = start_session(
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
        .observe_session_effect(
            &failed_effect,
            "public:failed:observe".into(),
            command_id(),
            SessionEffectObservedValue {
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
    let failed_session = commit_session(
        &engine,
        &failed_lease,
        &observed_failure.snapshot,
        CommitSessionTurnUpdate::Fail {
            source_key: "public:failed:terminal".into(),
            failure: TurnFailure {
                code: FailureCode::RuntimeFailed,
                message: "public certified effect failed".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 16,
    );
    assert_eq!(
        failed_session.lifecycle,
        latte_core::SessionLifecycle::Failed
    );

    engine.release_lease(&success_lease).unwrap();
    engine.release_lease(&failed_lease).unwrap();
    drop(engine);
    let success_shown =
        scenario.output(&["--json", "show", &success_session_id.to_string()], |_| {});
    assert!(success_shown.status.success());
    let success_session = json(&success_shown)["data"]["session"].clone();
    assert_eq!(success_session["lifecycle"], "failed");
    let success_turns = success_session["turns"].as_array().unwrap();
    assert_eq!(success_turns.len(), 2);
    assert_eq!(success_turns[0]["turn_id"], success_parent_id.to_string());
    assert_eq!(success_turns[0]["status"], "completed");
    assert_eq!(success_turns[1]["turn_id"], follow_up_id.to_string());
    assert_eq!(success_turns[1]["status"], "failed");
    assert!(
        success_session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "completion"
                && entry["text"] == "public parent verified")
    );

    let failed_shown = scenario.output(&["--json", "show", &failed_session_id.to_string()], |_| {});
    assert!(failed_shown.status.success());
    let failed_session = json(&failed_shown)["data"]["session"].clone();
    assert_eq!(failed_session["lifecycle"], "failed");
    assert_eq!(
        failed_session["turns"][0]["turn_id"],
        failed_turn_id.to_string()
    );
    assert_eq!(failed_session["turns"][0]["status"], "failed");

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        2
    );
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(format!("/resume {failed_session_id}\r").as_bytes());
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
    let session_id = session_id();
    let stale = engine.acquire_session_lease(session_id, now, 100).unwrap();
    let turn_id = turn_id();
    let initial = engine
        .create_session_v2(
            session_id,
            turn_id,
            binding(),
            "public lease recovery",
            now + 1,
        )
        .unwrap();
    let running = start_session(&engine, &stale, &initial, "public:lease:start", now + 2);
    let prepared = prepare_read(
        &engine,
        &stale,
        &running,
        "public-unknown-effect",
        "lease-read.txt",
        now + 3,
    );
    let started = start_effect(&engine, &stale, &prepared, "public-unknown-effect", now + 4);
    let (_, started_revision) = active_turn(&started.snapshot);

    let fresh = engine
        .acquire_session_lease(session_id, now + 200, 120_000)
        .unwrap();
    let stale_commit = engine.commit_session_turn_update(
        SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: started.snapshot.revision,
            expected_turn_revision: started_revision,
            command_id: command_id(),
            request_id: None,
            effect_id: None,
            update: CommitSessionTurnUpdate::AppendTranscript {
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
        .recover_session_after_lease_loss(session_id, turn_id, &stale, started_revision, now + 202)
        .unwrap();
    let recovered = match recovered {
        SessionLeaseLossRecovery::Recovered(response) => response.snapshot,
        other => panic!("expected recovered lifecycle, got {other:?}"),
    };
    assert_eq!(
        recovered.lifecycle,
        latte_core::SessionLifecycle::ReconciliationRequired
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
    pty.write(format!("/resume {session_id}\r").as_bytes());
    assert!(pty.wait_for_output(b"Reconciliation", Duration::from_secs(5)));
    pty.write(CTRL_R);
    assert!(pty.wait_for_output(b"Ctrl+A confirm failed", Duration::from_secs(5)));
    pty.write(CTRL_A);
    assert!(
        wait_until(Duration::from_secs(5), || {
            projection
                .effect_status("public-unknown-effect")
                .is_ok_and(|status| status == latte_engine::EffectStatus::ObservedFailed)
                && projection.list_sessions().is_ok_and(|sessions| {
                    sessions.len() == 1
                        && sessions[0].lifecycle == latte_core::SessionLifecycle::Failed
                        && sessions[0].pending.is_none()
                })
        }),
        "final TUI did not reconcile public fixture: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());

    drop(projection);
    let shown = scenario.output(&["--json", "show", &session_id.to_string()], |_| {});
    assert!(shown.status.success());
    let session = json(&shown)["data"]["session"].clone();
    assert_eq!(session["lifecycle"], "failed");
    assert!(
        session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "failure"
                && entry["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("acknowledged failed"))
    );
}
