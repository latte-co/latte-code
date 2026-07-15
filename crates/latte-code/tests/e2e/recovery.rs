use super::{
    headless::{assert_tool_result_reached_provider, write_file_reply},
    support::{ProviderReply, Scenario, ScriptedProvider, json},
};

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

#[test]
#[allow(clippy::too_many_lines)]
fn run_waiting_resume_allow_and_deny_are_durable_across_processes() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("done")]);
    scenario.write_config_with_database(
        provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "state/nested/custom.db",
    );
    let execute = |args: &[&str], include_secret: bool| {
        scenario.output(args, |command| {
            if include_secret {
                command.env("TEST_OPENAI_KEY", "secret");
            }
        })
    };

    let waiting = execute(&["--json", "run", "finish safely"], true);
    let run_id = run_id_from_waiting(&waiting);
    let completed = execute(&["--json", "resume", &run_id, "--allow"], true);
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["status"], "completed");
    assert_eq!(
        json(&completed)["data"]["run"]["handoff"]["evidence"][0]["status"],
        "passed"
    );
    let shown = execute(&["--json", "show", &run_id], true);
    assert!(shown.status.success());
    assert_eq!(json(&shown)["data"]["run"]["status"], "completed");
    let listed = execute(&["list"], true);
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&run_id));
    assert!(scenario.root().join("state/nested/custom.db").exists());
    assert!(!scenario.root().join(".latte/latte-code.db").exists());
    provider.assert_consumed();

    let denied_scenario = Scenario::new();
    let denied_provider = ScriptedProvider::start([ProviderReply::completion("done")]);
    denied_scenario.write_config_with_database(
        denied_provider.endpoint(),
        r#"["/usr/bin/true"]"#,
        "state/deny.db",
    );
    let waiting = denied_scenario.output(&["--json", "run", "deny safely"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let denied_run_id = run_id_from_waiting(&waiting);
    let waiting_show = denied_scenario.output(&["--json", "show", &denied_run_id], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let effect_id = json(&waiting_show)["data"]["run"]["pending_permission"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let denied = denied_scenario.output(&["--json", "resume", &denied_run_id, "--deny"], |_| {});
    assert_eq!(
        denied.status.code(),
        Some(11),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(json(&denied)["status"], "denied");
    assert_eq!(json(&denied)["data"]["run"]["status"], "failed");
    assert_eq!(
        json(&denied)["data"]["run"]["failure"]["code"],
        "permission_denied"
    );
    let shown_denied = denied_scenario.output(&["--json", "show", &denied_run_id], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(json(&shown_denied)["data"]["run"]["status"], "failed");
    assert!(json(&shown_denied)["data"]["run"]["pending_permission"].is_null());
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(denied_scenario.root())
        .database_path(denied_scenario.root().join("state/deny.db"))
        .build()
        .unwrap();
    assert_eq!(
        engine.effect_status(&effect_id).unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
    assert!(
        engine
            .runtime_checkpoint(latte_core::RunId::from_uuid(
                uuid::Uuid::parse_str(&denied_run_id).unwrap(),
            ))
            .unwrap()
            .is_none()
    );
    denied_provider.assert_consumed();
}

#[test]
fn write_file_deny_never_mutates_and_never_reenters_the_provider() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([write_file_reply("deny-write")]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);

    let waiting = scenario.output(&["--json", "run", "create new.txt"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let run_id = run_id_from_waiting(&waiting);
    assert!(!scenario.root().join("new.txt").exists());
    let waiting_show = scenario.output(&["--json", "show", &run_id], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let effect_id = json(&waiting_show)["data"]["run"]["pending_permission"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let denied = scenario.output(&["--json", "resume", &run_id, "--deny"], |_| {});
    assert_eq!(denied.status.code(), Some(11));
    assert_eq!(json(&denied)["status"], "denied");
    assert_eq!(json(&denied)["data"]["run"]["status"], "failed");
    assert!(!scenario.root().join("new.txt").exists());
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert_eq!(
        engine.effect_status(&effect_id).unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
}

#[test]
fn write_file_allow_resumes_in_a_new_process_verifies_and_completes_once() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        write_file_reply("allow-write"),
        ProviderReply::completion("done"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);

    let waiting = scenario.output(&["--json", "run", "create new.txt"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let run_id = run_id_from_waiting(&waiting);
    assert!(!scenario.root().join("new.txt").exists());

    let completed = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert!(
        completed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["status"], "completed");
    assert_eq!(
        json(&completed)["data"]["run"]["handoff"]["evidence"][0]["status"],
        "passed"
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_tool_result_reached_provider(&requests[1].body);

    let repeated = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(repeated.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn failed_verification_is_durable_and_never_claims_completion() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        write_file_reply("failing-verification-write"),
        ProviderReply::completion("done"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/usr/bin/grep","-q","not-present","new.txt"]"#,
    );

    let waiting = scenario.output(&["--json", "run", "create new.txt"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    let run_id = run_id_from_waiting(&waiting);
    let failed = scenario.output(&["--json", "resume", &run_id, "--allow"], |command| {
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(
        failed.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert_eq!(json(&failed)["status"], "failed");
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    let shown = scenario.output(&["--json", "show", &run_id], |_| {});
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert_ne!(json(&shown)["data"]["run"]["status"], "completed");
    assert_eq!(
        json(&shown)["data"]["run"]["failure"]["code"],
        "runtime_failed"
    );
    assert_eq!(
        json(&shown)["data"]["run"]["failure"]["message"],
        "verification failed"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_tool_result_reached_provider(&requests[1].body);
}
