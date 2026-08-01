use super::support::{PtySession, Scenario, json, wait_until};
use latte_core::{
    FailureCode, Handoff, IdSource, PendingInput, Retryability, RunFailure, RunId, SystemIdSource,
    ThreadCommandId, ThreadId, ThreadLifecycle, ThreadProviderBindingV2, TranscriptKind,
};
use latte_engine::{
    CommitThreadRunUpdate, EngineHandle, Lease, ProcessOutput, ProcessTermination, StorageError,
    ThreadCommitRequest, ThreadEffectDescriptor, ThreadEffectObservedValue, ThreadEffectPolicy,
    ThreadEffectRequest, ThreadEffectStartRequest,
};
use std::{collections::BTreeMap, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_C: &[u8] = b"\x1b[99;5u";

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
        provider_name: "v2-boundary".into(),
        provider_type: "openai-chat".into(),
        protocol: "openai-chat-completions-v1".into(),
        model: "v2-boundary-model".into(),
        config_fingerprint: "v2-boundary-config".into(),
        tools_fingerprint: "v2-boundary-tools".into(),
        aliases: BTreeMap::new(),
        credential_ref_id: "env:V2_BOUNDARY_KEY".into(),
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

fn commit_with_command(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    command_id: ThreadCommandId,
    update: CommitThreadRunUpdate,
    now: u64,
) -> Result<latte_engine::ThreadCommitResponse, StorageError> {
    let (run_id, run_revision) = active_run(snapshot);
    engine.commit_thread_run_update(
        ThreadCommitRequest {
            thread_id: snapshot.thread_id,
            run_id,
            expected_thread_revision: snapshot.revision,
            expected_run_revision: run_revision,
            command_id,
            request_id: None,
            effect_id: None,
            update,
        },
        lease,
        now,
    )
}

fn commit(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    update: CommitThreadRunUpdate,
    now: u64,
) -> latte_core::ThreadSnapshot {
    commit_with_command(engine, lease, snapshot, command_id(), update, now)
        .unwrap()
        .snapshot
}

fn start(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    source: &str,
    now: u64,
) -> latte_core::ThreadSnapshot {
    commit(
        engine,
        lease,
        snapshot,
        CommitThreadRunUpdate::Start {
            source_key: source.into(),
        },
        now,
    )
}

fn prepare_effect(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    effect_id: &str,
    name: &str,
    input: serde_json::Value,
    now: u64,
) -> latte_engine::ThreadEffectPrepared {
    let (run_id, run_revision) = active_run(snapshot);
    engine
        .prepare_thread_effect(
            ThreadEffectRequest {
                thread_id: snapshot.thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: run_revision,
                command_id: command_id(),
                source_key: format!("boundary:{effect_id}:prepare"),
                descriptor: ThreadEffectDescriptor {
                    effect_id: effect_id.into(),
                    tool_call_id: format!("call-{effect_id}"),
                    name: name.into(),
                    input,
                    attempt: 1,
                },
            },
            lease,
            now,
        )
        .unwrap()
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
                source_key: format!("boundary:{effect_id}:start"),
                effect_id: effect_id.into(),
            },
            prepared.operation_digest.clone(),
            lease,
            now,
        )
        .unwrap()
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_tui_cancels_waiting_input_and_denies_prepared_permission_without_execution() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let input_thread_id = thread_id();
    let lease = engine
        .acquire_thread_lease(input_thread_id, now, 120_000)
        .unwrap();
    let input_run_id = run_id();
    let input = engine
        .create_thread_v2(
            input_thread_id,
            input_run_id,
            binding(),
            "boundary waiting input",
            now + 1,
        )
        .unwrap();
    let input = start(&engine, &lease, &input, "boundary:input:start", now + 2);
    let input = commit(
        &engine,
        &lease,
        &input,
        CommitThreadRunUpdate::RequestInput {
            source_key: "boundary:input:request".into(),
            request: PendingInput {
                request_id: "boundary-input-request".into(),
                prompt: "Provide a boundary value".into(),
            },
        },
        now + 3,
    );
    assert_eq!(input.lifecycle, ThreadLifecycle::WaitingInput);
    let mut input_tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(input_tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        input_tui.wait_for_output(b"Input required", Duration::from_secs(5)),
        "waiting input projection was not rendered: {}",
        String::from_utf8_lossy(&input_tui.output())
    );
    assert!(input_tui.wait_for_output(b"Provide a boundary value", Duration::from_secs(5)));
    engine.release_lease(&lease).unwrap();
    drop(engine);
    input_tui.write(CTRL_C);
    assert!(wait_until(Duration::from_secs(5), || {
        let shown = scenario.output(&["--json", "show", &input_run_id.to_string()], |_| {});
        shown.status.code() == Some(1)
            && json(&shown)["data"]["run"]["status"] == "failed"
            && json(&shown)["data"]["run"]["failure"]["code"] == "cancelled"
    }));
    input_tui.write(F10);
    assert!(input_tui.finish(Duration::from_secs(5)).0.success());
    let input_shown = scenario.output(&["--json", "show", &input_run_id.to_string()], |_| {});
    assert_eq!(input_shown.status.code(), Some(1));
    assert_eq!(
        json(&input_shown)["data"]["run"]["failure"]["code"],
        "cancelled"
    );
    let input_listed = scenario.output(&["--json", "list"], |_| {});
    assert!(input_listed.status.success());
    assert_eq!(
        json(&input_listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let permission_now = latte_core::wall_time_ms();
    let permission_thread_id = thread_id();
    let lease = engine
        .acquire_thread_lease(permission_thread_id, permission_now, 120_000)
        .unwrap();
    let permission_run_id = run_id();
    let permission = engine
        .create_thread_v2(
            permission_thread_id,
            permission_run_id,
            binding(),
            "boundary permission denial",
            permission_now + 1,
        )
        .unwrap();
    let permission = start(
        &engine,
        &lease,
        &permission,
        "boundary:permission:start",
        permission_now + 2,
    );
    let prepared = prepare_effect(
        &engine,
        &lease,
        &permission,
        "boundary-denied-write",
        "write_file",
        serde_json::json!({
            "path":"must-not-exist.txt",
            "content":"denied effect must never execute\n",
            "create_intent":true
        }),
        permission_now + 3,
    );
    assert_eq!(prepared.policy, ThreadEffectPolicy::Ask);
    assert_eq!(
        prepared.snapshot.lifecycle,
        ThreadLifecycle::WaitingPermission
    );
    assert!(!scenario.root().join("must-not-exist.txt").exists());
    let mut permission_tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(permission_tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(permission_tui.wait_for_output(b"Permission required", Duration::from_secs(5)));
    assert!(permission_tui.wait_for_output(b"must-not-exist.txt", Duration::from_secs(5)));
    engine.release_lease(&lease).unwrap();
    drop(engine);
    permission_tui.write(b"d");
    assert!(wait_until(Duration::from_secs(5), || {
        let shown = scenario.output(&["--json", "show", &permission_run_id.to_string()], |_| {});
        shown.status.code() == Some(11)
            && json(&shown)["data"]["run"]["status"] == "failed"
            && json(&shown)["data"]["run"]["failure"]["code"] == "permission_denied"
    }));
    assert!(!scenario.root().join("must-not-exist.txt").exists());
    permission_tui.write(F10);
    assert!(permission_tui.finish(Duration::from_secs(5)).0.success());

    let shown = scenario.output(&["--json", "show", &permission_run_id.to_string()], |_| {});
    assert_eq!(shown.status.code(), Some(11));
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert_eq!(
        json(&shown)["data"]["run"]["failure"]["code"],
        "permission_denied"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn failed_effect_verification_and_interrupted_child_tree_are_final_binary_visible() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(
        scenario.root().join("boundary-read.txt"),
        "boundary fixture\n",
    )
    .unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let effect_thread_id = thread_id();
    let effect_lease = engine
        .acquire_thread_lease(effect_thread_id, now, 120_000)
        .unwrap();

    let effect_run_id = run_id();
    let effect = engine
        .create_thread_v2(
            effect_thread_id,
            effect_run_id,
            binding(),
            "boundary observed failure",
            now + 1,
        )
        .unwrap();
    let effect = start(
        &engine,
        &effect_lease,
        &effect,
        "boundary:effect:start",
        now + 2,
    );
    let prepared = prepare_effect(
        &engine,
        &effect_lease,
        &effect,
        "boundary-observed-failure",
        "read_file",
        serde_json::json!({"path":"boundary-read.txt"}),
        now + 3,
    );
    assert_eq!(prepared.policy, ThreadEffectPolicy::Allow);
    let started = start_effect(
        &engine,
        &effect_lease,
        &prepared,
        "boundary-observed-failure",
        now + 4,
    );
    let observed = engine
        .observe_thread_effect(
            &started,
            "boundary:effect:observe-failed".into(),
            command_id(),
            ThreadEffectObservedValue {
                result: r#"{"error":"certified boundary read failure"}"#.into(),
                payload: Some(serde_json::json!({
                    "tool_call_id":"call-boundary-observed-failure",
                    "name":"read_file",
                    "error":"certified boundary read failure"
                })),
                success: false,
            },
            &effect_lease,
            now + 5,
        )
        .unwrap();
    let effect_failed = commit(
        &engine,
        &effect_lease,
        &observed.snapshot,
        CommitThreadRunUpdate::Fail {
            source_key: "boundary:effect:terminal".into(),
            failure: RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "boundary observed effect failed".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 6,
    );
    assert_eq!(effect_failed.lifecycle, ThreadLifecycle::Failed);

    let tree_thread_id = thread_id();
    let tree_lease = engine
        .acquire_thread_lease(tree_thread_id, now + 7, 120_000)
        .unwrap();
    let parent_run_id = run_id();
    let parent = engine
        .create_thread_v2(
            tree_thread_id,
            parent_run_id,
            binding(),
            "boundary immutable parent",
            now + 7,
        )
        .unwrap();
    let parent = start(
        &engine,
        &tree_lease,
        &parent,
        "boundary:parent:start",
        now + 8,
    );
    let parent = commit(
        &engine,
        &tree_lease,
        &parent,
        CommitThreadRunUpdate::Complete {
            source_key: "boundary:parent:complete".into(),
            handoff: Handoff {
                summary: "boundary parent completed".into(),
                files_changed: Vec::new(),
                evidence: Vec::new(),
            },
        },
        now + 9,
    );
    assert_eq!(parent.lifecycle, ThreadLifecycle::Ready);
    let immutable_parent = parent.runs[0].clone();
    let interrupted_run_id = run_id();
    let child = engine
        .create_thread_follow_up_v2(
            tree_thread_id,
            interrupted_run_id,
            parent.revision,
            "boundary interrupted follow-up",
            now + 10,
        )
        .unwrap();
    let child = start(
        &engine,
        &tree_lease,
        &child,
        "boundary:child:start",
        now + 11,
    );
    let child = commit(
        &engine,
        &tree_lease,
        &child,
        CommitThreadRunUpdate::Interrupt {
            source_key: "boundary:child:interrupt".into(),
            reconciliation_effect_id: None,
        },
        now + 12,
    );
    assert_eq!(child.lifecycle, ThreadLifecycle::Interrupted);
    assert_eq!(child.runs[0], immutable_parent);
    assert_eq!(child.runs[1].parent_run_id, Some(parent_run_id));

    let verification_thread_id = thread_id();
    let verification_lease = engine
        .acquire_thread_lease(verification_thread_id, now + 13, 120_000)
        .unwrap();
    let verification_run_id = run_id();
    let verification = engine
        .create_thread_v2(
            verification_thread_id,
            verification_run_id,
            binding(),
            "boundary failed verification",
            now + 13,
        )
        .unwrap();
    let verification = start(
        &engine,
        &verification_lease,
        &verification,
        "boundary:verification:start",
        now + 14,
    );
    let (_, verification_revision) = active_run(&verification);
    engine
        .record_thread_verification(
            verification_run_id,
            verification_revision,
            "boundary-verification-failed",
            &ProcessOutput {
                exit_code: Some(9),
                stdout: "verification stdout sentinel".into(),
                stderr: "verification stderr sentinel".into(),
                stdout_truncated: false,
                stderr_truncated: false,
                termination: ProcessTermination::Exited,
            },
            &verification_lease,
            now + 15,
        )
        .unwrap();
    let completion_error = engine
        .complete_thread_verified(
            &verification,
            "must not complete".into(),
            "boundary-verification-failed".into(),
            &verification_lease,
            now + 16,
        )
        .unwrap_err();
    assert!(
        completion_error
            .to_string()
            .contains("passing verification evidence"),
        "unexpected verified-completion rejection: {completion_error}"
    );
    let verification_failed = commit(
        &engine,
        &verification_lease,
        &verification,
        CommitThreadRunUpdate::Fail {
            source_key: "boundary:verification:terminal".into(),
            failure: RunFailure {
                code: FailureCode::VerificationFailed,
                message: "boundary verification failed with exit 9".into(),
                retryability: Retryability::Terminal,
            },
        },
        now + 17,
    );
    assert_eq!(verification_failed.lifecycle, ThreadLifecycle::Failed);
    engine.release_lease(&effect_lease).unwrap();
    engine.release_lease(&tree_lease).unwrap();
    engine.release_lease(&verification_lease).unwrap();
    drop(engine);

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        tui.wait_for_output(
            b"boundary verification failed with exit 9",
            Duration::from_secs(5)
        ),
        "latest negative projection was not rendered: {}",
        String::from_utf8_lossy(&tui.output())
    );
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());

    for (id, code, status, message) in [
        (
            effect_run_id,
            Some(1),
            "failed",
            Some("boundary observed effect failed"),
        ),
        (parent_run_id, Some(0), "completed", None),
        (interrupted_run_id, Some(130), "interrupted", None),
        (
            verification_run_id,
            Some(1),
            "failed",
            Some("boundary verification failed with exit 9"),
        ),
    ] {
        let shown = scenario.output(&["--json", "show", &id.to_string()], |_| {});
        assert_eq!(shown.status.code(), code);
        assert_eq!(json(&shown)["data"]["run"]["status"], status);
        if let Some(message) = message {
            assert_eq!(json(&shown)["data"]["run"]["failure"]["message"], message);
        }
    }
    let parent_shown = scenario.output(&["--json", "show", &parent_run_id.to_string()], |_| {});
    assert_eq!(
        json(&parent_shown)["data"]["run"]["handoff"]["summary"],
        "boundary parent completed"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 4);
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn command_revision_and_lease_fences_preserve_paged_projection_in_final_binary() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let run_id = run_id();
    let initial = engine
        .create_thread_v2(
            thread_id,
            run_id,
            binding(),
            "boundary command and lease fencing",
            now + 1,
        )
        .unwrap();
    let started = start(&engine, &lease, &initial, "boundary:fence:start", now + 2);
    let (_, started_run_revision) = active_run(&started);
    let fixed_request = ThreadCommitRequest {
        thread_id,
        run_id,
        expected_thread_revision: started.revision,
        expected_run_revision: started_run_revision,
        command_id: command_id(),
        request_id: None,
        effect_id: None,
        update: CommitThreadRunUpdate::AppendTranscript {
            source_key: "boundary:fence:history:00".into(),
            kind: TranscriptKind::System,
            text: "boundary history 00".into(),
            payload: Some(serde_json::json!({"ordinal":0,"stable":true})),
        },
    };
    let first = engine
        .commit_thread_run_update(fixed_request.clone(), &lease, now + 3)
        .unwrap();
    assert_eq!(first.snapshot.revision, started.revision + 1);
    assert_eq!(first.snapshot.sequence, started.sequence + 1);

    let replay = engine
        .commit_thread_run_update(fixed_request.clone(), &lease, now + 4)
        .unwrap();
    assert_eq!(replay, first);
    let after_replay = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_replay.revision, first.snapshot.revision);
    assert_eq!(after_replay.sequence, first.snapshot.sequence);
    assert_eq!(
        after_replay
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.text == "boundary history 00")
            .count(),
        1
    );

    let mut conflicting_replay = fixed_request.clone();
    let CommitThreadRunUpdate::AppendTranscript { text, payload, .. } =
        &mut conflicting_replay.update
    else {
        unreachable!()
    };
    *text = "conflicting replay must not persist".into();
    *payload = Some(serde_json::json!({"ordinal":0,"stable":false}));
    assert!(matches!(
        engine.commit_thread_run_update(conflicting_replay, &lease, now + 5),
        Err(StorageError::ThreadCommandReplayMismatch)
    ));

    let (_, current_run_revision) = active_run(&first.snapshot);
    let stale_thread = ThreadCommitRequest {
        thread_id,
        run_id,
        expected_thread_revision: first.snapshot.revision - 1,
        expected_run_revision: current_run_revision,
        command_id: command_id(),
        request_id: None,
        effect_id: None,
        update: CommitThreadRunUpdate::AppendTranscript {
            source_key: "boundary:fence:stale-thread".into(),
            kind: TranscriptKind::System,
            text: "stale thread revision must not persist".into(),
            payload: None,
        },
    };
    assert!(matches!(
        engine.commit_thread_run_update(stale_thread, &lease, now + 6),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    let stale_run = ThreadCommitRequest {
        thread_id,
        run_id,
        expected_thread_revision: first.snapshot.revision,
        expected_run_revision: current_run_revision - 1,
        command_id: command_id(),
        request_id: None,
        effect_id: None,
        update: CommitThreadRunUpdate::AppendTranscript {
            source_key: "boundary:fence:stale-run".into(),
            kind: TranscriptKind::System,
            text: "stale run revision must not persist".into(),
            payload: None,
        },
    };
    assert!(matches!(
        engine.commit_thread_run_update(stale_run, &lease, now + 7),
        Err(StorageError::StaleRevision { .. })
    ));
    let after_rejections = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_rejections.revision, first.snapshot.revision);
    assert_eq!(after_rejections.sequence, first.snapshot.sequence);
    assert!(!after_rejections.transcript.entries.iter().any(|entry| {
        entry.text == "conflicting replay must not persist"
            || entry.text == "stale thread revision must not persist"
            || entry.text == "stale run revision must not persist"
    }));

    let mut current = after_rejections;
    for ordinal in 1_u64..=11 {
        current = commit(
            &engine,
            &lease,
            &current,
            CommitThreadRunUpdate::AppendTranscript {
                source_key: format!("boundary:fence:history:{ordinal:02}"),
                kind: TranscriptKind::System,
                text: format!("boundary history {ordinal:02}"),
                payload: Some(serde_json::json!({"ordinal":ordinal,"stable":true})),
            },
            now + 10 + ordinal,
        );
    }
    let authoritative_revision = current.revision;
    let authoritative_sequence = current.sequence;
    let (_, authoritative_run_revision) = active_run(&current);

    let zero_limit = engine.thread_snapshot_v2(thread_id, None, 0).unwrap();
    assert_eq!(zero_limit.transcript.entries.len(), 1);
    assert!(zero_limit.transcript.has_more);
    let mut after = None;
    let mut texts = Vec::new();
    let mut pages = 0;
    loop {
        let page = engine.thread_snapshot_v2(thread_id, after, 3).unwrap();
        pages += 1;
        texts.extend(
            page.transcript
                .entries
                .iter()
                .map(|entry| entry.text.clone()),
        );
        if !page.transcript.has_more {
            break;
        }
        after = page.transcript.next_after;
        assert!(after.is_some());
    }
    assert_eq!(pages, 5);
    assert_eq!(texts.len(), 13);
    assert_eq!(texts.first().unwrap(), "boundary command and lease fencing");
    assert_eq!(texts[1], "boundary history 00");
    assert_eq!(texts.last().unwrap(), "boundary history 11");
    assert_eq!(
        texts
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        13
    );

    let independent_runtime = engine
        .acquire_lease("v2-live-foreign", now + 100, 120_000)
        .unwrap();
    assert_ne!(independent_runtime.fencing_token(), lease.fencing_token());
    engine.release_lease(&independent_runtime).unwrap();
    let expired_request = ThreadCommitRequest {
        thread_id,
        run_id,
        expected_thread_revision: authoritative_revision,
        expected_run_revision: authoritative_run_revision,
        command_id: command_id(),
        request_id: None,
        effect_id: None,
        update: CommitThreadRunUpdate::AppendTranscript {
            source_key: "boundary:fence:expired-owner".into(),
            kind: TranscriptKind::System,
            text: "expired owner must not persist".into(),
            payload: None,
        },
    };
    assert!(matches!(
        engine.commit_thread_run_update(expired_request, &lease, now + 120_001),
        Err(StorageError::LeaseLost)
    ));
    let after_expiry = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_expiry.revision, authoritative_revision);
    assert_eq!(after_expiry.sequence, authoritative_sequence);
    assert!(
        !after_expiry
            .transcript
            .entries
            .iter()
            .any(|entry| entry.text == "expired owner must not persist")
    );

    let shown = scenario.output(&["--json", "show", &run_id.to_string()], |_| {});
    assert_eq!(shown.status.code(), Some(70));
    assert_eq!(json(&shown)["data"]["run"]["status"], "running");
    assert_eq!(
        json(&shown)["data"]["run"]["revision"],
        authoritative_run_revision
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_runs = json(&listed)["data"]["runs"].as_array().unwrap().clone();
    assert_eq!(listed_runs.len(), 1);
    assert_eq!(listed_runs[0]["run_id"], run_id.to_string());
    assert_eq!(listed_runs[0]["status"], "running");

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        tui.wait_for_output(b"boundary history 11", Duration::from_secs(5)),
        "final TUI omitted authoritative transcript tail: {}",
        String::from_utf8_lossy(&tui.output())
    );
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
    engine.release_lease(&lease).unwrap();
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn wrong_effect_identity_digest_and_observation_authority_never_mutate_final_projection() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(
        scenario.root().join("effect-boundary.txt"),
        "effect authority sentinel\n",
    )
    .unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let run_id = run_id();
    let initial = engine
        .create_thread_v2(
            thread_id,
            run_id,
            binding(),
            "boundary effect authority",
            now + 1,
        )
        .unwrap();
    let running = start(
        &engine,
        &lease,
        &initial,
        "boundary:effect-authority:start",
        now + 2,
    );
    let effect_id = "boundary-authorized-read";
    let prepared = prepare_effect(
        &engine,
        &lease,
        &running,
        effect_id,
        "read_file",
        serde_json::json!({"path":"effect-boundary.txt"}),
        now + 3,
    );
    assert_eq!(prepared.policy, ThreadEffectPolicy::Allow);
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Prepared
    );
    let prepared_revision = prepared.snapshot.revision;
    let prepared_sequence = prepared.snapshot.sequence;
    let (prepared_run_id, prepared_run_revision) = active_run(&prepared.snapshot);
    assert_eq!(prepared_run_id, run_id);

    let start_request = ThreadEffectStartRequest {
        thread_id,
        run_id,
        expected_thread_revision: prepared_revision,
        expected_run_revision: prepared_run_revision,
        command_id: command_id(),
        source_key: "boundary:effect-authority:start-exact".into(),
        effect_id: effect_id.into(),
    };
    let wrong_digest = engine
        .start_thread_effect(start_request.clone(), "0".repeat(64), &lease, now + 4)
        .unwrap_err();
    assert!(
        wrong_digest
            .to_string()
            .contains("canonical thread effect digest mismatch")
    );
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Prepared
    );
    let after_wrong_digest = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_wrong_digest.revision, prepared_revision);
    assert_eq!(after_wrong_digest.sequence, prepared_sequence);

    let wrong_id = engine.start_thread_effect(
        ThreadEffectStartRequest {
            effect_id: "boundary-missing-effect".into(),
            command_id: command_id(),
            source_key: "boundary:effect-authority:wrong-id".into(),
            ..start_request.clone()
        },
        prepared.operation_digest.clone(),
        &lease,
        now + 5,
    );
    assert!(wrong_id.is_err());
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Prepared
    );
    let after_wrong_id = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_wrong_id.revision, prepared_revision);
    assert_eq!(after_wrong_id.sequence, prepared_sequence);

    let foreign_scenario = Scenario::new();
    let foreign_engine = build_engine(&foreign_scenario);
    let foreign = foreign_engine
        .acquire_lease("v2-effect-foreign", now + 6, 120_000)
        .unwrap();
    let foreign_start = engine.start_thread_effect(
        ThreadEffectStartRequest {
            command_id: command_id(),
            source_key: "boundary:effect-authority:foreign-start".into(),
            ..start_request.clone()
        },
        prepared.operation_digest.clone(),
        &foreign,
        now + 7,
    );
    assert!(foreign_start.is_err());
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Prepared
    );
    let after_foreign_start = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_foreign_start.revision, prepared_revision);
    assert_eq!(after_foreign_start.sequence, prepared_sequence);

    let started = engine
        .start_thread_effect(
            start_request,
            prepared.operation_digest.clone(),
            &lease,
            now + 8,
        )
        .unwrap();
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Started
    );
    let started_revision = started.snapshot.revision;
    let started_sequence = started.snapshot.sequence;
    let observed_value = ThreadEffectObservedValue {
        result: r#"{"value":"must remain uncertified"}"#.into(),
        payload: Some(serde_json::json!({
            "tool_call_id":"call-boundary-authorized-read",
            "name":"read_file",
            "value":"must remain uncertified"
        })),
        success: true,
    };
    assert!(matches!(
        engine.observe_thread_effect(
            &started,
            "boundary:effect-authority:foreign-observe".into(),
            command_id(),
            observed_value.clone(),
            &foreign,
            now + 9,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Started
    );
    let after_foreign_observe = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_foreign_observe.revision, started_revision);
    assert_eq!(after_foreign_observe.sequence, started_sequence);

    assert!(matches!(
        engine.observe_thread_effect(
            &started,
            "boundary:effect-authority:expired-observe".into(),
            command_id(),
            observed_value,
            &lease,
            now + 120_001,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert_eq!(
        engine.effect_status(effect_id).unwrap(),
        latte_engine::EffectStatus::Started
    );
    let after_expired_observe = engine.thread_snapshot_v2(thread_id, None, 500).unwrap();
    assert_eq!(after_expired_observe.revision, started_revision);
    assert_eq!(after_expired_observe.sequence, started_sequence);
    assert!(
        !after_expired_observe
            .transcript
            .entries
            .iter()
            .any(|entry| {
                entry.text.contains("must remain uncertified")
                    || entry.text.contains("foreign-observe")
                    || entry.text.contains("expired-observe")
            })
    );

    let shown = scenario.output(&["--json", "show", &run_id.to_string()], |_| {});
    assert_eq!(shown.status.code(), Some(70));
    assert_eq!(json(&shown)["data"]["run"]["status"], "running");
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 1);
    assert_eq!(
        json(&listed)["data"]["runs"][0]["run_id"],
        run_id.to_string()
    );
    assert_eq!(json(&listed)["data"]["runs"][0]["status"], "running");

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        tui.wait_for_output(b"effect-boundary.txt", Duration::from_secs(5)),
        "final TUI omitted the still-started authorized descriptor: {}",
        String::from_utf8_lossy(&tui.output())
    );
    assert!(
        !tui.output()
            .windows(b"must remain uncertified".len())
            .any(|window| window == b"must remain uncertified")
    );
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
    engine.release_lease(&lease).unwrap();
    foreign_engine.release_lease(&foreign).unwrap();
}
