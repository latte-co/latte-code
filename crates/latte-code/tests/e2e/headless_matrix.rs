use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use serde_json::Value;
use std::time::Duration;

#[test]
fn public_engine_embedding_config_contract_covers_jsonc_environment_and_fail_closed_validation() {
    let scenario = Scenario::new();
    let config_dir = scenario.root().join(".latte");
    std::fs::create_dir_all(&config_dir).unwrap();
    let path = config_dir.join("latte-engine.jsonc");
    std::fs::write(
        &path,
        r#"{
            // Public embedding configuration remains JSONC.
            database: { path: "${PATH}" },
            runtime: { command_buffer: 4, event_buffer: 8 },
            providers: { local: {
                base_url: "http://127.0.0.1:9",
                api_key: "${PATH}",
            } },
        }"#,
    )
    .unwrap();
    let loaded = latte_engine::config::Config::load(scenario.root()).unwrap();
    assert_eq!(loaded.runtime.command_buffer, 4);
    assert_eq!(loaded.runtime.event_buffer, 8);
    assert!(!loaded.database.path.is_empty());
    let provider = loaded.providers.get("local").unwrap();
    assert!(provider.base_url.starts_with("http://"));
    let debug = format!("{provider:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(&provider.api_key));

    let defaults: latte_engine::config::Config = json5::from_str("{}").unwrap();
    assert_eq!(defaults.database.path, ".latte/latte-code.db");
    assert_eq!(defaults.runtime.command_buffer, 32);
    assert_eq!(defaults.runtime.event_buffer, 128);

    let missing =
        latte_engine::config::Config::load_path(&config_dir.join("missing.jsonc")).unwrap_err();
    assert!(missing.to_string().contains("cannot read configuration"));

    let invalid_cases = [
        ("{", "invalid JSONC configuration"),
        (r#"{database:{path:""}}"#, "database.path must not be empty"),
        (
            r"{runtime:{command_buffer:0,event_buffer:1}}",
            "runtime buffer sizes must be greater than zero",
        ),
        (
            r"{runtime:{command_buffer:1,event_buffer:0}}",
            "runtime buffer sizes must be greater than zero",
        ),
        (
            r#"{providers:{empty:{base_url:"",api_key:"key"}}}"#,
            "provider empty requires base_url and api_key",
        ),
        (
            r#"{providers:{empty:{base_url:"http://localhost",api_key:""}}}"#,
            "provider empty requires base_url and api_key",
        ),
        (
            r#"{providers:{missing:{base_url:"http://localhost",api_key:"${LATTE_E2E_INTENTIONALLY_MISSING}"}}}"#,
            "references missing environment variable",
        ),
        (
            r#"{database:{path:"${}"}}"#,
            "invalid environment placeholder",
        ),
        (
            r#"{providers:{nested:{base_url:"${A${B}}",api_key:"key"}}}"#,
            "invalid environment placeholder",
        ),
        (r"{unknown:true}", "invalid JSONC configuration"),
    ];
    for (index, (text, expected)) in invalid_cases.into_iter().enumerate() {
        let case_path = config_dir.join(format!("invalid-{index}.jsonc"));
        std::fs::write(&case_path, text).unwrap();
        let error = latte_engine::config::Config::load_path(&case_path).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "case {index}: {error}"
        );
    }

    write_workspace_config(&scenario, r#"{database:{path:".latte/final.db"}}"#);
    let final_cli = scenario.output(&["--json", "list"], |_| {});
    assert!(final_cli.status.success());
    assert!(scenario.root().join(".latte/final.db").exists());
}

fn write_workspace_config(scenario: &Scenario, text: &str) {
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(scenario.root().join(".latte/latte-code.jsonc"), text).unwrap();
}

fn write_home_provider_config(scenario: &Scenario) {
    std::fs::create_dir_all(scenario.home().join(".latte")).unwrap();
    std::fs::write(
        scenario.home().join(".latte/latte-code.jsonc"),
        r#"{
            version: 1,
            default_model: "primary/mock",
            providers: { primary: {
                type: "openai-chat",
                models: ["mock"],
                base_url: "http://127.0.0.1:9",
                api_key: { source: "env", name: "TEST_OPENAI_KEY" }
            } }
        }"#,
    )
    .unwrap();
}

fn waiting_run_id(output: &std::process::Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(10),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(output)["error"]["code"], "permission_required");
    json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
}

fn pending_effect(scenario: &Scenario, run_id: &str) -> Value {
    let shown = scenario.output(&["--json", "show", run_id], |_| {});
    assert_eq!(shown.status.code(), Some(10));
    json(&shown)["data"]["run"]["pending_permission"].clone()
}

fn tool_messages(request: &Value) -> Vec<&Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_cli_rejects_invalid_application_registry_and_alias_contracts() {
    let configuration_cases = [
        (r"{version:2}", "version must be 1"),
        (
            r#"{default_model:"missing/mock"}"#,
            "default_model provider must name a configured provider",
        ),
        (
            r#"{providers:{"":{type:"openai-chat",models:["mock"],base_url:"http://127.0.0.1:9",api_key:{source:"env",name:"OPENAI_API_KEY"}}}}"#,
            "provider names must not be empty",
        ),
        (
            r#"{default_model:" "}"#,
            "default_model must use provider/model",
        ),
        (
            r#"{providers:{primary:{endpoint:"http://127.0.0.1:9"}}}"#,
            "requires exactly one of base_url or endpoint",
        ),
        (
            r"{providers:{primary:{base_url:null,endpoint:null}}}",
            "requires exactly one of base_url or endpoint",
        ),
        (
            r"{providers:{primary:{timeout_ms:0}}}",
            "timeout/attempts are out of range",
        ),
        (
            r"{providers:{primary:{max_attempts:11}}}",
            "timeout/attempts are out of range",
        ),
        (
            r"{providers:{primary:{temperature:2.5}}}",
            "temperature must be between 0 and 2",
        ),
        (
            r#"{providers:{primary:{models:["mock","mock"]}}}"#,
            "models must be unique, non-empty, and bounded",
        ),
        (
            r"{providers:{primary:{models:{mock:{options:{context_window:0}}}}}}",
            "model mock options are invalid",
        ),
        (
            r"{providers:{primary:{models:{mock:{options:{context_window:64,max_tokens:64}}}}}}",
            "model mock options are invalid",
        ),
        (
            r#"{providers:{primary:{models:{mock:{options:{reasoning_effort:" "}}}}}}"#,
            "model mock options are invalid",
        ),
        (
            r#"{providers:{primary:{models:{mock:{name:" "}}}}}"#,
            "model mock options are invalid",
        ),
        (
            r"{thread:{max_input_bytes:32,reserved_output_bytes:32}}",
            "invalid thread configuration",
        ),
        (r"{unexpected:true}", "invalid merged configuration"),
    ];
    for (overlay, expected) in configuration_cases {
        let scenario = Scenario::new();
        write_home_provider_config(&scenario);
        write_workspace_config(&scenario, overlay);
        let output = scenario.output(&["--json", "list"], |_| {});
        assert_eq!(
            output.status.code(),
            Some(2),
            "case {overlay}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(json(&output)["error"]["code"], "configuration");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "case {overlay}: {}",
            json(&output)
        );
    }

    let scenario = Scenario::new();
    write_workspace_config(&scenario, "[]");
    let output = scenario.output(&["--json", "list"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert!(
        json(&output)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("top-level configuration must be an object")
    );

    let alias_cases = [
        (r#"{missing:"wire"}"#, "unknown canonical tool"),
        (
            r#"{read_file:"same",search:"same"}"#,
            "tool alias collision",
        ),
        (r#"{read_file:"not a wire name"}"#, "invalid alias"),
    ];
    for (aliases, expected) in alias_cases {
        let scenario = Scenario::new();
        write_home_provider_config(&scenario);
        write_workspace_config(
            &scenario,
            &format!(
                r#"{{providers:{{primary:{{base_url:null,endpoint:"http://127.0.0.1:9",aliases:{aliases}}}}}}}"#
            ),
        );
        let output = scenario.output(
            &["--json", "run", "must fail before transport"],
            |command| {
                command.env("TEST_OPENAI_KEY", "alias-contract-secret");
            },
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "aliases {aliases}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(json(&output)["error"]["code"], "configuration");
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "aliases {aliases}: {}",
            json(&output)
        );
    }
}

#[test]
fn configured_alias_rejects_an_unmapped_provider_tool_name_before_execution() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "unmapped-alias",
        "read_file",
        &serde_json::json!({"path": "must-not-run.txt"}),
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        r#",aliases:{read_file:"wire_read_file"}"#,
    );

    let output = scenario.output(
        &["--json", "run", "reject an unmapped response alias"],
        |command| {
            command.env("TEST_OPENAI_KEY", "alias-response-secret");
        },
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["data"]["run"]["status"], "failed");
    assert!(
        json(&output)["data"]["run"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("provider returned an unknown tool alias")
    );
    assert!(!scenario.root().join("must-not-run.txt").exists());
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    assert!(
        provider.requests()[0].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "wire_read_file")
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn legacy_multi_tool_queue_survives_four_processes_and_verifies_exact_outputs() {
    let scenario = Scenario::new();
    std::fs::create_dir(scenario.root().join("work")).unwrap();
    std::fs::write(scenario.root().join("work/input.txt"), "fixture-before\n").unwrap();

    let read = serde_json::json!({"path": "work/input.txt", "max_output": 1024});
    let write = serde_json::json!({
        "path": "work/generated.txt",
        "content": "created-by-matrix\n",
        "create_intent": true
    });
    let process = serde_json::json!({
        "shell": "test \"$(basename \"$PWD\")\" = work; printf \"$MATRIX_FLAG-123\"; printf stderr-456 >&2; exit 7",
        "cwd": "work",
        "env": {"MATRIX_FLAG": "env-ok"},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 6,
        "stderr_cap": 6
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            ("matrix-read", "read_file", &read),
            ("matrix-write", "write_file", &write),
            ("matrix-process", "process", &process),
        ]),
        ProviderReply::completion("matrix queue and verification complete"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/bin/sh","-c","test -f work/generated.txt && grep -q created-by-matrix work/generated.txt"]"#,
    );
    let invoke = |args: &[&str]| {
        scenario.output(args, |command| {
            command.env("TEST_OPENAI_KEY", "matrix-secret");
        })
    };

    let first = invoke(&["--json", "run", "exercise the durable tool queue"]);
    let run_id = waiting_run_id(&first);
    let pending = pending_effect(&scenario, &run_id);
    assert_eq!(pending["request_id"], "matrix-write");
    assert!(!scenario.root().join("work/generated.txt").exists());
    assert_eq!(provider.requests().len(), 1);

    let second = invoke(&["--json", "resume", &run_id, "--allow"]);
    assert_eq!(waiting_run_id(&second), run_id);
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("work/generated.txt")).unwrap(),
        "created-by-matrix\n"
    );
    let pending = pending_effect(&scenario, &run_id);
    assert_eq!(pending["request_id"], "matrix-process");
    assert_eq!(provider.requests().len(), 1);

    let third = invoke(&["--json", "resume", &run_id, "--allow"]);
    assert_eq!(waiting_run_id(&third), run_id);
    assert!(
        pending_effect(&scenario, &run_id)["request_id"]
            .as_str()
            .unwrap()
            .starts_with("verify-")
    );
    assert!(provider.wait_for_calls(2, Duration::from_secs(1)));
    provider.assert_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let results = tool_messages(&requests[1].body);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["tool_call_id"], "matrix-read");
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("fixture-before")
    );
    assert_eq!(results[1]["tool_call_id"], "matrix-write");
    assert_eq!(results[2]["tool_call_id"], "matrix-process");
    let process_result = results[2]["content"].as_str().unwrap();
    for expected in [
        "env-ok",
        "stderr",
        "\"exit_code\":7",
        "\"stdout_truncated\":true",
        "\"stderr_truncated\":true",
    ] {
        assert!(
            process_result.contains(expected),
            "missing {expected} in {process_result}"
        );
    }

    let fourth = invoke(&["--json", "resume", &run_id, "--allow"]);
    assert!(
        fourth.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&fourth.stdout),
        String::from_utf8_lossy(&fourth.stderr)
    );
    assert_eq!(json(&fourth)["status"], "completed");
    assert_eq!(
        json(&fourth)["data"]["run"]["handoff"]["summary"],
        "matrix queue and verification complete"
    );
    assert_eq!(
        json(&fourth)["data"]["run"]["handoff"]["evidence"][0]["status"],
        "passed"
    );

    let repeated = invoke(&["--json", "resume", &run_id, "--allow"]);
    assert_eq!(repeated.status.code(), Some(1));
    assert_eq!(provider.requests().len(), 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn fragmented_sse_tool_call_executes_and_reenters_stream_with_ordered_history() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("stream.txt"), "stream fixture\n").unwrap();
    let usage = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 2, "completion_tokens": 0, "total_tokens": 2}
    });
    let tool_start = serde_json::json!({
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "sse-read",
                "function": {"name": "read_file", "arguments": "{\"path\":"}
            }]}
        }]
    });
    let tool_end = serde_json::json!({
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "function": {"arguments": "\"stream.txt\"}"}
            }]},
            "finish_reason": "tool_calls"
        }]
    });
    let tool_stream = format!(
        ": keepalive\r\nevent: message\r\ndata: {usage}\r\n\r\ndata:{tool_start}\n\ndata: {tool_end}\n\ndata: [DONE]\n\n"
    );
    let completion_stream = concat!(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2,\"total_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"stream tool \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let provider = ScriptedProvider::start([
        ProviderReply::raw(200, "text/event-stream", tool_stream.as_bytes()),
        ProviderReply::raw(200, "text/event-stream", completion_stream.as_bytes()),
    ]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",streaming:true",
    );
    let output = scenario.output(
        &["--json", "run", "read through a fragmented stream"],
        |command| {
            command.env("TEST_OPENAI_KEY", "stream-matrix-secret");
        },
    );

    assert!(
        output.status.success(),
        "calls={} stdout={} stderr={}",
        provider.requests().len(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        json(&output)["data"]["run"]["handoff"]["summary"],
        "stream tool complete"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[1].body["stream"], true);
    let results = tool_messages(&requests[1].body);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "sse-read");
    assert_eq!(results[0]["name"], "read_file");
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("stream fixture")
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn malformed_sse_variants_fail_durably_without_retry_or_side_effects() {
    let mut cases = [
        (
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "ended without [DONE]",
        ),
        ("data: not-json\n\ndata: [DONE]\n\n", "invalid SSE JSON"),
        (
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":128,\"id\":\"overflow\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n",
            "too many stream tool calls",
        ),
        (
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"first\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"changed\",\"function\":{}}]}}]}\n\n",
                "data: [DONE]\n\n"
            ),
            "stream tool id changed",
        ),
    ]
    .into_iter()
    .map(|(stream, expected)| (stream.as_bytes().to_vec(), expected))
    .collect::<Vec<_>>();

    cases.push((
        concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"stable\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"write_file\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes()
        .to_vec(),
        "stream tool name changed",
    ));

    let argument_chunk = "x".repeat(100_000);
    let argument_start = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "oversized-arguments",
            "function": {"name": "read_file", "arguments": argument_chunk}
        }]}}]
    });
    let argument_continuation = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "function": {"arguments": "x".repeat(100_000)}
        }]}}]
    });
    let oversized_arguments = format!(
        "data: {argument_start}\n\ndata: {argument_continuation}\n\ndata: {argument_continuation}\n\n"
    );
    cases.push((
        oversized_arguments.into_bytes(),
        "stream tool arguments exceed limit",
    ));
    cases.push((
        format!("data: {}\n\n", "x".repeat(256 * 1024 + 1)).into_bytes(),
        "SSE event exceeds limit",
    ));
    cases.push((b"data: \xff\n\n".to_vec(), "SSE data is not UTF-8"));
    cases.push((vec![b'x'; 4 * 1024 * 1024 + 1], "SSE body exceeds limit"));
    cases.push((
        b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n".to_vec(),
        "stream tool call missing id",
    ));
    cases.push((
        b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"missing-name\",\"function\":{\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n".to_vec(),
        "stream tool call missing name",
    ));
    cases.push((
        b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"bad-arguments\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\"}}]}}]}\n\ndata: [DONE]\n\n".to_vec(),
        "EOF while parsing an object",
    ));
    cases.push((
        b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"invalid id\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n".to_vec(),
        "must match [A-Za-z0-9_-]{1,256} and be unique",
    ));
    cases.push((
        b"data: {\"choices\":[{\"delta\":{\"content\":\"unterminated\"}}]}\n".to_vec(),
        "ended without [DONE]",
    ));

    for (stream, expected) in cases {
        let scenario = Scenario::new();
        let provider =
            ScriptedProvider::start([ProviderReply::raw(200, "text/event-stream", stream)]);
        scenario.write_config_with_provider_fields(
            provider.endpoint(),
            r#"["/bin/pwd"]"#,
            ".latte/latte-code.db",
            ",streaming:true,max_attempts:3",
        );
        let output = scenario.output(&["--json", "run", "reject malformed stream"], |command| {
            command.env("TEST_OPENAI_KEY", "malformed-stream-secret");
        });
        assert_eq!(
            output.status.code(),
            Some(1),
            "expected {expected}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(json(&output)["data"]["run"]["status"], "failed");
        assert!(
            json(&output)["data"]["run"]["failure"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{}",
            json(&output)
        );
        provider.assert_consumed();
        assert_eq!(provider.requests().len(), 1);
    }
}
