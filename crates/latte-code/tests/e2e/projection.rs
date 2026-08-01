use super::support::{ProviderReply, PtySession, Scenario, ScriptedProvider, wait_until};
use latte_core::{
    Evidence, FailureCode, Handoff, IdSource, PendingPermission, Retryability, RunFailure,
    SystemIdSource, ThreadCommandId, ThreadId, ThreadProviderBindingV2, TranscriptKind,
    VerificationStatus,
};
use latte_engine::{
    CommitThreadRunUpdate, EngineHandle, Lease, ThreadCommitRequest, ThreadEffectDescriptor,
    ThreadEffectPolicy,
};
use std::{collections::BTreeMap, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_P: &[u8] = b"\x1b[112;5u";

fn binding() -> ThreadProviderBindingV2 {
    ThreadProviderBindingV2 {
        version: 1,
        provider_name: "fixture".into(),
        provider_type: "openai-chat".into(),
        protocol: "openai-chat-completions-v1".into(),
        model: "projection-model".into(),
        config_fingerprint: "fixture-config".into(),
        tools_fingerprint: "fixture-tools".into(),
        aliases: BTreeMap::new(),
        credential_ref_id: "env:FIXTURE_KEY".into(),
        data_scope_id: "workspace".into(),
        credential_generation: 1,
    }
}

fn fixture_engine(scenario: &Scenario) -> (EngineHandle, Lease, latte_core::ThreadSnapshot) {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let ids = SystemIdSource::default();
    let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
    let lease = engine
        .acquire_thread_lease(thread_id, 1_000, 60_000)
        .unwrap();
    let snapshot = engine
        .create_thread_v2(
            thread_id,
            latte_core::RunId::from_uuid(ids.next_uuid_v7()),
            binding(),
            "fixture prompt",
            1_001,
        )
        .unwrap();
    (engine, lease, snapshot)
}

fn commit(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
    update: CommitThreadRunUpdate,
    now_ms: u64,
) -> latte_core::ThreadSnapshot {
    let run_id = snapshot.latest_run_id.unwrap();
    let run_revision = snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;
    engine
        .commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: snapshot.thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: run_revision,
                command_id: ThreadCommandId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update,
            },
            lease,
            now_ms,
        )
        .unwrap()
        .snapshot
}

fn start(
    engine: &EngineHandle,
    lease: &Lease,
    snapshot: &latte_core::ThreadSnapshot,
) -> latte_core::ThreadSnapshot {
    commit(
        engine,
        lease,
        snapshot,
        CommitThreadRunUpdate::Start {
            source_key: "fixture:start".into(),
        },
        1_002,
    )
}

fn finish_fixture(engine: EngineHandle, lease: &Lease) {
    engine.release_lease(lease).unwrap();
    drop(engine);
}

fn render_fixture(scenario: &Scenario, expected: &[u8], rows: u16, columns: u16) -> PtySession {
    scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let pty = PtySession::spawn_with_size(scenario.command(&["tui"]), rows, columns);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        pty.wait_for_output(expected, Duration::from_secs(5)),
        "fixture was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn rich_completed_projection_renders_tool_pair_failure_and_handoff_evidence() {
    let scenario = Scenario::new();
    let (engine, lease, initial) = fixture_engine(&scenario);
    let mut snapshot = start(&engine, &lease, &initial);
    for (index, (kind, text, payload)) in [
        (
            TranscriptKind::Assistant,
            "I will inspect and verify the projection.",
            None,
        ),
        (
            TranscriptKind::ToolCall,
            "inspect a rich process action",
            Some(serde_json::json!({"descriptor": {
                "tool_call_id": "rich-call",
                "effect_id": "rich-effect",
                "name": "process",
                "input": {
                    "path": "src/example.rs",
                    "query": "projection needle",
                    "cwd": ".",
                    "argv": ["/bin/sh", "-c", "printf projection-ok"]
                }
            }})),
        ),
        (
            TranscriptKind::System,
            "process started",
            Some(serde_json::json!({"status":"started","effect_id":"rich-effect"})),
        ),
        (
            TranscriptKind::ToolResult,
            "projection-ok",
            Some(serde_json::json!({"tool_call_id":"rich-call","name":"process"})),
        ),
        (
            TranscriptKind::ToolResult,
            "unpaired tool failure",
            Some(serde_json::json!({"tool_call_id":"orphan-call","name":"search","error":"not found"})),
        ),
        (TranscriptKind::Permission, "permission audit card", None),
        (TranscriptKind::Input, "input audit card", None),
        (TranscriptKind::Failure, "recoverable audit failure", None),
        (TranscriptKind::System, "durable system note", None),
    ]
    .into_iter()
    .enumerate()
    {
        snapshot = commit(
            &engine,
            &lease,
            &snapshot,
            CommitThreadRunUpdate::AppendTranscript {
                source_key: format!("fixture:card:{index}"),
                kind,
                text: text.into(),
                payload,
            },
            1_010 + u64::try_from(index).unwrap(),
        );
    }
    snapshot = commit(
        &engine,
        &lease,
        &snapshot,
        CommitThreadRunUpdate::Complete {
            source_key: "fixture:complete".into(),
            handoff: Handoff {
                summary: "rich completion summary".into(),
                files_changed: vec!["src/example.rs".into(), "docs/projection.md".into()],
                evidence: vec![
                    Evidence {
                        name: "unit tests".into(),
                        status: VerificationStatus::Passed,
                        summary: "95 percent".into(),
                    },
                    Evidence {
                        name: "lint".into(),
                        status: VerificationStatus::Failed,
                        summary: "fixture failure".into(),
                    },
                    Evidence {
                        name: "benchmark".into(),
                        status: VerificationStatus::NotRun,
                        summary: String::new(),
                    },
                ],
            },
        },
        1_030,
    );
    assert_eq!(snapshot.lifecycle, latte_core::ThreadLifecycle::Ready);
    assert_eq!(
        snapshot.runs[0].status,
        latte_core::ThreadRunStatus::Completed
    );
    finish_fixture(engine, &lease);

    let mut pty = render_fixture(&scenario, b"rich completion summary", 40, 120);
    // Palette Navigation avoids terminal Escape ambiguity. Expand/collapse
    // both projected actions so metadata and success/failure details render.
    pty.write(CTRL_P);
    pty.write(b"j\r");
    pty.write(b"\r\x1b[C\x1b[D j \r");
    assert!(pty.wait_for_output(b"Command", Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"unpaired tool failure", Duration::from_secs(5)));
    pty.write(F10);
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert!(
        output
            .windows(b"CHANGED".len())
            .any(|value| value == b"CHANGED")
    );
    assert!(
        output
            .windows(b"VERIFIED".len())
            .any(|value| value == b"VERIFIED")
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn seeded_lifecycle_matrix_is_visible_through_the_final_tui() {
    for state in ["failed", "interrupted", "reconciliation"] {
        let scenario = Scenario::new();
        let (engine, lease, initial) = fixture_engine(&scenario);
        let mut snapshot = start(&engine, &lease, &initial);
        let expected: &[u8] = match state {
            "failed" => {
                snapshot = commit(
                    &engine,
                    &lease,
                    &snapshot,
                    CommitThreadRunUpdate::Fail {
                        source_key: "fixture:failed".into(),
                        failure: RunFailure {
                            code: FailureCode::RuntimeFailed,
                            message: "matrix terminal failure".into(),
                            retryability: Retryability::Terminal,
                        },
                    },
                    1_020,
                );
                b"matrix terminal failure"
            }
            "interrupted" => {
                snapshot = commit(
                    &engine,
                    &lease,
                    &snapshot,
                    CommitThreadRunUpdate::Interrupt {
                        source_key: "fixture:interrupted".into(),
                        reconciliation_effect_id: None,
                    },
                    1_020,
                );
                b"Interrupted"
            }
            "reconciliation" => {
                let descriptor = ThreadEffectDescriptor {
                    effect_id: "matrix-effect".into(),
                    tool_call_id: "matrix-call".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":"matrix.txt"}),
                    attempt: 1,
                };
                let descriptor_json = serde_json::to_string(&descriptor).unwrap();
                let digest = "e".repeat(64);
                snapshot = commit(
                    &engine,
                    &lease,
                    &snapshot,
                    CommitThreadRunUpdate::PrepareEffect {
                        source_key: "fixture:prepare".into(),
                        effect_id: descriptor.effect_id.clone(),
                        operation_digest: digest.clone(),
                        descriptor_json: descriptor_json.clone(),
                        canonical_descriptor_json: descriptor_json,
                        policy: ThreadEffectPolicy::Allow,
                        description: "read matrix.txt".into(),
                        checkpoint_json: r#"{"phase":"prepared"}"#.into(),
                    },
                    1_018,
                );
                snapshot = commit(
                    &engine,
                    &lease,
                    &snapshot,
                    CommitThreadRunUpdate::StartEffect {
                        source_key: "fixture:start-effect".into(),
                        effect_id: descriptor.effect_id,
                        operation_digest: digest,
                        checkpoint_json: r#"{"phase":"started"}"#.into(),
                    },
                    1_019,
                );
                snapshot = commit(
                    &engine,
                    &lease,
                    &snapshot,
                    CommitThreadRunUpdate::Interrupt {
                        source_key: "fixture:unknown".into(),
                        reconciliation_effect_id: None,
                    },
                    1_020,
                );
                b"reconciliation required"
            }
            _ => unreachable!(),
        };
        assert_eq!(snapshot.lifecycle_label_for_test(), state);
        finish_fixture(engine, &lease);

        let mut pty = render_fixture(&scenario, expected, 28, 82);
        if matches!(state, "failed" | "interrupted") {
            let output_start = pty.output().len();
            pty.write(b"/new\r");
            assert!(
                wait_until(Duration::from_secs(5), || {
                    let output = pty.output();
                    output.get(output_start..).is_some_and(|tail| {
                        tail.windows(b"Describe an outcome".len())
                            .any(|window| window == b"Describe an outcome")
                    })
                }),
                "terminal Session could not switch to /new: {}",
                String::from_utf8_lossy(&pty.output())
            );
        }
        pty.write(F10);
        assert!(pty.finish(Duration::from_secs(5)).0.success());
    }
}

#[cfg(unix)]
#[test]
fn constrained_idle_view_and_retry_progress_render_in_real_ptys() {
    let narrow = Scenario::new();
    narrow.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let mut narrow_pty = PtySession::spawn_with_size(narrow.command(&["tui"]), 9, 38);
    assert!(narrow_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(narrow_pty.wait_for_output(b"Latte Code", Duration::from_secs(5)));
    narrow_pty.write(F10);
    assert!(narrow_pty.finish(Duration::from_secs(5)).0.success());

    let retry = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::json(503, &serde_json::json!({"error":"retry matrix"}))
            .header("Retry-After", "0"),
        // Keep the second request in flight long enough for an instrumented,
        // contended PTY renderer to expose the retry progress frame.
        ProviderReply::completion("retry matrix completed").delayed(Duration::from_secs(1)),
    ]);
    retry.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",timeout_ms:2000,max_attempts:2",
    );
    let mut command = retry.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "retry-secret");
    let mut retry_pty = PtySession::spawn_with_size(command, 24, 74);
    assert!(retry_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    retry_pty.write(b"retry with visible progress\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(retry_pty.wait_for_output(b"provider attempt 2", Duration::from_secs(5)));
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(retry.root())
        .database_path(retry.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == TranscriptKind::Assistant
                        && entry.text == "retry matrix completed"
                })
        })
    }));
    provider.assert_consumed();
    retry_pty.write(F10);
    assert!(retry_pty.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
fn permission_projection_matrix_renders_public_operation_and_target_variants() {
    for (name, input, expected_operation, expected_target) in [
        (
            Some("read_file"),
            serde_json::json!({"path":"src/read target.rs"}),
            "Read file",
            "src/read target.rs",
        ),
        (
            Some("list_directory"),
            serde_json::json!({"path":"src/components"}),
            "List directory",
            "src/components",
        ),
        (
            Some("search"),
            serde_json::json!({"query":"projection needle"}),
            "Search workspace",
            "projection needle",
        ),
        (
            Some("custom_fixture_tool"),
            serde_json::json!({}),
            "custom fixture tool",
            "Not exposed by runtime",
        ),
        (
            None,
            serde_json::json!({}),
            "Repository operation",
            "Not exposed by runtime",
        ),
    ] {
        let scenario = Scenario::new();
        let (engine, lease, initial) = fixture_engine(&scenario);
        let mut snapshot = start(&engine, &lease, &initial);
        let request_id = format!("projection-permission-{}", name.unwrap_or("generic"));
        if let Some(name) = name {
            snapshot = commit(
                &engine,
                &lease,
                &snapshot,
                CommitThreadRunUpdate::AppendTranscript {
                    source_key: format!("fixture:permission:{name}:descriptor"),
                    kind: TranscriptKind::ToolCall,
                    text: format!("inspect {expected_operation}"),
                    payload: Some(serde_json::json!({"descriptor": {
                        "tool_call_id": format!("call-{name}"),
                        "effect_id": request_id,
                        "name": name,
                        "input": input,
                    }})),
                },
                1_010,
            );
        }
        snapshot = commit(
            &engine,
            &lease,
            &snapshot,
            CommitThreadRunUpdate::RequestPermission {
                source_key: format!("fixture:permission:{}:request", name.unwrap_or("generic")),
                request: PendingPermission {
                    request_id,
                    operation_digest: "d".repeat(64),
                    description: "bounded public fixture scope".into(),
                },
            },
            1_011,
        );
        assert_eq!(
            snapshot.lifecycle,
            latte_core::ThreadLifecycle::WaitingPermission
        );
        finish_fixture(engine, &lease);

        let mut pty = render_fixture(&scenario, b"Permission required", 26, 88);
        assert!(
            pty.wait_for_visible_text(expected_operation, Duration::from_secs(5)),
            "permission operation was not visible: {}",
            String::from_utf8_lossy(&pty.output())
        );
        assert!(
            pty.wait_for_visible_text(expected_target, Duration::from_secs(5)),
            "permission target was not visible: {}",
            String::from_utf8_lossy(&pty.output())
        );
        pty.write(b"\r ");
        assert!(pty.is_running(), "inert permission keys exited the TUI");
        pty.write(F10);
        assert!(pty.finish(Duration::from_secs(5)).0.success());
    }
}

trait LifecycleLabelForTest {
    fn lifecycle_label_for_test(&self) -> &'static str;
}

impl LifecycleLabelForTest for latte_core::ThreadSnapshot {
    fn lifecycle_label_for_test(&self) -> &'static str {
        match self.lifecycle {
            latte_core::ThreadLifecycle::Ready => "ready",
            latte_core::ThreadLifecycle::Running => "running",
            latte_core::ThreadLifecycle::WaitingPermission => "permission",
            latte_core::ThreadLifecycle::WaitingInput => "input",
            latte_core::ThreadLifecycle::Interrupted => "interrupted",
            latte_core::ThreadLifecycle::Failed => "failed",
            latte_core::ThreadLifecycle::ReconciliationRequired => "reconciliation",
        }
    }
}
