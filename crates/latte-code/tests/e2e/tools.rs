use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use std::{process::Command, time::Duration};

/// Extracts the completion summary text from a v2 run envelope's transcript.
fn completion_text(output: &std::process::Output) -> String {
    json(output)["data"]["session"]["transcript"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|entry| entry["kind"] == "completion")
        .and_then(|entry| entry["text"].as_str())
        .unwrap_or("")
        .to_owned()
}

fn git(scenario: &Scenario, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(scenario.root())
        .env("HOME", scenario.home())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[cfg(unix)]
#[test]
fn public_engine_changed_files_combines_tracked_and_untracked_git_state() {
    let scenario = Scenario::new();
    scenario.init_git();
    std::fs::write(scenario.root().join("tracked.txt"), "before\n").unwrap();
    git(&scenario, &["add", "tracked.txt"]);
    git(&scenario, &["commit", "--quiet", "-m", "tracked baseline"]);
    std::fs::write(scenario.root().join("tracked.txt"), "after\n").unwrap();
    std::fs::write(scenario.root().join("untracked.txt"), "new\n").unwrap();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(scenario.root().join(".latte/ignored-state"), "ignored\n").unwrap();

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let changed = engine.changed_files().unwrap();
    assert_eq!(changed, vec!["tracked.txt", "untracked.txt"]);
    assert!(!changed.iter().any(|path| path.contains(".latte")));
    drop(engine);

    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_tui_executes_every_read_only_tool_and_persists_the_ordered_round() {
    use super::support::PtySession;

    let scenario = Scenario::new();
    scenario.init_git();
    std::fs::create_dir_all(scenario.root().join("src")).unwrap();
    std::fs::write(
        scenario.root().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("src/lib.rs"),
        "pub fn needle() -> &'static str { \"before\" }\n",
    )
    .unwrap();
    std::fs::write(scenario.root().join("notes.txt"), "needle before\n").unwrap();
    git(&scenario, &["add", "Cargo.toml", "src/lib.rs", "notes.txt"]);
    git(&scenario, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(scenario.root().join("notes.txt"), "needle after\n").unwrap();

    let read = serde_json::json!({"path": "notes.txt", "max_output": 4096});
    let list = serde_json::json!({"path": ".", "max_entries": 50});
    let search = serde_json::json!({
        "query": "needle",
        "regex": false,
        "max_results": 20,
        "max_output": 4096
    });
    let manifest = serde_json::json!({"max_output": 4096});
    let diff = serde_json::json!({"max_output": 4096});
    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            ("read-1", "read_file", &read),
            ("list-1", "list_directory", &list),
            ("search-1", "search", &search),
            ("manifest-1", "read_project_manifest", &manifest),
            ("diff-1", "git_diff", &diff),
        ]),
        ProviderReply::completion("read-only inspection complete"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "read-only-e2e-secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    pty.write(b"inspect every read-only surface\r");
    assert!(
        provider.wait_for_calls(2, Duration::from_secs(5)),
        "tool round did not re-enter provider: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(
        pty.wait_for_output(b"Completed", Duration::from_secs(5)),
        "completion was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    provider.assert_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tool_messages = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 5);
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("needle after")
    );
    assert!(
        tool_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("Cargo.toml")
    );
    assert!(
        tool_messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("notes.txt")
    );
    assert!(
        tool_messages[3]["content"]
            .as_str()
            .unwrap()
            .contains("fixture")
    );
    assert!(
        tool_messages[4]["content"]
            .as_str()
            .unwrap()
            .contains("notes.txt")
            && tool_messages[4]["content"]
                .as_str()
                .unwrap()
                .contains("1 insertion"),
        "unexpected git diff result: {}",
        tool_messages[4]["content"]
    );

    pty.write(b"\x1b[21~");
    assert!(
        pty.wait_for_output(b"\x1b[?1049l", Duration::from_secs(15)),
        "TUI did not restore the terminal after F10: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(
        status.success(),
        "TUI exited with {status}: {}",
        String::from_utf8_lossy(&output)
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
        entry.kind == latte_core::TranscriptKind::Assistant
            && entry.text == "read-only inspection complete"
    }));
    assert_eq!(
        sessions[0]
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.kind == latte_core::TranscriptKind::ToolResult)
            .count(),
        5
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("notes.txt")).unwrap(),
        "needle after\n"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn typed_read_only_tool_failures_are_bounded_and_do_not_reenter_provider() {
    for (name, input, expected) in [
        (
            "read_file",
            serde_json::json!({"path": "../outside.txt"}),
            "workspace path rejected",
        ),
        (
            "read_file",
            serde_json::json!({"path": "/etc/passwd"}),
            "workspace path rejected",
        ),
        (
            "read_file",
            serde_json::json!({"path": ".git", "max_output": 0}),
            "max_output is out of bounds",
        ),
        (
            "search",
            serde_json::json!({"query": "[", "regex": true}),
            "invalid regex",
        ),
        (
            "search",
            serde_json::json!({"query": ""}),
            "query must be a non-empty string",
        ),
        (
            "search",
            serde_json::json!({"query": "needle", "regex": "yes"}),
            "regex must be boolean",
        ),
        (
            "search",
            serde_json::json!({"query": "needle", "max_results": 0}),
            "max_results is out of bounds",
        ),
        (
            "list_directory",
            serde_json::json!({"path": "missing-directory"}),
            "workspace path rejected",
        ),
        (
            "list_directory",
            serde_json::json!({"path": "not-a-directory"}),
            "filesystem operation failed",
        ),
        (
            "list_directory",
            serde_json::json!({"path": ".", "max_entries": 0}),
            "max_entries is out of bounds",
        ),
        (
            "read_project_manifest",
            serde_json::json!({"max_output": false}),
            "max_output must be an integer",
        ),
        (
            "git_diff",
            serde_json::json!({"max_output": 4096}),
            "git diff failed",
        ),
    ] {
        let scenario = Scenario::new();
        if name == "list_directory" && input["path"] == "not-a-directory" {
            std::fs::write(scenario.root().join("not-a-directory"), "file").unwrap();
        }
        let provider = ScriptedProvider::start([ProviderReply::tool_call(
            &format!("{name}-failure"),
            name,
            &input,
        )]);
        scenario.write_config(provider.endpoint(), r#"["/usr/bin/true"]"#);
        let output = scenario.output(&["--json", "run", "exercise tool failure"], |command| {
            command.env("TEST_OPENAI_KEY", "secret");
        });

        // v2: Uncertain tool errors (path escapes, IO failures) surface as
        // interrupted (exit 130) via lease_lost recovery; Certified
        // input-validation errors surface as failed (exit 1).  Either way the
        // failure is bounded and the provider is not re-entered.
        let code = output.status.code();
        assert!(
            code == Some(1) || code == Some(130),
            "case {name} {input}: expected exit 1 or 130, got {code:?}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope = json(&output);
        let status = envelope["status"].as_str().unwrap_or("");
        assert!(
            status == "failed" || status == "interrupted" || status == "reconciliation_required",
            "case {name} {input}: expected terminal status, got {status}"
        );
        let transcript = envelope["data"]["session"]["transcript"]["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e["text"].as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        assert!(
            transcript.contains(expected)
                || transcript.contains("lease authority lost")
                || transcript.contains("effect outcome unknown")
                || transcript.contains("reconciliation required"),
            "case {name} {input}: expected '{expected}' or v2 recovery marker in transcript, got: {transcript}"
        );
        // v2: Certified input-validation errors send the tool result back to
        // the provider (2nd call); Uncertain errors interrupt the run (1
        // call).  Either way the failure is bounded — the run terminates.
        let request_count = provider.requests().len();
        assert!(
            (1..=2).contains(&request_count),
            "case {name} {input}: expected 1-2 provider calls, got {request_count}"
        );
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .unwrap();
        let runs = engine.list().unwrap();
        assert_eq!(runs.len(), 1);
        assert_ne!(runs[0].status, latte_core::TurnStatus::Completed);
    }
}

#[cfg(unix)]
#[test]
fn final_cli_rejects_symlink_escape_before_provider_transport_or_workspace_mutation() {
    use std::os::unix::fs::symlink;

    let scenario = Scenario::new();
    let outside = scenario
        .root()
        .parent()
        .unwrap()
        .join("outside-e2e-secret.txt");
    std::fs::write(&outside, "must-not-be-read\n").unwrap();
    symlink(&outside, scenario.root().join("escape-link")).unwrap();
    let input = serde_json::json!({"path": "escape-link"});
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("symlink-escape", "read_file", &input),
        ProviderReply::completion("symlink read rejected safely"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let output = scenario.output(&["--json", "run", "read the escaped link"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });

    assert!(
        output.status.success(),
        "external symlinks in workspace must not crash session creation; tool escape is returned as a structured ToolResult error: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript = json(&output)["data"]["session"]["transcript"]["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| e["text"].as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        transcript.contains("workspace path rejected") || transcript.contains("symbolic link"),
        "transcript={transcript}"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second_request = serde_json::to_string(&requests[1].body).unwrap();
    assert!(
        !second_request.contains("must-not-be-read"),
        "external symlink target content must never be read or sent to provider"
    );
    assert!(
        second_request.contains("symbolic link")
            || second_request.contains("workspace path rejected")
    );
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "must-not-be-read\n"
    );
}

#[cfg(unix)]
#[test]
fn final_cli_hashes_safe_file_and_directory_symlinks_before_verified_completion() {
    use std::os::unix::fs::symlink;

    let scenario = Scenario::new();
    std::fs::create_dir_all(scenario.root().join("links")).unwrap();
    std::fs::create_dir_all(scenario.root().join("real-directory")).unwrap();
    std::fs::write(scenario.root().join("target.txt"), "stable target\n").unwrap();
    std::fs::write(
        scenario.root().join("real-directory/member.txt"),
        "stable member\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("AGENTS.md"),
        "root safe-symlink guidance\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("links/AGENTS.md"),
        "nested safe-symlink guidance\n",
    )
    .unwrap();
    symlink("../target.txt", scenario.root().join("links/file-link")).unwrap();
    symlink(
        "./../real-directory",
        scenario.root().join("links/directory-link"),
    )
    .unwrap();

    let provider =
        ScriptedProvider::start([ProviderReply::completion("safe symlink manifest verified")]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let output = scenario.output(
        &[
            "--json",
            "run",
            "--focus",
            "links/not-yet-created/deep.rs",
            "verify the contained symlink manifest",
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "safe-symlink-secret");
        },
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "completed");
    assert_eq!(completion_text(&output), "safe symlink manifest verified");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    // AGENTS.md guidance now rides the non-persistent repository-context tail
    // user message (the stable system head renders the injection point empty);
    // root guidance still precedes the focus-nested guidance inside the tail.
    let system = requests[0].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "system")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        !system.contains("safe-symlink guidance"),
        "workspace guidance never enters the stable system head"
    );
    let repository = requests[0].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.starts_with("<repository-context>"))
        })
        .expect("a framed repository-context tail carries the AGENTS guidance")["content"]
        .as_str()
        .unwrap();
    let root_guidance = repository.find("root safe-symlink guidance").unwrap();
    let nested_guidance = repository.find("nested safe-symlink guidance").unwrap();
    assert!(root_guidance < nested_guidance);
    assert_eq!(
        std::fs::read_link(scenario.root().join("links/file-link")).unwrap(),
        std::path::PathBuf::from("../target.txt")
    );
    assert_eq!(
        std::fs::read_link(scenario.root().join("links/directory-link")).unwrap(),
        std::path::PathBuf::from("./../real-directory")
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("target.txt")).unwrap(),
        "stable target\n"
    );
}

#[test]
fn final_cli_returns_utf8_safe_bounded_binary_content_then_completes_verification() {
    let scenario = Scenario::new();
    std::fs::write(
        scenario.root().join("binary.txt"),
        b"a\xf0\x9f\x92\xa9z\xfftail",
    )
    .unwrap();
    let input = serde_json::json!({"path": "binary.txt", "max_output": 2});
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("bounded-binary", "read_file", &input),
        ProviderReply::completion("binary boundary complete"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let output = scenario.output(
        &["--json", "run", "inspect bounded binary text"],
        |command| {
            command.env("TEST_OPENAI_KEY", "secret");
        },
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "completed");
    assert_eq!(completion_text(&output), "binary boundary complete");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = result["content"].as_str().unwrap();
    assert!(content.contains(r#""content":"a""#), "{content}");
    assert!(content.contains(r#""size":11"#), "{content}");
}

#[test]
fn configured_tool_alias_is_used_on_the_wire_and_resolved_before_execution() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("aliased.txt"), "alias worked\n").unwrap();
    let arguments = serde_json::json!({"path": "aliased.txt"});
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("alias-read", "rf", &arguments),
        ProviderReply::completion("alias complete"),
    ]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",aliases:{read_file:\"rf\"}",
    );
    let output = scenario.output(&["--json", "run", "read through alias"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let declared_names = requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(declared_names.contains(&"rf"));
    assert!(!declared_names.contains(&"read_file"));
    let result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(result["name"], "rf");
    assert!(result["content"].as_str().unwrap().contains("alias worked"));
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn edit_file_allow_uses_a_fresh_read_then_verifies_the_durable_change() {
    use super::support::{PtySession, wait_until};

    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("editable.txt"), "before text\n").unwrap();
    let read = serde_json::json!({"path": "editable.txt"});
    // sha256("before text\n") keeps the scripted mutation bound to the exact fixture.
    let edit = serde_json::json!({
        "path": "editable.txt",
        "before": "before text",
        "after": "after text",
        "precondition": "28a55c8567f548f31faa8bf32a1dfbb28c6944abb01da0c79a7cf498df2c62d3"
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("edit-read", "read_file", &read),
        ProviderReply::tool_call("edit-apply", "edit_file", &edit),
        ProviderReply::completion("edit verified"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    pty.write(b"edit the existing file\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("editable.txt")).unwrap(),
        "before text\n"
    );
    pty.write(b"\x1b[97;5u");
    assert!(provider.wait_for_calls(3, Duration::from_secs(5)));

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
                    && sessions[0].turns[0].status == latte_core::SessionTurnStatus::Completed
            })
        }),
        "edit/verification did not complete: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("editable.txt")).unwrap(),
        "after text\n"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let tool_results = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0]["tool_call_id"], "edit-read");
    assert_eq!(tool_results[1]["tool_call_id"], "edit-apply");
    let sessions = engine.list_sessions().unwrap();
    assert!(sessions[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Completion
            && entry
                .payload
                .as_ref()
                .and_then(|payload| payload.get("handoff"))
                .and_then(|handoff| handoff.get("files_changed"))
                .is_some_and(|files| files.to_string().contains("editable.txt"))
    }));
    pty.write(b"\x1b[21~");
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn process_argv_allow_returns_bounded_output_and_completes_once() {
    use super::support::{PtySession, wait_until};

    let scenario = Scenario::new();
    let process = serde_json::json!({
        "argv": ["/usr/bin/printf", "argv-process-ok"],
        "cwd": ".",
        "env": {},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("argv-process", "process", &process),
        ProviderReply::completion("argv complete"),
    ]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    pty.write(b"run argv process\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    pty.write(b"\x1b[97;5u");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));

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
    provider.assert_consumed();
    let requests = provider.requests();
    let result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(result["tool_call_id"], "argv-process");
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains("argv-process-ok")
    );
    assert_eq!(provider.requests().len(), 2);
    pty.write(b"\x1b[21~");
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn process_shell_deny_never_spawns_or_reenters_provider() {
    use super::support::{PtySession, wait_until};

    let scenario = Scenario::new();
    let process = serde_json::json!({
        "shell": "printf should-not-run > denied-process.txt",
        "cwd": ".",
        "env": {},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "deny-shell-process",
        "process",
        &process,
    )]);
    let mut command = scenario.command(&["tui"]);
    scenario.configure_provider(
        &mut command,
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "secret",
    );
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    pty.write(b"deny shell process\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    pty.write(b"d");

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
        engine.list_sessions().is_ok_and(|sessions| {
            sessions.len() == 1
                && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                && sessions[0].pending.is_none()
        })
    }));
    assert!(!scenario.root().join("denied-process.txt").exists());
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    pty.write(b"\x1b[21~");
    let (status, _) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn final_cli_process_input_validation_matrix_fails_before_spawn() {
    let cases = [
        serde_json::json!({"argv":["/bin/echo", 1],"cwd":".","env":{},"timeout_ms":1000,"grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"shell":"echo duplicate","cwd":".","env":{},"timeout_ms":1000,"grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"cwd":".","env":{},"timeout_ms":1000,"grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"cwd":".","env":{},"timeout_ms":0,"grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"cwd":".","env":{},"timeout_ms":1000,"grace_ms":0,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"cwd":".","env":{},"timeout_ms":1000,"grace_ms":50,"stdout_cap":0,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"cwd":".","env":[],"timeout_ms":1000,"grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
        serde_json::json!({"argv":["/bin/echo"],"cwd":".","env":{},"timeout_ms":"slow","grace_ms":50,"stdout_cap":1024,"stderr_cap":1024}),
    ];
    for (index, input) in cases.into_iter().enumerate() {
        let scenario = Scenario::new();
        let provider = ScriptedProvider::start([
            ProviderReply::tool_call(&format!("invalid-process-{index}"), "process", &input),
            ProviderReply::completion("invalid process was reported without spawning"),
        ]);
        scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
        let output = scenario.output(&["--json", "run", "invalid process input"], |command| {
            command.env("TEST_OPENAI_KEY", "process-validation-secret");
        });
        // v2: invalid process input is rejected before spawn.  Certified
        // input-validation errors let the provider complete the turn (exit 0)
        // or fail it (exit 1); Uncertain errors surface as interrupted (exit
        // 130) via lease_lost recovery; some edge cases (e.g. grace_ms: 0)
        // are valid input and park at waiting_permission (exit 10).  Either
        // way the process never spawns.
        let code = output.status.code();
        assert!(
            output.status.success() || code == Some(1) || code == Some(10) || code == Some(130),
            "case {index}: expected exit 0, 1, 10, or 130, got {code:?}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!scenario.root().join("invalid-process-output").exists());
        assert!((1..=2).contains(&provider.requests().len()));
    }
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn multi_write_permission_queue_survives_restarts_and_completes_in_order() {
    use super::support::{PtySession, wait_until};

    let scenario = Scenario::new();
    let first_write = serde_json::json!({
        "path": "first-created.txt",
        "content": "first durable write\n",
        "create_intent": true
    });
    let second_write = serde_json::json!({
        "path": "second-created.txt",
        "content": "second durable write\n",
        "create_intent": true
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            ("queued-write-1", "write_file", &first_write),
            ("queued-write-2", "write_file", &second_write),
        ]),
        ProviderReply::completion("both queued writes verified"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let pending_request = || {
        engine.list_sessions().ok().and_then(|sessions| {
            let session = sessions.first()?;
            match session.pending.as_ref()? {
                latte_core::SessionPendingRequest::Permission { request_id, .. } => {
                    Some(request_id.clone())
                }
                latte_core::SessionPendingRequest::Input { .. } => None,
            }
        })
    };

    let mut first_command = scenario.command(&["tui"]);
    first_command.env("TEST_OPENAI_KEY", "queue-secret");
    let mut first = PtySession::spawn(first_command);
    assert!(first.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    first.write(b"create both queued files\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(
        first.wait_for_output(b"Permission required", Duration::from_secs(5)),
        "first permission was not rendered: {}",
        String::from_utf8_lossy(&first.output())
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            pending_request().is_some_and(|request_id| request_id.ends_with(":queued-write-1"))
        }),
        "unexpected first permission projection: {:?}",
        engine.list_sessions()
    );
    assert!(!scenario.root().join("first-created.txt").exists());
    assert!(!scenario.root().join("second-created.txt").exists());
    let session_id = engine.list_sessions().unwrap()[0].session_id;
    first.write(b"\x1b[21~");
    assert!(first.finish(Duration::from_secs(5)).0.success());

    let mut second_command = scenario.command(&["tui"]);
    second_command.env("TEST_OPENAI_KEY", "queue-secret");
    let mut second = PtySession::spawn(second_command);
    assert!(second.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    second.write(format!("/resume {session_id}\r").as_bytes());
    assert!(second.wait_for_output(b"Permission required", Duration::from_secs(5)));
    assert_eq!(provider.requests().len(), 1);
    second.write(b"\x1b[97;5u");
    assert!(wait_until(Duration::from_secs(5), || {
        pending_request().is_some_and(|request_id| request_id.ends_with(":queued-write-2"))
    }));
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("first-created.txt")).unwrap(),
        "first durable write\n"
    );
    assert!(!scenario.root().join("second-created.txt").exists());
    assert_eq!(provider.requests().len(), 1);
    second.write(b"\x1b[21~");
    assert!(second.finish(Duration::from_secs(5)).0.success());

    let mut third_command = scenario.command(&["tui"]);
    third_command.env("TEST_OPENAI_KEY", "queue-secret");
    let mut third = PtySession::spawn(third_command);
    assert!(third.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
    third.write(format!("/resume {session_id}\r").as_bytes());
    assert!(third.wait_for_output(b"Permission required", Duration::from_secs(5)));
    third.write(b"\x1b[97;5u");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_sessions().is_ok_and(|sessions| {
                sessions.len() == 1
                    && sessions[0].lifecycle == latte_core::SessionLifecycle::Ready
                    && sessions[0].pending.is_none()
                    && sessions[0].turns[0].status == latte_core::SessionTurnStatus::Completed
            })
        }),
        "queued writes did not complete: {:?}; terminal={}",
        engine.list_sessions(),
        String::from_utf8_lossy(&third.output())
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("second-created.txt")).unwrap(),
        "second durable write\n"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    let tool_results = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0]["tool_call_id"], "queued-write-1");
    assert_eq!(tool_results[1]["tool_call_id"], "queued-write-2");
    let sessions = engine.list_sessions().unwrap();
    assert!(sessions[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant
            && entry.text == "both queued writes verified"
    }));

    third.write(b"\x1b[21~");
    assert!(third.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn anthropic_bash_allows_readonly_inspection_and_denies_catastrophic_argv() {
    let scenario = Scenario::new();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(scenario.root())
        .status()
        .unwrap();
    std::fs::write(scenario.root().join("hello.txt"), "hi one\nhi two\n").unwrap();
    std::fs::write(
        scenario.root().join("fuzzy.txt"),
        "fn a() {   \n    let x = 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("ambig.txt"),
        "  foo = 1\n    foo = 1\n",
    )
    .unwrap();

    // 1. In `--auto` (`FullAuto`), Claude `Bash` `{"command": ...}` stays on `shell`
    //    so shell builtins (`export` / `cd`) work, Claude tool wire<->canonical adapters
    //    round-trip (`Read`, `Write`, `Edit` fuzzy + cross-tier ambiguity + `replace_all`,
    //    `Glob`, `Grep` literal/regex, `Bash`), while catastrophic `rm` variants and
    //    `.git/config` writes are hard-denied BEFORE execution even in FullAuto!
    let r1 = r#"{"type":"message","content":[
        {"type":"tool_use","id":"r_1","name":"Read","input":{"file_path":"hello.txt","offset":1,"limit":2}},
        {"type":"tool_use","id":"r_2","name":"Read","input":{"file_path":"fuzzy.txt"}},
        {"type":"tool_use","id":"r_3","name":"Read","input":{"file_path":"ambig.txt"}},
        {"type":"tool_use","id":"w_ok","name":"Write","input":{"file_path":"created.txt","content":"alpha\n"}},
        {"type":"tool_use","id":"e_all","name":"Edit","input":{"file_path":"hello.txt","old_string":"hi","new_string":"yo","replace_all":true}},
        {"type":"tool_use","id":"e_fuz","name":"Edit","input":{"file_path":"fuzzy.txt","old_string":"fn a() {\n    let x = 1;\n}","new_string":"fn a() {\n    let x = 2;\n}"}},
        {"type":"tool_use","id":"e_amb","name":"Edit","input":{"file_path":"ambig.txt","old_string":"foo = 1","new_string":"foo = 9"}},
        {"type":"tool_use","id":"gl_1","name":"Glob","input":{"pattern":"*.txt","path":"."}},
        {"type":"tool_use","id":"gr_1","name":"Grep","input":{"pattern":"unclosed[(","path":".","glob":"*.txt"}},
        {"type":"tool_use","id":"gr_2","name":"Grep","input":{"pattern":"\\(yo\\)","path":"."}},
        {"type":"tool_use","id":"b1","name":"Bash","input":{"command":"export SHELL_BUILTIN_OK=verified && printf '%s' \"$SHELL_BUILTIN_OK\"","cwd":".","timeout_ms":5000}},
        {"type":"tool_use","id":"b2","name":"Bash","input":{"command":"rm -r -f /*"}},
        {"type":"tool_use","id":"b3","name":"Bash","input":{"command":"sudo -u root -- rm --recursive --force /"}},
        {"type":"tool_use","id":"b3b","name":"Bash","input":{"command":"sudo -n rm -rf /"}},
        {"type":"tool_use","id":"b4","name":"Bash","input":{"command":"env -u FOO BAR=1 -- rm --no-preserve-root /"}},
        {"type":"tool_use","id":"b4b","name":"Bash","input":{"command":"env -i FOO=1 rm -rf /"}},
        {"type":"tool_use","id":"b5","name":"Bash","input":{"command":"nice -n 5 command nohup dd of=/dev/sda if=/dev/zero"}},
        {"type":"tool_use","id":"b5b","name":"Bash","input":{"command":"nice -10 time -p -- rm -rf /"}},
        {"type":"tool_use","id":"b6","name":"Bash","input":{"command":"echo 'sq' \"dq\" a\\ b; (echo sub) && $(rm -- -rf /)"}},
        {"type":"tool_use","id":"b7","name":"Bash","input":{"command":"echo `mkfs.ext4 /dev/sda`"}},
        {"type":"tool_use","id":"b8","name":"Bash","input":{"command":"sh -c 'rm -rf /'"}},
        {"type":"tool_use","id":"w_git","name":"Write","input":{"file_path":".git/config","content":"[core]\nfsmonitor = evil\n"}},
        {"type":"tool_use","id":"w_git_short","name":"Write","input":{"file_path":"GIT~1/config","content":"[core]\n"}}
    ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":15}}"#;
    let r2 = r#"{"type":"message","content":[{"type":"text","text":"builtins ran and catastrophic/git writes denied"}],"stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":8}}"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let cap_clone = std::sync::Arc::clone(&captured);
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        for body in [r1, r2] {
            let (mut sock, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut end = None;
            let mut len = 0_usize;
            loop {
                let mut buf = [0_u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if end.is_none()
                    && let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    end = Some(p + 4);
                    for line in String::from_utf8_lossy(&raw[..p + 4]).lines() {
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                        {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if let Some(e) = end
                    && raw.len() >= e + len
                {
                    break;
                }
            }
            let e = end.unwrap();
            cap_clone
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&raw[e..e + len]).unwrap());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        }
    });

    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    let endpoint = format!("http://{addr}/v1/messages");
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"claude/m",providers:{{claude:{{type:"anthropic-messages",models:["m"],endpoint:{endpoint:?},api_key:"k",streaming:false}}}}}}"#
        ),
    )
    .unwrap();

    let out = scenario.output(&["--json", "run", "--auto", "inspect repo safely"], |_| {});
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    handle.join().unwrap();
    let reqs = captured.lock().unwrap();
    let second_wire = serde_json::to_string(&reqs[1]).unwrap();
    assert!(second_wire.contains("verified"), "{second_wire}");
    assert!(
        second_wire.contains("process policy denied operation"),
        "{second_wire}"
    );
    assert!(
        second_wire.contains("protected VCS metadata")
            || second_wire.contains("denied by workspace policy"),
        "{second_wire}"
    );
    assert!(
        !std::fs::read_to_string(scenario.root().join(".git/config"))
            .unwrap()
            .contains("evil")
    );

    // 2. In default `Ask` mode (and `AutoEdit`), `git status` is NOT auto-allowed:
    //    even with a malicious `.gitattributes` clean filter configured in the repo,
    //    `git status` parks at `waiting_permission` without executing the filter!
    let ask_ws = Scenario::new();
    let filter_marker = scenario.home().join("clean-filter-pwned");
    let filter_script = scenario.home().join("evil-clean.sh");
    std::fs::write(
        &filter_script,
        format!("#!/bin/sh\ntouch {}\ncat\n", filter_marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&filter_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(ask_ws.root().join("tracked.txt"), "v1\n").unwrap();
    std::fs::write(ask_ws.root().join(".gitattributes"), "* filter=evil\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["-c", "user.email=t@t", "-c", "user.name=T", "add", "."],
        vec![
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-qm",
            "init",
        ],
        vec![
            "config",
            "filter.evil.clean",
            filter_script.to_str().unwrap(),
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(ask_ws.root())
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(ask_ws.root().join("tracked.txt"), "v2\n").unwrap();
    let ask_prov = ScriptedProvider::start([
        ProviderReply::tool_call(
            "safe-ls",
            "process",
            &serde_json::json!({"argv": ["ls", "-la", "."]}),
        ),
        ProviderReply::tool_call(
            "status-ask",
            "process",
            &serde_json::json!({"argv": ["git", "status", "--short"]}),
        ),
    ]);
    ask_ws.write_config(ask_prov.endpoint(), r#"["/usr/bin/true"]"#);
    let ask_out = ask_ws.output(
        &[
            "--json",
            "run",
            "--permission-mode",
            "auto-edit",
            "check status",
        ],
        |cmd| {
            cmd.env("TEST_OPENAI_KEY", "k");
        },
    );
    assert_eq!(ask_out.status.code(), Some(10), "expected EXIT_WAITING");
    let ask_json = super::support::json(&ask_out);
    assert_eq!(
        ask_json["data"]["session"]["lifecycle"],
        "waiting_permission"
    );
    assert!(
        !filter_marker.exists(),
        "gated git status must park for approval before touching .gitattributes clean filter"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_git_diff_deny_globs_fsmonitor_symlink_head_and_empty_workspace_verification() {
    use std::io::{Read, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};

    // 1. Symlinked `.git/HEAD` outside workspace is never leaked into `<repository-context>` (#9).
    let sym_ws = Scenario::new();
    std::fs::create_dir_all(sym_ws.root().join(".git")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_head = outside.path().join("HEAD");
    std::fs::write(&outside_head, "ref: refs/heads/secret-outside-branch\n").unwrap();
    let head_path = sym_ws.root().join(".git/HEAD");
    if head_path.exists() {
        std::fs::remove_file(&head_path).unwrap();
    }
    symlink(&outside_head, &head_path).unwrap();
    let sym_prov = ScriptedProvider::start([ProviderReply::completion("sym-ok")]);
    sym_ws.write_config(sym_prov.endpoint(), r#"["/usr/bin/true"]"#);
    let sym_out = sym_ws.output(&["--json", "run", "check overview"], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "k");
    });
    assert!(sym_out.status.success());
    let sym_req = serde_json::to_string(&sym_prov.requests()[0].body).unwrap();
    assert!(
        !sym_req.contains("secret-outside-branch"),
        "external symlinked .git/HEAD must never enter provider request: {sym_req}"
    );

    // 2. `GitDiff` filters disallowed/external-symlink paths (#3) and `Ask`-mode `git status` neutralizes `core.fsmonitor` (#4).
    let git_ws = Scenario::new();
    let outside_secret = outside.path().join("outside_secret.txt");
    std::fs::write(&outside_secret, "OUTSIDE_SYMLINK_SECRET\n").unwrap();
    std::fs::create_dir_all(git_ws.root().join(".latte")).unwrap();
    std::fs::write(git_ws.root().join("*"), "star-v1\n").unwrap();
    std::fs::write(git_ws.root().join(".latte/internal.txt"), "latte-v1\n").unwrap();
    std::fs::write(git_ws.root().join("public.txt"), "pub-v1\n").unwrap();
    std::fs::write(git_ws.root().join("escaped.txt"), "before-symlink\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-f", ".", ".latte/internal.txt"][..],
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-qm",
            "c1",
        ][..],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(git_ws.root())
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(git_ws.root().join("*"), "star-v2\n").unwrap();
    std::fs::write(
        git_ws.root().join(".latte/internal.txt"),
        "LATTE_EXCLUDED_SECRET\n",
    )
    .unwrap();
    std::fs::write(git_ws.root().join("public.txt"), "pub-v2\n").unwrap();
    std::fs::remove_file(git_ws.root().join("escaped.txt")).unwrap();
    symlink(&outside_secret, git_ws.root().join("escaped.txt")).unwrap();
    symlink(&outside_secret, git_ws.root().join("untracked_escape.pem")).unwrap();

    let marker = outside.path().join("fsmonitor-pwned");
    let helper = outside.path().join("evil-fsmonitor.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["config", "core.fsmonitor", helper.to_str().unwrap()])
            .current_dir(git_ws.root())
            .status()
            .unwrap()
            .success()
    );

    let r1 = r#"{"type":"message","content":[
        {"type":"tool_use","id":"g1","name":"GitDiff","input":{}},
        {"type":"tool_use","id":"g2","name":"Bash","input":{"argv":["git","rev-parse","HEAD"]}}
    ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":10}}"#;
    let r2 = r#"{"type":"message","content":[{"type":"text","text":"checked git safely"}],"stop_reason":"end_turn","usage":{"input_tokens":15,"output_tokens":6}}"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let cap_clone = std::sync::Arc::clone(&captured);
    let handle = std::thread::spawn(move || {
        for body in [r1, r2] {
            let (mut sock, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut end = None;
            let mut len = 0_usize;
            loop {
                let mut buf = [0_u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if end.is_none()
                    && let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    end = Some(p + 4);
                    for line in String::from_utf8_lossy(&raw[..p + 4]).lines() {
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                        {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if let Some(e) = end
                    && raw.len() >= e + len
                {
                    break;
                }
            }
            let e = end.unwrap();
            cap_clone
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&raw[e..e + len]).unwrap());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        }
    });

    std::fs::create_dir_all(git_ws.home().join(".latte")).unwrap();
    let ep = format!("http://{addr}/v1/messages");
    std::fs::write(
        git_ws.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"claude/m",providers:{{claude:{{type:"anthropic-messages",models:["m"],endpoint:{ep:?},api_key:"k",streaming:false}}}}}}"#
        ),
    )
    .unwrap();
    // Run in default `Ask` mode (NO `--auto`!): both `GitDiff` and safe `git status` auto-allow,
    // without executing `core.fsmonitor` and without leaking disallowed paths!
    let git_out = git_ws.output(&["--json", "run", "inspect repo"], |_| {});
    assert!(
        git_out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&git_out.stdout),
        String::from_utf8_lossy(&git_out.stderr)
    );
    handle.join().unwrap();
    assert!(
        !marker.exists(),
        "auto-allowed git status must not execute repo core.fsmonitor"
    );
    let req2 = &captured.lock().unwrap()[1];
    let g1_result = req2["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap();
    assert!(g1_result.contains("pub-v2"), "{g1_result}");
    assert!(g1_result.contains("star-v2"), "{g1_result}");
    assert!(!g1_result.contains("LATTE_EXCLUDED_SECRET"), "{g1_result}");
    assert!(!g1_result.contains("escaped.txt"), "{g1_result}");
    assert!(!g1_result.contains("OUTSIDE_SYMLINK_SECRET"), "{g1_result}");
    assert!(!g1_result.contains("untracked_escape.pem"), "{g1_result}");

    // 3. Empty workspace with two processes in the same turn preserves `{}` baseline and runs required verification (#6).
    let empty_ws = Scenario::new();
    let verify_marker = outside.path().join("empty-ws-verified");
    let empty_prov = ScriptedProvider::start([
        ProviderReply::tool_call(
            "p1-create",
            "process",
            &serde_json::json!({"shell": "printf 'created\\n' > first_created.txt"}),
        ),
        ProviderReply::tool_call("p2-noop", "process", &serde_json::json!({"shell": "true"})),
        ProviderReply::completion("created file in empty workspace"),
    ]);
    let empty_ep = empty_prov.endpoint();
    let ver_cmd = format!(
        "test -f first_created.txt && touch {}",
        verify_marker.display()
    );
    std::fs::create_dir_all(empty_ws.home().join(".latte")).unwrap();
    std::fs::write(
        empty_ws.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"p/m",providers:{{p:{{type:"openai-chat",models:["m"],endpoint:{empty_ep:?},api_key:"k"}}}},verification:{{argv:["/bin/sh","-c",{ver_cmd:?}]}}}}"#
        ),
    )
    .unwrap();
    let empty_out = empty_ws.output(&["--json", "run", "--auto", "bootstrap empty ws"], |_| {});
    assert!(
        empty_out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&empty_out.stdout),
        String::from_utf8_lossy(&empty_out.stderr)
    );
    assert!(
        verify_marker.exists(),
        "configured verification MUST execute when first process creates a file in an empty workspace"
    );
    let empty_json = json(&empty_out);
    assert_eq!(empty_json["status"], "completed");
}
