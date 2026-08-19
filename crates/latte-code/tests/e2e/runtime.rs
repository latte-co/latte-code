use super::recovery::{ServerChild, server_binding};
use super::support::{ProviderReply, Scenario, ScriptedProvider, assert_process_group_gone};
use std::time::Duration;

/// Drives a `process` tool call through the v2 HTTP contract: the session
/// parks at `WaitingPermission`, the decision is `POSTed` over HTTP, and the
/// supervised process runs to completion. Returns the server (kept alive for
/// provider/process-group assertions), the session id, and the consumed
/// permission coordinates (for idempotency re-decision checks).
fn allow_process_to_completion(
    scenario: &Scenario,
    provider: &ScriptedProvider,
    prompt: &str,
) -> (ServerChild, String, String, u64, u64) {
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let server = ServerChild::start(scenario);
    let workspace = server.create_workspace(scenario);
    let binding = server_binding(scenario);
    let session_id = server.create_session(&workspace, prompt, &binding);
    let (revision, request_id, run_revision) = server.wait_for_permission(&session_id);
    let (allow_status, allow_body) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert_eq!(allow_status, 200, "allow: {allow_body:?}");
    let terminal = server.wait_for_terminal(&session_id);
    assert_eq!(terminal["lifecycle"], "ready");
    assert_eq!(terminal["runs"][0]["status"], "completed");
    (server, session_id, request_id, revision, run_revision)
}

/// Extracts the tool-result message content from the provider's second request.
fn tool_result_content(requests: &[super::support::ProviderRequest]) -> &str {
    requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
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
    let (server, session_id, request_id, revision, run_revision) =
        allow_process_to_completion(&scenario, &provider, "run bounded argv");

    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = tool_result_content(&requests);
    assert!(result.contains("12345"));
    assert!(result.contains("abcd"));
    assert!(result.contains("\"stdout_truncated\":true"));
    assert!(result.contains("\"stderr_truncated\":true"));
    assert!(result.contains("\"exit_code\":7"));

    // A repeated permission decision on the consumed request is rejected and
    // never re-executes the effect or re-enters the provider.
    let (repeat_status, _) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert!(
        repeat_status == 404 || repeat_status == 409,
        "repeat permission returned {repeat_status}"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[cfg(unix)]
#[test]
fn legacy_process_timeout_reaps_group_then_reenters_provider_once() {
    let scenario = Scenario::new();
    let pgid_file = scenario.root().join("legacy-timeout-pgid");
    let pgid_path = pgid_file.to_str().unwrap().to_owned();
    let shell = r#"echo $$ > "$LATTE_E2E_PGID_FILE"; exec sleep 30"#;
    let process = serde_json::json!({
        "shell": shell,
        "cwd": ".",
        "env": {"LATTE_E2E_PGID_FILE": pgid_path},
        // This scenario proves timeout cleanup, not a 100 ms scheduling SLA. Leave
        // enough time for a loaded CI runner to start the shell and publish its PID.
        "timeout_ms": 1_000,
        "grace_ms": 50,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("legacy-timeout-process", "process", &process),
        ProviderReply::completion("timeout observed"),
    ]);
    let (_server, _session_id, _request_id, _revision, _run_revision) =
        allow_process_to_completion(&scenario, &provider, "run timeout process");

    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_process_group_gone(pgid, Duration::from_secs(5));
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = tool_result_content(&requests);
    assert!(result.contains("timed_out"));
}

#[cfg(unix)]
#[test]
fn legacy_process_exit_with_live_group_reaps_survivors_then_reenters_provider_once() {
    let scenario = Scenario::new();
    let pgid_file = scenario.root().join("legacy-exit-live-group-pgid");
    let pgid_path = pgid_file.to_str().unwrap().to_owned();
    // The supervised shell publishes its own PID (the process-group leader), spawns
    // a long-lived background child in that same group, and then exits promptly.
    // The leader's `wait()` resolves as `Exited`, but the group is still alive
    // because of the detached `sleep`, forcing supervise() down its post-exit
    // group-shutdown path (SIGTERM the survivors, confirm the group is gone).
    let shell = r#"echo $$ > "$LATTE_E2E_PGID_FILE"; sleep 30 & exit 0"#;
    let process = serde_json::json!({
        "shell": shell,
        "cwd": ".",
        "env": {"LATTE_E2E_PGID_FILE": pgid_path},
        // The process exits on its own well before this deadline; the timeout only
        // guards against a hung shell on a loaded runner, it is not the trigger.
        "timeout_ms": 5_000,
        "grace_ms": 200,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("legacy-exit-live-group-process", "process", &process),
        ProviderReply::completion("exit with live group observed"),
    ]);
    let (_server, _session_id, _request_id, _revision, _run_revision) =
        allow_process_to_completion(&scenario, &provider, "run exit-with-live-group process");

    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_process_group_gone(pgid, Duration::from_secs(5));
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = tool_result_content(&requests);
    // The leader exited cleanly, so the observed outcome is a normal exit, not a
    // timeout or cancellation, even though the supervisor still reaped the group.
    assert!(result.contains("\"exit_code\":0"));
    assert!(!result.contains("timed_out"));
}

#[cfg(unix)]
#[test]
fn legacy_process_timeout_escalates_to_sigkill_when_group_ignores_sigterm() {
    let scenario = Scenario::new();
    let pgid_file = scenario.root().join("legacy-sigkill-escalation-pgid");
    let pgid_path = pgid_file.to_str().unwrap().to_owned();
    // The supervised shell ignores SIGTERM and stays busy in the foreground, so
    // the supervisor's graceful SIGTERM cannot reap it. After the grace window the
    // supervisor must escalate to SIGKILL to certify the group is gone, exercising
    // the terminate-and-reap SIGKILL path rather than the graceful shutdown path.
    let shell = r#"echo $$ > "$LATTE_E2E_PGID_FILE"; trap '' TERM; while :; do sleep 0.2; done"#;
    let process = serde_json::json!({
        "shell": shell,
        "cwd": ".",
        "env": {"LATTE_E2E_PGID_FILE": pgid_path},
        // Bound the busy loop so the timeout lane fires, then keep the grace window
        // large enough that a loaded runner still reaches the SIGKILL escalation.
        "timeout_ms": 1_000,
        "grace_ms": 200,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("legacy-sigkill-escalation-process", "process", &process),
        ProviderReply::completion("sigkill escalation observed"),
    ]);
    let (_server, _session_id, _request_id, _revision, _run_revision) =
        allow_process_to_completion(&scenario, &provider, "run sigkill escalation process");

    let pgid = std::fs::read_to_string(&pgid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_process_group_gone(pgid, Duration::from_secs(5));
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let result = tool_result_content(&requests);
    assert!(result.contains("timed_out"));
}
