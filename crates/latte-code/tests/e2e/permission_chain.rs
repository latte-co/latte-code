use super::support::{ProviderReply, Scenario, ScriptedProvider, json};

fn waiting_run_id(output: &std::process::Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(10),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn process_permission_then_verification_permission_fails_durably_on_second_resume() {
    let scenario = Scenario::new();
    let process = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf permission-chain-out; printf permission-chain-err >&2; exit 3"
        ],
        "cwd": ".",
        "env": {},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 4_096,
        "stderr_cap": 4_096
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("permission-chain-process", "process", &process),
        ProviderReply::completion("process observed; verify now"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/bin/sh","-c","printf verification-chain-failed >&2; exit 7"]"#,
    );

    let first_wait = scenario.output(
        &["--json", "run", "exercise two permission phases"],
        |cmd| {
            cmd.env("TEST_OPENAI_KEY", "permission-chain-secret");
        },
    );
    let run_id = waiting_run_id(&first_wait);
    let first_show = scenario.output(&["--json", "show", &run_id], |_| {});
    assert_eq!(
        json(&first_show)["data"]["run"]["pending_permission"]["description"],
        "allow process"
    );
    assert_eq!(provider.requests().len(), 1);

    let verification_wait = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "permission-chain-secret");
    });
    assert_eq!(waiting_run_id(&verification_wait), run_id);
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let process_result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(process_result.contains("permission-chain-out"));
    assert!(process_result.contains("permission-chain-err"));
    assert!(process_result.contains("\"exit_code\":3"));

    let verification_show = scenario.output(&["--json", "show", &run_id], |_| {});
    assert_eq!(
        json(&verification_show)["data"]["run"]["status"],
        "waiting_permission"
    );
    assert_eq!(
        json(&verification_show)["data"]["run"]["pending_permission"]["description"],
        "allow verification command"
    );
    let verification_request =
        json(&verification_show)["data"]["run"]["pending_permission"]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
    assert!(verification_request.starts_with("verify-"));
    assert_ne!(verification_request, "permission-chain-process");

    let failed = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "permission-chain-secret");
    });
    assert_eq!(
        failed.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert_eq!(json(&failed)["status"], "failed");
    assert!(
        json(&failed)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("verification failed")
    );
    assert_eq!(provider.requests().len(), 2);

    let shown = scenario.output(&["--json", "show", &run_id], |_| {});
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert!(json(&shown)["data"]["run"]["pending_permission"].is_null());
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id && run["status"] == "failed")
    );

    let repeated = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "permission-chain-secret");
    });
    assert_eq!(repeated.status.code(), Some(1));
    assert_eq!(provider.requests().len(), 2);
}
