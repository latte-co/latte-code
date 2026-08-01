use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use latte_core::RunId;

fn waiting_run_id(output: &std::process::Output) -> RunId {
    assert_eq!(output.status.code(), Some(10));
    let value = json(output);
    let id = value["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap();
    RunId::from_uuid(uuid::Uuid::parse_str(id).unwrap())
}

fn open_engine(scenario: &Scenario) -> latte_engine::EngineHandle {
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn invoke(scenario: &Scenario, args: &[&str]) -> std::process::Output {
    scenario.output(args, |command| {
        command.env("TEST_OPENAI_KEY", "checkpoint-boundary-secret");
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_checkpoint_corruption_matrix_fails_closed_in_fresh_final_cli_processes() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "checkpoint-write",
        "write_file",
        &serde_json::json!({
            "path": "must-not-be-written.txt",
            "content": "checkpoint boundary\n",
            "create_intent": true
        }),
    )]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let started = invoke(
        &scenario,
        &[
            "--json",
            "run",
            "create a publicly persisted permission checkpoint",
        ],
    );
    let run_id = waiting_run_id(&started);
    provider.assert_consumed();
    let engine = open_engine(&scenario);
    let state = engine.show(run_id).unwrap();
    let original_text = engine.runtime_checkpoint(run_id).unwrap().unwrap();
    let original: serde_json::Value = serde_json::from_str(&original_text).unwrap();
    assert_eq!(original["tool_queue"][0]["id"], "checkpoint-write");
    assert_eq!(original["pending"]["phase"], "tool");

    let mut cases: Vec<(&str, serde_json::Value, &str)> = Vec::new();

    let mut nested_assistant = original.clone();
    nested_assistant["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{"id":"nested-call","name":"read_file","input":{"path":"x"}}]
        }));
    cases.push((
        "nested assistant",
        nested_assistant,
        "assistant before tool round resolved",
    ));

    let mut duplicate_call = original.clone();
    duplicate_call["messages"][2]["tool_calls"] = serde_json::json!([
        {"id":"duplicate","name":"read_file","input":{"path":"a"}},
        {"id":"duplicate","name":"read_file","input":{"path":"b"}}
    ]);
    duplicate_call["tool_queue"] = duplicate_call["messages"][2]["tool_calls"].clone();
    duplicate_call["pending"]["call"] = duplicate_call["tool_queue"][0].clone();
    duplicate_call["pending"]["effect_id"] = "duplicate".into();
    cases.push((
        "duplicate call id",
        duplicate_call,
        "invalid or duplicate assistant tool call id",
    ));

    let mut orphan_tool = original.clone();
    orphan_tool["messages"] = serde_json::json!([
        original["messages"][0].clone(),
        original["messages"][1].clone(),
        {"role":"tool","tool_call_id":"orphan","name":"read_file","content":"none"}
    ]);
    cases.push(("orphan tool", orphan_tool, "orphan tool result"));

    let mut extra_tool = original.clone();
    extra_tool["messages"].as_array_mut().unwrap().extend([
        serde_json::json!({
            "role":"tool","tool_call_id":"checkpoint-write",
            "name":"write_file","content":"first"
        }),
        serde_json::json!({
            "role":"tool","tool_call_id":"checkpoint-write",
            "name":"write_file","content":"second"
        }),
    ]);
    cases.push(("extra tool", extra_tool, "orphan tool result"));

    let mut wrong_order = original.clone();
    wrong_order["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "role":"tool","tool_call_id":"wrong-id","name":"write_file","content":"none"
        }));
    cases.push(("out of order tool", wrong_order, "out-of-order tool result"));

    let mut interrupted_round = original.clone();
    interrupted_round["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"role":"user","content":"interrupt"}));
    cases.push((
        "interrupted round",
        interrupted_round,
        "message interrupts tool round",
    ));

    let mut queue_mismatch = original.clone();
    queue_mismatch["tool_cursor"] = serde_json::json!(1);
    cases.push(("queue mismatch", queue_mismatch, "tool queue mismatch"));

    let mut invalid_resolved_queue = original.clone();
    invalid_resolved_queue["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "role":"tool","tool_call_id":"checkpoint-write",
            "name":"write_file","content":"resolved"
        }));
    invalid_resolved_queue["pending"] = serde_json::Value::Null;
    invalid_resolved_queue["tool_cursor"] = serde_json::json!(0);
    cases.push((
        "invalid resolved queue",
        invalid_resolved_queue,
        "invalid resolved crash queue",
    ));

    let mut orphan_cursor = original.clone();
    orphan_cursor["messages"] = serde_json::json!([
        original["messages"][0].clone(),
        original["messages"][1].clone()
    ]);
    orphan_cursor["pending"] = serde_json::Value::Null;
    orphan_cursor["tool_queue"] = serde_json::json!([]);
    orphan_cursor["tool_cursor"] = serde_json::json!(1);
    cases.push(("orphan queue cursor", orphan_cursor, "orphan queue cursor"));

    let mut invalid_input = original.clone();
    invalid_input["pending_input"] = serde_json::json!({
        "id":"bad id","prompt":"","secret":true
    });
    cases.push((
        "invalid input request",
        invalid_input,
        "invalid input request",
    ));

    let mut invalid_pending_id = original.clone();
    invalid_pending_id["pending"]["call"]["id"] = "bad id".into();
    cases.push((
        "invalid pending id",
        invalid_pending_id,
        "invalid pending call id",
    ));

    let mut pending_queue_mismatch = original.clone();
    pending_queue_mismatch["pending"]["call"]["id"] = "other-call".into();
    cases.push((
        "pending queue mismatch",
        pending_queue_mismatch,
        "pending queue mismatch",
    ));

    let mut premature_verification = original.clone();
    premature_verification["pending"]["phase"] = "verification".into();
    cases.push((
        "premature verification",
        premature_verification,
        "premature verification",
    ));

    let mut multiple_waits = original.clone();
    multiple_waits["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "role":"tool","tool_call_id":"checkpoint-write",
            "name":"write_file","content":"resolved"
        }));
    multiple_waits["tool_queue"] = serde_json::json!([]);
    multiple_waits["tool_cursor"] = serde_json::json!(0);
    multiple_waits["pending"]["phase"] = "verification".into();
    multiple_waits["final_message"] = "verification pending".into();
    multiple_waits["pending_input"] = serde_json::json!({
        "id":"also-input","prompt":"must not coexist","secret":false
    });
    cases.push(("multiple waits", multiple_waits, "multiple wait payloads"));

    let mut phase_message_mismatch = original.clone();
    phase_message_mismatch["final_message"] = "unexpected final".into();
    cases.push((
        "phase final mismatch",
        phase_message_mismatch,
        "pending phase and final message mismatch",
    ));

    let mut missing_pending = original.clone();
    missing_pending["pending"] = serde_json::Value::Null;
    cases.push((
        "missing pending",
        missing_pending,
        "missing pending operation",
    ));

    let mut binding_mismatch = original.clone();
    binding_mismatch["pending"]["operation_digest"] = "wrong-digest".into();
    cases.push((
        "permission binding mismatch",
        binding_mismatch,
        "pending effect binding mismatch",
    ));

    for (name, payload, expected) in cases {
        let now = latte_core::wall_time_ms();
        let lease = engine
            .acquire_run_lease(run_id, &format!("agent-{run_id}"), now, 60_000)
            .unwrap();
        engine
            .persist_runtime_checkpoint(
                run_id,
                state.revision,
                &lease,
                &serde_json::to_string(&payload).unwrap(),
                now,
            )
            .unwrap();
        engine.release_lease(&lease).unwrap();
        let resumed = invoke(
            &scenario,
            &["--json", "resume", &run_id.to_string(), "--allow"],
        );
        assert_eq!(
            resumed.status.code(),
            Some(1),
            "case {name}; stdout={} stderr={}",
            String::from_utf8_lossy(&resumed.stdout),
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert!(
            json(&resumed)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "case {name}: {}",
            json(&resumed)
        );
        assert!(!scenario.root().join("must-not-be-written.txt").exists());
    }

    let now = latte_core::wall_time_ms();
    let lease = engine
        .acquire_run_lease(run_id, &format!("agent-{run_id}"), now, 60_000)
        .unwrap();
    let malformed = engine
        .persist_runtime_checkpoint(run_id, state.revision, &lease, "{", now)
        .unwrap_err();
    assert!(malformed.to_string().contains("EOF while parsing"));
    engine.release_lease(&lease).unwrap();

    let now = latte_core::wall_time_ms();
    let lease = engine
        .acquire_run_lease(run_id, &format!("agent-{run_id}"), now, 60_000)
        .unwrap();
    engine
        .persist_runtime_checkpoint(run_id, state.revision, &lease, &original_text, now)
        .unwrap();
    engine.release_lease(&lease).unwrap();
    drop(engine);
    let denied = invoke(
        &scenario,
        &["--json", "resume", &run_id.to_string(), "--deny"],
    );
    assert_eq!(denied.status.code(), Some(11));
    assert_eq!(
        json(&denied)["data"]["run"]["failure"]["code"],
        "permission_denied"
    );
    assert!(!scenario.root().join("must-not-be-written.txt").exists());
}
