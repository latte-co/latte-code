use super::support::{PtySession, Scenario, json, wait_until};
use latte_core::{
    FailureCode, Handoff, IdSource, PendingInput, Retryability, RunFailure, RunId, SystemIdSource,
    ThreadCommandId, ThreadEvent, ThreadEventEnvelope, ThreadEventId, ThreadId, ThreadLifecycle,
    ThreadPendingRequest, ThreadProviderBindingV2, ThreadRunStatus, ThreadSessionSummary,
    ThreadTransientProgress, TranscriptEntry, TranscriptEntryId, TranscriptKind,
};
use latte_engine::{
    CancellationToken, CommitThreadRunUpdate, EngineBuilder, EngineHandle, Lease, ProcessOutput,
    ProcessTermination, StorageError, SubscriptionError, ThreadCommitRequest,
    ThreadEffectDescriptor, ThreadEffectObservedValue, ThreadEffectPolicy, ThreadEffectRequest,
    ThreadEffectStartRequest,
};
use std::{collections::BTreeMap, time::Duration};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use latte_tui::{
    ConnectionState,
    thread::{
        ActiveConversation, PendingInputSubmission, PendingModelSwitch, PendingSubmission,
        ThreadModelOption, ThreadPermissionMode, ThreadStartupPresentation, ThreadUiAction,
        ThreadUiInput, ThreadUiModel, reduce,
    },
};

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

fn create_started_effect_thread(
    engine: &EngineHandle,
    prompt: &str,
    now: u64,
) -> (latte_core::ThreadSnapshot, Lease, RunId) {
    let thread_id = thread_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let run_id = run_id();
    let snapshot = engine
        .create_thread_v2(thread_id, run_id, binding(), prompt, now + 1)
        .unwrap();
    let snapshot = start(
        engine,
        &lease,
        &snapshot,
        &format!("permission-summary:{run_id}:start"),
        now + 2,
    );
    (snapshot, lease, run_id)
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
fn public_tui_projection_reducer_matrix_tracks_authoritative_engine_snapshot() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let run_id = run_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let created = engine
        .create_thread_v2(
            thread_id,
            run_id,
            binding(),
            "public reducer authoritative session",
            now + 1,
        )
        .unwrap();
    let started = start(&engine, &lease, &created, "reducer:start", now + 2);
    let ready = commit(
        &engine,
        &lease,
        &started,
        CommitThreadRunUpdate::Complete {
            source_key: "reducer:complete".into(),
            handoff: Handoff {
                summary: "public reducer session completed".into(),
                files_changed: vec!["src/reducer.rs".into()],
                evidence: Vec::new(),
            },
        },
        now + 3,
    );
    engine.release_lease(&lease).unwrap();

    let startup = ThreadStartupPresentation {
        default_provider: "v2-boundary".into(),
        default_model: "v2-boundary-model".into(),
        model_catalog: vec![
            ThreadModelOption {
                provider_name: "v2-boundary".into(),
                model: "v2-boundary-model".into(),
                name: Some("Boundary default".into()),
                is_default: true,
            },
            ThreadModelOption {
                provider_name: "secondary".into(),
                model: "secondary-model".into(),
                name: None,
                is_default: false,
            },
        ],
        workspace_display: scenario.root().display().to_string(),
        permission_mode: ThreadPermissionMode::Ask,
    };
    let mut model = ThreadUiModel::with_startup(startup);

    let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert!(reduce(&mut model, ThreadUiInput::Key(release)).is_empty());
    assert_eq!(
        reduce(&mut model, key(KeyCode::F(10))),
        vec![ThreadUiAction::Quit]
    );
    model.connection = ConnectionState::Disconnected;
    assert_eq!(
        reduce(
            &mut model,
            modified_key(KeyCode::Char('r'), KeyModifiers::CONTROL)
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    model.connection = ConnectionState::Connected;

    let mut no_models = ThreadUiModel::default();
    no_models.composer = "/model".into();
    assert!(reduce(&mut no_models, key(KeyCode::Enter)).is_empty());
    assert!(no_models.status.contains("Provider setup required"));
    assert!(no_models.status.contains("~/.latte/latte-code.jsonc"));

    let mut validation_model = ThreadUiModel::with_startup(model.startup.clone().unwrap());
    validation_model.composer = "/new unexpected".into();
    assert!(reduce(&mut validation_model, key(KeyCode::Enter)).is_empty());
    assert!(
        validation_model
            .status
            .contains("does not accept arguments")
    );
    validation_model.composer.clear();
    assert!(reduce(&mut validation_model, key(KeyCode::Enter)).is_empty());

    let mut paste_model = ThreadUiModel::default();
    let oversized_paste = format!("\u{1b}[31mred\u{7}{}", "x".repeat(17_000));
    assert!(reduce(&mut paste_model, ThreadUiInput::Paste(oversized_paste)).is_empty());
    assert!(!paste_model.composer.contains('\u{1b}'));
    assert!(!paste_model.composer.contains('\u{7}'));
    assert!(paste_model.composer.len() <= 16 * 1024);

    let mut draft_model = ThreadUiModel::with_startup(model.startup.clone().unwrap());
    draft_model.composer = "/model".into();
    assert!(reduce(&mut draft_model, key(KeyCode::Enter)).is_empty());
    assert!(reduce(&mut draft_model, key(KeyCode::Down)).is_empty());
    assert!(reduce(&mut draft_model, key(KeyCode::Enter)).is_empty());
    assert!(
        draft_model
            .status
            .contains("New sessions will use secondary")
    );
    draft_model.composer = "start with the selected provider".into();
    assert!(matches!(
        reduce(&mut draft_model, key(KeyCode::F(5))).as_slice(),
        [ThreadUiAction::StartWithModel { provider_name, model, .. }]
            if provider_name == "secondary" && model == "secondary-model"
    ));
    assert_eq!(model.selected_thread(), None);
    assert!(model.authority_enabled());
    assert!(reduce(&mut model, ThreadUiInput::Resize(1, 1)).is_empty());
    assert_eq!(model.size, (1, 1));
    assert!(reduce(&mut model, ThreadUiInput::Disconnected).is_empty());
    assert_eq!(model.connection, ConnectionState::Disconnected);
    assert_eq!(
        reduce(&mut model, ThreadUiInput::Lagged),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    assert_eq!(model.connection, ConnectionState::SnapshotRequired);
    assert!(reduce(&mut model, ThreadUiInput::Connected).is_empty());
    assert_eq!(model.connection, ConnectionState::Connected);

    let summary = ThreadSessionSummary {
        thread_id,
        title: "public reducer authoritative session".into(),
        workspace_root: scenario.root().display().to_string(),
        parent_thread_id: None,
        lifecycle: ThreadLifecycle::Ready,
        provider_name: ready.binding.provider_name.clone(),
        model: ready.binding.model.clone(),
        created_at_ms: now + 1,
        updated_at_ms: now + 3,
    };
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalog(vec![summary.clone()])
        )
        .is_empty()
    );
    assert_eq!(
        model.active_conversation,
        Some(ActiveConversation::NewSessionDraft)
    );
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalog(vec![summary.clone()])
        )
        .is_empty()
    );
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalogReady {
                sessions: Vec::new(),
                query: None,
            }
        )
        .is_empty()
    );
    assert!(model.session_picker);
    assert_eq!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalogReady {
                sessions: vec![summary.clone()],
                query: Some(summary.title.clone()),
            }
        ),
        vec![ThreadUiAction::OpenSession { thread_id }]
    );
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalogReady {
                sessions: Vec::new(),
                query: Some("missing-title".into()),
            }
        )
        .is_empty()
    );
    assert!(model.status.contains("No exact session match"));
    let mut duplicate = summary.clone();
    duplicate.thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionCatalogReady {
                sessions: vec![summary.clone(), duplicate],
                query: Some(summary.title.clone()),
            }
        )
        .is_empty()
    );
    assert!(model.status.contains("Multiple sessions match"));

    assert!(
        reduce(
            &mut model,
            ThreadUiInput::SessionOpened(Box::new(ready.clone()))
        )
        .is_empty()
    );
    assert_eq!(
        model.active_conversation,
        Some(ActiveConversation::Session(thread_id))
    );
    assert_eq!(model.selected_thread(), Some(&ready));

    for progress in [
        ThreadTransientProgress::ProviderAttempt { run_id, number: 2 },
        ThreadTransientProgress::AssistantDelta {
            run_id,
            text: "streamed delta".into(),
        },
        ThreadTransientProgress::ToolProgress {
            run_id,
            name: "read_file".into(),
            detail: "bounded detail".into(),
        },
    ] {
        assert!(reduce(&mut model, ThreadUiInput::Progress(progress)).is_empty());
    }
    assert_eq!(model.progress.len(), 3);
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::CommandError("typed command failed".into())
        )
        .is_empty()
    );
    assert!(model.status.contains("Command rejected"));
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::CommandCompleted("typed command completed".into())
        )
        .is_empty()
    );

    let missing_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id: ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        revision: 1,
        sequence: 1,
        event: ThreadEvent::LifecycleChanged {
            lifecycle: ThreadLifecycle::Running,
            run_id: Some(run_id),
        },
    };
    assert_eq!(
        reduce(&mut model, ThreadUiInput::Event(missing_event)),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    let gap_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision.saturating_add(2),
        sequence: ready.sequence.saturating_add(2),
        event: ThreadEvent::LifecycleChanged {
            lifecycle: ThreadLifecycle::Running,
            run_id: Some(run_id),
        },
    };
    assert_eq!(
        reduce(&mut model, ThreadUiInput::Event(gap_event)),
        vec![ThreadUiAction::RefreshSnapshots]
    );

    let mut event_model = ThreadUiModel::default();
    reduce(
        &mut event_model,
        ThreadUiInput::Snapshot(vec![ready.clone()]),
    );
    let lifecycle_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision + 1,
        sequence: ready.sequence + 1,
        event: ThreadEvent::LifecycleChanged {
            lifecycle: ThreadLifecycle::Interrupted,
            run_id: Some(run_id),
        },
    };
    assert!(reduce(&mut event_model, ThreadUiInput::Event(lifecycle_event)).is_empty());
    assert_eq!(
        event_model.sessions[0].lifecycle,
        ThreadLifecycle::Interrupted
    );
    let transcript_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision + 2,
        sequence: ready.sequence + 2,
        event: ThreadEvent::TranscriptAppended {
            entry: TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                sequence: ready.sequence + 2,
                run_id: Some(run_id),
                kind: TranscriptKind::System,
                text: "event appended card".into(),
                payload: None,
                source_key: "event:append".into(),
                created_at_ms: now + 4,
            },
        },
    };
    assert!(reduce(&mut event_model, ThreadUiInput::Event(transcript_event)).is_empty());
    assert!(
        event_model.sessions[0]
            .transcript
            .entries
            .iter()
            .any(|entry| entry.text == "event appended card")
    );
    let linked_run = latte_core::ThreadRunSummary {
        run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        parent_run_id: Some(run_id),
        ordinal: 1,
        status: ThreadRunStatus::Queued,
        run_revision: 0,
        completed_at_ms: None,
        failure_code: None,
    };
    let run_linked_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision + 3,
        sequence: ready.sequence + 3,
        event: ThreadEvent::RunLinked {
            run: linked_run.clone(),
        },
    };
    assert!(reduce(&mut event_model, ThreadUiInput::Event(run_linked_event)).is_empty());
    assert_eq!(
        event_model.sessions[0].latest_run_id,
        Some(linked_run.run_id)
    );
    let binding_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision + 4,
        sequence: ready.sequence + 4,
        event: ThreadEvent::BindingChanged {
            provider_name: "secondary".into(),
            model: "secondary-model".into(),
        },
    };
    assert_eq!(
        reduce(&mut event_model, ThreadUiInput::Event(binding_event)),
        vec![ThreadUiAction::RefreshSnapshots]
    );

    let mut feedback_model = ThreadUiModel::default();
    feedback_model.pending_submission = Some(PendingSubmission {
        submission_id: 7,
        prompt: "pending prompt".into(),
        thread_id: None,
        after_sequence: 0,
    });
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::SubmissionAssigned {
                submission_id: 6,
                thread_id,
            }
        )
        .is_empty()
    );
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::SubmissionAssigned {
                submission_id: 7,
                thread_id,
            }
        )
        .is_empty()
    );
    assert_eq!(
        feedback_model
            .pending_submission
            .as_ref()
            .unwrap()
            .thread_id,
        Some(thread_id)
    );
    assert_eq!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::SubmissionError { submission_id: 7 }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::SubmissionCompleted { submission_id: 7 }
        )
        .is_empty()
    );
    feedback_model.pending_input_submission = Some(PendingInputSubmission {
        submission_id: 8,
        thread_id,
        run_id,
        request_id: "pending-input".into(),
        value: "pending value".into(),
        after_sequence: 0,
    });
    assert_eq!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::InputSubmissionError { submission_id: 8 }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::InputSubmissionCompleted { submission_id: 8 }
        )
        .is_empty()
    );
    feedback_model.pending_model_switch = Some(PendingModelSwitch {
        switch_id: 9,
        thread_id,
        provider_name: "secondary".into(),
        model: "secondary-model".into(),
    });
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::ModelSwitchError {
                switch_id: 8,
                error: "stale".into(),
            }
        )
        .is_empty()
    );
    assert_eq!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::ModelSwitchCompleted { switch_id: 9 }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    feedback_model.pending_model_switch = Some(PendingModelSwitch {
        switch_id: 10,
        thread_id,
        provider_name: "secondary".into(),
        model: "secondary-model".into(),
    });
    assert!(
        reduce(
            &mut feedback_model,
            ThreadUiInput::ModelSwitchError {
                switch_id: 10,
                error: "authoritative rejection".into(),
            }
        )
        .is_empty()
    );
    assert!(feedback_model.pending_model_switch.is_none());
    assert!(feedback_model.status.contains("authoritative rejection"));

    let mut queued_model = ThreadUiModel::default();
    queued_model.sessions = vec![ready.clone()];
    queued_model.active_conversation = Some(ActiveConversation::Session(thread_id));
    queued_model.pending_submission = Some(PendingSubmission {
        submission_id: 11,
        prompt: "queued public follow-up".into(),
        thread_id: Some(thread_id),
        after_sequence: ready.sequence,
    });
    queued_model.queued_follow_up = Some("queued public follow-up".into());
    queued_model.reconciliation_confirmation = Some((thread_id, "obsolete-effect".into()));
    assert_eq!(
        reduce(
            &mut queued_model,
            ThreadUiInput::Snapshot(vec![ready.clone()])
        ),
        vec![ThreadUiAction::FollowUp {
            submission_id: 11,
            thread_id,
            expected_thread_revision: ready.revision,
            prompt: "queued public follow-up".into(),
        }]
    );
    assert!(queued_model.reconciliation_confirmation.is_none());

    let mut coalesced = ThreadUiModel::default();
    for input in [
        ThreadTransientProgress::AssistantDelta {
            run_id,
            text: "first".into(),
        },
        ThreadTransientProgress::AssistantDelta {
            run_id,
            text: " second".into(),
        },
        ThreadTransientProgress::ToolProgress {
            run_id,
            name: "read_file".into(),
            detail: "old detail".into(),
        },
        ThreadTransientProgress::ToolProgress {
            run_id,
            name: "read_file".into(),
            detail: "new detail".into(),
        },
    ] {
        assert!(reduce(&mut coalesced, ThreadUiInput::Progress(input)).is_empty());
    }
    assert_eq!(coalesced.progress.len(), 2);

    let mut reconciliation_model = ThreadUiModel::default();
    reduce(
        &mut reconciliation_model,
        ThreadUiInput::Snapshot(vec![ready.clone()]),
    );
    let reconciliation_event = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        thread_id,
        revision: ready.revision + 1,
        sequence: ready.sequence + 1,
        event: ThreadEvent::ReconciliationRequired {
            run_id,
            effect_id: "event-unknown-effect".into(),
        },
    };
    assert!(
        reduce(
            &mut reconciliation_model,
            ThreadUiInput::Event(reconciliation_event)
        )
        .is_empty()
    );
    assert_eq!(
        reconciliation_model.sessions[0].lifecycle,
        ThreadLifecycle::ReconciliationRequired
    );
    assert!(reduce(&mut feedback_model, ThreadUiInput::Tick).is_empty());

    drop(engine);
    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(tui.wait_for_output(b"public reducer session completed", Duration::from_secs(5)));
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
}

fn key(code: KeyCode) -> ThreadUiInput {
    ThreadUiInput::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> ThreadUiInput {
    ThreadUiInput::Key(KeyEvent::new(code, modifiers))
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_tui_key_and_render_matrix_covers_every_modal_and_viewport_tier() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let run_id = run_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let created = engine
        .create_thread_v2(
            thread_id,
            run_id,
            binding(),
            "render boundary session",
            now + 1,
        )
        .unwrap();
    let started = start(&engine, &lease, &created, "render:start", now + 2);
    let ready = commit(
        &engine,
        &lease,
        &started,
        CommitThreadRunUpdate::Complete {
            source_key: "render:complete".into(),
            handoff: Handoff {
                summary: "render boundary completed with a deliberately long summary".into(),
                files_changed: vec!["src/render.rs".into(), "tests/render.rs".into()],
                evidence: Vec::new(),
            },
        },
        now + 3,
    );
    engine.release_lease(&lease).unwrap();

    let startup = ThreadStartupPresentation {
        default_provider: "v2-boundary".into(),
        default_model: "v2-boundary-model".into(),
        model_catalog: vec![
            ThreadModelOption {
                provider_name: "v2-boundary".into(),
                model: "v2-boundary-model".into(),
                name: Some("Boundary default".into()),
                is_default: true,
            },
            ThreadModelOption {
                provider_name: "secondary".into(),
                model: "secondary-model".into(),
                name: Some("Secondary friendly".into()),
                is_default: false,
            },
        ],
        workspace_display: scenario.root().display().to_string(),
        permission_mode: ThreadPermissionMode::Ask,
    };
    let mut model = ThreadUiModel::with_startup(startup);

    model.composer = "/".into();
    assert!(reduce(&mut model, key(KeyCode::Up)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Esc)).is_empty());
    model.composer.clear();

    model.command_palette = true;
    model.command_index = 3;
    assert!(reduce(&mut model, key(KeyCode::Up)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Char('k'))).is_empty());
    assert!(
        reduce(
            &mut model,
            modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        )
        .is_empty()
    );
    model.command_palette = true;
    assert!(reduce(&mut model, key(KeyCode::Esc)).is_empty());

    model.composer = "/model".into();
    assert_eq!(reduce(&mut model, key(KeyCode::Enter)), Vec::new());
    for ch in "not-found".chars() {
        assert!(reduce(&mut model, key(KeyCode::Char(ch))).is_empty());
    }
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Backspace)).is_empty());
    for _ in 0..8 {
        assert!(reduce(&mut model, key(KeyCode::Backspace)).is_empty());
    }
    assert!(reduce(&mut model, key(KeyCode::Down)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Up)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Esc)).is_empty());

    let summary = ThreadSessionSummary {
        thread_id,
        title: "render boundary session".into(),
        workspace_root: scenario.root().display().to_string(),
        parent_thread_id: None,
        lifecycle: ThreadLifecycle::Ready,
        provider_name: ready.binding.provider_name.clone(),
        model: ready.binding.model.clone(),
        created_at_ms: now,
        updated_at_ms: now + 3,
    };
    reduce(
        &mut model,
        ThreadUiInput::SessionCatalogReady {
            sessions: vec![summary],
            query: None,
        },
    );
    for input in [
        key(KeyCode::Down),
        key(KeyCode::Char('j')),
        key(KeyCode::Up),
        key(KeyCode::Char('k')),
    ] {
        assert!(reduce(&mut model, input).is_empty());
    }
    assert!(matches!(
        reduce(&mut model, key(KeyCode::Enter)).as_slice(),
        [ThreadUiAction::OpenSession { thread_id: selected }] if *selected == thread_id
    ));
    model.session_picker = false;

    let mut rich = ready.clone();
    let next_sequence = rich.sequence + 1;
    rich.transcript.entries.extend([
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence,
            run_id: Some(run_id),
            kind: TranscriptKind::System,
            text: "system render annotation".into(),
            payload: None,
            source_key: "render:system".into(),
            created_at_ms: now + 4,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 1,
            run_id: Some(run_id),
            kind: TranscriptKind::ToolCall,
            text: "Read src/render.rs".into(),
            payload: Some(serde_json::json!({
                "descriptor":{
                    "effect_id":"render-effect",
                    "tool_call_id":"render-call",
                    "name":"read_file",
                    "input":{
                        "path":"src/render.rs",
                        "query":"render boundary",
                        "cwd":"src",
                        "argv":["/bin/pwd","--logical"]
                    }
                }
            })),
            source_key: "render:tool-call".into(),
            created_at_ms: now + 5,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 2,
            run_id: Some(run_id),
            kind: TranscriptKind::ToolResult,
            text: "render tool result with enough detail to wrap across narrow viewports".into(),
            payload: Some(serde_json::json!({
                "tool_call_id":"render-call","name":"read_file","success":false,
                "provider_content":"render tool result","error":"bounded failure detail"
            })),
            source_key: "render:tool-result".into(),
            created_at_ms: now + 6,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 3,
            run_id: Some(run_id),
            kind: TranscriptKind::Failure,
            text: "rendered failure card".into(),
            payload: Some(serde_json::json!({"code":"runtime_failed"})),
            source_key: "render:failure".into(),
            created_at_ms: now + 7,
        },
    ]);
    let orphan_run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    rich.transcript.entries.extend([
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 4,
            run_id: Some(run_id),
            kind: TranscriptKind::ToolResult,
            text: "unpaired successful tool result".into(),
            payload: Some(serde_json::json!({
                "tool_call_id":"unpaired-call",
                "name":"search",
                "provider_content":"unpaired result"
            })),
            source_key: "render:unpaired-result".into(),
            created_at_ms: now + 8,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 5,
            run_id: Some(run_id),
            kind: TranscriptKind::System,
            text: "orphan started status".into(),
            payload: Some(serde_json::json!({
                "status":"started",
                "effect_id":"unmatched-effect"
            })),
            source_key: "render:orphan-started".into(),
            created_at_ms: now + 9,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 6,
            run_id: Some(orphan_run_id),
            kind: TranscriptKind::ToolCall,
            text: "x".repeat(500),
            payload: Some(serde_json::json!({
                "descriptor":{
                    "effect_id":"orphan-effect",
                    "tool_call_id":"orphan-call",
                    "name":"invalid\nname",
                    "input":{"path":"orphan.txt"}
                }
            })),
            source_key: "render:orphan-call".into(),
            created_at_ms: now + 10,
        },
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence: next_sequence + 7,
            run_id: None,
            kind: TranscriptKind::System,
            text: format!("{}{}", "bounded".repeat(400), '\u{7}'),
            payload: None,
            source_key: "render:bounded-system".into(),
            created_at_ms: now + 11,
        },
    ]);
    rich.sequence = next_sequence + 7;
    model.active_conversation = Some(ActiveConversation::Session(thread_id));
    reduce(&mut model, ThreadUiInput::Snapshot(vec![rich.clone()]));

    let mut assigned_model = ThreadUiModel::with_startup(model.startup.clone().unwrap());
    assigned_model.composer = "assign this durable session".into();
    let assignment = reduce(&mut assigned_model, key(KeyCode::Enter));
    let assignment_id = match assignment.as_slice() {
        [ThreadUiAction::Start { submission_id, .. }] => *submission_id,
        actions => panic!("unexpected draft assignment actions: {actions:?}"),
    };
    reduce(
        &mut assigned_model,
        ThreadUiInput::Snapshot(vec![rich.clone()]),
    );
    assert!(assigned_model.pending_submission.is_some());
    reduce(
        &mut assigned_model,
        ThreadUiInput::SubmissionAssigned {
            submission_id: assignment_id,
            thread_id,
        },
    );
    assert_eq!(
        assigned_model.active_conversation,
        Some(ActiveConversation::Session(thread_id))
    );

    model.focus = latte_tui::thread::ThreadFocus::Navigation;
    for input in [
        key(KeyCode::Char('?')),
        key(KeyCode::Down),
        key(KeyCode::Char('j')),
        key(KeyCode::Up),
        key(KeyCode::Char('k')),
        key(KeyCode::PageUp),
        key(KeyCode::PageDown),
        key(KeyCode::Home),
        key(KeyCode::Right),
        key(KeyCode::Left),
        key(KeyCode::Char(' ')),
        key(KeyCode::Enter),
        key(KeyCode::Esc),
    ] {
        assert!(reduce(&mut model, input).is_empty());
    }
    model.focus = latte_tui::thread::ThreadFocus::Navigation;
    assert!(reduce(&mut model, key(KeyCode::Right)).is_empty());
    assert!(!model.expanded_actions.is_empty());
    let expanded_render_model = model.clone();

    let mut running = rich.clone();
    running.lifecycle = ThreadLifecycle::Running;
    running.active_run_id = Some(run_id);
    running.runs.last_mut().unwrap().status = ThreadRunStatus::Running;
    reduce(&mut model, ThreadUiInput::Snapshot(vec![running.clone()]));

    let mut active_keys = model.clone();
    assert!(matches!(
        reduce(
            &mut active_keys,
            modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        )
        .as_slice(),
        [ThreadUiAction::Cancel { thread_id: selected }] if *selected == thread_id
    ));
    assert!(
        reduce(
            &mut active_keys,
            modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        )
        .is_empty()
    );
    let mut idle_keys = ThreadUiModel::default();
    idle_keys.connection = ConnectionState::Connected;
    assert!(
        reduce(
            &mut idle_keys,
            modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        )
        .is_empty()
    );

    let mut busy_picker = ThreadUiModel::with_startup(model.startup.clone().unwrap());
    busy_picker.active_conversation = Some(ActiveConversation::Session(thread_id));
    reduce(
        &mut busy_picker,
        ThreadUiInput::Snapshot(vec![rich.clone()]),
    );
    busy_picker.composer = "/model".into();
    assert!(reduce(&mut busy_picker, key(KeyCode::Enter)).is_empty());
    reduce(
        &mut busy_picker,
        ThreadUiInput::Snapshot(vec![running.clone()]),
    );
    assert!(reduce(&mut busy_picker, key(KeyCode::Enter)).is_empty());
    assert!(busy_picker.status.contains("disabled while work"));

    model.focus = latte_tui::thread::ThreadFocus::Composer;
    model.composer = "/new".into();
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(model.status.contains("switching is disabled"));
    model.composer = "queue while running".into();
    assert!(matches!(
        reduce(&mut model, key(KeyCode::Enter)).as_slice(),
        [ThreadUiAction::QueueFollowUp { prompt, .. }] if prompt == "queue while running"
    ));
    model.pending_submission = None;
    model.composer = "second queued prompt".into();
    assert!(matches!(
        reduce(&mut model, key(KeyCode::Enter)).as_slice(),
        [ThreadUiAction::QueueFollowUp { prompt, .. }] if prompt == "second queued prompt"
    ));

    reduce(&mut model, ThreadUiInput::Snapshot(vec![rich.clone()]));
    model.queued_follow_up = None;
    model.pending_submission = None;
    model.composer = "/model".into();
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(model.status.contains("already selected"));
    model.composer = "/model".into();
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(reduce(&mut model, key(KeyCode::Down)).is_empty());
    let switch = reduce(&mut model, key(KeyCode::Enter));
    assert!(matches!(
        switch.as_slice(),
        [ThreadUiAction::SwitchModel { provider_name, model, .. }]
            if provider_name == "secondary" && model == "secondary-model"
    ));
    let switch_id = model.pending_model_switch.as_ref().unwrap().switch_id;
    assert_eq!(
        reduce(
            &mut model,
            ThreadUiInput::ModelSwitchCompleted { switch_id }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    model.composer = "blocked by switch".into();
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(model.status.contains("model switch"));
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::ModelSwitchError {
                switch_id,
                error: "provider rejected switch".into(),
            }
        )
        .is_empty()
    );
    assert!(model.status.contains("rejected"));
    model.pending_model_switch = None;

    let mut terminal = rich.clone();
    terminal.lifecycle = ThreadLifecycle::Failed;
    terminal.active_run_id = None;

    let mut stranded_model = model.clone();
    stranded_model.active_conversation = Some(ActiveConversation::Session(thread_id));
    stranded_model.queued_follow_up = Some("stranded queued follow-up".into());
    stranded_model.pending_submission = Some(PendingSubmission {
        submission_id: 90,
        prompt: "stranded queued follow-up".into(),
        thread_id: Some(thread_id),
        after_sequence: terminal.sequence,
    });
    reduce(
        &mut stranded_model,
        ThreadUiInput::Snapshot(vec![terminal.clone()]),
    );
    assert!(stranded_model.composer.contains("stranded queued"));
    assert!(stranded_model.status.contains("active child ended"));

    reduce(&mut model, ThreadUiInput::Snapshot(vec![terminal.clone()]));
    model.composer = "cannot run terminal child".into();
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(model.status.contains("no runnable child"));
    model.pending_submission = Some(PendingSubmission {
        submission_id: 91,
        prompt: "restore after pre-durable failure".into(),
        thread_id: Some(thread_id),
        after_sequence: terminal.sequence,
    });
    assert_eq!(
        reduce(
            &mut model,
            ThreadUiInput::SubmissionError { submission_id: 91 }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    reduce(&mut model, ThreadUiInput::Snapshot(vec![terminal]));
    assert!(model.composer.contains("restore after"));

    model.reconciliation_confirmation = Some((thread_id, "render-effect".into()));
    assert!(reduce(&mut model, key(KeyCode::Char('d'))).is_empty());
    assert!(model.reconciliation_confirmation.is_none());
    model.connection = ConnectionState::Disconnected;
    assert!(
        reduce(
            &mut model,
            ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
                run_id,
                text: "ignored while disconnected".into(),
            })
        )
        .is_empty()
    );
    model.connection = ConnectionState::Connected;

    assert!(
        reduce(
            &mut model,
            ThreadUiInput::Paste("wide 界\tline\nnext".into())
        )
        .is_empty()
    );
    model.pending_submission = Some(PendingSubmission {
        submission_id: 44,
        prompt: "pending".into(),
        thread_id: Some(thread_id),
        after_sequence: ready.sequence,
    });
    let before = model.composer.clone();
    reduce(&mut model, ThreadUiInput::Paste("blocked".into()));
    assert_eq!(model.composer, format!("{before}blocked"));
    assert!(reduce(&mut model, key(KeyCode::Enter)).is_empty());
    assert!(model.pending_submission.is_some());
    model.pending_submission = None;

    let mut render_models = Vec::new();
    render_models.push(expanded_render_model);

    let mut permission = rich.clone();
    permission.lifecycle = ThreadLifecycle::WaitingPermission;
    permission.active_run_id = Some(run_id);
    permission.runs.last_mut().unwrap().status = ThreadRunStatus::WaitingPermission;
    permission.pending = Some(ThreadPendingRequest::Permission {
        run_id,
        request_id: "render-effect".into(),
        description: "read the renderer before editing it".into(),
        expected_run_revision: permission.runs.last().unwrap().run_revision,
    });
    let mut permission_model = model.clone();
    reduce(
        &mut permission_model,
        ThreadUiInput::Snapshot(vec![permission]),
    );
    assert!(reduce(&mut permission_model, key(KeyCode::Enter)).is_empty());
    assert!(reduce(&mut permission_model, ThreadUiInput::FrameRendered).is_empty());
    assert!(matches!(
        reduce(&mut permission_model, key(KeyCode::Char('d'))).as_slice(),
        [ThreadUiAction::ResolvePermission { allow: false, .. }]
    ));
    assert!(matches!(
        reduce(
            &mut permission_model,
            modified_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        )
        .as_slice(),
        [ThreadUiAction::ResolvePermission { allow: true, .. }]
    ));
    render_models.push(permission_model);

    let mut waiting_input = running.clone();
    waiting_input.lifecycle = ThreadLifecycle::WaitingInput;
    waiting_input.runs.last_mut().unwrap().status = ThreadRunStatus::WaitingInput;
    waiting_input.pending = Some(ThreadPendingRequest::Input {
        run_id,
        request_id: "render-input".into(),
        prompt: "Which rendering tier should be verified?".into(),
        expected_run_revision: waiting_input.runs.last().unwrap().run_revision,
    });
    let mut input_model = model.clone();
    reduce(
        &mut input_model,
        ThreadUiInput::Snapshot(vec![waiting_input.clone()]),
    );
    assert!(reduce(&mut input_model, key(KeyCode::Char('x'))).is_empty());
    assert!(
        reduce(
            &mut input_model,
            modified_key(KeyCode::Enter, KeyModifiers::SHIFT)
        )
        .is_empty()
    );
    assert!(reduce(&mut input_model, key(KeyCode::Backspace)).is_empty());
    input_model.input.push_str("wide\tinput\nsecond row");
    let input_actions = reduce(&mut input_model, key(KeyCode::Enter));
    assert!(matches!(
        input_actions.as_slice(),
        [ThreadUiAction::ProvideInput { value, .. }] if value.contains("wide")
    ));
    assert!(reduce(&mut input_model, key(KeyCode::Char('z'))).is_empty());
    let input_submission_id = input_model
        .pending_input_submission
        .as_ref()
        .unwrap()
        .submission_id;
    assert!(
        reduce(
            &mut input_model,
            ThreadUiInput::InputSubmissionCompleted {
                submission_id: input_submission_id,
            }
        )
        .is_empty()
    );
    assert_eq!(
        reduce(
            &mut input_model,
            ThreadUiInput::InputSubmissionError {
                submission_id: input_submission_id,
            }
        ),
        vec![ThreadUiAction::RefreshSnapshots]
    );
    let pending_input = input_model.pending_input_submission.clone().unwrap();
    let mut durable_input = waiting_input;
    durable_input.sequence = pending_input.after_sequence + 1;
    durable_input.transcript.entries.push(TranscriptEntry {
        entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        sequence: durable_input.sequence,
        run_id: Some(run_id),
        kind: TranscriptKind::User,
        text: latte_core::redact_thread_text(&pending_input.value),
        payload: None,
        source_key: format!("{run_id}:input:{}:card", pending_input.request_id),
        created_at_ms: now + 12,
    });
    reduce(
        &mut input_model,
        ThreadUiInput::Snapshot(vec![durable_input]),
    );
    assert!(input_model.pending_input_submission.is_none());
    render_models.push(input_model);

    let mut reconciliation = rich.clone();
    reconciliation.lifecycle = ThreadLifecycle::ReconciliationRequired;
    reconciliation.active_run_id = None;
    reconciliation.transcript.entries.push(TranscriptEntry {
        entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        sequence: reconciliation.sequence + 1,
        run_id: Some(run_id),
        kind: TranscriptKind::Failure,
        text: "process result requires reconciliation".into(),
        payload: Some(serde_json::json!({
            "status":"unknown",
            "effect_id":"render-effect"
        })),
        source_key: "render:unknown".into(),
        created_at_ms: now + 8,
    });
    reconciliation.sequence += 1;
    let mut reconciliation_model = model.clone();
    reduce(
        &mut reconciliation_model,
        ThreadUiInput::Snapshot(vec![reconciliation]),
    );
    reconciliation_model.reconciliation_confirmation = Some((thread_id, "render-effect".into()));
    assert!(reduce(&mut reconciliation_model, key(KeyCode::Enter)).is_empty());
    assert!(matches!(
        reduce(
            &mut reconciliation_model,
            modified_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        )
        .as_slice(),
        [ThreadUiAction::ReconcileUnknown { effect_id, .. }] if effect_id == "render-effect"
    ));
    render_models.push(reconciliation_model);

    let mut running_model = model.clone();
    reduce(&mut running_model, ThreadUiInput::Snapshot(vec![running]));
    reduce(
        &mut running_model,
        ThreadUiInput::Progress(ThreadTransientProgress::AssistantDelta {
            run_id,
            text: format!("streamed assistant detail{}", "x".repeat(70_000)),
        }),
    );
    reduce(
        &mut running_model,
        ThreadUiInput::Progress(ThreadTransientProgress::ToolProgress {
            run_id,
            name: "read_file".into(),
            detail: "reading src/render.rs".into(),
        }),
    );
    render_models.push(running_model);

    let startup = model.startup.clone().unwrap();
    let mut idle_model = ThreadUiModel::with_startup(startup.clone());
    idle_model.composer = "wide 界\tcomposer\nsecond row".into();
    render_models.push(idle_model.clone());

    let mut failed_draft = idle_model.clone();
    failed_draft.submission_error = Some("unable to persist this draft".into());
    failed_draft.connection = ConnectionState::Disconnected;
    render_models.push(failed_draft);

    let mut stale_draft = idle_model;
    stale_draft.connection = ConnectionState::SnapshotRequired;
    render_models.push(stale_draft);

    let mut slash_model = ThreadUiModel::with_startup(startup.clone());
    slash_model.composer = "/".into();
    render_models.push(slash_model);

    let mut picker_model = ThreadUiModel::with_startup(startup);
    picker_model.active_conversation = Some(ActiveConversation::Session(thread_id));
    reduce(
        &mut picker_model,
        ThreadUiInput::Snapshot(vec![rich.clone()]),
    );
    picker_model.composer = "/model".into();
    assert!(reduce(&mut picker_model, key(KeyCode::Enter)).is_empty());
    render_models.push(picker_model);

    for mut render_model in render_models {
        for (width, height) in [(140, 44), (52, 16), (39, 5), (39, 4), (32, 8), (1, 1)] {
            render_model.size = (width, height);
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| latte_tui::thread::render(frame, &render_model))
                .unwrap();
        }
    }

    for (width, height) in [(140, 44), (100, 32), (78, 22), (52, 16), (32, 8)] {
        model.size = (width, height);
        for state in 0..6 {
            model.help = state == 1;
            model.command_palette = state == 2;
            model.session_picker = state == 3;
            model.submission_error = (state == 4).then(|| "public submission failure".into());
            model.connection = if state == 5 {
                ConnectionState::SnapshotRequired
            } else {
                ConnectionState::Connected
            };
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| latte_tui::thread::render(frame, &model))
                .unwrap();
            assert!(!terminal.backend().buffer().content().is_empty());
        }
    }
    drop(engine);

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(tui.wait_for_visible_text("render boundary completed", Duration::from_secs(5)));
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_engine_thread_creation_catalog_and_binding_preconditions_fail_closed() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .enabled_tools(["read_file", "list_directory"])
        .deny_globs(["private/**"])
        .database_path(scenario.database_path())
        .conversation_root(scenario.home().join(".latte/latte-code/sessions"))
        .build()
        .unwrap();
    assert_eq!(engine.tool_descriptors().len(), 3);
    assert!(format!("{engine:?}").contains("EngineHandle"));
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let run_id = run_id();

    assert!(matches!(
        engine.create_thread_v2(thread_id, run_id, binding(), " \n ", now),
        Err(StorageError::InvalidData(message)) if message.contains("prompt must not be empty")
    ));
    let mut invalid_binding = binding();
    invalid_binding.provider_name.clear();
    assert!(matches!(
        engine.create_thread_v2(thread_id, run_id, invalid_binding, "invalid binding", now),
        Err(StorageError::InvalidData(message)) if message.contains("provider_name")
    ));
    let foreign_thread = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let foreign_lease = engine
        .acquire_thread_lease(foreign_thread, now, 60_000)
        .unwrap();
    assert!(matches!(
        engine.create_started_thread_v2_snapshot(
            thread_id,
            run_id,
            binding(),
            "wrong scope",
            &foreign_lease,
            now + 1,
            None,
        ),
        Err(StorageError::LeaseLost)
    ));
    engine.release_lease(&foreign_lease).unwrap();
    let expired = engine.acquire_thread_lease(thread_id, now, 1).unwrap();
    assert!(matches!(
        engine.create_started_thread_v2_snapshot(
            thread_id,
            run_id,
            binding(),
            "expired authority",
            &expired,
            now + 2,
            None,
        ),
        Err(StorageError::LeaseLost)
    ));
    let lease = engine
        .acquire_thread_lease(thread_id, now + 2, 60_000)
        .unwrap();
    let running = engine
        .create_started_thread_v2_snapshot(
            thread_id,
            run_id,
            binding(),
            "valid atomic thread",
            &lease,
            now + 3,
            None,
        )
        .unwrap();
    assert_eq!(running.lifecycle, ThreadLifecycle::Running);
    assert!(matches!(
        engine.create_thread_follow_up_v2(
            thread_id,
            RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            running.revision,
            "",
            now + 4,
        ),
        Err(StorageError::InvalidData(message)) if message.contains("follow-up must not be empty")
    ));
    assert!(matches!(
        engine.create_thread_follow_up_v2(
            thread_id,
            RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            running.revision,
            "not ready",
            now + 4,
        ),
        Err(StorageError::InvalidData(message)) if message.contains("ready thread")
    ));
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            running.revision,
            &binding(),
            &lease,
            now + 4,
        ),
        Err(StorageError::InvalidData(message)) if message.contains("ready thread")
    ));
    assert!(
        engine
            .list_thread_sessions_v2_for_workspace(scenario.root().to_str().unwrap(), 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        engine
            .find_thread_sessions_v2_by_exact_title_for_workspace(
                scenario.root().to_str().unwrap(),
                "",
                100,
            )
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        engine.thread_snapshot_v2(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            None,
            10,
        ),
        Err(StorageError::ThreadNotFound(_))
    ));
    assert!(
        engine
            .thread_session_v2(ThreadId::from_uuid(
                SystemIdSource::default().next_uuid_v7()
            ))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        engine.thread_run_changed_files(RunId::from_uuid(
            SystemIdSource::default().next_uuid_v7()
        )),
        Err(StorageError::InvalidData(message)) if message.contains("baseline")
    ));

    let ready = commit(
        &engine,
        &lease,
        &running,
        CommitThreadRunUpdate::Complete {
            source_key: "preconditions:complete".into(),
            handoff: Handoff {
                summary: "precondition parent complete".into(),
                files_changed: Vec::new(),
                evidence: Vec::new(),
            },
        },
        now + 5,
    );
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            ready.revision.saturating_add(1),
            &binding(),
            &lease,
            now + 6,
        ),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    let mut invalid_binding = binding();
    invalid_binding.model = "bad\nmodel".into();
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            ready.revision,
            &invalid_binding,
            &lease,
            now + 6,
        ),
        Err(StorageError::InvalidData(message)) if message.contains("model")
    ));
    let wrong_lease = engine
        .acquire_thread_lease(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            now + 6,
            60_000,
        )
        .unwrap();
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            ready.revision,
            &binding(),
            &wrong_lease,
            now + 7,
        ),
        Err(StorageError::LeaseLost)
    ));
    engine.release_lease(&wrong_lease).unwrap();
    let mut alternate = binding();
    alternate.provider_name = "alternate".into();
    alternate.model = "alternate-model".into();
    let switched = engine
        .switch_thread_binding_v2(thread_id, ready.revision, &alternate, &lease, now + 7)
        .unwrap();
    assert_eq!(switched.binding.model, "alternate-model");

    assert!(matches!(
        engine.create_thread_follow_up_v2(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            0,
            "missing thread",
            now + 8,
        ),
        Err(StorageError::ThreadNotFound(_))
    ));
    assert!(matches!(
        engine.create_thread_follow_up_v2(
            thread_id,
            RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            switched.revision.saturating_add(1),
            "stale follow-up",
            now + 8,
        ),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    assert!(matches!(
        engine.create_started_thread_follow_up_v2(
            thread_id,
            RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            switched.revision,
            "wrong scoped follow-up",
            &foreign_lease,
            now + 8,
        ),
        Err(StorageError::LeaseLost)
    ));
    let child_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let child = engine
        .create_started_thread_follow_up_v2(
            thread_id,
            child_id,
            switched.revision,
            "valid atomic follow-up",
            &lease,
            now + 9,
        )
        .unwrap();
    assert_eq!(child.active_run_id, Some(child_id));
    assert_eq!(child.runs.len(), 2);
    assert_eq!(
        engine
            .thread_snapshot_tail_v2(thread_id, usize::MAX)
            .unwrap()
            .runs
            .len(),
        2
    );
    engine.release_lease(&lease).unwrap();
    drop(engine);

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_json = json(&listed);
    let sessions = listed_json["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["runs"].as_array().unwrap().len(), 2);

    let jsonl_engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .conversation_root(scenario.home().join(".latte/latte-code/sessions"))
        .build()
        .unwrap();
    assert_eq!(
        jsonl_engine
            .thread_snapshot_tail_v2(thread_id, 500)
            .unwrap()
            .runs
            .len(),
        2
    );
    let session_files = scenario.session_files();
    let [session_file] = session_files.as_slice() else {
        panic!("expected one JSONL Session file");
    };
    let original = std::fs::read_to_string(session_file).unwrap();
    let mut lines = original.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let entry: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let corruptions = [
        {
            let mut value = header.clone();
            value["workspace_id"] = serde_json::json!("foreign-workspace");
            format!("{}\n", serde_json::to_string(&value).unwrap())
        },
        {
            let mut value = entry.clone();
            value["record"] = serde_json::json!("unknown");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("seq");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("entry_id");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&entry).unwrap(),
            serde_json::to_string(&entry).unwrap()
        ),
        {
            let mut value = entry.clone();
            value["entry_id"] = serde_json::json!("not-a-uuid");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value["run_id"] = serde_json::json!(7);
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("kind");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("content");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("source_key");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        {
            let mut value = entry.clone();
            value.as_object_mut().unwrap().remove("created_at_ms");
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header).unwrap(),
                serde_json::to_string(&value).unwrap()
            )
        },
        format!(
            "{}\n{{invalid json}}\n",
            serde_json::to_string(&header).unwrap()
        ),
        String::new(),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            "x".repeat(2 * 1024 * 1024 + 1)
        ),
    ];
    for corruption in corruptions {
        std::fs::write(session_file, corruption).unwrap();
        assert!(matches!(
            jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
            Err(StorageError::InvalidData(_))
        ));
        std::fs::write(session_file, &original).unwrap();
    }

    let insert_outbox = |entry: &TranscriptEntry| {
        rusqlite::Connection::open(scenario.database_path())
            .unwrap()
            .execute(
                "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) \
                 VALUES(?1,?2,?3,NULL,'user',?4,?5,?6)",
                rusqlite::params![
                    thread_id.to_string(),
                    i64::try_from(entry.sequence).unwrap(),
                    entry.entry_id.to_string(),
                    entry.source_key,
                    serde_json::to_string(entry).unwrap(),
                    i64::try_from(entry.created_at_ms).unwrap(),
                ],
            )
            .unwrap();
    };
    let clear_outbox = || {
        rusqlite::Connection::open(scenario.database_path())
            .unwrap()
            .execute(
                "DELETE FROM conversation_outbox WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
    };
    let conflicting = TranscriptEntry {
        entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        sequence: entry["seq"].as_u64().unwrap(),
        run_id: None,
        kind: TranscriptKind::User,
        text: "conflicting entry identity".into(),
        payload: None,
        source_key: "coverage:conflicting-entry".into(),
        created_at_ms: now + 20,
    };
    insert_outbox(&conflicting);
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    clear_outbox();

    let mut later_entry = entry.clone();
    later_entry["seq"] = serde_json::json!(2);
    std::fs::write(
        session_file,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&later_entry).unwrap()
        ),
    )
    .unwrap();
    let earlier = TranscriptEntry {
        sequence: 1,
        source_key: "coverage:earlier-entry".into(),
        ..conflicting.clone()
    };
    insert_outbox(&earlier);
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    clear_outbox();
    std::fs::write(session_file, &original).unwrap();

    let last_sequence = original
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value["seq"].as_u64())
        .max()
        .unwrap();
    let oversized_entry = TranscriptEntry {
        entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
        sequence: last_sequence + 1,
        run_id: None,
        kind: TranscriptKind::User,
        text: "x".repeat(2 * 1024 * 1024 + 1),
        payload: None,
        source_key: "coverage:oversized-entry".into(),
        created_at_ms: now + 21,
    };
    insert_outbox(&oversized_entry);
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    clear_outbox();
    std::fs::write(session_file, &original).unwrap();

    let oversized = std::fs::File::create(session_file).unwrap();
    oversized.set_len(64 * 1024 * 1024 + 1).unwrap();
    drop(oversized);
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    std::fs::write(session_file, &original).unwrap();

    let symlink_target = scenario.root().join("symlink-target.jsonl");
    std::fs::write(&symlink_target, &original).unwrap();
    std::fs::remove_file(session_file).unwrap();
    std::os::unix::fs::symlink(&symlink_target, session_file).unwrap();
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    std::fs::remove_file(session_file).unwrap();
    std::fs::write(session_file, &original).unwrap();

    std::fs::remove_file(session_file).unwrap();
    std::fs::create_dir(session_file).unwrap();
    assert!(matches!(
        jsonl_engine.thread_snapshot_tail_v2(thread_id, 500),
        Err(StorageError::InvalidData(_))
    ));
    std::fs::remove_dir(session_file).unwrap();
    std::fs::write(session_file, &original).unwrap();
    assert!(jsonl_engine.thread_snapshot_tail_v2(thread_id, 500).is_ok());

    let foreign_workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(foreign_workspace.path().join(".git")).unwrap();
    let foreign_engine = EngineBuilder::new()
        .workspace_root(foreign_workspace.path())
        .database_path(scenario.database_path())
        .conversation_root(scenario.home().join(".latte/latte-code/sessions"))
        .build()
        .unwrap();
    assert!(matches!(
        foreign_engine.rename_thread_session_v2(thread_id, "foreign rename"),
        Err(StorageError::InvalidData(message)) if message.contains("current workspace")
    ));
    assert!(matches!(
        foreign_engine.thread_snapshot_tail_v2(thread_id, 1),
        Err(StorageError::InvalidData(message)) if message.contains("foreign workspace")
    ));

    EngineBuilder::new().workspace_root("/").build().unwrap();

    let linked_root = tempfile::tempdir().unwrap();
    let common = linked_root.path().join("common");
    let git_dir = linked_root.path().join("git-dir");
    let worktree = linked_root.path().join("linked-worktree");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: ../git-dir\n").unwrap();
    std::fs::write(git_dir.join("commondir"), "../common\n").unwrap();
    EngineBuilder::new()
        .workspace_root(&worktree)
        .build()
        .unwrap();

    let standalone_git_dir = linked_root.path().join("standalone-git-dir");
    let standalone = linked_root.path().join("standalone-worktree");
    std::fs::create_dir_all(&standalone_git_dir).unwrap();
    std::fs::create_dir_all(&standalone).unwrap();
    std::fs::write(standalone.join(".git"), "gitdir: ../standalone-git-dir\n").unwrap();
    EngineBuilder::new()
        .workspace_root(&standalone)
        .build()
        .unwrap();

    let absolute_git_dir = linked_root.path().join("absolute-git-dir");
    let absolute_common = linked_root.path().join("absolute-common");
    let absolute_worktree = linked_root.path().join("absolute-worktree");
    std::fs::create_dir_all(&absolute_git_dir).unwrap();
    std::fs::create_dir_all(&absolute_common).unwrap();
    std::fs::create_dir_all(&absolute_worktree).unwrap();
    std::fs::write(
        absolute_worktree.join(".git"),
        format!("gitdir: {}\n", absolute_git_dir.display()),
    )
    .unwrap();
    std::fs::write(
        absolute_git_dir.join("commondir"),
        format!("{}\n", absolute_common.display()),
    )
    .unwrap();
    EngineBuilder::new()
        .workspace_root(&absolute_worktree)
        .build()
        .unwrap();

    let conversation_target = tempfile::tempdir().unwrap();
    let conversation_link = linked_root.path().join("conversation-link");
    std::os::unix::fs::symlink(conversation_target.path(), &conversation_link).unwrap();
    assert!(matches!(
        EngineBuilder::new()
            .workspace_root(&worktree)
            .conversation_root(&conversation_link)
            .build(),
        Err(StorageError::InvalidData(message)) if message.contains("symlink")
    ));
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
    input_tui.write(format!("/resume {input_thread_id}\r").as_bytes());
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
        let shown = scenario.output(&["--json", "show", &input_thread_id.to_string()], |_| {});
        shown.status.success()
            && json(&shown)["data"]["session"]["lifecycle"] == "failed"
            && json(&shown)["data"]["session"]["runs"]
                .as_array()
                .is_some_and(|runs| runs.iter().any(|run| run["failure_code"] == "cancelled"))
    }));
    input_tui.write(F10);
    assert!(input_tui.finish(Duration::from_secs(5)).0.success());
    let input_shown = scenario.output(&["--json", "show", &input_thread_id.to_string()], |_| {});
    assert!(input_shown.status.success());
    assert_eq!(
        json(&input_shown)["data"]["session"]["runs"][0]["failure_code"],
        "cancelled"
    );
    let input_listed = scenario.output(&["--json", "list"], |_| {});
    assert!(input_listed.status.success());
    assert_eq!(
        json(&input_listed)["data"]["sessions"]
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
    permission_tui.write(format!("/resume {permission_thread_id}\r").as_bytes());
    assert!(permission_tui.wait_for_output(b"Permission required", Duration::from_secs(5)));
    assert!(permission_tui.wait_for_output(b"must-not-exist.txt", Duration::from_secs(5)));
    engine.release_lease(&lease).unwrap();
    drop(engine);
    permission_tui.write(b"d");
    assert!(wait_until(Duration::from_secs(5), || {
        let shown = scenario.output(
            &["--json", "show", &permission_thread_id.to_string()],
            |_| {},
        );
        shown.status.success()
            && json(&shown)["data"]["session"]["runs"]
                .as_array()
                .is_some_and(|runs| {
                    runs.iter().any(|run| {
                        run["status"] == "failed" && run["failure_code"] == "permission_denied"
                    })
                })
    }));
    assert!(!scenario.root().join("must-not-exist.txt").exists());
    permission_tui.write(F10);
    assert!(permission_tui.finish(Duration::from_secs(5)).0.success());

    let shown = scenario.output(
        &["--json", "show", &permission_thread_id.to_string()],
        |_| {},
    );
    assert!(shown.status.success());
    // Denial terminalizes the child run but returns the conversation to Ready.
    assert_eq!(json(&shown)["data"]["session"]["lifecycle"], "ready");
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["failure_code"],
        "permission_denied"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        1
    );
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
    tui.write(format!("/resume {verification_thread_id}\r").as_bytes());
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

    let effect_shown = scenario.output(&["--json", "show", &effect_thread_id.to_string()], |_| {});
    assert!(effect_shown.status.success());
    let effect_session = json(&effect_shown)["data"]["session"].clone();
    assert_eq!(effect_session["lifecycle"], "failed");
    assert_eq!(effect_session["runs"][0]["status"], "failed");
    assert!(
        effect_session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "failure"
                && entry["text"] == "boundary observed effect failed")
    );

    let tree_shown = scenario.output(&["--json", "show", &tree_thread_id.to_string()], |_| {});
    assert!(tree_shown.status.success());
    let tree_session = json(&tree_shown)["data"]["session"].clone();
    assert_eq!(tree_session["lifecycle"], "interrupted");
    let tree_runs = tree_session["runs"].as_array().unwrap();
    assert_eq!(tree_runs.len(), 2);
    assert_eq!(tree_runs[0]["run_id"], parent_run_id.to_string());
    assert_eq!(tree_runs[0]["status"], "completed");
    assert_eq!(tree_runs[1]["run_id"], interrupted_run_id.to_string());
    assert_eq!(tree_runs[1]["status"], "interrupted");
    assert!(
        tree_session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "completion"
                && entry["text"] == "boundary parent completed")
    );

    let verification_shown = scenario.output(
        &["--json", "show", &verification_thread_id.to_string()],
        |_| {},
    );
    assert!(verification_shown.status.success());
    let verification_session = json(&verification_shown)["data"]["session"].clone();
    assert_eq!(verification_session["lifecycle"], "failed");
    assert_eq!(verification_session["runs"][0]["status"], "failed");
    assert!(
        verification_session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "failure"
                && entry["text"] == "boundary verification failed with exit 9")
    );

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        3
    );
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

    let shown = scenario.output(&["--json", "show", &thread_id.to_string()], |_| {});
    assert!(shown.status.success());
    let session = json(&shown)["data"]["session"].clone();
    assert_eq!(session["lifecycle"], "running");
    assert_eq!(
        session["runs"][0]["run_revision"],
        authoritative_run_revision
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_sessions = json(&listed)["data"]["sessions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(listed_sessions.len(), 1);
    assert_eq!(listed_sessions[0]["thread_id"], thread_id.to_string());
    assert_eq!(listed_sessions[0]["lifecycle"], "running");

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {thread_id}\r").as_bytes());
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

    let shown = scenario.output(&["--json", "show", &thread_id.to_string()], |_| {});
    assert!(shown.status.success());
    assert_eq!(json(&shown)["data"]["session"]["lifecycle"], "running");
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_json = json(&listed);
    let listed_sessions = listed_json["data"]["sessions"].as_array().unwrap();
    assert_eq!(listed_sessions.len(), 1);
    assert_eq!(listed_sessions[0]["thread_id"], thread_id.to_string());
    assert_eq!(listed_sessions[0]["lifecycle"], "running");

    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {thread_id}\r").as_bytes());
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

#[test]
#[allow(clippy::too_many_lines)]
fn atomic_session_and_follow_up_enforce_scope_and_remain_final_binary_visible() {
    let scenario = Scenario::new();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let parent_run_id = run_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let wrong_scope = engine
        .acquire_lease("v2-atomic-wrong-scope", now, 120_000)
        .unwrap();

    assert!(matches!(
        engine.create_started_thread_v2_snapshot(
            thread_id,
            parent_run_id,
            binding(),
            "atomic boundary session",
            &wrong_scope,
            now + 1,
            None,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert!(engine.thread_session_v2(thread_id).unwrap().is_none());
    assert!(engine.show(parent_run_id).is_err());

    let started = engine
        .create_started_thread_v2_snapshot(
            thread_id,
            parent_run_id,
            binding(),
            "atomic boundary session",
            &lease,
            now + 2,
            None,
        )
        .unwrap();
    assert_eq!(started.lifecycle, ThreadLifecycle::Running);
    assert_eq!(active_run(&started), (parent_run_id, 1));

    let mut changed_binding = binding();
    changed_binding.provider_name = "v2-boundary-next".into();
    changed_binding.model = "v2-boundary-next-model".into();
    changed_binding.config_fingerprint = "v2-boundary-next-config".into();
    assert!(
        engine
            .switch_thread_binding_v2(
                thread_id,
                started.revision,
                &changed_binding,
                &lease,
                now + 3,
            )
            .is_err()
    );
    assert_eq!(
        engine.thread_snapshot_v2(thread_id, None, 100).unwrap(),
        started
    );

    let completed = commit(
        &engine,
        &lease,
        &started,
        CommitThreadRunUpdate::Complete {
            source_key: "boundary:atomic:complete".into(),
            handoff: Handoff {
                summary: "atomic parent completed".into(),
                files_changed: Vec::new(),
                evidence: Vec::new(),
            },
        },
        now + 4,
    );
    assert_eq!(completed.lifecycle, ThreadLifecycle::Ready);
    assert!(
        engine
            .switch_thread_binding_v2(thread_id, completed.revision, &binding(), &lease, now + 5,)
            .is_err()
    );
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            completed.revision - 1,
            &changed_binding,
            &lease,
            now + 6,
        ),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    assert!(matches!(
        engine.switch_thread_binding_v2(
            thread_id,
            completed.revision,
            &changed_binding,
            &wrong_scope,
            now + 7,
        ),
        Err(StorageError::LeaseLost)
    ));
    let switched = engine
        .switch_thread_binding_v2(
            thread_id,
            completed.revision,
            &changed_binding,
            &lease,
            now + 8,
        )
        .unwrap();
    assert_eq!(switched.lifecycle, ThreadLifecycle::Ready);
    assert_eq!(switched.binding, changed_binding);

    let workspace = std::fs::canonicalize(scenario.root())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        engine
            .list_thread_sessions_v2_for_workspace(&workspace, 10)
            .unwrap()
            .len(),
        1
    );
    assert!(
        engine
            .list_thread_sessions_v2_for_workspace("/definitely/not/this/workspace", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engine
            .find_thread_sessions_v2_by_exact_title_for_workspace(
                &workspace,
                "atomic boundary session",
                10,
            )
            .unwrap()
            .len(),
        1
    );
    assert!(
        engine
            .find_thread_sessions_v2_by_exact_title_for_workspace(&workspace, "", 10)
            .unwrap()
            .is_empty()
    );
    assert!(engine
        .find_thread_sessions_v2_by_exact_title_for_workspace(
            &workspace,
            "atomic boundary",
            10,
        )
        .unwrap()
        .is_empty());
    assert_eq!(
        engine.list_threads_v2_for_workspace(&workspace).unwrap()[0].thread_id,
        thread_id
    );
    assert!(
        engine
            .list_threads_v2_for_workspace("/definitely/not/this/workspace")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engine.thread_session_v2(thread_id).unwrap().unwrap().model,
        "v2-boundary-next-model"
    );
    let tail = engine.thread_snapshot_tail_v2(thread_id, 2).unwrap();
    assert_eq!(tail.transcript.entries.len(), 2);
    assert_eq!(
        tail.transcript.entries.last().unwrap().text,
        "Model switched to v2-boundary-next/v2-boundary-next-model"
    );

    assert!(matches!(
        engine.rename_thread_session_v2(thread_id, "  "),
        Err(StorageError::InvalidData(message)) if message.contains("title")
    ));
    let renamed = engine
        .rename_thread_session_v2(thread_id, "Renamed atomic boundary")
        .unwrap();
    assert_eq!(renamed.title, "Renamed atomic boundary");
    assert_eq!(
        engine
            .search_thread_sessions_v2("renamed atomic", 10)
            .unwrap()[0]
            .thread_id,
        thread_id
    );
    assert_eq!(
        engine
            .search_thread_sessions_v2(&thread_id.to_string()[..12], 10)
            .unwrap()[0]
            .thread_id,
        thread_id
    );
    assert!(engine.search_thread_sessions_v2("", 0).unwrap().is_empty());
    assert!(
        engine
            .search_thread_sessions_v2("definitely absent", 10)
            .unwrap()
            .is_empty()
    );

    let fork_thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let fork = engine
        .fork_thread_session_v2(
            thread_id,
            fork_thread_id,
            Some("Forked atomic boundary"),
            now + 20,
        )
        .unwrap();
    assert_eq!(fork.lifecycle, ThreadLifecycle::Ready);
    assert!(fork.runs.is_empty());
    let fork_summary = engine.thread_session_v2(fork_thread_id).unwrap().unwrap();
    assert_eq!(fork_summary.parent_thread_id, Some(thread_id));
    assert_eq!(
        fork.transcript.entries.len(),
        switched.transcript.entries.len()
    );
    assert!(
        fork.transcript
            .entries
            .iter()
            .zip(&switched.transcript.entries)
            .all(|(forked, source)| {
                forked.kind == source.kind
                    && forked.text == source.text
                    && forked.payload == source.payload
                    && forked.run_id.is_none()
            })
    );
    assert_eq!(
        engine
            .search_thread_sessions_v2("forked atomic", 10)
            .unwrap()[0]
            .thread_id,
        fork_thread_id
    );
    assert!(
        engine
            .fork_thread_session_v2(thread_id, fork_thread_id, Some("duplicate"), now + 21,)
            .is_err()
    );
    let default_fork_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let default_fork = engine
        .fork_thread_session_v2(thread_id, default_fork_id, None, now + 22)
        .unwrap();
    assert_eq!(
        engine
            .thread_session_v2(default_fork_id)
            .unwrap()
            .unwrap()
            .title,
        "Renamed atomic boundary (fork)"
    );
    assert!(default_fork.runs.is_empty());
    let missing_source = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    assert!(matches!(
        engine.rename_thread_session_v2(missing_source, "missing"),
        Err(StorageError::ThreadNotFound(id)) if id == missing_source
    ));
    let bounded_title = engine
        .rename_thread_session_v2(thread_id, &"x".repeat(1_025))
        .unwrap();
    assert!(bounded_title.title.ends_with('…'));
    assert!(bounded_title.title.len() <= 123);
    assert_eq!(
        engine.search_thread_sessions_v2("   ", 10).unwrap().len(),
        3
    );
    assert!(matches!(
        engine.fork_thread_session_v2(
            missing_source,
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            Some("missing"),
            now + 22,
        ),
        Err(StorageError::ThreadNotFound(id)) if id == missing_source
    ));

    let memory_root = tempfile::tempdir().unwrap();
    let memory_engine = EngineBuilder::new()
        .workspace_root(memory_root.path())
        .build()
        .unwrap();
    let memory_thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let memory_lease = memory_engine
        .acquire_thread_lease(memory_thread_id, now + 26, 120_000)
        .unwrap();
    let memory_running = memory_engine
        .create_started_thread_v2_snapshot(
            memory_thread_id,
            run_id(),
            binding(),
            "memory-only session",
            &memory_lease,
            now + 27,
            None,
        )
        .unwrap();
    let memory_ready = commit(
        &memory_engine,
        &memory_lease,
        &memory_running,
        CommitThreadRunUpdate::Complete {
            source_key: "boundary:memory:complete".into(),
            handoff: Handoff {
                summary: "memory session completed".into(),
                files_changed: Vec::new(),
                evidence: Vec::new(),
            },
        },
        now + 28,
    );
    assert_eq!(memory_ready.lifecycle, ThreadLifecycle::Ready);
    assert!(matches!(
        memory_engine.fork_thread_session_v2(
            memory_thread_id,
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            None,
            now + 29,
        ),
        Err(StorageError::InvalidData(message)) if message.contains("requires JSONL")
    ));
    memory_engine.release_lease(&memory_lease).unwrap();

    let fork_lease = engine
        .acquire_thread_lease(fork_thread_id, now + 23, 120_000)
        .unwrap();
    let fork_run_id = run_id();
    let fork_running = engine
        .create_started_thread_follow_up_v2(
            fork_thread_id,
            fork_run_id,
            fork.revision,
            "independent fork child",
            &fork_lease,
            now + 24,
        )
        .unwrap();
    let fork_completed = commit(
        &engine,
        &fork_lease,
        &fork_running,
        CommitThreadRunUpdate::Complete {
            source_key: "boundary:fork:complete".into(),
            handoff: Handoff {
                summary: "fork child completed independently".into(),
                files_changed: Vec::new(),
                evidence: Vec::new(),
            },
        },
        now + 25,
    );
    assert_eq!(fork_completed.lifecycle, ThreadLifecycle::Ready);
    assert_eq!(fork_completed.runs.len(), 1);
    engine.release_lease(&fork_lease).unwrap();

    let follow_up_id = run_id();
    assert!(matches!(
        engine.create_started_thread_follow_up_v2(
            thread_id,
            follow_up_id,
            switched.revision - 1,
            "atomic follow-up must not appear",
            &lease,
            now + 9,
        ),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    assert!(engine.show(follow_up_id).is_err());
    assert!(matches!(
        engine.create_started_thread_follow_up_v2(
            thread_id,
            follow_up_id,
            switched.revision,
            "atomic follow-up must not appear",
            &wrong_scope,
            now + 10,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert!(engine.show(follow_up_id).is_err());
    let follow_up = engine
        .create_started_thread_follow_up_v2(
            thread_id,
            follow_up_id,
            switched.revision,
            "atomic follow-up accepted",
            &lease,
            now + 11,
        )
        .unwrap();
    assert_eq!(follow_up.lifecycle, ThreadLifecycle::Running);
    assert_eq!(active_run(&follow_up), (follow_up_id, 1));
    assert_eq!(follow_up.runs.len(), 2);
    engine.release_lease(&lease).unwrap();
    engine.release_lease(&wrong_scope).unwrap();
    drop(engine);

    let shown = scenario.output(&["--json", "show", &thread_id.to_string()], |_| {});
    assert!(shown.status.success());
    let session = json(&shown)["data"]["session"].clone();
    assert_eq!(session["lifecycle"], "interrupted");
    let runs = session["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["run_id"], parent_run_id.to_string());
    assert_eq!(runs[0]["status"], "completed");
    assert_eq!(runs[1]["run_id"], follow_up_id.to_string());
    assert_eq!(runs[1]["status"], "interrupted");
    assert!(
        session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |entry| entry["kind"] == "completion" && entry["text"] == "atomic parent completed"
            )
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        3
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_unknown_reconciliation_is_fenced_and_final_binary_visible() {
    let scenario = Scenario::new();
    std::fs::write(
        scenario.root().join("unknown-boundary.txt"),
        "unknown boundary sentinel\n",
    )
    .unwrap();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let thread_id = thread_id();
    let run_id = run_id();
    let lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    let wrong_scope = engine
        .acquire_lease("v2-unknown-wrong-scope", now, 120_000)
        .unwrap();
    let running = engine
        .create_started_thread_v2_snapshot(
            thread_id,
            run_id,
            binding(),
            "explicit unknown boundary",
            &lease,
            now + 1,
            None,
        )
        .unwrap();
    let prepared = prepare_effect(
        &engine,
        &lease,
        &running,
        "explicit-unknown-effect",
        "read_file",
        serde_json::json!({"path":"unknown-boundary.txt"}),
        now + 2,
    );
    let started = start_effect(
        &engine,
        &lease,
        &prepared,
        "explicit-unknown-effect",
        now + 3,
    );
    assert!(matches!(
        engine.mark_thread_effect_unknown(
            &started,
            "boundary:unknown:wrong-scope".into(),
            command_id(),
            &wrong_scope,
            now + 4,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert_eq!(
        engine.effect_status("explicit-unknown-effect").unwrap(),
        latte_engine::EffectStatus::Started
    );

    let unknown = engine
        .mark_thread_effect_unknown(
            &started,
            "boundary:unknown:mark".into(),
            command_id(),
            &lease,
            now + 5,
        )
        .unwrap();
    assert_eq!(unknown.lifecycle, ThreadLifecycle::ReconciliationRequired);
    assert_eq!(
        engine.effect_status("explicit-unknown-effect").unwrap(),
        latte_engine::EffectStatus::Unknown
    );
    let run_revision = unknown
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;
    assert!(matches!(
        engine.reconcile_thread_effect_unknown(
            thread_id,
            run_id,
            unknown.revision,
            run_revision,
            "explicit-unknown-effect".into(),
            "boundary:unknown:wrong-scope-reconcile".into(),
            command_id(),
            &wrong_scope,
            now + 6,
        ),
        Err(StorageError::LeaseLost)
    ));
    assert!(matches!(
        engine.reconcile_thread_effect_unknown(
            thread_id,
            run_id,
            unknown.revision - 1,
            run_revision,
            "explicit-unknown-effect".into(),
            "boundary:unknown:stale-reconcile".into(),
            command_id(),
            &lease,
            now + 7,
        ),
        Err(StorageError::StaleThreadRevision { .. })
    ));
    let reconciled = engine
        .reconcile_thread_effect_unknown(
            thread_id,
            run_id,
            unknown.revision,
            run_revision,
            "explicit-unknown-effect".into(),
            "boundary:unknown:reconcile".into(),
            command_id(),
            &lease,
            now + 8,
        )
        .unwrap();
    assert_eq!(reconciled.lifecycle, ThreadLifecycle::Failed);
    assert!(reconciled.pending.is_none());
    assert_eq!(
        engine.effect_status("explicit-unknown-effect").unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
    engine.release_lease(&lease).unwrap();
    engine.release_lease(&wrong_scope).unwrap();
    drop(engine);

    let shown = scenario.output(&["--json", "show", &thread_id.to_string()], |_| {});
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
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    let listed_json = json(&listed);
    let listed_sessions = listed_json["data"]["sessions"].as_array().unwrap();
    assert_eq!(listed_sessions.len(), 1);
    assert_eq!(listed_sessions[0]["lifecycle"], "failed");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_change_feeds_require_snapshot_reload_after_lag_and_close_cleanly() {
    let scenario = Scenario::new();
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();
    let mut legacy_events = engine.subscribe();
    let mut thread_events = engine.subscribe_threads();
    assert_eq!(legacy_events.try_recv().unwrap(), None);
    assert_eq!(thread_events.try_recv().unwrap(), None);

    let thread_id = thread_id();
    let thread_run_id = run_id();
    let thread_lease = engine
        .acquire_thread_lease(thread_id, now, 120_000)
        .unwrap();
    assert_eq!(thread_lease.scope(), format!("thread:{thread_id}"));
    let mut snapshot = engine
        .create_started_thread_v2_snapshot(
            thread_id,
            thread_run_id,
            binding(),
            "change feed snapshot fallback",
            &thread_lease,
            now + 1,
            None,
        )
        .unwrap();
    let created_event = thread_events.try_recv().unwrap().unwrap();
    assert_eq!(created_event.thread_id, thread_id);
    assert_eq!(created_event.revision, snapshot.revision);

    snapshot = commit(
        &engine,
        &thread_lease,
        &snapshot,
        CommitThreadRunUpdate::AppendTranscript {
            source_key: "boundary:feed:observed".into(),
            kind: TranscriptKind::System,
            text: "change feed observed event".into(),
            payload: None,
        },
        now + 2,
    );
    let observed_event = thread_events.recv().await.unwrap();
    assert_eq!(observed_event.thread_id, thread_id);
    assert_eq!(observed_event.sequence, snapshot.sequence);

    for ordinal in 0_u64..70 {
        snapshot = commit(
            &engine,
            &thread_lease,
            &snapshot,
            CommitThreadRunUpdate::AppendTranscript {
                source_key: format!("boundary:feed:lag:{ordinal:02}"),
                kind: TranscriptKind::System,
                text: format!("change feed durable card {ordinal:02}"),
                payload: Some(serde_json::json!({"ordinal":ordinal})),
            },
            now + 3 + ordinal,
        );
    }
    assert!(matches!(
        thread_events.try_recv(),
        Err(SubscriptionError::Lagged(count)) if count >= 6
    ));
    let authoritative_tail = engine.thread_snapshot_tail_v2(thread_id, 5).unwrap();
    assert_eq!(authoritative_tail.revision, snapshot.revision);
    assert_eq!(authoritative_tail.sequence, snapshot.sequence);
    assert_eq!(authoritative_tail.transcript.entries.len(), 5);
    assert_eq!(
        authoritative_tail.transcript.entries.last().unwrap().text,
        "change feed durable card 69"
    );

    let mut legacy_ids = Vec::new();
    for ordinal in 0_u64..40 {
        let id = run_id();
        engine.create_run(id, now + 100 + ordinal).unwrap();
        legacy_ids.push(id);
    }
    let legacy_lease = engine
        .acquire_lease("change-feed-legacy", now + 200, 120_000)
        .unwrap();
    assert_eq!(legacy_lease.scope(), "runtime");
    for (ordinal, id) in legacy_ids.iter().copied().enumerate() {
        engine
            .apply_transition(
                id,
                0,
                latte_core::Transition::Start,
                now + 201 + u64::try_from(ordinal).unwrap(),
                &legacy_lease,
            )
            .unwrap();
    }
    assert!(matches!(
        legacy_events.try_recv(),
        Err(SubscriptionError::Lagged(count)) if count >= 8
    ));

    let mut next_legacy_event = engine.subscribe();
    assert_eq!(next_legacy_event.try_recv().unwrap(), None);
    let event_run_id = run_id();
    engine.create_run(event_run_id, now + 300).unwrap();
    engine
        .apply_transition(
            event_run_id,
            0,
            latte_core::Transition::Start,
            now + 301,
            &legacy_lease,
        )
        .unwrap();
    let event = next_legacy_event.recv().await.unwrap();
    assert_eq!(event.run_id, event_run_id);
    assert_eq!(event.revision, 1);

    engine.release_lease(&legacy_lease).unwrap();
    let missing_run = run_id();
    assert!(matches!(
        engine.acquire_run_lease(missing_run, "missing-run-owner", now + 400, 120_000),
        Err(StorageError::RunNotFound(id)) if id == missing_run
    ));
    assert!(matches!(
        engine.acquire_run_lease(thread_run_id, "linked-run-owner", now + 401, 120_000,),
        Err(StorageError::LinkedRunRequiresThreadCommit)
    ));
    let exact_run_lease = engine
        .acquire_run_lease(event_run_id, "exact-run-owner", now + 402, 120_000)
        .unwrap();
    let same_epoch = engine
        .acquire_run_lease(event_run_id, "exact-run-owner", now + 403, 120_000)
        .unwrap();
    assert_eq!(same_epoch.fencing_token(), exact_run_lease.fencing_token());
    let renewed = engine.renew_lease(&same_epoch, now + 404, 240_000).unwrap();
    assert_eq!(renewed.fencing_token(), same_epoch.fencing_token());
    assert!(renewed.expires_at_ms() > same_epoch.expires_at_ms());
    assert!(matches!(
        engine.acquire_lease("competing-exact-owner", now + 405, 120_000),
        Err(StorageError::EngineUnavailable)
    ));
    engine.release_lease(&renewed).unwrap();
    engine.release_lease(&thread_lease).unwrap();

    let mut closed_legacy = engine.subscribe();
    let mut closed_threads = engine.subscribe_threads();
    drop(engine);
    assert_eq!(closed_legacy.try_recv(), Err(SubscriptionError::Closed));
    assert_eq!(closed_threads.try_recv(), Err(SubscriptionError::Closed));

    let shown = scenario.output(&["--json", "show", &thread_id.to_string()], |_| {});
    assert!(shown.status.success());
    assert_eq!(json(&shown)["data"]["session"]["lifecycle"], "interrupted");
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn effect_validation_and_permission_summaries_are_final_binary_visible() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::write(
        scenario.root().join("permission-summary.txt"),
        "before permission summary\n",
    )
    .unwrap();
    let default_database = latte_code::DatabaseConfig::default();
    assert_eq!(default_database.path, ".latte/latte-code.db");
    let verification: latte_code::VerificationConfig =
        serde_json::from_value(serde_json::json!({"argv":["/bin/true"]})).unwrap();
    assert_eq!(verification.cwd, ".");
    assert_eq!(verification.timeout_ms, 120_000);
    let memory_engine = EngineBuilder::new()
        .workspace_root(scenario.root())
        .enabled_tools(Vec::<String>::new())
        .deny_globs(Vec::<String>::new())
        .build()
        .unwrap();
    assert!(format!("{memory_engine:?}").contains("EngineHandle"));
    assert_eq!(
        memory_engine
            .tool_descriptors()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        vec!["process"]
    );
    drop(memory_engine);
    let engine = build_engine(&scenario);
    let now = latte_core::wall_time_ms();

    let (validation, validation_lease, _) =
        create_started_effect_thread(&engine, "effect descriptor validation", now);
    let (validation_run_id, validation_run_revision) = active_run(&validation);
    let valid_descriptor = ThreadEffectDescriptor {
        effect_id: "validation-effect".into(),
        tool_call_id: "call-validation-effect".into(),
        name: "read_file".into(),
        input: serde_json::json!({"path":"permission-summary.txt"}),
        attempt: 1,
    };
    let invalid_descriptors = [
        ThreadEffectDescriptor {
            effect_id: String::new(),
            ..valid_descriptor.clone()
        },
        ThreadEffectDescriptor {
            name: "read\nfile".into(),
            ..valid_descriptor.clone()
        },
        ThreadEffectDescriptor {
            tool_call_id: "invalid tool call id".into(),
            ..valid_descriptor.clone()
        },
        ThreadEffectDescriptor {
            attempt: 0,
            ..valid_descriptor.clone()
        },
        ThreadEffectDescriptor {
            input: serde_json::json!(["not", "an", "object"]),
            ..valid_descriptor.clone()
        },
    ];
    for (ordinal, descriptor) in invalid_descriptors.into_iter().enumerate() {
        let error = engine
            .prepare_thread_effect(
                ThreadEffectRequest {
                    thread_id: validation.thread_id,
                    run_id: validation_run_id,
                    expected_thread_revision: validation.revision,
                    expected_run_revision: validation_run_revision,
                    command_id: command_id(),
                    source_key: format!("validation:{ordinal}"),
                    descriptor,
                },
                &validation_lease,
                now + 3 + u64::try_from(ordinal).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, StorageError::InvalidData(_)));
    }
    for (ordinal, descriptor) in [
        ThreadEffectDescriptor {
            effect_id: "validation-denied-process".into(),
            tool_call_id: "call-validation-denied-process".into(),
            name: "process".into(),
            input: serde_json::json!({"shell":"rm -rf /","cwd":"."}),
            attempt: 1,
        },
        ThreadEffectDescriptor {
            effect_id: "validation-unknown-tool".into(),
            tool_call_id: "call-validation-unknown-tool".into(),
            name: "unknown_tool".into(),
            input: serde_json::json!({}),
            attempt: 1,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let error = engine
            .prepare_thread_effect(
                ThreadEffectRequest {
                    thread_id: validation.thread_id,
                    run_id: validation_run_id,
                    expected_thread_revision: validation.revision,
                    expected_run_revision: validation_run_revision,
                    command_id: command_id(),
                    source_key: format!("policy-rejection:{ordinal}"),
                    descriptor,
                },
                &validation_lease,
                now + 8 + u64::try_from(ordinal).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, StorageError::InvalidData(_)));
    }
    assert_eq!(
        engine
            .thread_snapshot_v2(validation.thread_id, None, 500)
            .unwrap(),
        validation
    );

    let prepared = prepare_effect(
        &engine,
        &validation_lease,
        &validation,
        "summary-read",
        "read_file",
        serde_json::json!({"path":"permission-summary.txt"}),
        now + 10,
    );
    assert_eq!(prepared.policy, ThreadEffectPolicy::Allow);
    let started = start_effect(
        &engine,
        &validation_lease,
        &prepared,
        "summary-read",
        now + 11,
    );
    let foreign_thread_id = thread_id();
    let foreign_lease = engine
        .acquire_thread_lease(foreign_thread_id, now + 12, 120_000)
        .unwrap();
    let wrong_scope = engine
        .execute_started_thread_effect(&started, &foreign_lease, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        wrong_scope,
        latte_engine::ThreadEffectExecutionError::Uncertain(_)
    ));
    engine.release_lease(&foreign_lease).unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(cancelled.is_cancelled());
    cancelled.cancelled().await;
    let cancellation_error = engine
        .execute_started_thread_effect(&started, &validation_lease, &cancelled)
        .await
        .unwrap_err();
    assert!(matches!(
        cancellation_error,
        latte_engine::ThreadEffectExecutionError::Uncertain(message)
            if message.contains("tool cancelled after Started")
    ));

    let (changed, changed_lease, _) =
        create_started_effect_thread(&engine, "changed completion rejection", now + 14);
    std::fs::write(scenario.root().join("changed-after-start.txt"), "changed\n").unwrap();
    let changed_error = commit_with_command(
        &engine,
        &changed_lease,
        &changed,
        command_id(),
        CommitThreadRunUpdate::Complete {
            source_key: "changed-completion:complete".into(),
            handoff: Handoff {
                summary: "must not complete without verification".into(),
                files_changed: vec!["changed-after-start.txt".into()],
                evidence: Vec::new(),
            },
        },
        now + 17,
    )
    .unwrap_err();
    assert!(
        changed_error
            .to_string()
            .contains("use verified completion")
    );
    engine.release_lease(&changed_lease).unwrap();

    let mut leases = vec![validation_lease];
    let shell_process_thread_id;
    {
        let mut next_now = now + 20;
        let mut prepare = |prompt: &str, effect_id: &str, name: &str, input: serde_json::Value| {
            let (snapshot, lease, _) = create_started_effect_thread(&engine, prompt, next_now);
            let prepared = prepare_effect(
                &engine,
                &lease,
                &snapshot,
                effect_id,
                name,
                input,
                next_now + 3,
            );
            next_now += 20;
            leases.push(lease);
            prepared
        };

        let listed = prepare(
            "list permission summary",
            "summary-list",
            "list_directory",
            serde_json::json!({"path":"."}),
        );
        assert_eq!(listed.policy, ThreadEffectPolicy::Allow);
        let searched = prepare(
            "search permission summary",
            "summary-search",
            "search",
            serde_json::json!({"query":"permission summary"}),
        );
        assert_eq!(searched.policy, ThreadEffectPolicy::Allow);

        let created = prepare(
            "create permission summary",
            "summary-create",
            "write_file",
            serde_json::json!({
                "path":"permission-created.txt",
                "content":"new content",
                "create_intent":true
            }),
        );
        assert_eq!(created.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            created.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description.contains("create or replace") && description.contains("11 bytes")
        ));

        let replaced = prepare(
            "replace permission summary",
            "summary-replace",
            "write_file",
            serde_json::json!({
                "path":"permission-summary.txt",
                "content":"replacement",
                "create_intent":false,
                "precondition":"0".repeat(64)
            }),
        );
        assert_eq!(replaced.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            replaced.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description.contains("replace existing") && description.contains("11 bytes")
        ));

        let edited = prepare(
            "edit permission summary",
            "summary-edit",
            "edit_file",
            serde_json::json!({
                "path":"permission-summary.txt",
                "before":"before",
                "after":"after-value",
                "precondition":"0".repeat(64)
            }),
        );
        assert_eq!(edited.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            edited.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description.contains("6 bytes") && description.contains("11 bytes")
        ));

        let safe_process = prepare(
            "safe process permission summary",
            "summary-safe-process",
            "process",
            serde_json::json!({"argv":["/bin/pwd"],"cwd":"."}),
        );
        assert_eq!(safe_process.policy, ThreadEffectPolicy::Allow);
        let secret_process = prepare(
            "redacted process permission summary",
            "summary-secret-process",
            "process",
            serde_json::json!({
                "argv":["/bin/echo","TOKEN=must-not-persist","plain=value"],
                "cwd":"."
            }),
        );
        assert_eq!(secret_process.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            secret_process.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description.contains("TOKEN=[REDACTED]")
                    && description.contains("plain=value")
                    && !description.contains("must-not-persist")
        ));

        let long_process = prepare(
            "bounded process permission summary",
            "summary-long-process",
            "process",
            serde_json::json!({"argv":["/bin/echo","x".repeat(1000)],"cwd":"."}),
        );
        assert_eq!(long_process.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            long_process.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description.ends_with('…') && description.len() <= 363
        ));

        let shell_process = prepare(
            "shell process permission summary",
            "summary-shell-process",
            "process",
            serde_json::json!({"shell":"printf boundary","cwd":"."}),
        );
        assert_eq!(shell_process.policy, ThreadEffectPolicy::Ask);
        assert!(matches!(
            shell_process.snapshot.pending.as_ref(),
            Some(ThreadPendingRequest::Permission { description, .. })
                if description == "Run shell command (cwd: .)"
        ));
        shell_process_thread_id = shell_process.snapshot.thread_id;
    }
    for lease in &leases {
        engine.release_lease(lease).unwrap();
    }
    drop(engine);

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"].as_array().unwrap().len(),
        11
    );
    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {shell_process_thread_id}\r").as_bytes());
    assert!(tui.wait_for_output(b"Run shell command (cwd: .)", Duration::from_secs(5)));
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
}
