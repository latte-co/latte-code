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
