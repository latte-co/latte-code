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
    assert!(scenario.root().join(".latte/latte-code.db").exists());
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
    assert!(scenario.root().join("state/from-workspace.db").exists());
    assert!(!scenario.root().join("state/from-home.db").exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn configuration_and_provider_failures_are_typed_and_do_not_leak_secrets() {
    let missing = isolated_output(&["--json", "run", "do work"], |_, _| {});
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["error"]["code"], "configuration");
    assert!(
        json(&missing)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OPENAI_API_KEY")
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
    assert_eq!(empty_database.status.code(), Some(2));
    assert!(
        json(&empty_database)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("database.path must not be empty")
    );

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
