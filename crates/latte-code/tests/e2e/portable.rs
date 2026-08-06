use super::support::{ProviderReply, Scenario, ScriptedProvider, assert_secret_absent, json};

fn run_id(output: &std::process::Output) -> String {
    json(output)["data"]["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn waiting_run_id(output: &std::process::Output) -> String {
    json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
}

#[test]
fn final_binary_creates_and_reopens_its_configured_sqlite_database() {
    let scenario = Scenario::new();
    let first = scenario.output(&["--json", "list"], |_| {});
    assert!(
        first.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(json(&first)["data"]["runs"], serde_json::json!([]));
    assert!(scenario.database_path().is_file());

    let reopened = scenario.output(&["--json", "list"], |_| {});
    assert!(reopened.status.success());
    assert_eq!(json(&reopened)["data"]["runs"], serde_json::json!([]));
}

#[test]
fn global_storage_home_ignores_workspace_database_redirect_and_is_shared_across_workspaces() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "global-storage-input",
        "global storage prompt",
        false,
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        "workspace-redirect.db",
        ",compatibility_input_request:true",
    );

    let first = scenario.output(&["--json", "run", "persist globally"], |command| {
        command.env("TEST_OPENAI_KEY", "global-storage-secret");
    });
    assert_eq!(first.status.code(), Some(1));
    let persisted_run_id = waiting_run_id(&first);
    provider.assert_consumed();
    assert!(scenario.database_path().is_file());
    assert!(!scenario.root().join("workspace-redirect.db").exists());

    let second_workspace = scenario.root().join("second-workspace");
    std::fs::create_dir_all(second_workspace.join(".git")).unwrap();
    let second = scenario.output(&["--json", "list"], |command| {
        command.current_dir(&second_workspace);
    });
    assert!(second.status.success());
    assert_eq!(json(&second)["data"]["runs"][0]["run_id"], persisted_run_id);
    assert!(!second_workspace.join(".latte/latte-code.db").exists());
}

#[test]
fn final_binary_imports_legacy_workspace_sessions_once_and_exports_jsonl() {
    use latte_core::{IdSource, RunId, SystemIdSource, ThreadId, ThreadProviderBindingV2};

    let scenario = Scenario::new();
    let legacy_path = scenario.root().join(".latte/latte-code.db");
    std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    let legacy = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(&legacy_path)
        .build()
        .unwrap();
    let ids = SystemIdSource::default();
    let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
    legacy
        .create_thread_v2(
            thread_id,
            RunId::from_uuid(ids.next_uuid_v7()),
            ThreadProviderBindingV2 {
                version: 1,
                provider_name: "test".into(),
                provider_type: "openai-chat".into(),
                protocol: "chat".into(),
                model: "test-model".into(),
                config_fingerprint: "config".into(),
                tools_fingerprint: "tools".into(),
                aliases: std::collections::BTreeMap::new(),
                credential_ref_id: "env:TEST_KEY".into(),
                data_scope_id: "workspace".into(),
                credential_generation: 1,
            },
            "legacy conversation",
            1,
        )
        .unwrap();
    drop(legacy);
    let source_before = std::fs::read(&legacy_path).unwrap();

    let first = scenario.output(&["--json", "list"], |_| {});
    assert!(first.status.success());
    let second = scenario.output(&["--json", "list"], |_| {});
    assert!(second.status.success());
    assert_eq!(std::fs::read(&legacy_path).unwrap(), source_before);
    assert!(scenario.database_path().is_file());
    let imported = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let workspace = std::fs::canonicalize(scenario.root()).unwrap();
    let sessions = imported
        .list_thread_sessions_v2_for_workspace(workspace.to_str().unwrap(), 10)
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, thread_id);
    let session_files = scenario.session_files();
    assert_eq!(session_files.len(), 1);
    let conversation = std::fs::read_to_string(&session_files[0]).unwrap();
    assert!(conversation.contains(r#""content":"legacy conversation""#));
}

#[test]
fn final_binary_parses_loopback_provider_input_and_persists_waiting_projection() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "portable-input",
        "portable prompt",
        false,
    )]);
    let secret = "latte-portable-e2e-secret";
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let output = scenario.output(&["--json", "run", "portable provider journey"], |command| {
        command.env("TEST_OPENAI_KEY", secret);
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["status"], "failed");
    assert_eq!(json(&output)["error"]["code"], "runtime");
    let waiting_id = waiting_run_id(&output);
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/chat/completions");
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        &format!("Bearer {secret}")
    );

    let shown = scenario.output(&["--json", "show", &waiting_id], |_| {});
    assert_eq!(shown.status.code(), Some(10));
    assert_eq!(json(&shown)["data"]["run"]["status"], "waiting_input");
    assert_eq!(
        json(&shown)["data"]["run"]["pending_input"]["request_id"],
        "portable-input"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"][0]["run_id"], waiting_id);

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
fn final_binary_uses_inline_provider_secret_without_environment_inheritance() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "inline-portable-input",
        "inline portable prompt",
        false,
    )]);
    let secret = "latte-inline-portable-e2e-secret";
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{:?},api_key:{secret:?},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["verification-must-not-run"]}}}}"#,
            provider.endpoint()
        ),
    )
    .unwrap();

    let output = scenario.output(&["--json", "run", "inline provider journey"], |_| {});

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "runtime");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        &format!("Bearer {secret}")
    );
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
fn final_binary_persists_terminal_provider_failure_without_retrying() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::json(
        400,
        &serde_json::json!({"error": "portable rejection"}),
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        ".latte/latte-code.db",
        ",max_attempts:3",
    );
    let output = scenario.output(&["--json", "run", "persist provider failure"], |command| {
        command.env("TEST_OPENAI_KEY", "portable-failure-secret");
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["status"], "failed");
    assert_eq!(json(&output)["data"]["run"]["status"], "failed");
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);

    let id = run_id(&output);
    let shown = scenario.output(&["--json", "show", &id], |_| {});
    assert_eq!(shown.status.code(), Some(1));
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert!(
        json(&shown)["data"]["run"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("http 400")
    );
}
