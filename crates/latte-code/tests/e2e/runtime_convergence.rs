use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use serde_json::Value;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

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

fn waiting_run_id(output: &std::process::Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(10),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(output)["status"], "waiting");
    assert_eq!(json(output)["error"]["code"], "permission_required");
    json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
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
fn headless_multi_round_read_only_history_converges_after_cross_process_verification_allow() {
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
    let run_id = waiting_run_id(&first);
    assert!(provider.wait_for_calls(6, Duration::from_secs(5)));
    provider.assert_consumed();

    let waiting = invoke(&scenario, &["--json", "show", &run_id]);
    assert_eq!(waiting.status.code(), Some(10));
    assert_eq!(json(&waiting)["status"], "waiting");
    assert_eq!(
        json(&waiting)["data"]["run"]["status"],
        "waiting_permission"
    );
    let pending = &json(&waiting)["data"]["run"]["pending_permission"];
    assert!(
        pending["request_id"]
            .as_str()
            .unwrap()
            .starts_with("verify-")
    );
    assert_eq!(pending["description"], "allow verification command");
    assert_eq!(pending["operation_digest"].as_str().unwrap().len(), 64);

    let waiting_list = invoke(&scenario, &["--json", "list"]);
    assert!(waiting_list.status.success());
    let waiting_runs = json(&waiting_list)["data"]["runs"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(waiting_runs.len(), 1);
    assert_eq!(waiting_runs[0]["run_id"], run_id);
    assert_eq!(waiting_runs[0]["status"], "waiting_permission");
    assert_eq!(provider.requests().len(), 6);

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

    let completed = invoke(&scenario, &["--json", "resume", &run_id, "--allow"]);
    assert!(
        completed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["status"], "completed");
    assert_eq!(json(&completed)["data"]["run"]["run_id"], run_id);
    assert_eq!(json(&completed)["data"]["run"]["status"], "completed");
    assert_eq!(
        json(&completed)["data"]["run"]["handoff"]["summary"],
        "multi-round read-only convergence verified"
    );
    assert_eq!(
        json(&completed)["data"]["run"]["handoff"]["evidence"][0]["status"],
        "passed"
    );
    assert_eq!(provider.requests().len(), 6);

    let mut show_command = scenario.command(&["--json", "show", &run_id]);
    let mut list_command = scenario.command(&["--json", "list"]);
    show_command.stdout(Stdio::piped()).stderr(Stdio::piped());
    list_command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let show_child = show_command.spawn().unwrap();
    let list_child = list_command.spawn().unwrap();
    let shown = show_child.wait_with_output().unwrap();
    let listed = list_child.wait_with_output().unwrap();
    assert!(shown.status.success());
    assert!(listed.status.success());
    assert_eq!(json(&shown)["data"]["run"]["status"], "completed");
    assert_eq!(
        json(&shown)["data"]["run"]["handoff"]["summary"],
        "multi-round read-only convergence verified"
    );
    let runs = json(&listed)["data"]["runs"].as_array().unwrap().clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["run_id"], run_id);
    assert_eq!(runs[0]["status"], "completed");
    assert_eq!(provider.requests().len(), 6);

    let redundant = invoke(&scenario, &["--json", "resume", &run_id, "--allow"]);
    assert_eq!(redundant.status.code(), Some(1));
    assert_eq!(json(&redundant)["status"], "failed");
    assert!(
        !json(&redundant)["error"]["message"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(provider.requests().len(), 6);
}
