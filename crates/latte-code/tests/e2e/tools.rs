use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use std::{process::Command, time::Duration};

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
    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].lifecycle, latte_core::ThreadLifecycle::Ready);
    assert!(threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant
            && entry.text == "read-only inspection complete"
    }));
    assert_eq!(
        threads[0]
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

        assert_eq!(
            output.status.code(),
            Some(1),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{}",
            json(&output)
        );
        provider.assert_consumed();
        assert_eq!(provider.requests().len(), 1);
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .unwrap();
        let runs = engine.list().unwrap();
        assert_eq!(runs.len(), 1);
        assert_ne!(runs[0].status, latte_core::RunStatus::Completed);
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
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "symlink-escape",
        "read_file",
        &input,
    )]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let output = scenario.output(&["--json", "run", "read the escaped link"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });

    assert_eq!(output.status.code(), Some(1));
    let error = json(&output)["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("workspace path rejected") || error.contains("symbolic link"),
        "{error}"
    );
    assert!(provider.requests().is_empty());
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
    assert_eq!(
        json(&output)["data"]["run"]["handoff"]["summary"],
        "safe symlink manifest verified"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let system = requests[0].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "system")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let root_guidance = system.find("root safe-symlink guidance").unwrap();
    let nested_guidance = system.find("nested safe-symlink guidance").unwrap();
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
    assert_eq!(
        json(&output)["data"]["run"]["handoff"]["summary"],
        "binary boundary complete"
    );
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
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].runs[0].status == latte_core::ThreadRunStatus::Completed
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
    let threads = engine.list_threads_v2().unwrap();
    assert!(threads[0].transcript.entries.iter().any(|entry| {
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
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1 && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
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
        engine.list_threads_v2().is_ok_and(|threads| {
            threads.len() == 1
                && threads[0].lifecycle == latte_core::ThreadLifecycle::Failed
                && threads[0].pending.is_none()
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

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let pending_request = || {
        engine.list_threads_v2().ok().and_then(|threads| {
            let thread = threads.first()?;
            match thread.pending.as_ref()? {
                latte_core::ThreadPendingRequest::Permission { request_id, .. } => {
                    Some(request_id.clone())
                }
                latte_core::ThreadPendingRequest::Input { .. } => None,
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
        engine.list_threads_v2()
    );
    assert!(!scenario.root().join("first-created.txt").exists());
    assert!(!scenario.root().join("second-created.txt").exists());
    first.write(b"\x1b[21~");
    assert!(first.finish(Duration::from_secs(5)).0.success());

    let mut second_command = scenario.command(&["tui"]);
    second_command.env("TEST_OPENAI_KEY", "queue-secret");
    let mut second = PtySession::spawn(second_command);
    assert!(second.wait_for_output(b"\x1b[>3u", Duration::from_secs(5)));
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
    assert!(third.wait_for_output(b"Permission required", Duration::from_secs(5)));
    third.write(b"\x1b[97;5u");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Ready
                    && threads[0].pending.is_none()
                    && threads[0].runs[0].status == latte_core::ThreadRunStatus::Completed
            })
        }),
        "queued writes did not complete: {:?}; terminal={}",
        engine.list_threads_v2(),
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
    let threads = engine.list_threads_v2().unwrap();
    assert!(threads[0].transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Assistant
            && entry.text == "both queued writes verified"
    }));

    third.write(b"\x1b[21~");
    assert!(third.finish(Duration::from_secs(5)).0.success());
}
