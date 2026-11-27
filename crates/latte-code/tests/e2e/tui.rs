use super::{
    headless::write_file_reply,
    support::{
        ProviderReply, PtySession, Scenario, ScriptedProvider, assert_process_group_gone,
        wait_until,
    },
};
use latte_core::{
    IdSource, SessionId, SessionProviderBinding, SystemIdSource, TranscriptEntry,
    TranscriptEntryId, TranscriptKind, TurnId,
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
    engine.list_sessions().is_ok_and(|sessions| {
        sessions.len() == 1
            && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
            && sessions[0]
                .turns
                .last()
                .is_some_and(|run| run.status == latte_core::SessionTurnStatus::Completed)
            && sessions[0]
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

fn session_boundary_binding() -> SessionProviderBinding {
    SessionProviderBinding {
        binding_version: 1,
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
fn tui_status_bar_renders_the_context_usage_meter() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("meter done")]);
    // Tight budget with a 1% trigger: once the turn completes the meter must
    // show a non-zero fill and the amber "compaction due" hint.
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["/usr/bin/true"]}},session:{{max_request_bytes:5600,max_input_bytes:5600,reserved_output_bytes:1,context_cap_bytes:65536,provider_timeout_ms:60000,compaction:{{enabled:true,max_summary_source_bytes:8192,trigger_ratio:1}}}}}}"#,
            endpoint = provider.endpoint()
        ),
    )
    .unwrap();
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "tui-meter-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    // TUI_READY (the kitty keyboard sequence) is emitted while terminal setup
    // is still in progress: alternate-screen/mouse/paste enable and the first
    // draw happen afterwards, and that setup's termios reconfiguration can
    // flush bytes already written to the PTY (observed as a lost prompt in the
    // empty composer on slower macOS runners). Wait for a string that only the
    // post-setup first frame renders before typing, then submit in a single
    // write (the established pattern at the other PTY journeys).
    assert!(
        pty.wait_for_output(b"Describe an outcome", Duration::from_secs(5)),
        "idle first frame never rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );

    pty.write(b"show me the context meter\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    // Reconstruct the 40x120 screen buffer so Ratatui cell-diff cursor jumps
    // across an optional intermediate `Running · mock` frame never split
    // `Ready for follow-up` or the status-bar context meter.
    assert!(
        wait_for_screen_text(&pty, 40, 120, "Ready for follow-up", Duration::from_secs(5)),
        "session never settled:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert!(
        wait_for_screen_text(&pty, 40, 120, "Context ", Duration::from_secs(5)),
        "context meter never rendered:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    let screen = pty_screen_text(&pty.output(), 40, 120);
    let clean = screen
        .lines()
        .find(|line| line.contains("Context "))
        .expect("a status line contains the meter");
    let percent = clean
        .split("Context ")
        .nth(1)
        .and_then(|rest| rest.split('%').next())
        .and_then(|value| value.parse::<u8>().ok())
        .expect("a fill percentage follows Context");
    assert!(percent > 0, "the tight budget makes fill visible: {clean}");
    assert!(
        clean.contains("tokens"),
        "the meter shows token estimates: {clean}"
    );
    assert!(
        clean.contains("compaction due"),
        "the near-compaction hint is shown at the 1% trigger: {clean}"
    );

    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
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
            .and_then(|engine| engine.list_sessions())
            .is_ok_and(|sessions| sessions.is_empty())
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
        .list_sessions()
        .unwrap()
        .into_iter()
        .flat_map(|session| session.transcript.entries)
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
    assert!(
        !terminal.contains("\u{1b}[?1000h"),
        "mouse capture defaults off so native terminal copy/selection works"
    );
    assert!(!terminal.contains("\u{1b}[?1006h"));
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
            .and_then(|engine| engine.list_sessions().ok())
            .is_some_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].turns.len() == 1
                    && sessions[0].turns[0].status == latte_core::SessionTurnStatus::Failed
                    && sessions[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::User
                            && entry.text == "failed-start-visible-sentinel"
                    })
                    && sessions[0].transcript.entries.iter().any(|entry| {
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
            .and_then(|engine| engine.list_sessions().ok())
            .is_some_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].turns.len() == 2
                    && sessions[0]
                        .turns
                        .iter()
                        .all(|run| run.status == latte_core::SessionTurnStatus::Failed)
                    && sessions[0].transcript.entries.iter().any(|entry| {
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
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].turns[0].status == latte_core::SessionTurnStatus::Failed
                && sessions[0].transcript.entries.iter().any(|entry| {
                    entry.kind == latte_core::TranscriptKind::User
                        && entry.text == "wrong-model-visible-sentinel"
                })
                && sessions[0].transcript.entries.iter().any(|entry| {
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
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].turns.len() == 2
                && sessions[0].turns[1].status == latte_core::SessionTurnStatus::Completed
                && sessions[0].transcript.entries.iter().any(|entry| {
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
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
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
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].binding.provider_name == "alpha"
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
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].binding.provider_name == "beta"
                && sessions[0].binding.model == "beta-reasoning"
        })
    }));
    assert!(pty.wait_for_growth(before_switch, Duration::from_secs(5)));

    pty.write(b"follow up on beta\r");
    assert!(beta.wait_for_calls(1, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].transcript.entries.iter().any(|entry| {
                    entry.kind == TranscriptKind::Assistant && entry.text == "beta completed"
                })
        })
    }));
    let request = beta.requests().pop().unwrap();
    assert_eq!(request.body["model"], "beta-reasoning");
    assert_eq!(request.body["reasoning_effort"], "high");
    assert_eq!(request.body["max_tokens"], 4096);

    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].binding.provider_name, "beta");
    assert_eq!(sessions[0].binding.model, "beta-reasoning");
    assert!(sessions[0].transcript.entries.iter().any(|entry| {
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
    parent_session_id: SessionId,
) {
    tui.write(b"/rename Renamed TUI source\r");
    assert!(wait_until(Duration::from_secs(5), || {
        engine
            .search_sessions("renamed tui source", 10)
            .is_ok_and(|sessions| {
                sessions.len() == 1 && sessions[0].session_id == parent_session_id
            })
    }));
    tui.write(b"/fork TUI fork session\r");
    assert!(wait_until(Duration::from_secs(5), || {
        engine
            .list_session_summaries_for_workspace(workspace.to_str().unwrap(), 10)
            .is_ok_and(|sessions| {
                sessions.len() == 2
                    && sessions.iter().any(|session| {
                        session.parent_session_id == Some(parent_session_id)
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
            .and_then(|engine| engine.list_sessions().ok())
            .is_some_and(|sessions| {
                sessions.len() == 1 && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
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
    let session_id = engine.list_sessions().unwrap()[0].session_id;
    let workspace = std::fs::canonicalize(scenario.root()).unwrap();
    assert_eq!(
        engine
            .list_session_summaries_for_workspace(workspace.to_str().unwrap(), 10)
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
    let missing_session_id = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
    resumed.write(format!("/resume {missing_session_id}\r").as_bytes());
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

    rename_and_fork_from_tui(&mut resumed, &engine, &workspace, session_id);

    resumed.write(b"/new\r");
    assert!(wait_until(Duration::from_secs(2), || provider
        .requests()
        .len()
        == 1));
    assert_eq!(engine.list_sessions().unwrap().len(), 2);
    assert_eq!(provider.requests().len(), 1);
    resumed.write(F10);
    assert!(resumed.finish(Duration::from_secs(5)).0.success());
    provider.assert_consumed();
}

#[test]
#[allow(clippy::too_many_lines)]
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
        let session_id = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let turn_id = TurnId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let now = latte_core::wall_time_ms() + offset;
        let stale = local_engine
            .acquire_session_lease(session_id, now, 1)
            .unwrap();
        local_engine
            .create_started_session_v2_snapshot(
                session_id,
                turn_id,
                session_boundary_binding(),
                "duplicate session title",
                &stale,
                now,
                None,
            )
            .unwrap();
        local_engine
            .recover_session_after_lease_loss(session_id, turn_id, &stale, 1, now + 2)
            .unwrap();
    }
    drop(local_engine);

    let foreign_workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(foreign_workspace.path().join(".git")).unwrap();
    let foreign_session_id = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let foreign_engine = latte_engine::EngineBuilder::new()
        .workspace_root(foreign_workspace.path())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let foreign_now = latte_core::wall_time_ms();
    let foreign_lease = foreign_engine
        .acquire_session_lease(foreign_session_id, foreign_now, 1)
        .unwrap();
    let foreign_turn_id = TurnId::from_uuid(SystemIdSource::default().next_uuid_v7());
    foreign_engine
        .create_started_session_v2_snapshot(
            foreign_session_id,
            foreign_turn_id,
            session_boundary_binding(),
            "foreign session title",
            &foreign_lease,
            foreign_now,
            None,
        )
        .unwrap();
    foreign_engine
        .recover_session_after_lease_loss(
            foreign_session_id,
            foreign_turn_id,
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
    pty.write(format!("/resume {foreign_session_id}\r").as_bytes());
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
    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].lifecycle, latte_core::SessionLifecycle::Ready);
    assert!(sessions[0].transcript.entries.iter().any(|entry| {
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
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && ["first answer", "queued answer"].iter().all(|answer| {
                    sessions[0].transcript.entries.iter().any(|entry| {
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
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::WaitingInput
                    && matches!(
                        sessions[0].pending.as_ref(),
                        Some(latte_core::SessionPendingRequest::Input {
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
    let session_id = engine.list_sessions().unwrap()[0].session_id;
    first.write(F10);
    let (status, _) = first.finish(Duration::from_secs(5));
    assert!(status.success());
    assert_eq!(provider.requests().len(), 1);

    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "input-e2e-secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    resumed.write(format!("/resume {session_id}\r").as_bytes());
    assert!(resumed.wait_for_output(b"Input required", Duration::from_secs(5)));
    resumed.write(b"durable-first");
    resumed.write(SHIFT_ENTER);
    resumed.write(b"durable-second\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].pending.is_none()
                    && sessions[0].transcript.entries.iter().any(|entry| {
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
    let session_id = engine.list_sessions().unwrap()[0].session_id;

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
    resumed.write(format!("/resume {session_id}\r").as_bytes());
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

    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].lifecycle,
        latte_core::SessionLifecycle::WaitingInput
    );
    assert_eq!(
        sessions[0]
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
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Failed
                    && sessions[0].pending.is_none()
            })
        }),
        "invalid input id did not fail closed: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let sessions = engine.list_sessions().unwrap();
    assert!(
        !sessions[0]
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
fn completed_tui_session_accepts_a_follow_up_as_an_immutable_child_with_history() {
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
    let first = engine.list_sessions().unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].turns.len(), 1);
    let parent_id = first[0].turns[0].turn_id;

    pty.write(b"follow up prompt\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].turns.len() == 2
                    && sessions[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::Assistant
                            && entry.text == "second answer"
                    })
            })
        }),
        "follow-up did not complete: {}",
        String::from_utf8_lossy(&pty.output())
    );
    provider.assert_consumed();
    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions[0].turns[0].turn_id, parent_id);
    assert_eq!(sessions[0].turns[1].parent_turn_id, Some(parent_id));
    assert_eq!(sessions[0].turns[1].turn_ordinal, 1);
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
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].turns.len() == 1
        })
    }));
    let initial = engine.list_sessions().unwrap().remove(0);
    let session_id = initial.session_id;
    let parent_turn_id = initial.turns[0].turn_id;
    first.write(F10);
    assert!(first.finish(Duration::from_secs(5)).0.success());
    drop(engine);

    let connection = Connection::open(scenario.database_path()).unwrap();
    let initial_last_sequence: i64 = connection
        .query_row(
            "SELECT last_seq FROM sessions WHERE session_id=?1",
            [session_id.to_string()],
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
            turn_id: Some(parent_turn_id),
            kind: TranscriptKind::Assistant,
            text,
            payload: None,
            source_key: format!("bounded-tail-fixture:{offset}"),
            created_at_ms: u64::try_from(sequence).unwrap(),
        };
        connection
            .execute(
                "INSERT INTO conversation_outbox(\
                    session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms\
                 ) VALUES(?1,?2,?3,?4,'assistant',?5,?6,?7)",
                params![
                    session_id.to_string(),
                    sequence,
                    entry.entry_id.to_string(),
                    parent_turn_id.to_string(),
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
            "UPDATE sessions SET last_seq=?1,updated_at_ms=?2 WHERE session_id=?3",
            params![
                seeded_last_sequence,
                seeded_last_sequence,
                session_id.to_string()
            ],
        )
        .unwrap();
    drop(connection);

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let tail = engine.session_snapshot_tail_v2(session_id, 500).unwrap();
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
    resumed.write(format!("/resume {session_id}\r").as_bytes());
    assert!(
        resumed.wait_for_output(b"newest bounded-tail marker", Duration::from_secs(5)),
        "TUI resumed an old transcript page: {}",
        String::from_utf8_lossy(&resumed.output())
    );
    resumed.write(b"follow up beyond the 500-card boundary\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].pending.is_none()
                && sessions[0].active_turn_id.is_none()
                && sessions[0].turns.len() == 2
                && sessions[0].transcript.entries.iter().any(|entry| {
                    entry.sequence > u64::try_from(seeded_last_sequence).unwrap()
                        && entry.kind == TranscriptKind::User
                        && entry.text == "follow up beyond the 500-card boundary"
                })
                && sessions[0].transcript.entries.iter().any(|entry| {
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
        let sessions = engine.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].lifecycle,
            latte_core::SessionLifecycle::WaitingPermission
        );
        assert!(sessions[0].pending.is_some());
    };
    assert_still_waiting();
    let denied_session_id = latte_engine::EngineBuilder::new()
        .workspace_root(denied_scenario.root())
        .database_path(denied_scenario.database_path())
        .build()
        .unwrap()
        .list_sessions()
        .unwrap()[0]
        .session_id;
    assert!(!denied_scenario.root().join("new.txt").exists());
    assert_eq!(denied_provider.requests().len(), 1);

    let mut shifted_command = denied_scenario.command(&["tui"]);
    shifted_command.env("TEST_OPENAI_KEY", "secret");
    let mut shifted_pty = PtySession::spawn(shifted_command);
    assert!(shifted_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    shifted_pty.write(format!("/resume {denied_session_id}\r").as_bytes());
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
    deny_pty.write(format!("/resume {denied_session_id}\r").as_bytes());
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
    let denied_sessions = latte_engine::EngineBuilder::new()
        .workspace_root(denied_scenario.root())
        .database_path(denied_scenario.database_path())
        .build()
        .unwrap()
        .list_sessions()
        .unwrap();
    assert_eq!(
        denied_sessions[0].lifecycle,
        latte_core::SessionLifecycle::Ready
    );
    assert_eq!(denied_sessions[0].turns.len(), 2);
    assert!(denied_sessions[0].transcript.entries.iter().any(|entry| {
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
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&pgid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    }));
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
    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].lifecycle,
        latte_core::SessionLifecycle::ReconciliationRequired
    );
    let effect_ids = sessions[0]
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
        sessions[0]
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
    assert!(!sessions[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant && entry.text == "done"
    }));

    let mut restart_command = scenario.command(&["tui"]);
    restart_command.env("TEST_OPENAI_KEY", "secret");
    let mut restarted = PtySession::spawn(restart_command);
    assert!(restarted.wait_for_output(TUI_READY, Duration::from_secs(5)));
    restarted.write(format!("/resume {}\r", sessions[0].session_id).as_bytes());
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
        engine.list_sessions().unwrap()[0].lifecycle,
        latte_core::SessionLifecycle::ReconciliationRequired
    );

    restarted.write(CTRL_A);
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine
                .effect_status(&effect_ids[0])
                .is_ok_and(|status| status == latte_engine::EffectStatus::ObservedFailed)
                && engine.list_sessions().is_ok_and(|sessions| {
                    sessions.len() == 1
                        && sessions[0].lifecycle == latte_core::SessionLifecycle::Failed
                        && sessions[0].pending.is_none()
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
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&pgid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    }));
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
    let sessions = engine.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].lifecycle, latte_core::SessionLifecycle::Ready);
    assert_eq!(
        sessions[0]
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

#[allow(clippy::too_many_lines)]
fn pty_screen_text(output: &[u8], rows: usize, cols: usize) -> String {
    let mut grid = vec![vec![' '; cols]; rows];
    let mut row = 0usize;
    let mut col = 0usize;
    let decoded = String::from_utf8_lossy(output);
    let mut chars = decoded.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    let private = matches!(chars.peek(), Some('?' | '>' | '<'));
                    if private {
                        let _ = chars.next();
                    }
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            final_byte = c;
                            break;
                        }
                        params.push(c);
                    }
                    if private {
                        continue;
                    }
                    let nums: Vec<usize> = params
                        .split(';')
                        .map(|part| part.parse::<usize>().unwrap_or(0))
                        .collect();
                    let p1 = nums.first().copied().unwrap_or(0);
                    let p2 = nums.get(1).copied().unwrap_or(0);
                    match final_byte {
                        'H' | 'f' => {
                            row = p1.max(1).saturating_sub(1).min(rows.saturating_sub(1));
                            col = p2.max(1).saturating_sub(1).min(cols.saturating_sub(1));
                        }
                        'G' => col = p1.max(1).saturating_sub(1).min(cols.saturating_sub(1)),
                        'd' => row = p1.max(1).saturating_sub(1).min(rows.saturating_sub(1)),
                        'A' => row = row.saturating_sub(p1.max(1)),
                        'B' => row = (row + p1.max(1)).min(rows.saturating_sub(1)),
                        'C' => col = (col + p1.max(1)).min(cols.saturating_sub(1)),
                        'D' => col = col.saturating_sub(p1.max(1)),
                        'J' if p1 == 2 => {
                            for line in &mut grid {
                                line.fill(' ');
                            }
                            row = 0;
                            col = 0;
                        }
                        'K' => match p1 {
                            0 if row < rows => {
                                for cell in grid[row].iter_mut().skip(col) {
                                    *cell = ' ';
                                }
                            }
                            1 if row < rows => {
                                for cell in grid[row].iter_mut().take((col + 1).min(cols)) {
                                    *cell = ' ';
                                }
                            }
                            2 if row < rows => {
                                grid[row].fill(' ');
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    let _ = chars.next();
                }
            },
            '\r' => col = 0,
            '\n' => row = (row + 1).min(rows.saturating_sub(1)),
            c if c.is_control() => {}
            c => {
                if row < rows && col < cols {
                    grid[row][col] = c;
                    col += 1;
                }
            }
        }
    }
    grid.into_iter()
        .map(|line| line.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_screen_text(
    pty: &PtySession,
    rows: usize,
    cols: usize,
    needle: &str,
    timeout: Duration,
) -> bool {
    wait_until(timeout, || {
        pty_screen_text(&pty.output(), rows, cols).contains(needle)
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_slash_compact_validates_availability_compacts_history_and_refreshes_context_meter() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion(&"y".repeat(700)),
        ProviderReply::completion(&"z".repeat(700)),
        ProviderReply::completion("TUI-MANUAL-COMPACT-SUMMARY"),
    ]);
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["/usr/bin/true"]}},session:{{max_request_bytes:5600,max_input_bytes:5600,reserved_output_bytes:1,context_cap_bytes:65536,provider_timeout_ms:60000,compaction:{{enabled:true,max_summary_source_bytes:8192,trigger_ratio:100}}}}}}"#,
            endpoint = provider.endpoint()
        ),
    )
    .unwrap();
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "tui-compact-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Describe an outcome",
        Duration::from_secs(5)
    ));

    // 1. In SessionDraft, `/compact extra` rejects forbidden arguments and preserves composer.
    pty.write(b"/compact extra\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "/compact does not accept arguments",
            Duration::from_secs(5)
        ),
        "expected /compact argument validation error:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert_eq!(provider.requests().len(), 0);
    pty.write(&[0x7f; 20]);

    // 2. In SessionDraft, `/compact` reports disabled reason without calling the provider.
    pty.write(b"/compact\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "This command requires an open saved session",
            Duration::from_secs(5)
        ),
        "expected /compact disabled reason:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert_eq!(provider.requests().len(), 0);
    pty.write(&[0x7f; 12]);

    // 3. Complete turn 1; single-segment `/compact` reports `nothing to compact`.
    pty.write(b"first compactable turn\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1 && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
        })
    }));
    pty.write(b"/compact\r");
    assert!(
        wait_for_screen_text(&pty, 40, 120, "nothing to compact", Duration::from_secs(5)),
        "single-segment compact did not report nothing to compact:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert_eq!(provider.requests().len(), 1);

    // 4. Complete turn 2 so an older segment becomes compressible, then run `/compact`.
    pty.write(b"second turn before manual compact\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].turns.len() == 2
                && sessions[0].turns[1].status == latte_core::SessionTurnStatus::Completed
        })
    }));
    pty.write(b"/compact\r");
    assert!(provider.wait_for_calls(3, Duration::from_secs(5)));
    assert!(
        wait_for_screen_text(&pty, 40, 120, "compacted history", Duration::from_secs(5)),
        "productive /compact did not render notice:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "TUI-MANUAL-COMPACT-SUMMARY",
            Duration::from_secs(5)
        ),
        "productive /compact did not refresh summary card into transcript:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions[0]
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::CompactSummary)
        })
    }));

    // 5. Repeating `/compact` is idempotent and spends zero extra provider calls.
    pty.write(b"/compact\r");
    assert!(
        wait_for_screen_text(&pty, 40, 120, "nothing to compact", Duration::from_secs(5)),
        "repeat /compact did not report nothing to compact:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    provider.assert_consumed();

    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_slash_cancel_and_batch4_permission_outcomes_preserve_session_safety() {
    let cancel_scenario = Scenario::new();
    let cancel_provider = ScriptedProvider::start([
        ProviderReply::error(503, "busy").header("retry-after", "5"),
        ProviderReply::completion("unreachable-after-cancel"),
    ]);
    cancel_scenario.write_config_with_provider_fields(
        cancel_provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        ".latte/latte-code.db",
        ",max_attempts:3",
    );
    let mut cancel_command = cancel_scenario.command(&["tui"]);
    cancel_command.env("TEST_OPENAI_KEY", "tui-cancel-secret");
    let mut pty = PtySession::spawn(cancel_command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Describe an outcome",
        Duration::from_secs(5)
    ));

    // 1. `/cancel extra` and idle `/cancel` reject locally and preserve composer.
    pty.write(b"/cancel extra\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "/cancel does not accept arguments",
            Duration::from_secs(5)
        ),
        "expected /cancel argument validation error:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    pty.write(&[0x7f; 20]);

    pty.write(b"/cancel\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "No active turn to cancel",
            Duration::from_secs(5)
        ),
        "expected /cancel disabled reason:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    pty.write(&[0x7f; 12]);
    assert_eq!(cancel_provider.requests().len(), 0);

    // 2. Start a running turn, wait until TUI renders Running, verify `/compact`
    //    is disabled while running, then `/cancel` interrupts the turn without
    //    exiting the TUI or arming Ctrl+C exit.
    pty.write(b"slow turn for slash cancel\r");
    assert!(cancel_provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(
        wait_for_screen_text(&pty, 40, 120, "Running · mock", Duration::from_secs(5)),
        "TUI never rendered Running state:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );

    pty.write(b"/compact\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Compact requires an idle Ready session",
        Duration::from_secs(5)
    ));
    pty.write(&[0x7f; 12]);

    let cancel_engine = latte_engine::EngineBuilder::new()
        .workspace_root(cancel_scenario.root())
        .database_path(cancel_scenario.database_path())
        .build()
        .unwrap();
    pty.write(b"/cancel\r");
    assert!(
        wait_until(Duration::from_secs(5), || {
            cancel_engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Interrupted
                    && sessions[0].turns[0].status == latte_core::SessionTurnStatus::Interrupted
            })
        }),
        "slash /cancel did not interrupt active turn:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    assert!(pty.is_running());

    // Proves `/cancel` did NOT arm `ctrl_c_exit_armed_until`: a single Ctrl+C
    // now arms exit confirmation instead of quitting the TUI.
    pty.write(CTRL_C);
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Ctrl+C again to exit",
        Duration::from_secs(5)
    ));
    assert!(pty.is_running());
    pty.write(F10);
    let (cancel_status, _) = pty.finish(Duration::from_secs(5));
    assert!(cancel_status.success());

    // 3. Batch 4 outcomes: trigger `WaitingPermission`, verify Enter does not
    //    approve, deny with `d` -> `Permission denied · ready`, then follow up
    //    and cancel waiting permission with Ctrl+C -> `Cancelled`.
    let perm_scenario = Scenario::new();
    let perm_provider = ScriptedProvider::start([
        write_file_reply("batch4-deny-write"),
        write_file_reply("batch4-cancel-write"),
    ]);
    let mut perm_command = perm_scenario.command(&["tui"]);
    perm_scenario.configure_provider(
        &mut perm_command,
        perm_provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "tui-perm-secret",
    );
    let mut perm_pty = PtySession::spawn(perm_command);
    assert!(perm_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &perm_pty,
        40,
        120,
        "Describe an outcome",
        Duration::from_secs(5)
    ));

    perm_pty.write(b"propose write to deny\r");
    assert!(perm_provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &perm_pty,
        40,
        120,
        "Permission required",
        Duration::from_secs(5)
    ));
    perm_pty.write(ENTER);
    assert!(!perm_scenario.root().join("new.txt").exists());
    perm_pty.write(b"d");
    assert!(
        wait_for_screen_text(
            &perm_pty,
            40,
            120,
            "Permission denied · ready",
            Duration::from_secs(5)
        ),
        "expected distinct Permission denied · ready label:\n{}",
        pty_screen_text(&perm_pty.output(), 40, 120)
    );
    assert!(!perm_scenario.root().join("new.txt").exists());
    let perm_engine = latte_engine::EngineBuilder::new()
        .workspace_root(perm_scenario.root())
        .database_path(perm_scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        perm_engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].turns.len() == 1
                && sessions[0].turns[0].failure_code
                    == Some(latte_core::FailureCode::PermissionDenied)
        })
    }));

    // 4. Follow up on the same ready session, park on permission again, and
    //    cancel via Ctrl+C — verifying distinct `Cancelled` outcome.
    perm_pty.write(b"propose write to cancel\r");
    assert!(perm_provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &perm_pty,
        40,
        120,
        "Permission required",
        Duration::from_secs(5)
    ));
    perm_pty.write(CTRL_C);
    assert!(wait_until(Duration::from_secs(5), || {
        perm_engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Failed
                && sessions[0].turns.len() == 2
                && sessions[0].turns[1].failure_code == Some(latte_core::FailureCode::Cancelled)
        })
    }));
    assert!(wait_for_screen_text(
        &perm_pty,
        40,
        120,
        "Cancelled",
        Duration::from_secs(5)
    ));
    assert!(!perm_scenario.root().join("new.txt").exists());
    perm_provider.assert_consumed();

    perm_pty.write(F10);
    let (perm_status, _) = perm_pty.finish(Duration::from_secs(5));
    assert!(perm_status.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_supports_cursor_editing_ctrl_j_newline_and_permission_mode_switching() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("diff_target.txt"), "old_line\nkeep\n").unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "read-diff-1",
            "read_file",
            &serde_json::json!({"path": "diff_target.txt"}),
        ),
        ProviderReply::tool_call(
            "ask-edit-1",
            "edit_file",
            &serde_json::json!({
                "path": "diff_target.txt",
                "before": "old_line",
                "after": "new_line"
            }),
        ),
        ProviderReply::completion("diff approved and switched to auto"),
        ProviderReply::tool_call(
            "auto-write-2",
            "write_file",
            &serde_json::json!({"path": "auto_created.txt", "content": "written without second prompt\n"}),
        ),
        ProviderReply::completion("auto-edit write completed"),
    ]);
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}}}}"#,
            provider.endpoint()
        ),
    )
    .unwrap();
    let mut command = scenario.command(&["tui", "--permission-mode", "auto-edit"]);
    command
        .env("TEST_OPENAI_KEY", "tui-mode-secret")
        .env("LATTE_TUI_MOUSE_CAPTURE", "1");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Auto-edit",
        Duration::from_secs(5)
    ));

    // Exercise Shift+Tab (`\x1b[Z`) mode cycling (`Auto-edit -> Full-auto -> Ask`),
    // `/mode` no-arg cycling (`Ask -> Auto-edit`), `/mode bogus` error, and `/mode ask`.
    pty.write(b"\x1b[Z");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Full-auto mode",
        Duration::from_secs(5)
    ));
    pty.write(b"\x1b[Z");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Ask mode",
        Duration::from_secs(5)
    ));
    pty.write(b"/mode\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Permission mode set to Auto-edit mode",
        Duration::from_secs(5)
    ));
    pty.write(b"/mode bogus\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Unknown permission mode",
        Duration::from_secs(5)
    ));
    // Clear `/mode bogus` via Ctrl+U (`\x15`), then set `/mode ask`.
    pty.write(b"\x15/mode ask\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Permission mode set to Ask mode",
        Duration::from_secs(5)
    ));

    // Exercise cursor navigation: Home (`\x1b[H`), End (`\x1b[F`), Delete (`\x1b[3~`),
    // Ctrl+W (`\x17`), Up (`\x1b[A`), Down (`\x1b[B`), Left (`\x1b[D`), Right (`\x1b[C`), Ctrl+J (`\n`).
    pty.write(b"xac\x1b[H\x1b[3~\x1b[F\x1b[Db\x1b[C\nline2 extra\x17\x1b[A\x1b[B\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    // Wait for `edit_file` permission card with unified diff preview (`- old_line` / `+ new_line`),
    // then press `a` to approve AND switch the live session to Auto-edit mode over HTTP!
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "- old_line",
        Duration::from_secs(5)
    ));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "+ new_line",
        Duration::from_secs(5)
    ));
    pty.write(b"a");
    assert!(provider.wait_for_calls(3, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "diff approved and switched to auto",
        Duration::from_secs(5)
    ));

    // Exercise Up/Down prompt history recall + PageUp/PageDown scrolling, then submit follow-up
    // (which now runs in Auto-edit mode without asking permission!).
    pty.write(b"\x1b[5~\x1b[6~\x1b[A\x1b[Bfollow up write\r");
    assert!(provider.wait_for_calls(5, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "auto-edit write completed",
        Duration::from_secs(5)
    ));
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("diff_target.txt")).unwrap(),
        "new_line\nkeep\n"
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("auto_created.txt")).unwrap(),
        "written without second prompt\n"
    );
    let requests = provider.requests();
    let first_req = serde_json::to_string(&requests[0].body).unwrap();
    assert!(
        first_req.contains("abc\\nline2"),
        "expected cursor-edited multiline prompt in {first_req}"
    );

    pty.write(F10);
    let (status, raw_out) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    let terminal = String::from_utf8_lossy(&raw_out);
    assert!(
        terminal.contains("\u{1b}[?1000h") && terminal.contains("\u{1b}[?1000l"),
        "LATTE_TUI_MOUSE_CAPTURE=1 enables and restores mouse capture cleanly"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_switching_sessions_synchronizes_authoritative_permission_mode_per_session() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        // Session alpha initial prompt -> Ready.
        ProviderReply::completion("alpha initial ready"),
        // Session beta initial prompt -> Ready.
        ProviderReply::completion("beta initial ready"),
        // Session alpha follow-up after switching back from beta (must still be Full-auto!).
        ProviderReply::tool_call(
            "alpha-auto-write",
            "write_file",
            &serde_json::json!({
                "path": "alpha_auto.txt",
                "content": "alpha stayed full-auto\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("alpha follow-up completed without asking"),
        // Session beta follow-up after switching back from alpha (must still be Ask and park!).
        ProviderReply::tool_call(
            "beta-ask-write",
            "write_file",
            &serde_json::json!({
                "path": "beta_blocked.txt",
                "content": "must ask\n",
                "create_intent": true
            }),
        ),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}}}}"#
        ),
    )
    .unwrap();

    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "tui-mode-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Ask",
        Duration::from_secs(5)
    ));

    // 1. Set draft to Full-auto, create session `alpha`, and rename it.
    pty.write(b"/mode auto\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Full-auto",
        Duration::from_secs(5)
    ));
    pty.write(b"start alpha\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "alpha initial ready",
        Duration::from_secs(5)
    ));
    pty.write(b"/rename alpha\r");
    assert!(wait_until(Duration::from_secs(5), || {
        Connection::open(scenario.database_path())
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE title='alpha'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
            })
            == Some(1)
    }));

    // 2. Open `/new`, switch to `/mode ask`, create session `beta`, and rename it.
    pty.write(b"/new\r");
    pty.write(b"/mode ask\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Permission mode set to Ask mode",
        Duration::from_secs(5)
    ));
    pty.write(b"start beta\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "beta initial ready",
        Duration::from_secs(5)
    ));
    pty.write(b"/rename beta\r");
    assert!(wait_until(Duration::from_secs(5), || {
        Connection::open(scenario.database_path())
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE title='beta'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
            })
            == Some(1)
    }));

    // 3. Switch back to `alpha` via `/resume alpha` -> status bar MUST show `Full-auto`, NOT `Ask` (#11)!
    pty.write(b"/resume alpha\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Full-auto",
        Duration::from_secs(5)
    ));
    pty.write(b"alpha write now\r");
    assert!(provider.wait_for_calls(4, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "alpha follow-up completed without asking",
        Duration::from_secs(5)
    ));
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("alpha_auto.txt")).unwrap(),
        "alpha stayed full-auto\n"
    );

    // 4. Switch back to `beta` via `/resume beta` -> status bar MUST show `Ask`, and write_file MUST park!
    pty.write(b"/resume beta\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "beta initial ready",
        Duration::from_secs(5)
    ));
    pty.write(b"beta write now\r");
    assert!(provider.wait_for_calls(5, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Permission required",
        Duration::from_secs(5)
    ));
    // Deny with `n` once `Permission required` has been rendered (`FrameRendered` guard).
    pty.write(b"n");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Permission denied",
        Duration::from_secs(5)
    ));
    assert!(!scenario.root().join("beta_blocked.txt").exists());

    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_command_palette_slash_tab_draft_model_start_and_unconfigured_submission() {
    // 0. Invalid `tui` CLI flags fail closed with usage exit code 2.
    let bad_flag_scenario = Scenario::new();
    for bad_args in [
        &["tui", "--permission-mode"][..],
        &["tui", "--permission-mode", "bogus"][..],
        &["tui", "--unexpected-flag"][..],
    ] {
        let out = bad_flag_scenario.output(bad_args, |_| {});
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected usage error for {bad_args:?}"
        );
    }

    // 1. Unconfigured TUI inside `~/workspace-under-home` (`workspace_display_path` `~/...` branch):
    //    renders `Not configured` and refuses draft submission / `/model` picker cleanly.
    let unconfigured = Scenario::new();
    let canon_home = unconfigured.home().canonicalize().unwrap();
    let home_sub_ws = canon_home.join("workspace-under-home");
    std::fs::create_dir_all(home_sub_ws.join(".git")).unwrap();
    let mut unconf_cmd = unconfigured.command(&["tui"]);
    unconf_cmd
        .env("HOME", &canon_home)
        .current_dir(&home_sub_ws);
    let mut unconf_pty = PtySession::spawn(unconf_cmd);
    assert!(unconf_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &unconf_pty,
        40,
        120,
        "~/workspace-under-home",
        Duration::from_secs(5)
    ));
    assert!(wait_for_screen_text(
        &unconf_pty,
        40,
        120,
        "Not configured",
        Duration::from_secs(5)
    ));
    unconf_pty.write(b"/model\rhello without config\r");
    assert!(wait_for_screen_text(
        &unconf_pty,
        40,
        120,
        "Provider setup required",
        Duration::from_secs(5)
    ));
    unconf_pty.write(F10);
    assert!(unconf_pty.finish(Duration::from_secs(5)).0.success());

    // 2. Configured TUI with two models (`main/mock` default, `main/alt` secondary)
    //    and `/usr/bin/true` verification: exercises slash Tab completion, Command
    //    Palette disabled & required-argument branches, draft `/model` selection
    //    (`SessionUiAction::StartWithModel`), and verified `TranscriptKind::Completion` rendering.
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "alt-write",
            "write_file",
            &serde_json::json!({
                "path": "alt_verified.txt",
                "content": "created by alt model\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("alt verified completion card"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{
                version: 1,
                default_model: "main/mock",
                providers: {{
                    main: {{
                        type: "openai-chat",
                        models: ["mock", "alt"],
                        endpoint: {endpoint:?},
                        api_key: {{ source: "env", name: "TEST_OPENAI_KEY" }}
                    }}
                }},
                verification: {{ argv: ["/usr/bin/true"] }}
            }}"#
        ),
    )
    .unwrap();

    let mut command = scenario.command(&["tui", "--auto"]);
    command.env("TEST_OPENAI_KEY", "tui-alt-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    // On Draft: Command Palette (`Ctrl+P` = `\x1b[112;5u`) -> navigate 7x `j` to `Rename` (Disabled on draft) + Enter!
    let before_draft_palette = pty.output().len();
    pty.write(b"\x1b[112;5u");
    assert!(pty.wait_for_growth(before_draft_palette, Duration::from_secs(5)));
    pty.write(b"jjjjjjj\x1b[13u");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "open saved session",
        Duration::from_secs(5)
    ));

    // Slash popup Tab completion: `/mo` + Tab completes `/model`, then Enter opens the model picker;
    // Down (`\x1b[B`) + Enter selects `main/alt` on the draft session!
    pty.write(b"/mo\t\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "alt",
        Duration::from_secs(5)
    ));
    pty.write(b"\x1b[B\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "New sessions will use main/alt",
        Duration::from_secs(5)
    ));

    // Submit prompt on draft -> dispatches `SessionUiAction::StartWithModel` (`main/alt`),
    // writes `alt_verified.txt` in Full-auto mode, passes `/usr/bin/true` verification,
    // and renders `TranscriptKind::Completion`!
    pty.write(b"ship with alt\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "alt verified completion card",
        Duration::from_secs(5)
    ));
    assert_eq!(provider.requests()[0].body["model"], "alt");
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("alt_verified.txt")).unwrap(),
        "created by alt model\n"
    );

    // Wait until post-mutation verification (`/usr/bin/true`) finishes and renders the
    // final `VERIFIED` handoff block, then close `pty` cleanly.
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "VERIFIED",
        Duration::from_secs(5)
    ));
    pty.write(F10);
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());

    // 3. Reopen in a fresh TUI (zero background SSE mutations in flight), open the saved
    //    session, then open Command Palette (`Ctrl+P`) and select index 7 (`Rename`,
    //    `ArgumentPolicy::Required`) -> renders `/rename requires an argument`!
    let mut pty2 = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty2.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty2.write(b"/sessions\r");
    assert!(wait_for_screen_text(
        &pty2,
        40,
        120,
        "ship with alt",
        Duration::from_secs(5)
    ));
    pty2.write(b"\x1b[13u");
    assert!(wait_for_screen_text(
        &pty2,
        40,
        120,
        "VERIFIED",
        Duration::from_secs(5)
    ));
    pty2.write(b"\x1b[112;5u");
    assert!(wait_for_screen_text(
        &pty2,
        40,
        120,
        "Rename the current session",
        Duration::from_secs(5)
    ));
    pty2.write(b"jjjjjjj\x1b[13u");
    assert!(
        wait_for_screen_text(
            &pty2,
            40,
            120,
            "/rename requires an argument",
            Duration::from_secs(5)
        ),
        "expected `/rename requires an argument` on screen, got:\n{}",
        pty_screen_text(&pty2.output(), 40, 120)
    );

    pty2.write(F10);
    let (status2, _) = pty2.finish(Duration::from_secs(5));
    assert!(status2.success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tui_compacted_projection_live_permission_mode_multiline_editor_and_sigint() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("doc.txt"), "D".repeat(2400)).unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "t_read",
            "read_file",
            &serde_json::json!({"path": "doc.txt"}),
        ),
        ProviderReply::completion("first turn read doc"),
        ProviderReply::completion("second turn retained"),
        ProviderReply::completion("third turn retained"),
        ProviderReply::completion("compacted summary card text"),
        ProviderReply::completion("post-fork turn answer"),
        // Blank reply for TUI `/compact` on the forked session -> `CompactionError`!
        ProviderReply::completion("   "),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{
                version: 1,
                default_model: "main/mock",
                providers: {{
                    main: {{
                        type: "openai-chat",
                        models: ["mock"],
                        endpoint: {endpoint:?},
                        api_key: {{ source: "env", name: "TEST_OPENAI_KEY" }}
                    }}
                }},
                session: {{
                    max_request_bytes: 14000,
                    max_input_bytes: 12000,
                    reserved_output_bytes: 2000,
                    context_cap_bytes: 256,
                    compaction: {{
                        enabled: true,
                        mode: "elide_then_summarize",
                        max_summary_source_bytes: 32000,
                        retain_ratio: 1
                    }}
                }}
            }}"#
        ),
    )
    .unwrap();

    // 1. Seed a session via final binary: Turn 1 reads `doc.txt`, Turn 2 adds a second segment,
    //    manual `compact` #1 appends `tool_result_elision`, Turn 3 adds retained tail, and manual
    //    `compact` #2 appends `compact_summary` with `retain_from_sequence` (exercising
    //    `session_transcript_projection` `storage.rs:6938..6985` when TUI lists/opens sessions!).
    let r1 = scenario.output(&["--json", "run", "seed compacted session"], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(r1.status.success());
    let sid = serde_json::from_slice::<serde_json::Value>(&r1.stdout).unwrap()["data"]["session"]
        ["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let r2 = scenario.output(&["--json", "resume", &sid, "second segment"], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(r2.status.success());

    let c1 = scenario.output(&["compact", &sid], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(c1.status.success());

    let r3_prompt = format!("third-retained-{}", "t".repeat(300));
    let r3 = scenario.output(&["--json", "resume", &sid, &r3_prompt], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(r3.status.success());

    // Switch mode to `summarize` for the second manual compact so a `compact_summary` card
    // with `retain_from_sequence` is persisted alongside the earlier `tool_result_elision` card.
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{
                version: 1,
                default_model: "main/mock",
                providers: {{
                    main: {{
                        type: "openai-chat",
                        models: ["mock"],
                        endpoint: {endpoint:?},
                        api_key: {{ source: "env", name: "TEST_OPENAI_KEY" }}
                    }}
                }},
                session: {{
                    max_request_bytes: 14000,
                    max_input_bytes: 12000,
                    reserved_output_bytes: 2000,
                    context_cap_bytes: 256,
                    compaction: {{
                        enabled: true,
                        mode: "summarize",
                        max_summary_source_bytes: 32000,
                        retain_ratio: 1
                    }}
                }}
            }}"#
        ),
    )
    .unwrap();
    let c2 = scenario.output(&["compact", &sid], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(c2.status.success());
    let c2_stdout = String::from_utf8_lossy(&c2.stdout);
    assert!(
        c2_stdout.contains("summarized older history"),
        "expected Tier::Summarized in c2 output, got: {c2_stdout}"
    );
    // Seed `conversation_outbox` with the `compact_summary` card before running `context` so
    // `Storage::session_snapshot_projection_v2` -> `session_transcript_projection`
    // (`storage.rs:6938..6985`) executes over SQLite outbox rows, and append a `completion`
    // transcript card so opening the session in TUI renders `TranscriptKind::Completion`
    // (`tui/session.rs:4568..4580`).
    {
        let jsonl_path = scenario.session_files().remove(0);
        let raw_jsonl = std::fs::read_to_string(&jsonl_path).unwrap();
        let entries: Vec<serde_json::Value> = raw_jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let summary_record = entries
            .iter()
            .rev()
            .find(|v| v["kind"] == "compact_summary")
            .unwrap()
            .clone();
        let max_seq = entries
            .iter()
            .filter_map(|v| v["seq"].as_u64())
            .max()
            .unwrap();
        let mut completion_record = entries.last().unwrap().clone();
        completion_record["entry_id"] = serde_json::json!("01900000-0000-7000-8000-0000000000c9");
        completion_record["seq"] = serde_json::json!(max_seq + 1);
        completion_record["kind"] = serde_json::json!("completion");
        completion_record["source_key"] = serde_json::json!("e2e-completion-card");
        completion_record["content"] =
            serde_json::json!("verified completion line one\nverified completion line two");
        completion_record["payload"] = serde_json::Value::Null;
        std::fs::write(
            &jsonl_path,
            format!(
                "{raw_jsonl}{}\n",
                serde_json::to_string(&completion_record).unwrap()
            ),
        )
        .unwrap();

        let summary_outbox_entry = serde_json::json!({
            "entry_id": summary_record["entry_id"],
            "sequence": summary_record["seq"],
            "turn_id": summary_record["turn_id"],
            "kind": "compact_summary",
            "text": summary_record["content"],
            "payload": summary_record["payload"],
            "source_key": summary_record["source_key"],
            "created_at_ms": summary_record["created_at_ms"],
        });
        let conn = Connection::open(scenario.database_path()).unwrap();
        let _ = conn.busy_timeout(Duration::from_secs(5));
        conn.execute(
            "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) \
             VALUES(?1,?2,?3,NULL,'compact_summary',?4,?5,1)",
            rusqlite::params![
                &sid,
                i64::try_from(summary_record["seq"].as_u64().unwrap()).unwrap(),
                summary_record["entry_id"].as_str().unwrap(),
                summary_record["source_key"].as_str().unwrap(),
                serde_json::to_string(&summary_outbox_entry).unwrap(),
            ],
        )
        .unwrap();
    }
    let ctx_projected = scenario.output(&["--json", "context", &sid], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    });
    assert!(ctx_projected.status.success());

    let listed_projected = scenario.output(&["--json", "list"], |_| {});
    assert!(listed_projected.status.success());
    assert!(
        String::from_utf8_lossy(&listed_projected.stdout).contains("compacted summary card text")
    );

    // 2. Spawn `latte-code tui --permission-mode auto-edit` (`lib.rs:832..841`) with mouse capture
    //    enabled (`tui/lib.rs:165..166`) and open the session via `/resume seed compacted session`!
    let mut cmd = scenario.command(&["tui", "--permission-mode", "auto-edit"]);
    cmd.env("TEST_OPENAI_KEY", "tui-compact-key");
    cmd.env("LATTE_TUI_MOUSE_CAPTURE", "1");
    let mut pty = PtySession::spawn(cmd);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Describe an outcome",
        Duration::from_secs(5)
    ));

    pty.write(b"/resume seed compacted session\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "compacted summary card text",
            Duration::from_secs(5)
        ),
        "expected Compacted summary card in TUI:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );

    // Slash popup Enter on an argument-required command (`/rename` without argument)
    // sets `/rename requires an argument` (`tui/session.rs:1401..1405`), then `Ctrl+U` clears!
    pty.write(b"/rename\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "/rename requires an argument",
        Duration::from_secs(5)
    ));
    let before_clear_rename = pty.output().len();
    pty.write(b"\x1b[117;5u");
    assert!(pty.wait_for_growth(before_clear_rename, Duration::from_secs(5)));

    // 3. `/mode auto` (`PermissionModeCompleted`), plus a new turn followed by `/compact`
    //    failing on a blank summary (`CompactionError`).
    pty.write(b"/mode auto\r");
    assert!(
        wait_for_screen_text(&pty, 40, 120, "Full-auto", Duration::from_secs(5)),
        "expected Full-auto mode on open session:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );

    let post_fork_prompt = format!("post-fork-{}\r", "f".repeat(250));
    pty.write(post_fork_prompt.as_bytes());
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "post-fork turn answer",
        Duration::from_secs(5)
    ));
    pty.write(b"/compact\r");
    assert!(provider.wait_for_calls(7, Duration::from_secs(5)));
    provider.assert_consumed();

    // 4. Exercise multiline composer navigation & editing (`Up`, `Down`, `Home`, `End`,
    //    `Delete`, `Ctrl+A`, `Ctrl+D`, `Ctrl+E`, `Ctrl+W`, `Ctrl+U`).
    pty.write(b"first-line");
    pty.write(SHIFT_ENTER);
    pty.write(b"second word");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "second word",
        Duration::from_secs(5)
    ));
    let before_nav = pty.output().len();
    pty.write(
        b"\x1b[<64;10;10M\x1b[<65;10;10M\x1b[A\x1b[B\x1b[H\x1b[3~\x1b[F\x1b[97;5u\x1b[100;5u\x1b[101;5u\x1b[119;5u\x1b[117;5u\x1b[127u\x1b[117;5u",
    );
    assert!(pty.wait_for_growth(before_nav, Duration::from_secs(5)));

    // Install a conditional trigger rejecting ONLY `auto_edit` while allowing `full_auto`
    // and `ask`: rapid `/mode auto-edit` (fails) followed immediately by `/mode full-auto`
    // (succeeds) must keep `Full-auto` without stale failure rollback, while a standalone
    // `/mode auto-edit` failure surfaces `PermissionModeError`!
    {
        let conn = Connection::open(scenario.database_path()).unwrap();
        let _ = conn.busy_timeout(Duration::from_secs(5));
        conn.execute_batch(&format!(
            "CREATE TRIGGER block_perm_mode BEFORE UPDATE OF permission_mode ON sessions \
             WHEN NEW.session_id = '{sid}' AND NEW.permission_mode = 'auto_edit' \
             BEGIN SELECT RAISE(ABORT, 'mode locked'); END;"
        ))
        .unwrap();
    }
    pty.write(b"/mode ask\r");
    assert!(wait_for_screen_text(
        &pty,
        40,
        120,
        "Ask",
        Duration::from_secs(5)
    ));
    pty.write(b"/mode auto-edit\r/mode full-auto\r");
    assert!(
        wait_for_screen_text(&pty, 40, 120, "Full-auto", Duration::from_secs(5)),
        "newer /mode full-auto must win over failed /mode auto-edit:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    pty.write(b"/mode auto-edit\r");
    assert!(
        wait_for_screen_text(
            &pty,
            40,
            120,
            "Permission mode rejected",
            Duration::from_secs(5)
        ),
        "expected PermissionModeError notice:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );

    // 5. Deliver OS `SIGINT` signals: once composer clears and `again to exit` is armed,
    //    one more `SIGINT` exits 0 via `tui/session.rs:5946..5948`!
    for _ in 0..10 {
        if pty_screen_text(&pty.output(), 40, 120).contains("again to exit") {
            break;
        }
        pty.send_sigint();
        std::thread::sleep(Duration::from_millis(120));
    }
    assert!(
        wait_for_screen_text(&pty, 40, 120, "again to exit", Duration::from_secs(5)),
        "expected Ctrl+C exit arm prompt:\n{}",
        pty_screen_text(&pty.output(), 40, 120)
    );
    pty.send_sigint();
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}
