use super::support::{ProviderReply, Scenario, ScriptedProvider, assert_secret_absent, json};
use std::time::Duration;

fn run_with_provider(
    scenario: &Scenario,
    provider: &ScriptedProvider,
    provider_fields: &str,
) -> std::process::Output {
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        provider_fields,
    );
    scenario.output(
        &["--json", "run", "exercise provider boundary"],
        |command| {
            command.env("TEST_OPENAI_KEY", "latte-provider-e2e-secret");
        },
    )
}

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

/// Extracts the terminal failure message from a v2 run envelope's transcript.
fn failure_text(output: &std::process::Output) -> String {
    json(output)["data"]["session"]["transcript"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|entry| entry["kind"] == "failure")
        .and_then(|entry| entry["text"].as_str())
        .unwrap_or("")
        .to_owned()
}

#[test]
fn base_url_is_normalized_to_the_production_chat_completions_endpoint() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("base URL completed")]);
    let base_url = provider
        .endpoint()
        .strip_suffix("/chat/completions")
        .unwrap();
    scenario.write_config_with_base_url(&format!("{base_url}///"), r#"["/bin/pwd"]"#);
    let output = scenario.output(
        &["--json", "run", "exercise base URL normalization"],
        |command| {
            command.env("TEST_OPENAI_KEY", "latte-provider-e2e-secret");
        },
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(completion_text(&output), "base URL completed");
    provider.assert_consumed();
    assert_eq!(provider.requests()[0].path, "/chat/completions");
}

#[test]
fn retryable_http_is_retried_exactly_once_then_completes_durably() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::json(503, &serde_json::json!({"error": "retry"})).header("Retry-After", "0"),
        ProviderReply::json(
            200,
            &serde_json::json!({
                "choices": [{
                    "message": {"content": "recovered", "tool_calls": []},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 3,
                    "total_tokens": 14,
                    "prompt_tokens_details": {"cached_tokens": 2}
                }
            }),
        ),
    ]);
    let output = run_with_provider(
        &scenario,
        &provider,
        ",timeout_ms:2000,max_attempts:2,temperature:0.25,max_tokens:64",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "completed");
    assert_eq!(
        json(&output)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(requests[0].body["temperature"], 0.25);
    assert_eq!(requests[0].body["max_tokens"], 64);
    assert!(scenario.database_path().exists());
}

#[test]
fn terminal_http_and_malformed_success_never_retry_or_leak_the_secret() {
    let http_scenario = Scenario::new();
    let http_provider = ScriptedProvider::start([ProviderReply::json(
        400,
        &serde_json::json!({"error": "bad request"}),
    )
    .header("X-Request-Id", "safe-request-id")]);
    let http = run_with_provider(
        &http_scenario,
        &http_provider,
        ",timeout_ms:1000,max_attempts:3",
    );
    assert_eq!(http.status.code(), Some(1));
    assert_eq!(json(&http)["status"], "failed");
    assert_eq!(
        json(&http)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert!(failure_text(&http).contains("http 400 (request safe-request-id)"));
    http_provider.assert_consumed();
    assert_eq!(http_provider.requests().len(), 1);
    let database = std::fs::read(http_scenario.database_path()).unwrap();
    assert_secret_absent(
        "latte-provider-e2e-secret",
        &[
            ("stdout", &http.stdout),
            ("stderr", &http.stderr),
            ("database", &database),
        ],
    );

    let malformed_scenario = Scenario::new();
    let malformed_provider = ScriptedProvider::start([ProviderReply::json(
        200,
        &serde_json::json!({"choices": []}),
    )]);
    let malformed = run_with_provider(
        &malformed_scenario,
        &malformed_provider,
        ",timeout_ms:1000,max_attempts:3",
    );
    assert_eq!(malformed.status.code(), Some(1));
    assert_eq!(
        json(&malformed)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert!(failure_text(&malformed).contains("missing choices"));
    malformed_provider.assert_consumed();
    assert_eq!(malformed_provider.requests().len(), 1);
}

#[test]
fn provider_timeout_is_bounded_failed_and_called_once() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("too late").delayed(Duration::from_millis(250))
    ]);
    let output = run_with_provider(&scenario, &provider, ",timeout_ms:50,max_attempts:1");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        json(&output)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert!(failure_text(&output).contains("provider timeout"));
    assert_eq!(provider.requests().len(), 1);
    assert!(scenario.database_path().exists());
}

#[test]
fn streaming_sse_completion_reaches_the_binary_and_persists_exact_text() {
    let scenario = Scenario::new();
    let stream = concat!(
        ": heartbeat\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"streamed \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
        "data: [DONE]\n\n"
    );
    let provider = ScriptedProvider::start([ProviderReply::raw(
        200,
        "text/event-stream; charset=utf-8",
        stream.as_bytes(),
    )]);
    let output = run_with_provider(
        &scenario,
        &provider,
        ",timeout_ms:1000,max_attempts:1,streaming:true",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(completion_text(&output), "streamed answer");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["stream"], true);
}

#[test]
fn unsupported_empty_stream_response_falls_back_inline_once() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::raw(415, "application/json", Vec::new()),
        ProviderReply::completion("inline fallback"),
    ]);
    let output = run_with_provider(
        &scenario,
        &provider,
        ",timeout_ms:1000,max_attempts:1,streaming:true",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(completion_text(&output), "inline fallback");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["stream"], true);
    assert!(requests[1].body.get("stream").is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn streaming_accepts_inline_json_but_fallback_failures_are_terminal_and_single_attempt() {
    let inline_scenario = Scenario::new();
    let inline_provider =
        ScriptedProvider::start([ProviderReply::completion("inline while streaming")]);
    let inline = run_with_provider(
        &inline_scenario,
        &inline_provider,
        ",timeout_ms:1000,max_attempts:2,streaming:true",
    );
    assert!(inline.status.success());
    assert_eq!(completion_text(&inline), "inline while streaming");
    inline_provider.assert_consumed();
    assert_eq!(inline_provider.requests().len(), 1);
    assert_eq!(inline_provider.requests()[0].body["stream"], true);

    let fallback_http_scenario = Scenario::new();
    let fallback_http_provider = ScriptedProvider::start([
        ProviderReply::raw(415, "application/json", Vec::new()),
        ProviderReply::json(429, &serde_json::json!({"error": "limited"}))
            .header("X-Request-Id", "fallback-request"),
    ]);
    let fallback_http = run_with_provider(
        &fallback_http_scenario,
        &fallback_http_provider,
        ",timeout_ms:1000,max_attempts:3,streaming:true",
    );
    assert_eq!(fallback_http.status.code(), Some(1));
    assert!(failure_text(&fallback_http).contains("http 429 (request fallback-request)"));
    fallback_http_provider.assert_consumed();
    assert_eq!(fallback_http_provider.requests().len(), 2);
    assert!(
        fallback_http_provider.requests()[1]
            .body
            .get("stream")
            .is_none()
    );

    let fallback_malformed_scenario = Scenario::new();
    let fallback_malformed_provider = ScriptedProvider::start([
        ProviderReply::raw(422, "application/json", Vec::new()),
        ProviderReply::raw(200, "application/json", b"not-json".to_vec()),
    ]);
    let fallback_malformed = run_with_provider(
        &fallback_malformed_scenario,
        &fallback_malformed_provider,
        ",timeout_ms:1000,max_attempts:3,streaming:true",
    );
    assert_eq!(fallback_malformed.status.code(), Some(1));
    assert_eq!(
        json(&fallback_malformed)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert!(!failure_text(&fallback_malformed).is_empty());
    fallback_malformed_provider.assert_consumed();
    assert_eq!(fallback_malformed_provider.requests().len(), 2);
}

#[test]
fn nonstandard_input_and_provider_state_fail_closed_without_a_second_call() {
    for (body, expected) in [
        (
            serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [],
                    "input_request": {"id": "input-1", "prompt": "value?", "secret": false}
                }}]
            }),
            "requires compatibility_input_request",
        ),
        (
            serde_json::json!({
                "choices": [{"message": {"content": "ignored", "tool_calls": []}}],
                "provider_state": {"cursor": "opaque"}
            }),
            "does not support provider state",
        ),
    ] {
        let scenario = Scenario::new();
        let provider = ScriptedProvider::start([ProviderReply::json(200, &body)]);
        let output = run_with_provider(&scenario, &provider, ",timeout_ms:1000,max_attempts:3");
        assert_eq!(output.status.code(), Some(1));
        assert!(failure_text(&output).contains(expected));
        provider.assert_consumed();
        assert_eq!(provider.requests().len(), 1);
    }
}

#[test]
fn legacy_headless_input_wait_is_visible_but_cannot_be_misresumed_as_permission() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "legacy-input-1",
        "provide a value in an input-capable frontend",
        false,
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let waiting = scenario.output(&["--json", "run", "request legacy input"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(waiting.status.code(), Some(10));
    assert_eq!(json(&waiting)["status"], "waiting");
    let session_id = json(&waiting)["data"]["session"]["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let shown = scenario.output(&["--json", "show", &session_id], |_| {});
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["lifecycle"],
        "waiting_input"
    );
    assert_eq!(
        json(&shown)["data"]["session"]["pending"]["request_id"],
        "legacy-input-1"
    );

    // A waiting_input session cannot accept a follow-up turn; the v2 resume
    // contract requires a ready session, so this fails closed.
    let invalid_resume =
        scenario.output(&["--json", "resume", &session_id, "continue"], |command| {
            command.env("TEST_OPENAI_KEY", "secret");
        });
    assert_ne!(invalid_resume.status.code(), Some(0));
    let still_waiting = scenario.output(&["--json", "show", &session_id], |_| {});
    assert_eq!(
        json(&still_waiting)["data"]["session"]["lifecycle"],
        "waiting_input"
    );
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn secret_input_empty_assistant_and_nonobject_tool_input_fail_durably() {
    let cases = [
        (
            ProviderReply::input_request("secret-input", "password?", true),
            ",compatibility_input_request:true",
            "unsupported secret or invalid input",
        ),
        (ProviderReply::completion(""), "", "empty assistant outcome"),
        (
            ProviderReply::tool_call("nonobject-tool-input", "read_file", &serde_json::json!(1)),
            "",
            "tool call ids must",
        ),
    ];
    for (reply, fields, expected) in cases {
        let scenario = Scenario::new();
        let provider = ScriptedProvider::start([reply]);
        scenario.write_config_with_provider_fields(
            provider.endpoint(),
            r#"["/bin/pwd"]"#,
            ".latte/latte-code.db",
            fields,
        );
        let output = scenario.output(&["--json", "run", "fail safely"], |command| {
            command.env("TEST_OPENAI_KEY", "secret");
        });
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            json(&output)["data"]["session"]["runs"][0]["status"],
            "failed"
        );
        assert!(
            failure_text(&output).contains(expected),
            "{}",
            json(&output)
        );
        provider.assert_consumed();
        assert_eq!(provider.requests().len(), 1);
    }
}

#[test]
fn malformed_inline_tool_calls_are_terminal_and_never_reenter_the_provider() {
    let cases = [
        (
            serde_json::json!({
                "choices": [{"message": {"content": "", "tool_calls": [{
                    "id": "malformed-arguments",
                    "function": {"name": "read_file", "arguments": "{"}
                }]}}]
            }),
            "EOF while parsing an object",
        ),
        (
            serde_json::json!({
                "choices": [{"message": {"content": "", "tool_calls": [
                    {"id": "duplicate", "function": {"name": "read_file", "arguments": "{}"}},
                    {"id": "duplicate", "function": {"name": "search", "arguments": "{}"}}
                ]}}]
            }),
            "must match [A-Za-z0-9_-]{1,256} and be unique",
        ),
        (
            serde_json::json!({
                "choices": [{"message": {"content": "", "tool_calls": [{
                    "id": "contains whitespace",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]}}]
            }),
            "must match [A-Za-z0-9_-]{1,256} and be unique",
        ),
    ];

    for (body, expected) in cases {
        let scenario = Scenario::new();
        let provider = ScriptedProvider::start([ProviderReply::json(200, &body)]);
        let output = run_with_provider(&scenario, &provider, ",max_attempts:3");

        assert_eq!(
            output.status.code(),
            Some(1),
            "expected {expected}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(json(&output)["status"], "failed");
        assert_eq!(
            json(&output)["data"]["session"]["runs"][0]["status"],
            "failed"
        );
        assert!(
            failure_text(&output).contains(expected),
            "{}",
            json(&output)
        );
        provider.assert_consumed();
        assert_eq!(provider.requests().len(), 1);
    }
}
