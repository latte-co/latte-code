use super::{
    headless::write_file_reply,
    support::{
        ProviderReply, PtySession, Scenario, ScriptedProvider, assert_process_group_gone,
        wait_until,
    },
};
use latte_core::{
    IdSource, RunId, SystemIdSource, ThreadId, ThreadProviderBindingV2, TranscriptEntry,
    TranscriptEntryId, TranscriptKind,
};
use rusqlite::{Connection, params};
use std::{collections::BTreeMap, io::Write as _, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_A: &[u8] = b"\x1b[97;5u";
const CTRL_C: &[u8] = b"\x1b[99;5u";
const CTRL_R: &[u8] = b"\x1b[114;5u";
const SHIFT_ENTER: &[u8] = b"\x1b[13;2u";
const ENTER: &[u8] = b"\x1b[13u";
const MOUSE_SCROLL_UP: &[u8] = b"\x1b[<64;10;10M";

fn has_one_durable_terminal_tool_result(
    engine: &latte_engine::EngineHandle,
    tool_call_id: &str,
) -> bool {
    engine.list_threads_v2().is_ok_and(|threads| {
        threads.len() == 1
            && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
            && threads[0]
                .runs
                .last()
                .is_some_and(|run| run.status == latte_core::ThreadRunStatus::Completed)
            && threads[0]
                .transcript
                .entries
                .iter()
                .filter(|entry| {
                    entry.kind == latte_core::TranscriptKind::ToolResult
                        && entry
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("tool_call_id"))
                            .and_then(serde_json::Value::as_str)
                            == Some(tool_call_id)
                })
                .count()
                == 1
    })
}

fn session_boundary_binding() -> ThreadProviderBindingV2 {
    ThreadProviderBindingV2 {
        version: 1,
        provider_name: "main".into(),
        provider_type: "openai-chat".into(),
        protocol: "openai-chat-completions-v1".into(),
        model: "mock".into(),
        config_fingerprint: "session-boundary-config".into(),
        tools_fingerprint: "session-boundary-tools".into(),
        aliases: BTreeMap::new(),
        credential_ref_id: "env:TEST_OPENAI_KEY".into(),
        data_scope_id: "workspace".into(),
        credential_generation: 1,
    }
}

#[test]
fn explicit_tui_fails_cleanly_without_a_terminal() {
    let scenario = Scenario::new();
    let output = scenario.output(&["tui"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a TTY"));
}

#[test]
fn tui_without_provider_opens_and_guides_before_first_submission() {
    let scenario = Scenario::new();
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));

    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Not configured", Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Provider setup required", Duration::from_secs(5)));

    pty.write(b"prompt-kept-before-provider");
    pty.write(ENTER);
    assert!(pty.wait_for_output(b"~/.latte/latte-code.jsonc", Duration::from_secs(5)));
    assert!(pty.is_running());
    assert!(wait_until(Duration::from_secs(5), || {
        latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .and_then(|engine| engine.list_threads_v2())
            .is_ok_and(|threads| threads.is_empty())
    }));

    pty.write(SHIFT_ENTER);
    pty.write(b"second-line-still-editable");
    assert!(pty.is_running());
    pty.write(F10);
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let terminal = String::from_utf8_lossy(&output);
    assert!(terminal.contains("prompt-kept-before-provider"));
}

#[test]
fn tui_runs_inside_a_real_pty_and_restores_terminal_modes() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("done")]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);

    assert!(
        pty.wait_for_output(TUI_READY, Duration::from_secs(5)),
        "TUI never enabled keyboard disambiguation: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"first");
    pty.write(b"\x1b[13;2u");
    pty.write(b"second");
    pty.write(b"\r");
    assert!(
        provider.wait_for_calls(1, Duration::from_secs(5)),
        "TUI never dispatched the submitted prompt: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(
        pty.wait_for_output(b"done", Duration::from_secs(5)),
        "TUI never rendered the completed provider response: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let user_entries = engine
        .list_threads_v2()
        .unwrap()
        .into_iter()
        .flat_map(|thread| thread.transcript.entries)
        .filter(|entry| entry.kind == latte_core::TranscriptKind::User)
        .map(|entry| entry.text)
        .collect::<Vec<_>>();
    assert_eq!(user_entries, ["first\nsecond"]);

    pty.write(F10);
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let terminal = String::from_utf8_lossy(&output);
    assert!(terminal.contains("first"));
    assert!(terminal.contains("second"));
    assert!(terminal.contains("\u{1b}[>3u"));
    assert!(terminal.contains("\u{1b}[?1049h"));
    assert!(terminal.contains("\u{1b}[?1000h"));
    assert!(terminal.contains("\u{1b}[?1006h"));
    assert!(terminal.contains("\u{1b}[?1006l"));
    assert!(terminal.contains("\u{1b}[?1000l"));
    assert!(terminal.contains("\u{1b}[?1049l"));
    assert!(terminal.contains("\u{1b}[<1u"));
}

#[test]
fn tui_provider_configuration_failure_is_durable_and_keeps_multiline_input_usable() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    let sentinel = b"failed-start-visible-sentinel";
    pty.write(sentinel);
    pty.write(b"\r");
    assert!(pty.wait_for_output(
        b"The selected model could not be started",
        Duration::from_secs(5)
    ));

    assert!(wait_until(Duration::from_secs(5), || {
        latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .ok()
            .and_then(|engine| engine.list_threads_v2().ok())
            .is_some_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].runs.len() == 1
                    && threads[0].runs[0].status == latte_core::ThreadRunStatus::Failed
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::User
                            && entry.text == "failed-start-visible-sentinel"
                    })
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::Failure
                            && entry.text.contains("selected model could not be started")
                    })
            })
    }));
    assert!(pty.is_running());

    pty.write(b"after-error-first");
    pty.write(SHIFT_ENTER);
    pty.write(b"after-error-second");
    pty.write(b"\r");
    assert!(wait_until(Duration::from_secs(5), || {
        latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .ok()
            .and_then(|engine| engine.list_threads_v2().ok())
            .is_some_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].runs.len() == 2
                    && threads[0]
                        .runs
                        .iter()
                        .all(|run| run.status == latte_core::ThreadRunStatus::Failed)
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::User
                            && entry.text == "after-error-first\nafter-error-second"
                    })
            })
    }));

    let terminal = pty.output();
    assert!(
        terminal
            .windows(sentinel.len())
            .any(|value| value == sentinel)
    );
    assert!(
        !terminal
            .windows(b"prompt restored".len())
            .any(|value| value == b"prompt restored")
    );
    assert!(
        !terminal
            .windows(b"TEST_OPENAI_KEY".len())
            .any(|value| value == b"TEST_OPENAI_KEY")
    );

    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
fn tui_wrong_model_request_is_durable_retryable_and_never_restores_the_prompt() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::error(400, "model mock is not available"),
        ProviderReply::completion("retry completed"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "wrong-model-secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"wrong-model-visible-sentinel\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"! Failed", Duration::from_secs(5)));
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].runs[0].status == latte_core::ThreadRunStatus::Failed
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == latte_core::TranscriptKind::User
                        && entry.text == "wrong-model-visible-sentinel"
                })
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == latte_core::TranscriptKind::Failure
                        && entry.text.contains("http 400")
                })
        })
    }));

    pty.write(b"retry-first");
    pty.write(SHIFT_ENTER);
    pty.write(b"retry-second\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].runs.len() == 2
                && threads[0].runs[1].status == latte_core::ThreadRunStatus::Completed
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == latte_core::TranscriptKind::Assistant
                        && entry.text == "retry completed"
                })
        })
    }));
    let output = pty.output();
    assert!(
        !output
            .windows(b"prompt restored".len())
            .any(|value| value == b"prompt restored")
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["wrong-model-visible-sentinel", "retry-first\nretry-second"]
    );
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_model_picker_switches_provider_and_model_for_the_next_child() {
    let scenario = Scenario::new();
    let alpha = ScriptedProvider::start([ProviderReply::completion("alpha completed")]);
    let beta = ScriptedProvider::start([ProviderReply::completion("beta completed")]);
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "default_model": "alpha/alpha-default",
            "providers": {
                "alpha": {
                    "type": "openai-chat",
                    "models": {
                        "alpha-default": { "name": "Alpha Default" },
                        "alpha-fast": { "options": { "context_window": 32000 } }
                    },
                    "endpoint": alpha.endpoint(),
                    "api_key": {"source": "env", "name": "TEST_OPENAI_KEY"}
                },
                "beta": {
                    "type": "openai-chat",
                    "models": {
                        "beta-default": {},
                        "beta-reasoning": {
                            "name": "Beta Reasoning",
                            "options": {
                                "context_window": 128_000,
                                "reasoning_effort": "high",
                                "max_tokens": 4096
                            }
                        }
                    },
                    "endpoint": beta.endpoint(),
                    "api_key": {"source": "env", "name": "TEST_OPENAI_KEY"}
                }
            },
            "database": {"path": ".latte/latte-code.db"},
            "verification": {"argv": ["/usr/bin/true"]}
        }))
        .unwrap(),
    )
    .unwrap();
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "model-picker-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"first provider turn\r");
    assert!(alpha.wait_for_calls(1, Duration::from_secs(5)));
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].binding.provider_name == "alpha"
            })
        }),
        "alpha completion did not become durable: {}",
        String::from_utf8_lossy(&pty.output())
    );
    // The durable Ready transition can precede the next redraw. Synchronize
    // with the completed card from that exact projection before opening the
    // picker; this label is emitted contiguously by Ratatui's delta renderer.
    assert!(
        pty.wait_for_output(b"Completed", Duration::from_secs(5)),
        "alpha completion was durable but not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );

    pty.write(b"/model\r");
    assert!(
        pty.wait_for_output(b"beta-reasoning", Duration::from_secs(5)),
        "model picker was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"beta-reasoning");
    let before_switch = pty.output().len();
    pty.write(ENTER);
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].binding.provider_name == "beta"
                && threads[0].binding.model == "beta-reasoning"
        })
    }));
    assert!(pty.wait_for_growth(before_switch, Duration::from_secs(5)));

    pty.write(b"follow up on beta\r");
    assert!(beta.wait_for_calls(1, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == TranscriptKind::Assistant && entry.text == "beta completed"
                })
        })
    }));
    let request = beta.requests().pop().unwrap();
    assert_eq!(request.body["model"], "beta-reasoning");
    assert_eq!(request.body["reasoning_effort"], "high");
    assert_eq!(request.body["max_tokens"], 4096);

    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].binding.provider_name, "beta");
    assert_eq!(threads[0].binding.model, "beta-reasoning");
    assert!(threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == TranscriptKind::System
            && entry.text == "Model switched to beta/beta-reasoning"
    }));
    assert_eq!(alpha.requests().len(), 1);
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

fn rename_and_fork_from_tui(
    tui: &mut PtySession,
    engine: &latte_engine::EngineHandle,
    workspace: &std::path::Path,
    parent_thread_id: ThreadId,
) {
    tui.write(b"/rename Renamed TUI source\r");
    assert!(wait_until(Duration::from_secs(5), || {
        engine
            .search_thread_sessions_v2("renamed tui source", 10)
            .is_ok_and(|sessions| sessions.len() == 1 && sessions[0].thread_id == parent_thread_id)
    }));
    tui.write(b"/fork TUI fork session\r");
    assert!(wait_until(Duration::from_secs(5), || {
        engine
            .list_thread_sessions_v2_for_workspace(workspace.to_str().unwrap(), 10)
            .is_ok_and(|sessions| {
                sessions.len() == 2
                    && sessions.iter().any(|session| {
                        session.parent_thread_id == Some(parent_thread_id)
                            && session.title == "TUI fork session"
                    })
            })
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_new_and_resume_use_workspace_session_catalog_without_calling_provider() {
    let scenario = Scenario::new();
    let long_answer = format!(
        "OLDEST_MOUSE_MARKER\n{}\nNEWEST_MOUSE_MARKER",
        (0..60)
            .map(|index| format!("mouse history row {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let provider = ScriptedProvider::start([ProviderReply::completion(&long_answer)]);
    let mut first_command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut first_command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "session-e2e-secret",
    );
    let mut first = PtySession::spawn(first_command);
    assert!(first.wait_for_output(TUI_READY, Duration::from_secs(5)));
    first.write(b"seed session\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .ok()
            .and_then(|engine| engine.list_threads_v2().ok())
            .is_some_and(|sessions| {
                sessions.len() == 1 && sessions[0].lifecycle == latte_core::ThreadLifecycle::Ready
            })
    }));
    first.write(F10);
    assert!(first.finish(Duration::from_secs(5)).0.success());

    let session_files = scenario.session_files();
    let [session_file] = session_files.as_slice() else {
        panic!("expected exactly one Session transcript");
    };
    std::fs::OpenOptions::new()
        .append(true)
        .open(session_file)
        .unwrap()
        .write_all(br#"{"record":"entry"#)
        .unwrap();

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let thread_id = engine.list_threads_v2().unwrap()[0].thread_id;
    let workspace = std::fs::canonicalize(scenario.root()).unwrap();
    assert_eq!(
        engine
            .list_thread_sessions_v2_for_workspace(workspace.to_str().unwrap(), 10)
            .unwrap()
            .len(),
        1
    );

    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "session-e2e-secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(
        !resumed
            .output()
            .windows(b"NEWEST_MOUSE_MARKER".len())
            .any(|value| value == b"NEWEST_MOUSE_MARKER"),
        "TUI startup must keep a fresh draft instead of rendering the latest Session"
    );
    let missing_thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    resumed.write(format!("/resume {missing_thread_id}\r").as_bytes());
    assert!(
        resumed.wait_for_visible_text("No saved sessions", Duration::from_secs(5)),
        "missing exact Session id was not reported: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    resumed.write(b"\x1b[27u");

    resumed.write(b"/resume seed session\r");
    assert!(
        resumed.wait_for_output(b"NEWEST_MOUSE_MARKER", Duration::from_secs(5)),
        "TUI did not render resumed Session: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    assert!(
        !std::fs::read_to_string(session_file)
            .unwrap()
            .ends_with(r#"{"record":"entry"#),
        "resume must repair only the torn final JSONL line"
    );
    assert_eq!(provider.requests().len(), 1);

    for _ in 0..40 {
        resumed.write(MOUSE_SCROLL_UP);
    }
    assert!(
        resumed.wait_for_output(b"OLDEST_MOUSE_MARKER", Duration::from_secs(5)),
        "mouse wheel did not reveal older transcript rows: {}",
        String::from_utf8_lossy(&resumed.output())
    );

    let before_title_resume = resumed.output().len();
    resumed.write(b"/sessions seed session\r");
    assert!(
        resumed.wait_for_growth(before_title_resume, Duration::from_secs(5)),
        "exact-title session lookup did not redraw"
    );
    resumed.write(b"/sessions no such durable session\r");
    assert!(
        resumed.wait_for_visible_text("No saved sessions", Duration::from_secs(5)),
        "missing exact-title lookup was not reported: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    resumed.write(b"\x1b[27u");

    rename_and_fork_from_tui(&mut resumed, &engine, &workspace, thread_id);

    resumed.write(b"/new\r");
    assert!(wait_until(Duration::from_secs(2), || provider
        .requests()
        .len()
        == 1));
    assert_eq!(engine.list_threads_v2().unwrap().len(), 2);
    assert_eq!(provider.requests().len(), 1);
    resumed.write(F10);
    assert!(resumed.finish(Duration::from_secs(5)).0.success());
    provider.assert_consumed();
}

#[test]
fn tui_session_lookup_distinguishes_duplicate_missing_and_foreign_catalog_entries() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let local_engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    for offset in 0..2 {
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let now = latte_core::wall_time_ms() + offset;
        let stale = local_engine
            .acquire_thread_lease(thread_id, now, 1)
            .unwrap();
        local_engine
            .create_started_thread_v2(
                thread_id,
                run_id,
                session_boundary_binding(),
                "duplicate session title",
                &stale,
                now,
            )
            .unwrap();
        local_engine
            .recover_thread_after_lease_loss(thread_id, run_id, &stale, 1, now + 2)
            .unwrap();
    }
    drop(local_engine);

    let foreign_workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(foreign_workspace.path().join(".git")).unwrap();
    let foreign_thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let foreign_engine = latte_engine::EngineBuilder::new()
        .workspace_root(foreign_workspace.path())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let foreign_now = latte_core::wall_time_ms();
    let foreign_lease = foreign_engine
        .acquire_thread_lease(foreign_thread_id, foreign_now, 1)
        .unwrap();
    let foreign_run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    foreign_engine
        .create_started_thread_v2(
            foreign_thread_id,
            foreign_run_id,
            session_boundary_binding(),
            "foreign session title",
            &foreign_lease,
            foreign_now,
        )
        .unwrap();
    foreign_engine
        .recover_thread_after_lease_loss(
            foreign_thread_id,
            foreign_run_id,
            &foreign_lease,
            1,
            foreign_now + 2,
        )
        .unwrap();
    drop(foreign_engine);

    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"/resume duplicate session title\r");
    assert!(
        pty.wait_for_visible_text("Enter resume", Duration::from_secs(5)),
        "duplicate exact-title resume did not open the picker: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"\x1b[27u");
    pty.write(b"/sessions duplicate session title\r");
    assert!(
        pty.wait_for_visible_text("Enter resume", Duration::from_secs(5)),
        "duplicate exact-title result was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(
        pty.output()
            .windows(b"duplicate session title".len())
            .filter(|window| *window == b"duplicate session title")
            .count()
            >= 2,
        "duplicate title picker did not contain both matching Sessions"
    );
    pty.write(b"\x1b[27u");
    pty.write(format!("/resume {foreign_thread_id}\r").as_bytes());
    assert!(
        pty.wait_for_visible_text("belongs to another workspace", Duration::from_secs(5)),
        "foreign Session was not rejected: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert_eq!(status.code(), Some(70));
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|value| value == b"\x1b[?1049l")
    );
}

#[test]
fn tui_dispatches_prompt_and_consumes_runtime_feedback_without_fixed_sleeps() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("done")]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"check\r");
    assert!(
        provider.wait_for_calls(1, Duration::from_secs(5)),
        "TUI never dispatched the provider request: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(
        pty.wait_for_output(b"done", Duration::from_secs(5)),
        "TUI never rendered the provider result: {}",
        String::from_utf8_lossy(&pty.output())
    );

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].lifecycle, latte_core::ThreadLifecycle::Ready);
    assert!(threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant && entry.text == "done"
    }));
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert!(scenario.database_path().exists());
    let session_files = scenario.session_files();
    assert_eq!(session_files.len(), 1);
    let conversation = std::fs::read_to_string(&session_files[0]).unwrap();
    assert!(
        conversation
            .lines()
            .next()
            .unwrap()
            .contains(r#""record":"session""#)
    );
    assert!(conversation.contains(r#""record":"entry""#));
    assert!(conversation.contains(r#""content":"check""#));
    assert!(conversation.contains(r#""content":"done""#));
    assert!(!conversation.contains("secret"));
}

#[test]
fn running_tui_queues_and_automatically_runs_the_next_multiline_turn() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first answer").delayed(Duration::from_secs(1)),
        ProviderReply::completion("queued answer"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"first prompt\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));

    // The first durable user card unlocks a fresh composer while the provider
    // is still working. Enter sends that draft to the session runner mailbox;
    // no second Enter after completion is required.
    pty.write(b"\x1b[200~next draft first\x1b[201~");
    pty.write(SHIFT_ENTER);
    pty.write(b"\x1b[200~next draft second\x1b[201~");
    assert!(
        pty.wait_for_output(b"next draft second", Duration::from_secs(5)),
        "running composer did not retain the multiline draft: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert_eq!(provider.requests().len(), 1);
    pty.write(ENTER);
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && ["first answer", "queued answer"].iter().all(|answer| {
                    threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::Assistant && entry.text == *answer
                    })
                })
        })
    }));
    assert!(
        pty.wait_for_output(b"Completed", Duration::from_secs(5)),
        "TUI did not render the completed queued run: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let requests = provider.requests();
    assert_eq!(
        requests[1].body["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"],
        "next draft first\nnext draft second"
    );
    provider.assert_consumed();
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

#[test]
fn input_request_survives_tui_restart_and_exact_value_completes_the_same_child() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::input_request("input-1", "Which value should be used?", false),
        ProviderReply::completion("input accepted"),
    ]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "input-e2e-secret");
    let mut first = PtySession::spawn(command);
    assert!(first.wait_for_output(TUI_READY, Duration::from_secs(5)));
    first.write(b"request a value\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::WaitingInput
                    && matches!(
                        threads[0].pending.as_ref(),
                        Some(latte_core::ThreadPendingRequest::Input {
                            request_id,
                            prompt,
                            ..
                        }) if request_id == "input-1" && prompt == "Which value should be used?"
                    )
            })
        }),
        "input request was not durable: {}",
        String::from_utf8_lossy(&first.output())
    );
    let thread_id = engine.list_threads_v2().unwrap()[0].thread_id;
    first.write(F10);
    let (status, _) = first.finish(Duration::from_secs(5));
    assert!(status.success());
    assert_eq!(provider.requests().len(), 1);

    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "input-e2e-secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    resumed.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(resumed.wait_for_output(b"Input required", Duration::from_secs(5)));
    resumed.write(b"durable-first");
    resumed.write(SHIFT_ENTER);
    resumed.write(b"durable-second\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].pending.is_none()
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::Assistant
                            && entry.text == "input accepted"
                    })
            })
        }),
        "provided input did not complete the durable child: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let users = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(users, ["request a value", "durable-first\ndurable-second"]);
    resumed.write(F10);
    let (status, _) = resumed.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
fn failed_input_submission_restores_multiline_value_without_committing_a_user_card() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "input-restore",
        "Which value should be used?",
        false,
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let mut first_command = scenario.command(&["tui"]);
    first_command.env("TEST_OPENAI_KEY", "input-restore-secret");
    let mut first = PtySession::spawn(first_command);
    assert!(first.wait_for_output(TUI_READY, Duration::from_secs(5)));
    first.write(b"request a restorable value\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(first.wait_for_output(b"Input required", Duration::from_secs(5)));
    first.write(F10);
    assert!(first.finish(Duration::from_secs(5)).0.success());

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let thread_id = engine.list_threads_v2().unwrap()[0].thread_id;

    // Changing the immutable binding makes provider resolution fail before
    // ProvideInput can commit its user card.
    scenario.write_config_with_model(
        "http://127.0.0.1:1",
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        "different-model",
        ",compatibility_input_request:true",
    );
    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "input-restore-secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    resumed.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(resumed.wait_for_output(b"Input required", Duration::from_secs(5)));
    resumed.write(b"restore-first");
    resumed.write(SHIFT_ENTER);
    resumed.write(b"restore-second");
    let before_submit = resumed.output().len();
    resumed.write(ENTER);
    assert!(
        wait_until(Duration::from_secs(5), || {
            let output = resumed.output();
            let restored = &output[before_submit.min(output.len())..];
            restored
                .windows(b"restore-first".len())
                .any(|value| value == b"restore-first")
                && restored
                    .windows(b"restore-second".len())
                    .any(|value| value == b"restore-second")
        }),
        "input was not restored after the failed submission: {}",
        String::from_utf8_lossy(&resumed.output())
    );

    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(
        threads[0].lifecycle,
        latte_core::ThreadLifecycle::WaitingInput
    );
    assert_eq!(
        threads[0]
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.kind == latte_core::TranscriptKind::User)
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["request a restorable value"]
    );
    let screen = resumed.output();
    assert!(
        screen
            .windows(b"restore-first".len())
            .any(|value| value == b"restore-first")
    );
    assert!(
        screen
            .windows(b"restore-second".len())
            .any(|value| value == b"restore-second")
    );
    assert_eq!(provider.requests().len(), 1);
    resumed.write(F10);
    assert!(resumed.finish(Duration::from_secs(5)).0.success());
}

#[test]
fn invalid_input_request_id_fails_before_any_pending_card_is_persisted() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "bad:input:id",
        "This must not become pending",
        false,
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"reject an unsafe input id\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Failed
                    && threads[0].pending.is_none()
            })
        }),
        "invalid input id did not fail closed: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let threads = engine.list_threads_v2().unwrap();
    assert!(
        !threads[0]
            .transcript
            .entries
            .iter()
            .any(|entry| entry.kind == latte_core::TranscriptKind::Input)
    );
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
fn completed_tui_thread_accepts_a_follow_up_as_an_immutable_child_with_history() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first answer"),
        ProviderReply::completion("second answer"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"first prompt\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Completed", Duration::from_secs(5)));

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let first = engine.list_threads_v2().unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].runs.len(), 1);
    let parent_id = first[0].runs[0].run_id;

    pty.write(b"follow up prompt\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].runs.len() == 2
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::Assistant
                            && entry.text == "second answer"
                    })
            })
        }),
        "follow-up did not complete: {}",
        String::from_utf8_lossy(&pty.output())
    );
    provider.assert_consumed();
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads[0].runs[0].run_id, parent_id);
    assert_eq!(threads[0].runs[1].parent_run_id, Some(parent_id));
    assert_eq!(threads[0].runs[1].ordinal, 1);
    let requests = provider.requests();
    let second_messages = requests[1].body["messages"].as_array().unwrap();
    assert!(
        second_messages.iter().any(|message| {
            message["role"] == "assistant" && message["content"] == "first answer"
        })
    );
    assert!(
        second_messages.iter().any(|message| {
            message["role"] == "user" && message["content"] == "follow up prompt"
        })
    );
    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_resumes_the_newest_500_cards_and_reconciles_a_follow_up_after_the_boundary() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first bounded-tail answer"),
        ProviderReply::completion("answer after bounded-tail resume"),
    ]);
    let mut first_command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut first_command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut first = PtySession::spawn(first_command);
    assert!(first.wait_for_output(TUI_READY, Duration::from_secs(5)));
    first.write(b"oldest bounded-tail prompt\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(
        first.wait_for_output(b"Completed", Duration::from_secs(5)),
        "first turn did not complete: {}",
        String::from_utf8_lossy(&first.output())
    );

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].runs.len() == 1
        })
    }));
    let initial = engine.list_threads_v2().unwrap().remove(0);
    let thread_id = initial.thread_id;
    let parent_run_id = initial.runs[0].run_id;
    first.write(F10);
    assert!(first.finish(Duration::from_secs(5)).0.success());
    drop(engine);

    let connection = Connection::open(scenario.database_path()).unwrap();
    let initial_last_sequence: i64 = connection
        .query_row(
            "SELECT last_seq FROM threads_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    for offset in 1_i64..=501 {
        let sequence = initial_last_sequence + offset;
        let text = if offset == 501 {
            "newest bounded-tail marker".to_owned()
        } else {
            format!("bounded history card {offset}")
        };
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            sequence: u64::try_from(sequence).unwrap(),
            run_id: Some(parent_run_id),
            kind: TranscriptKind::Assistant,
            text,
            payload: None,
            source_key: format!("bounded-tail-fixture:{offset}"),
            created_at_ms: u64::try_from(sequence).unwrap(),
        };
        connection
            .execute(
                "INSERT INTO conversation_outbox(\
                    thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms\
                 ) VALUES(?1,?2,?3,?4,'assistant',?5,?6,?7)",
                params![
                    thread_id.to_string(),
                    sequence,
                    entry.entry_id.to_string(),
                    parent_run_id.to_string(),
                    entry.source_key,
                    serde_json::to_string(&entry).unwrap(),
                    sequence,
                ],
            )
            .unwrap();
    }
    let seeded_last_sequence = initial_last_sequence + 501;
    connection
        .execute(
            "UPDATE threads_v2 SET last_seq=?1,updated_at_ms=?2 WHERE thread_id=?3",
            params![
                seeded_last_sequence,
                seeded_last_sequence,
                thread_id.to_string()
            ],
        )
        .unwrap();
    drop(connection);

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let tail = engine.thread_snapshot_tail_v2(thread_id, 500).unwrap();
    assert_eq!(tail.transcript.entries.len(), 500);
    assert!(tail.transcript.has_more);
    assert_eq!(
        tail.transcript.entries.last().unwrap().text,
        "newest bounded-tail marker"
    );
    assert!(
        tail.transcript
            .entries
            .iter()
            .all(|entry| entry.text != "oldest bounded-tail prompt")
    );

    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    resumed.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(
        resumed.wait_for_output(b"newest bounded-tail marker", Duration::from_secs(5)),
        "TUI resumed an old transcript page: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    resumed.write(b"follow up beyond the 500-card boundary\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                && threads[0].pending.is_none()
                && threads[0].active_run_id.is_none()
                && threads[0].runs.len() == 2
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.sequence > u64::try_from(seeded_last_sequence).unwrap()
                        && entry.kind == TranscriptKind::User
                        && entry.text == "follow up beyond the 500-card boundary"
                })
                && threads[0].transcript.entries.iter().any(|entry| {
                    entry.kind == TranscriptKind::Assistant
                        && entry.text == "answer after bounded-tail resume"
                })
        })
    }));
    assert!(
        resumed.wait_for_output(b"Completed", Duration::from_secs(5)),
        "TUI did not reconcile the completed follow-up: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    resumed.write(b"\x1b[200~composer unlocked after tail resume\x1b[201~");
    assert!(
        resumed.wait_for_visible_text(
            "composer unlocked after tail resume",
            Duration::from_secs(5)
        ),
        "follow-up remained locked after durable acceptance: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    provider.assert_consumed();
    resumed.write(F10);
    assert!(resumed.finish(Duration::from_secs(5)).0.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn permission_card_requires_exact_keys_and_resolves_once() {
    let denied_scenario = Scenario::new();
    let denied_provider = ScriptedProvider::start([
        write_file_reply("tui-deny-write"),
        ProviderReply::completion("continued after permission denial"),
    ]);
    let mut denied_command = denied_scenario.command(&["tui"]);
    denied_scenario.configure_provider(
        &mut denied_command,
        denied_provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut denied_pty = PtySession::spawn(denied_command);
    assert!(denied_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    denied_pty.write(b"deny the write\r");
    assert!(denied_provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(denied_pty.wait_for_output(b"Permission required", Duration::from_secs(5)));

    denied_pty.write(b"\r");
    denied_pty.write(F10);
    let (status, _) = denied_pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let assert_still_waiting = || {
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(denied_scenario.root())
            .database_path(denied_scenario.database_path())
            .build()
            .unwrap();
        let threads = engine.list_threads_v2().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].lifecycle,
            latte_core::ThreadLifecycle::WaitingPermission
        );
        assert!(threads[0].pending.is_some());
    };
    assert_still_waiting();
    let denied_thread_id = latte_engine::EngineBuilder::new()
        .workspace_root(denied_scenario.root())
        .database_path(denied_scenario.database_path())
        .build()
        .unwrap()
        .list_threads_v2()
        .unwrap()[0]
        .thread_id;
    assert!(!denied_scenario.root().join("new.txt").exists());
    assert_eq!(denied_provider.requests().len(), 1);

    let mut shifted_command = denied_scenario.command(&["tui"]);
    shifted_command.env("TEST_OPENAI_KEY", "secret");
    let mut shifted_pty = PtySession::spawn(shifted_command);
    assert!(shifted_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    shifted_pty.write(format!("/resume {denied_thread_id}\r").as_bytes());
    assert!(shifted_pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    shifted_pty.write(SHIFT_ENTER);
    shifted_pty.write(F10);
    let (status, _) = shifted_pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert_still_waiting();
    assert!(!denied_scenario.root().join("new.txt").exists());
    assert_eq!(denied_provider.requests().len(), 1);

    let mut deny_command = denied_scenario.command(&["tui"]);
    deny_command.env("TEST_OPENAI_KEY", "secret");
    let mut deny_pty = PtySession::spawn(deny_command);
    assert!(deny_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    deny_pty.write(format!("/resume {denied_thread_id}\r").as_bytes());
    assert!(deny_pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    deny_pty.write(b"d");
    assert!(deny_pty.wait_for_output(b"permission denied", Duration::from_secs(5)));
    assert!(!denied_scenario.root().join("new.txt").exists());
    deny_pty.write(b"retry after denial");
    deny_pty.write(SHIFT_ENTER);
    deny_pty.write(b"with a multiline prompt\r");
    assert!(denied_provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        deny_pty.wait_for_output(b"continued", Duration::from_secs(5)),
        "TUI did not render the post-denial follow-up: {}",
        String::from_utf8_lossy(&deny_pty.output())
    );
    let denied_threads = latte_engine::EngineBuilder::new()
        .workspace_root(denied_scenario.root())
        .database_path(denied_scenario.database_path())
        .build()
        .unwrap()
        .list_threads_v2()
        .unwrap();
    assert_eq!(
        denied_threads[0].lifecycle,
        latte_core::ThreadLifecycle::Ready
    );
    assert_eq!(denied_threads[0].runs.len(), 2);
    assert!(denied_threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == TranscriptKind::User
            && entry.text == "retry after denial\nwith a multiline prompt"
    }));
    denied_provider.assert_consumed();
    deny_pty.write(F10);
    let (status, _) = deny_pty.finish(Duration::from_secs(5));
    assert!(status.success());

    let allowed_scenario = Scenario::new();
    let allowed_provider = ScriptedProvider::start([
        write_file_reply("tui-allow-write"),
        ProviderReply::completion("done"),
    ]);
    let mut allowed_command = allowed_scenario.command(&["tui"]);
    allowed_scenario.configure_provider(
        &mut allowed_command,
        allowed_provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut allowed_pty = PtySession::spawn(allowed_command);
    assert!(allowed_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    allowed_pty.write(b"allow the write\r");
    assert!(allowed_provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(allowed_pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    allowed_pty.write(CTRL_A);
    assert!(allowed_provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(allowed_pty.wait_for_output(b"done", Duration::from_secs(5)));
    assert_eq!(
        std::fs::read_to_string(allowed_scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    allowed_provider.assert_consumed();
    allowed_pty.write(F10);
    let (status, _) = allowed_pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn ctrl_c_cancels_the_active_process_group_before_the_second_press_exits() {
    let scenario = Scenario::new();
    let pgid_file = scenario.root().join("active-process-group");
    let shell = format!("echo $$ > {}; sleep 30 & wait", pgid_file.display());
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "tui-cancel-process",
        "process",
        &serde_json::json!({
            "shell": shell,
            "cwd": ".",
            "env": {},
            "timeout_ms": 60_000,
            "grace_ms": 50,
            "stdout_cap": 1_024,
            "stderr_cap": 1_024
        }),
    )]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"start a cancellable process\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    pty.write(CTRL_A);
    assert!(wait_until(Duration::from_secs(5), || pgid_file.exists()));
    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();

    pty.write(CTRL_C);
    assert_process_group_gone(pgid, Duration::from_secs(5));
    assert!(pty.is_running(), "the first Ctrl+C must not exit the TUI");
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);

    // Crossterm suppresses duplicate Ctrl+C reports for 120 ms so one physical
    // key cannot accidentally satisfy both stages of the exit contract.
    std::thread::sleep(Duration::from_millis(130));
    assert!(pty.is_running());
    pty.write(CTRL_C);
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let terminal = String::from_utf8_lossy(&output);
    assert!(terminal.contains("\u{1b}[?1049h"));
    assert!(terminal.contains("\u{1b}[?1049l"));
    assert!(terminal.contains("\u{1b}[<1u"));

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(
        threads[0].lifecycle,
        latte_core::ThreadLifecycle::ReconciliationRequired
    );
    let effect_ids = threads[0]
        .transcript
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .payload
                .as_ref()?
                .get("descriptor")?
                .get("effect_id")?
                .as_str()
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(effect_ids.len(), 1);
    assert_eq!(
        engine.effect_status(&effect_ids[0]).unwrap(),
        latte_engine::EffectStatus::Unknown
    );
    assert_eq!(
        threads[0]
            .transcript
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == latte_core::TranscriptKind::ToolResult
                    && entry
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("tool_call_id"))
                        .and_then(serde_json::Value::as_str)
                        == Some("tui-cancel-process")
            })
            .count(),
        0
    );
    assert!(!threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant && entry.text == "done"
    }));

    let mut restart_command = scenario.command(&["tui"]);
    restart_command.env("TEST_OPENAI_KEY", "secret");
    let mut restarted = PtySession::spawn(restart_command);
    assert!(restarted.wait_for_output(TUI_READY, Duration::from_secs(5)));
    restarted.write(format!("/resume {}\r", threads[0].thread_id).as_bytes());
    assert!(restarted.wait_for_output(b"Reconciliation", Duration::from_secs(5)));
    restarted.write(CTRL_R);
    assert!(
        restarted.wait_for_output(b"Ctrl+A confirm failed", Duration::from_secs(5)),
        "reconciliation confirmation did not open: {}",
        String::from_utf8_lossy(&restarted.output())
    );

    let before_enter = restarted.output().len();
    restarted.write(b"\r");
    assert!(restarted.wait_for_growth(before_enter, Duration::from_secs(5)));
    assert_eq!(
        engine.effect_status(&effect_ids[0]).unwrap(),
        latte_engine::EffectStatus::Unknown
    );
    assert_eq!(
        engine.list_threads_v2().unwrap()[0].lifecycle,
        latte_core::ThreadLifecycle::ReconciliationRequired
    );

    restarted.write(CTRL_A);
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine
                .effect_status(&effect_ids[0])
                .is_ok_and(|status| status == latte_engine::EffectStatus::ObservedFailed)
                && engine.list_threads_v2().is_ok_and(|threads| {
                    threads.len() == 1
                        && threads[0].lifecycle == latte_core::ThreadLifecycle::Failed
                        && threads[0].pending.is_none()
                })
        }),
        "unknown effect was not reconciled exactly once: {}",
        String::from_utf8_lossy(&restarted.output())
    );
    assert_eq!(provider.requests().len(), 1);
    restarted.write(F10);
    let (status, _) = restarted.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
fn process_timeout_reaps_the_group_and_returns_one_terminal_tool_result() {
    let scenario = Scenario::new();
    let terminal_bound = Duration::from_secs(5);
    let pgid_file = scenario.root().join("timed-out-process-group");
    let shell = format!("echo $$ > {}; exec sleep 30", pgid_file.display());
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "tui-timeout-process",
            "process",
            &serde_json::json!({
                "shell": shell,
                "cwd": ".",
                "env": {},
                "timeout_ms": 100,
                "grace_ms": 50,
                "stdout_cap": 1_024,
                "stderr_cap": 1_024
            }),
        ),
        ProviderReply::completion("done"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"run a process with a deadline\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    pty.write(CTRL_A);
    assert!(wait_until(Duration::from_secs(5), || pgid_file.exists()));
    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(provider.wait_for_calls(2, terminal_bound));
    assert!(pty.wait_for_output(b"done", terminal_bound));
    assert_process_group_gone(pgid, terminal_bound);
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tool_results = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert!(
        tool_results[0]["content"]
            .as_str()
            .unwrap()
            .contains("timed_out")
    );

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(
        wait_until(terminal_bound, || has_one_durable_terminal_tool_result(
            &engine,
            "tui-timeout-process"
        )),
        "timed-out process result did not reach one durable terminal state: {}",
        String::from_utf8_lossy(&pty.output())
    );

    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].lifecycle, latte_core::ThreadLifecycle::Ready);
    assert_eq!(
        threads[0]
            .transcript
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == latte_core::TranscriptKind::ToolResult
                    && entry
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("tool_call_id"))
                        .and_then(serde_json::Value::as_str)
                        == Some("tui-timeout-process")
            })
            .count(),
        1
    );
}
