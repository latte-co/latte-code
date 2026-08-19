use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

fn git(scenario: &Scenario, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(scenario.root())
        .env("HOME", scenario.home())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn invoke(scenario: &Scenario, args: &[&str]) -> std::process::Output {
    scenario.output(args, |command| {
        command.env("TEST_OPENAI_KEY", "runtime-convergence-secret");
    })
}

fn tool_messages(request: &Value) -> Vec<&Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect()
}

fn assistant_tool_calls(request: &Value) -> Vec<(&str, &str)> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| {
            message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|call| {
                    (
                        call["id"].as_str().unwrap(),
                        call["function"]["name"].as_str().unwrap(),
                    )
                })
        })
        .collect()
}

fn assert_declares_read_only_tools(request: &Value) {
    let names = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for expected in [
        "read_file",
        "search",
        "list_directory",
        "git_diff",
        "read_project_manifest",
    ] {
        assert!(
            names.contains(&expected),
            "provider request omitted {expected}: {names:?}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn headless_multi_round_read_only_history_converges_in_one_shot() {
    let scenario = Scenario::new();
    scenario.init_git();
    std::fs::create_dir_all(scenario.root().join("src/nested")).unwrap();
    std::fs::write(
        scenario.root().join("src/lib.rs"),
        "pub fn round_sentinel() -> &'static str { \"round-sentinel\" }\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("src/nested/mod.rs"),
        "pub const NESTED: &str = \"listed-by-runtime-convergence\";\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("Cargo.toml"),
        "[package]\nname = \"runtime-convergence-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        scenario.root().join("notes.txt"),
        "needle before\nround-sentinel before\n",
    )
    .unwrap();
    git(
        &scenario,
        &[
            "add",
            "Cargo.toml",
            "src/lib.rs",
            "src/nested/mod.rs",
            "notes.txt",
        ],
    );
    git(
        &scenario,
        &["commit", "--quiet", "-m", "runtime convergence fixture"],
    );
    std::fs::write(
        scenario.root().join("notes.txt"),
        "needle after\nround-sentinel after\n",
    )
    .unwrap();

    let read = serde_json::json!({
        "path": "notes.txt",
        "max_output": 4_096
    });
    let search = serde_json::json!({
        "query": "round-sentinel",
        "regex": false,
        "max_results": 20,
        "max_output": 4_096
    });
    let list = serde_json::json!({
        "path": "src",
        "max_entries": 20
    });
    let diff = serde_json::json!({
        "path": "notes.txt",
        "max_output": 4_096
    });
    let manifest = serde_json::json!({
        "max_output": 4_096
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("round-read", "read_file", &read),
        ProviderReply::tool_call("round-search", "search", &search),
        ProviderReply::tool_call("round-list", "list_directory", &list),
        ProviderReply::tool_call("round-diff", "git_diff", &diff),
        ProviderReply::tool_call("round-manifest", "read_project_manifest", &manifest),
        ProviderReply::completion("multi-round read-only convergence verified"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/bin/sh","-c","test -f notes.txt && grep -q 'needle after' notes.txt && git diff -- notes.txt | grep -q '^+round-sentinel after'"]"#,
    );

    // v2: a read-only run converges in one shot — no file change means no
    // verification permission phase, so the embedded `run` exits 0 completed.
    let first = invoke(
        &scenario,
        &[
            "--json",
            "run",
            "--focus",
            "src/lib.rs",
            "inspect the workspace over several durable provider rounds",
        ],
    );
    assert_eq!(
        first.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(json(&first)["status"], "completed");
    let session_id = json(&first)["data"]["session"]["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        json(&first)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    assert!(provider.wait_for_calls(6, Duration::from_secs(5)));
    provider.assert_consumed();

    let requests = provider.requests();
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(
            request.headers["authorization"],
            "Bearer runtime-convergence-secret"
        );
        assert_declares_read_only_tools(&request.body);
    }
    assert_eq!(requests.len(), 6);
    assert!(
        requests[0].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("runtime-convergence-fixture")
    );
    assert_eq!(
        requests[0].body["messages"][1]["content"],
        "inspect the workspace over several durable provider rounds"
    );

    let expected_rounds = [
        ("round-read", "read_file"),
        ("round-search", "search"),
        ("round-list", "list_directory"),
        ("round-diff", "git_diff"),
        ("round-manifest", "read_project_manifest"),
    ];
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            tool_messages(&request.body).len(),
            index,
            "request {} did not retain exactly the prior tool rounds",
            index + 1
        );
        assert_eq!(
            assistant_tool_calls(&request.body),
            expected_rounds[..index],
            "request {} changed ordered assistant tool history",
            index + 1
        );
    }

    let final_history = tool_messages(&requests[5].body);
    assert_eq!(final_history[0]["tool_call_id"], "round-read");
    assert_eq!(final_history[0]["name"], "read_file");
    assert!(
        final_history[0]["content"]
            .as_str()
            .unwrap()
            .contains("needle after")
    );
    assert_eq!(final_history[1]["tool_call_id"], "round-search");
    assert_eq!(final_history[1]["name"], "search");
    assert!(
        final_history[1]["content"]
            .as_str()
            .unwrap()
            .contains("notes.txt")
    );
    assert_eq!(final_history[2]["tool_call_id"], "round-list");
    assert_eq!(final_history[2]["name"], "list_directory");
    let list_result = final_history[2]["content"].as_str().unwrap();
    assert!(list_result.contains("lib.rs"));
    assert!(list_result.contains("nested"));
    assert_eq!(final_history[3]["tool_call_id"], "round-diff");
    assert_eq!(final_history[3]["name"], "git_diff");
    let diff_result = final_history[3]["content"].as_str().unwrap();
    for expected in ["notes.txt", "2 insertions", "2 deletions"] {
        assert!(
            diff_result.contains(expected),
            "git diff result omitted {expected}: {diff_result}"
        );
    }
    assert_eq!(final_history[4]["tool_call_id"], "round-manifest");
    assert_eq!(final_history[4]["name"], "read_project_manifest");
    assert!(
        final_history[4]["content"]
            .as_str()
            .unwrap()
            .contains("runtime-convergence-fixture")
    );

    // The completed session is durable and visible through the v2 show/list
    // contract (`data.session` / `data.sessions[]`).
    let shown = invoke(&scenario, &["--json", "show", &session_id]);
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    assert_eq!(
        json(&shown)["data"]["session"]["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "completion")
            .and_then(|entry| entry["payload"]["handoff"]["summary"].as_str()),
        Some("multi-round read-only convergence verified")
    );
    let listed = invoke(&scenario, &["--json", "list"]);
    assert!(listed.status.success());
    let listed_json = json(&listed);
    let sessions = listed_json["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["thread_id"], session_id);
    assert_eq!(sessions[0]["lifecycle"], "ready");
    assert_eq!(provider.requests().len(), 6);
}

#[test]
fn headless_provider_error_after_bounded_rounds_fails_durably_without_unbounded_requests() {
    let scenario = Scenario::new();
    std::fs::write(
        scenario.root().join("bounded.txt"),
        "bounded round fixture\n",
    )
    .unwrap();
    let read = serde_json::json!({"path": "bounded.txt", "max_output": 1024});
    // The v2 thread runtime has no agent step limit: the loop is bounded by the
    // provider. 16 read-only tool rounds are served, then the provider errors,
    // which must terminate the run durably without an unbounded extra request.
    let provider = ScriptedProvider::start(
        (0..16)
            .map(|index| {
                ProviderReply::tool_call(&format!("bounded-round-{index}"), "read_file", &read)
            })
            .chain(std::iter::once(ProviderReply::error(
                400,
                "simulated bounded-round termination",
            ))),
    );
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);

    let output = invoke(
        &scenario,
        &[
            "--json",
            "run",
            "keep reading until the bounded agent loop stops",
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["status"], "failed");
    let session = &json(&output)["data"]["session"];
    assert_eq!(session["runs"][0]["status"], "failed");
    // The provider error is recorded durably in the transcript failure card.
    assert!(
        session["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "failure"
                && entry["text"].as_str().unwrap_or("").contains("provider"))
    );
    provider.assert_consumed();
    let requests = provider.requests();
    // 16 tool rounds + 1 terminating provider error, and no retry storm.
    assert_eq!(requests.len(), 17);
    assert_eq!(tool_messages(&requests[0].body).len(), 0);
    assert_eq!(tool_messages(&requests[16].body).len(), 16);
    assert_eq!(
        assistant_tool_calls(&requests[16].body).last(),
        Some(&("bounded-round-15", "read_file"))
    );

    let session_id = session["thread_id"].as_str().unwrap().to_owned();
    let shown = invoke(&scenario, &["--json", "show", &session_id]);
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert_eq!(provider.requests().len(), 17);
}
