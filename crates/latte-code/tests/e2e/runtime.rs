use super::support::{ProviderReply, Scenario, ScriptedProvider, assert_process_group_gone, json};
use std::time::Duration;

fn run_id_from_waiting(output: &std::process::Output) -> String {
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

fn run_then_allow(
    scenario: &Scenario,
    provider: &ScriptedProvider,
    prompt: &str,
) -> (String, std::process::Output) {
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let waiting = scenario.output(&["--json", "run", prompt], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let run_id = run_id_from_waiting(&waiting);
    let completed = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    (run_id, completed)
}

#[cfg(unix)]
#[test]
fn legacy_process_argv_approval_preserves_bounded_dual_stream_result() {
    let scenario = Scenario::new();
    let process = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf 123456789; printf abcdefghi >&2; exit 7"
        ],
        "cwd": ".",
        "env": {},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 5,
        "stderr_cap": 4
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("legacy-argv-process", "process", &process),
        ProviderReply::completion("legacy argv observed"),
    ]);
    let (run_id, completed) = run_then_allow(&scenario, &provider, "run bounded argv");

    assert!(
        completed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["data"]["run"]["status"], "completed");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(result.contains("12345"));
    assert!(result.contains("abcd"));
    assert!(result.contains("\"stdout_truncated\":true"));
    assert!(result.contains("\"stderr_truncated\":true"));
    assert!(result.contains("\"exit_code\":7"));

    let repeated = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(repeated.status.code(), Some(1));
    assert_eq!(provider.requests().len(), 2);
}

#[cfg(unix)]
#[test]
fn legacy_process_timeout_reaps_group_then_reenters_provider_once() {
    let scenario = Scenario::new();
    let pgid_file = scenario.root().join("legacy-timeout-pgid");
    let shell = format!("echo $$ > {}; exec sleep 30", pgid_file.display());
    let process = serde_json::json!({
        "shell": shell,
        "cwd": ".",
        "env": {},
        "timeout_ms": 100,
        "grace_ms": 50,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("legacy-timeout-process", "process", &process),
        ProviderReply::completion("timeout observed"),
    ]);
    let (_, completed) = run_then_allow(&scenario, &provider, "run timeout process");

    assert!(
        completed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_process_group_gone(pgid, Duration::from_secs(5));
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(result.contains("timed_out"));
}
