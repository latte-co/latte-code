use super::support::{ProviderReply, Scenario, ScriptedProvider, json};
use latte_core::{RunId, RunStatus, Transition};
use latte_engine::EngineHandle;
use std::time::{SystemTime, UNIX_EPOCH};

fn wall_now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn fixture_engine(scenario: &Scenario) -> EngineHandle {
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn run_id(output: &std::process::Output) -> RunId {
    let text = json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();
    RunId::from_uuid(uuid::Uuid::parse_str(&text).unwrap())
}

fn invoke(scenario: &Scenario, args: &[&str]) -> std::process::Output {
    scenario.output(args, |command| {
        command.env("TEST_OPENAI_KEY", "resume-boundary-secret");
    })
}

fn advance_same_owner_fencing_token(engine: &EngineHandle, run_id: RunId) -> u64 {
    let owner = format!("agent-{run_id}");
    let now = wall_now_ms();
    let current = engine.acquire_lease(&owner, now, 60_000).unwrap();
    let advanced = engine
        .acquire_lease(&owner, current.expires_at_ms(), 120_000)
        .unwrap();
    assert!(advanced.fencing_token() > current.fencing_token());
    advanced.fencing_token()
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_cli_reissues_expired_process_and_tool_permissions_before_exactly_once_execution() {
    let process_scenario = Scenario::new();
    let process = serde_json::json!({
        "shell": "printf rebound-process > process-rebound.txt",
        "cwd": ".",
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 1_024,
        "stderr_cap": 1_024
    });
    let process_provider = ScriptedProvider::start([
        ProviderReply::tool_call("rebound-process", "process", &process),
        ProviderReply::completion("rebound process complete"),
    ]);
    process_scenario.write_config(process_provider.endpoint(), r#"["/bin/pwd"]"#);
    let first = invoke(
        &process_scenario,
        &["--json", "run", "execute after permission rebind"],
    );
    assert_eq!(first.status.code(), Some(10));
    let process_run = run_id(&first);
    assert!(!process_scenario.root().join("process-rebound.txt").exists());
    assert_eq!(process_provider.requests().len(), 1);

    let process_engine = fixture_engine(&process_scenario);
    let new_process_token = advance_same_owner_fencing_token(&process_engine, process_run);
    let first_resume = invoke(
        &process_scenario,
        &["--json", "resume", &process_run.to_string(), "--allow"],
    );
    assert_eq!(first_resume.status.code(), Some(10));
    let refreshed = json(&first_resume)["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(refreshed.contains(&process_run.to_string()));
    let rebound_state = process_engine.show(process_run).unwrap();
    assert_eq!(rebound_state.status, RunStatus::WaitingPermission);
    assert!(
        rebound_state
            .pending_permission
            .as_ref()
            .unwrap()
            .request_id
            .ends_with(&format!("-lease-{new_process_token}"))
    );
    assert!(!process_scenario.root().join("process-rebound.txt").exists());
    assert_eq!(process_provider.requests().len(), 1);

    let second_resume = invoke(
        &process_scenario,
        &["--json", "resume", &process_run.to_string(), "--allow"],
    );
    assert!(
        second_resume.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second_resume.stdout),
        String::from_utf8_lossy(&second_resume.stderr)
    );
    assert_eq!(json(&second_resume)["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(process_scenario.root().join("process-rebound.txt")).unwrap(),
        "rebound-process"
    );
    process_provider.assert_consumed();
    assert_eq!(process_provider.requests().len(), 2);

    let tool_scenario = Scenario::new();
    let write = serde_json::json!({
        "path": "created-after-rebind.txt",
        "content": "rebound-tool\n",
        "create_intent": true
    });
    let tool_provider = ScriptedProvider::start([
        ProviderReply::tool_call("rebound-write", "write_file", &write),
        ProviderReply::completion("rebound tool complete"),
    ]);
    tool_scenario.write_config(tool_provider.endpoint(), r#"["/bin/pwd"]"#);
    let first = invoke(
        &tool_scenario,
        &["--json", "run", "create after permission rebind"],
    );
    assert_eq!(first.status.code(), Some(10));
    let tool_run = run_id(&first);
    let tool_engine = fixture_engine(&tool_scenario);
    let new_tool_token = advance_same_owner_fencing_token(&tool_engine, tool_run);

    let first_resume = invoke(
        &tool_scenario,
        &["--json", "resume", &tool_run.to_string(), "--allow"],
    );
    assert_eq!(first_resume.status.code(), Some(10));
    let rebound_state = tool_engine.show(tool_run).unwrap();
    assert_eq!(rebound_state.status, RunStatus::WaitingPermission);
    assert!(
        rebound_state
            .pending_permission
            .as_ref()
            .unwrap()
            .request_id
            .ends_with(&format!("-lease-{new_tool_token}"))
    );
    assert!(
        !tool_scenario
            .root()
            .join("created-after-rebind.txt")
            .exists()
    );
    assert_eq!(tool_provider.requests().len(), 1);

    let second_resume = invoke(
        &tool_scenario,
        &["--json", "resume", &tool_run.to_string(), "--allow"],
    );
    assert!(
        second_resume.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second_resume.stdout),
        String::from_utf8_lossy(&second_resume.stderr)
    );
    assert_eq!(json(&second_resume)["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(tool_scenario.root().join("created-after-rebind.txt")).unwrap(),
        "rebound-tool\n"
    );
    tool_provider.assert_consumed();
    assert_eq!(tool_provider.requests().len(), 2);
}

#[cfg(unix)]
#[test]
fn final_cli_resumes_public_interrupted_checkpoint_at_verification_without_provider_reentry() {
    let scenario = Scenario::new();
    let provider =
        ScriptedProvider::start([ProviderReply::completion("source checkpoint complete")]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let source = invoke(&scenario, &["--json", "run", "produce a valid checkpoint"]);
    assert!(source.status.success());
    let source_run = RunId::from_uuid(
        uuid::Uuid::parse_str(json(&source)["data"]["run"]["run_id"].as_str().unwrap()).unwrap(),
    );
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);

    let engine = fixture_engine(&scenario);
    let checkpoint = engine
        .runtime_checkpoint(source_run)
        .unwrap()
        .expect("completed final binary persists a resumable checkpoint");
    let now = wall_now_ms();
    let source_owner = format!("agent-{source_run}");
    let lease = engine.acquire_lease(&source_owner, now, 120_000).unwrap();
    let interrupted_run = RunId::from_uuid(uuid::Uuid::now_v7());
    engine.create_run(interrupted_run, now + 1).unwrap();
    let running = engine
        .apply_transition(interrupted_run, 0, Transition::Start, now + 2, &lease)
        .unwrap();
    let cancelling = engine
        .apply_transition(
            interrupted_run,
            running.revision,
            Transition::Cancel,
            now + 3,
            &lease,
        )
        .unwrap();
    let interrupted = engine
        .apply_transition(
            interrupted_run,
            cancelling.revision,
            Transition::Interrupt,
            now + 4,
            &lease,
        )
        .unwrap();
    engine
        .persist_runtime_checkpoint(
            interrupted_run,
            interrupted.revision,
            &lease,
            &checkpoint,
            now + 5,
        )
        .unwrap();
    engine.release_lease(&lease).unwrap();

    let resumed = invoke(
        &scenario,
        &["--json", "resume", &interrupted_run.to_string(), "--allow"],
    );
    assert!(
        resumed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(json(&resumed)["status"], "completed");
    assert_eq!(
        json(&resumed)["data"]["run"]["handoff"]["summary"],
        "source checkpoint complete"
    );
    assert_eq!(provider.requests().len(), 1);
}

#[cfg(unix)]
#[test]
fn interrupted_final_message_reissues_verification_permission_before_completion() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion(
        "verification permission checkpoint",
    )]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let source = invoke(
        &scenario,
        &["--json", "run", "create final verification checkpoint"],
    );
    assert!(source.status.success());
    let source_run = RunId::from_uuid(
        uuid::Uuid::parse_str(json(&source)["data"]["run"]["run_id"].as_str().unwrap()).unwrap(),
    );
    provider.assert_consumed();

    let engine = fixture_engine(&scenario);
    let checkpoint = engine.runtime_checkpoint(source_run).unwrap().unwrap();
    let now = wall_now_ms();
    let lease = engine
        .acquire_lease(&format!("agent-{source_run}"), now, 120_000)
        .unwrap();
    let interrupted_run = RunId::from_uuid(uuid::Uuid::now_v7());
    engine.create_run(interrupted_run, now + 1).unwrap();
    let running = engine
        .apply_transition(interrupted_run, 0, Transition::Start, now + 2, &lease)
        .unwrap();
    let cancelling = engine
        .apply_transition(
            interrupted_run,
            running.revision,
            Transition::Cancel,
            now + 3,
            &lease,
        )
        .unwrap();
    let interrupted = engine
        .apply_transition(
            interrupted_run,
            cancelling.revision,
            Transition::Interrupt,
            now + 4,
            &lease,
        )
        .unwrap();
    engine
        .persist_runtime_checkpoint(
            interrupted_run,
            interrupted.revision,
            &lease,
            &checkpoint,
            now + 5,
        )
        .unwrap();
    engine.release_lease(&lease).unwrap();
    drop(engine);

    scenario.write_config(provider.endpoint(), r#"["/bin/sh","-c","exit 0"]"#);
    let waiting = invoke(
        &scenario,
        &["--json", "resume", &interrupted_run.to_string(), "--allow"],
    );
    assert_eq!(waiting.status.code(), Some(10));
    assert_eq!(json(&waiting)["error"]["code"], "permission_required");
    let shown = invoke(&scenario, &["--json", "show", &interrupted_run.to_string()]);
    assert_eq!(shown.status.code(), Some(10));
    assert_eq!(json(&shown)["data"]["run"]["status"], "waiting_permission");

    let completed = invoke(
        &scenario,
        &["--json", "resume", &interrupted_run.to_string(), "--allow"],
    );
    assert!(
        completed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["data"]["run"]["status"], "completed");
    assert_eq!(provider.requests().len(), 1);
}
