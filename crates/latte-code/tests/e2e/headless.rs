use super::support::{
    ProviderReply, Scenario, ScriptedProvider, assert_secret_absent, isolated_output, json,
};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};

#[test]
fn help_list_show_and_usage_have_stable_envelopes_and_exits() {
    let help = isolated_output(&["--help"], |_, _| {});
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("latte-code [--json] run"));

    let json_help = isolated_output(&["--json", "--help"], |_, _| {});
    assert!(json_help.status.success());
    assert_eq!(json(&json_help)["status"], "completed");
    assert_eq!(json(&json_help)["version"], 1);

    let list = isolated_output(&["--json", "list"], |_, _| {});
    assert!(list.status.success());
    assert_eq!(json(&list)["data"]["runs"], serde_json::json!([]));

    let missing = isolated_output(
        &["--json", "show", "01900000-0000-7000-8000-000000000001"],
        |scenario, _| {
            scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
        },
    );
    assert_eq!(missing.status.code(), Some(4));
    assert_eq!(json(&missing)["error"]["code"], "run_not_found");
    let missing_text = isolated_output(
        &["show", "01900000-0000-7000-8000-000000000002"],
        |scenario, _| {
            scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
        },
    );
    assert_eq!(missing_text.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&missing_text.stderr).contains("was not found"));

    let invalid = isolated_output(&["--json", "show", "not-a-run"], |_, _| {});
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(json(&invalid)["error"]["code"], "usage");

    let unknown = isolated_output(&["wat"], |_, _| {});
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("expected:"));
}

#[test]
fn text_mode_run_show_and_list_preserve_the_user_visible_summary() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("plain-text handoff")]);
    let run = scenario.output(&["run", "complete in text mode"], |command| {
        scenario.configure_provider(
            command,
            provider.endpoint(),
            r#"["/bin/pwd"]"#,
            "latte-e2e-text-mode-key",
        );
    });
    assert!(run.status.success());
    let rendered = String::from_utf8(run.stdout).unwrap();
    assert!(rendered.contains(": Completed (revision "));
    assert!(rendered.contains("plain-text handoff"));
    let run_id = rendered
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|value| value.trim_end_matches(':'))
        .unwrap();

    let shown = scenario.output(&["show", run_id], |_| {});
    assert!(shown.status.success());
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains(&format!("run {run_id}: Completed")));
    assert!(shown.contains("plain-text handoff"));

    let listed = scenario.output(&["list"], |_| {});
    assert!(listed.status.success());
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains(run_id));
    assert!(listed.contains("Completed"));
    provider.assert_consumed();
}

#[test]
fn layered_configuration_rejects_non_objects_unknown_keys_and_invalid_thread_budgets() {
    for (config, expected) in [
        ("[]", "top-level configuration must be an object"),
        ("{unknown_key:true}", "unknown field `unknown_key`"),
        (
            "{thread:{max_input_bytes:5,reserved_output_bytes:5}}",
            "reserved output must be smaller than input budget",
        ),
    ] {
        let output = isolated_output(&["--json", "list"], |scenario, _| {
            std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
            std::fs::write(scenario.root().join(".latte/latte-code.jsonc"), config).unwrap();
        });
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(json(&output)["error"]["code"], "configuration");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let parentless_database = isolated_output(&["--json", "list"], |scenario, _| {
        scenario.write_config_with_database("http://127.0.0.1:1", r#"["/usr/bin/true"]"#, "/");
    });
    assert!(parentless_database.status.success());
}

#[test]
fn filesystem_startup_failures_are_typed_before_command_execution() {
    for invalid_home in ["", "relative-storage-home"] {
        let output = isolated_output(&["--json", "list"], |_, command| {
            command.env("LATTE_CODE_HOME", invalid_home);
        });
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(json(&output)["error"]["code"], "configuration");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("LATTE_CODE_HOME"))
        );
    }

    let unreadable_config = isolated_output(&["--json", "list"], |scenario, _| {
        std::fs::create_dir_all(scenario.root().join(".latte/latte-code.jsonc")).unwrap();
    });
    assert_eq!(unreadable_config.status.code(), Some(2));
    assert_eq!(json(&unreadable_config)["error"]["code"], "configuration");
    assert!(
        json(&unreadable_config)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read")
    );

    let blocked_database_parent = isolated_output(&["--json", "list"], |scenario, command| {
        std::fs::write(scenario.root().join("blocked-parent"), "not a directory").unwrap();
        command.env("LATTE_CODE_HOME", scenario.root().join("blocked-parent"));
    });
    assert_eq!(blocked_database_parent.status.code(), Some(70));
    assert_eq!(
        json(&blocked_database_parent)["error"]["code"],
        "database_directory"
    );
    assert!(
        json(&blocked_database_parent)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot create")
    );

    let database_is_directory = isolated_output(&["--json", "list"], |scenario, command| {
        let storage_home = scenario.root().join("database-directory");
        std::fs::create_dir(&storage_home).unwrap();
        std::fs::create_dir(storage_home.join("state.db")).unwrap();
        command.env("LATTE_CODE_HOME", storage_home);
    });
    assert_eq!(database_is_directory.status.code(), Some(70));
    assert_eq!(
        json(&database_is_directory)["error"]["code"],
        "engine_initialization"
    );
    assert!(
        json(&database_is_directory)["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );

    let invalid_legacy_database = isolated_output(&["--json", "list"], |scenario, _| {
        std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
        std::fs::write(
            scenario.root().join(".latte/latte-code.db"),
            "not a SQLite database",
        )
        .unwrap();
    });
    assert_eq!(invalid_legacy_database.status.code(), Some(70));
    assert_eq!(
        json(&invalid_legacy_database)["error"]["code"],
        "legacy_import"
    );
}

#[test]
fn nested_build_directory_discovers_workspace_and_uses_defaults() {
    let scenario = Scenario::new();
    let nested = scenario.root().join("target/debug");
    std::fs::create_dir_all(&nested).unwrap();
    let output = scenario.output(&["--json", "list"], |command| {
        command.current_dir(&nested);
    });

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(scenario.database_path().exists());
    assert!(!nested.join(".latte/latte-code.db").exists());
}

#[test]
fn home_and_workspace_configuration_precedence_reaches_the_final_binary() {
    let scenario = Scenario::new();
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        r#"{version:1,database:{path:"state/from-home.db"}}"#,
    )
    .unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        r#"{version:1,database:{path:"state/from-workspace.db"}}"#,
    )
    .unwrap();

    let output = scenario.output(&["--json", "list"], |_| {});
    assert!(output.status.success());
    assert!(scenario.database_path().exists());
    assert!(!scenario.root().join("state/from-workspace.db").exists());
    assert!(!scenario.root().join("state/from-home.db").exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn configuration_and_provider_failures_are_typed_and_do_not_leak_secrets() {
    let missing = isolated_output(&["--json", "run", "do work"], |scenario, _| {
        scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    });
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["error"]["code"], "configuration");
    assert!(
        json(&missing)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TEST_OPENAI_KEY")
    );

    let missing_deny = isolated_output(
        &[
            "--json",
            "resume",
            "01900000-0000-7000-8000-000000000001",
            "--deny",
        ],
        |_, _| {},
    );
    assert_eq!(missing_deny.status.code(), Some(4));
    assert_eq!(json(&missing_deny)["error"]["code"], "run_not_found");

    let empty_database = isolated_output(&["--json", "list"], |scenario, _| {
        scenario.write_config_with_database("http://127.0.0.1:1", r#"["/usr/bin/true"]"#, "  ");
    });
    assert!(empty_database.status.success());

    let invalid_verification =
        isolated_output(&["--json", "run", "do work"], |scenario, command| {
            scenario.write_config("http://127.0.0.1:1", "not-json");
            command.env("TEST_OPENAI_KEY", "secret");
        });
    assert_eq!(invalid_verification.status.code(), Some(2));
    assert!(
        json(&invalid_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid JSONC")
    );

    let empty_verification = isolated_output(&["--json", "run", "do work"], |scenario, command| {
        scenario.write_config("http://127.0.0.1:1", "[]");
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(empty_verification.status.code(), Some(2));
    assert!(
        json(&empty_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must not be empty")
    );

    let scenario = Scenario::new();
    let secret = "latte-e2e-secret-transport-sentinel";
    let transport = scenario.output(&["--json", "run", "do work"], |command| {
        scenario.configure_provider(command, "http://127.0.0.1:1", r#"["/bin/pwd"]"#, secret);
    });
    assert_eq!(transport.status.code(), Some(1));
    assert_eq!(json(&transport)["status"], "failed");
    assert_eq!(json(&transport)["data"]["run"]["status"], "failed");
    let database = std::fs::read(scenario.database_path()).unwrap();
    assert_secret_absent(
        secret,
        &[
            ("stdout", &transport.stdout),
            ("stderr", &transport.stderr),
            ("database", &database),
        ],
    );

    let missing_name = "TEST_OPENAI_KEY";
    let output = isolated_output(&["--json", "run", "do work"], |scenario, _| {
        scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    });
    assert_eq!(output.status.code(), Some(2));
    assert!(
        json(&output)["error"]["message"]
            .as_str()
            .unwrap()
            .contains(missing_name)
    );
}

#[test]
fn scripted_provider_read_only_completion_checks_the_production_wire_contract() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("existing.txt"), "unchanged\n").unwrap();
    let provider = ScriptedProvider::start([ProviderReply::completion("done")]);
    let secret = "latte-e2e-secret-readonly-sentinel";
    let output = scenario.output(&["--json", "run", "inspect the workspace"], |command| {
        scenario.configure_provider(command, provider.endpoint(), r#"["/bin/pwd"]"#, secret);
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "completed");
    assert!(json(&output)["data"]["run"]["pending_permission"].is_null());
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("existing.txt")).unwrap(),
        "unchanged\n"
    );
    assert!(provider.wait_for_calls(1, Duration::from_secs(1)));
    provider.assert_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        &format!("Bearer {secret}")
    );
    assert_eq!(request.body["model"], "mock");
    assert!(
        request.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("inspect the workspace"))
            })
    );
    let tool_names = request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        tool_names,
        [
            "edit_file",
            "git_diff",
            "list_directory",
            "process",
            "read_file",
            "read_project_manifest",
            "search",
            "write_file",
        ]
        .into_iter()
        .collect()
    );
    for tool in request.body["tools"].as_array().unwrap() {
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }
    let database = std::fs::read(scenario.database_path()).unwrap();
    assert_secret_absent(
        secret,
        &[
            ("stdout", &output.stdout),
            ("stderr", &output.stderr),
            ("database", &database),
        ],
    );
}

#[test]
fn read_only_tool_loop_redacts_file_secrets_before_provider_reentry() {
    let scenario = Scenario::new();
    let file_secret = "sk-latte-e2e-file-secret-sentinel";
    let file_secret_line = format!("OPENAI_API_KEY={file_secret}");
    std::fs::write(
        scenario.root().join("secrets.txt"),
        format!("public context\n{file_secret_line}\n"),
    )
    .unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "read-secret-file",
            "read_file",
            &serde_json::json!({"path": "secrets.txt"}),
        ),
        ProviderReply::completion("done"),
    ]);
    let output = scenario.output(
        &["--json", "run", "inspect secrets.txt safely"],
        |command| {
            scenario.configure_provider(
                command,
                provider.endpoint(),
                r#"["/bin/pwd"]"#,
                "latte-e2e-auth-key",
            );
        },
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "completed");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_tool_result_reached_provider(&requests[1].body);
    let tool_messages = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 1);
    assert!(!tool_messages[0]["content"].as_str().unwrap().is_empty());
    let provider_body = serde_json::to_vec(&requests[1].body).unwrap();
    let database = std::fs::read(scenario.database_path()).unwrap();
    assert_secret_absent(
        file_secret,
        &[
            ("stdout", &output.stdout),
            ("stderr", &output.stderr),
            ("provider request body", &provider_body),
            ("database", &database),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("secrets.txt")).unwrap(),
        format!("public context\n{file_secret_line}\n")
    );
}

pub(super) fn write_file_reply(id: &str) -> ProviderReply {
    ProviderReply::tool_call(
        id,
        "write_file",
        &serde_json::json!({
            "path": "new.txt",
            "content": "created by e2e\n",
            "create_intent": true
        }),
    )
}

pub(super) fn assert_tool_result_reached_provider(request: &Value) {
    assert!(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "tool" && message["tool_call_id"].as_str().is_some()
            })
    );
}
