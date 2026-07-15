use super::{
    headless::write_file_reply,
    support::{
        ProviderReply, PtySession, Scenario, ScriptedProvider, assert_process_group_gone,
        wait_until,
    },
};
use std::time::Duration;

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_A: &[u8] = b"\x1b[97;5u";
const CTRL_C: &[u8] = b"\x1b[99;5u";
const CTRL_R: &[u8] = b"\x1b[114;5u";
const SHIFT_ENTER: &[u8] = b"\x1b[13;2u";

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

#[test]
fn explicit_tui_fails_cleanly_without_a_terminal() {
    let scenario = Scenario::new();
    let output = scenario.output(&["tui"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a TTY"));
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
    assert!(terminal.contains("\u{1b}[?1049l"));
    assert!(terminal.contains("\u{1b}[<1u"));
}

#[test]
fn tui_commits_prompt_before_a_provider_configuration_failure() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    let sentinel = b"failed-start-visible-sentinel";
    pty.write(sentinel);
    pty.write(b"\r");
    assert!(pty.wait_for_output(
        b"selected model could not be started",
        Duration::from_secs(5)
    ));

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
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::User
                            && entry.text.as_bytes() == sentinel
                    })
                    && threads[0]
                        .transcript
                        .entries
                        .iter()
                        .any(|entry| entry.kind == latte_core::TranscriptKind::Failure)
            })
        }),
        "provider configuration failure was not durably projected: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].lifecycle, latte_core::ThreadLifecycle::Failed);
    assert_eq!(threads[0].runs.len(), 1);
    assert_eq!(
        threads[0].runs[0].status,
        latte_core::ThreadRunStatus::Failed
    );
    assert!(pty.is_running());
    let terminal = pty.output();
    assert!(
        !terminal
            .windows(b"prompt has been restored".len())
            .any(|value| value == b"prompt has been restored")
    );
    assert!(
        !terminal
            .windows(b"Unable to submit".len())
            .any(|value| value == b"Unable to submit")
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
    first.write(F10);
    let (status, _) = first.finish(Duration::from_secs(5));
    assert!(status.success());
    assert_eq!(provider.requests().len(), 1);

    let mut resumed_command = scenario.command(&["tui"]);
    resumed_command.env("TEST_OPENAI_KEY", "input-e2e-secret");
    let mut resumed = PtySession::spawn(resumed_command);
    assert!(resumed.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(resumed.wait_for_output(b"Input required", Duration::from_secs(5)));
    resumed.write(b"durable-value\r");
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
    assert_eq!(users, ["request a value", "durable-value"]);
    resumed.write(F10);
    let (status, _) = resumed.finish(Duration::from_secs(5));
    assert!(status.success());
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
fn permission_card_requires_exact_keys_and_resolves_once() {
    let denied_scenario = Scenario::new();
    let denied_provider = ScriptedProvider::start([write_file_reply("tui-deny-write")]);
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
    assert!(!denied_scenario.root().join("new.txt").exists());
    assert_eq!(denied_provider.requests().len(), 1);

    let mut shifted_command = denied_scenario.command(&["tui"]);
    shifted_command.env("TEST_OPENAI_KEY", "secret");
    let mut shifted_pty = PtySession::spawn(shifted_command);
    assert!(shifted_pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
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
    assert!(deny_pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    deny_pty.write(b"d");
    assert!(deny_pty.wait_for_output(b"permission denied", Duration::from_secs(5)));
    assert!(!denied_scenario.root().join("new.txt").exists());
    denied_provider.assert_consumed();
    assert_eq!(denied_provider.requests().len(), 1);
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
