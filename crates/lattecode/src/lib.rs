#![allow(clippy::semicolon_if_nothing_returned)]
use latte_core::{FailureCode, PermissionDecision, RunState, RunStatus, RuntimeCommand};
use latte_engine::{EngineBuilder, StorageError};
use latte_headless::{
    HeadlessCommand,
    provider::OpenAiProvider,
    runtime::{AgentRuntime, RuntimeError, VerificationPlan},
};
use serde_json::{Value, json};
use std::io::IsTerminal;

const JSON_VERSION: u8 = 1;
const EXIT_COMPLETED: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 4;
const EXIT_WAITING: i32 = 10;
const EXIT_DENIED: i32 = 11;
const EXIT_INTERNAL: i32 = 70;
const EXIT_INTERRUPTED: i32 = 130;
const HELP: &str = "Lattecode agent\n\nUsage:\n  lattecode tui\n  lattecode [--json] run [--focus <path>] <prompt>\n  lattecode [--json] resume <run-id> (--allow|--deny)\n  lattecode [--json] show <run-id>\n  lattecode [--json] list\n  lattecode [--json] --help\n\nWith no arguments Lattecode opens the TUI when stdin/stdout are terminals; otherwise it prints this help. run/resume require LATTE_OPENAI_ENDPOINT, LATTE_OPENAI_MODEL, LATTE_OPENAI_API_KEY, and LATTE_VERIFY_ARGV.";

struct EngineProjection {
    engine: latte_engine::EngineHandle,
    subscription: latte_engine::Subscription,
}

fn command_requires_configuration(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::Run { .. }
            | RuntimeCommand::Resume { .. }
            | RuntimeCommand::ProvideInput { .. }
            | RuntimeCommand::ResolvePermission {
                decision: PermissionDecision::Allow,
                ..
            }
    )
}
impl latte_tui::ProjectionClient for EngineProjection {
    fn snapshots(&mut self) -> Result<Vec<RunState>, String> {
        self.engine.list().map_err(|e| e.to_string())
    }
    fn poll(&mut self) -> latte_tui::ProjectionPoll {
        match self.subscription.try_recv() {
            Ok(Some(_)) => latte_tui::ProjectionPoll::Event,
            Ok(None) => latte_tui::ProjectionPoll::Empty,
            Err(latte_engine::SubscriptionError::Lagged(n)) => latte_tui::ProjectionPoll::Lagged(n),
            Err(latte_engine::SubscriptionError::Closed) => latte_tui::ProjectionPoll::Closed,
        }
    }
}

/// Executes the process-level CLI and returns its documented exit code.
///
/// Keeping orchestration in the library lets process-level integration tests
/// exercise the exact controller used by the binary.
pub async fn run_cli() -> i32 {
    execute().await
}

#[allow(clippy::too_many_lines)]
async fn execute() -> i32 {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.first().is_some_and(|value| value == "--json");
    if json {
        args.remove(0);
    }
    let implicit_tui = args.is_empty()
        && !json
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if implicit_tui || matches!(args.as_slice(), [command] if command == "tui") {
        return execute_tui();
    }
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "help")
        )
    {
        if json {
            emit_data("completed", &json!({ "help": HELP }));
        } else {
            println!("{HELP}");
        }
        return EXIT_COMPLETED;
    }

    let command = match latte_headless::parse(&args) {
        Ok(command) => command,
        Err(error) => return emit_error(json, "usage", "usage", &error, EXIT_USAGE, true),
    };
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "current_directory",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    let state_dir = root.join(".latte");
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        return emit_error(
            json,
            "internal",
            "state_directory",
            &error.to_string(),
            EXIT_INTERNAL,
            false,
        );
    }
    let engine = match EngineBuilder::new()
        .workspace_root(&root)
        .database_path(state_dir.join("lattecode.db"))
        .build()
    {
        Ok(engine) => engine,
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "engine_initialization",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };

    match command {
        HeadlessCommand::Show { run_id } => match engine.show(run_id) {
            Ok(run) => render_run(&run, json),
            Err(StorageError::RunNotFound(_)) => emit_error(
                json,
                "not_found",
                "run_not_found",
                &format!("run {run_id} was not found"),
                EXIT_NOT_FOUND,
                false,
            ),
            Err(error) => emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            ),
        },
        HeadlessCommand::List => match engine.list() {
            Ok(runs) => {
                if json {
                    emit_data("completed", &json!({ "runs": runs }));
                } else {
                    for run in runs {
                        println!("{}\t{:?}\trev {}", run.run_id, run.status, run.revision)
                    }
                }
                EXIT_COMPLETED
            }
            Err(error) => emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            ),
        },
        HeadlessCommand::Resume {
            run_id,
            allow: false,
        } => deny_headless(&engine, run_id, json),
        command @ (HeadlessCommand::Run { .. } | HeadlessCommand::Resume { .. }) => {
            let provider = match provider() {
                Ok(value) => value,
                Err(error) => {
                    return emit_error(json, "usage", "configuration", &error, EXIT_USAGE, false);
                }
            };
            let plan = match verification() {
                Ok(value) => value,
                Err(error) => {
                    return emit_error(json, "usage", "configuration", &error, EXIT_USAGE, false);
                }
            };
            let runtime = AgentRuntime::new(engine, provider, &root, plan);
            let result = match command {
                HeadlessCommand::Run { prompt, focus } => {
                    runtime.run_with_focus(&prompt, focus.as_deref()).await
                }
                HeadlessCommand::Resume { run_id, allow } => runtime.resume(run_id, allow).await,
                _ => unreachable!(),
            };
            match result {
                Ok(run) => render_run(&run, json),
                Err(RuntimeError::PermissionRequired { run_id }) => emit_error(
                    json,
                    "waiting",
                    "permission_required",
                    &format!("permission required for run {run_id}"),
                    EXIT_WAITING,
                    false,
                ),
                Err(error) => emit_error(
                    json,
                    "failed",
                    "runtime",
                    &error.to_string(),
                    EXIT_FAILED,
                    false,
                ),
            }
        }
    }
}

fn deny_headless(
    engine: &latte_engine::EngineHandle,
    run_id: latte_core::RunId,
    json: bool,
) -> i32 {
    let state = match engine.show(run_id) {
        Ok(state) => state,
        Err(StorageError::RunNotFound(_)) => {
            return emit_error(
                json,
                "not_found",
                "run_not_found",
                &format!("run {run_id} was not found"),
                EXIT_NOT_FOUND,
                false,
            );
        }
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    if state.status != RunStatus::WaitingPermission || state.pending_permission.is_none() {
        return emit_error(
            json,
            "failed",
            "invalid_state",
            "run is not waiting for permission",
            EXIT_FAILED,
            false,
        );
    }
    let now = wall_time_ms();
    let lease = match engine.acquire_lease(&format!("agent-{run_id}"), now, 60_000) {
        Ok(lease) => lease,
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    let denied = engine.deny_waiting_permission(run_id, state.revision, &lease, now);
    let _ = engine.release_lease(&lease);
    match denied {
        Ok(run) => render_run(&run, json),
        Err(error) => emit_error(
            json,
            "failed",
            "storage",
            &error.to_string(),
            EXIT_FAILED,
            false,
        ),
    }
}

fn wall_time_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[allow(clippy::too_many_lines)]
fn execute_tui() -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "interactive TUI requires a TTY; use --json list/show/run/resume for non-interactive use"
        );
        return EXIT_USAGE;
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_INTERNAL;
        }
    };
    let state_dir = root.join(".latte");
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        eprintln!("{error}");
        return EXIT_INTERNAL;
    }
    let engine = match EngineBuilder::new()
        .workspace_root(&root)
        .database_path(state_dir.join("lattecode.db"))
        .build()
    {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_INTERNAL;
        }
    };
    let (plan, configuration_error) = match verification() {
        Ok(plan) => (plan, None),
        Err(error) => (
            VerificationPlan {
                argv: Vec::new(),
                cwd: ".".into(),
                timeout_ms: 0,
                grace_ms: 0,
                stdout_cap: 0,
                stderr_cap: 0,
            },
            Some(error),
        ),
    };
    let service = latte_headless::service::RuntimeCommandService::new(
        engine.clone(),
        &root,
        plan,
        provider as fn() -> Result<OpenAiProvider, String>,
    );
    let command_actor = latte_headless::service::RuntimeCommandActor::start(service.clone(), 32);
    let (feedback_tx, feedback_rx) = std::sync::mpsc::channel();
    let mut projection = EngineProjection {
        engine: engine.clone(),
        subscription: engine.subscribe(),
    };
    match latte_tui::run_with_feedback(
        &mut projection,
        move |action| match action {
            latte_tui::UiAction::Command(command) => {
                if command_requires_configuration(&command)
                    && let Some(error) = &configuration_error
                {
                    let _ = feedback_tx.send(Err(format!("configuration: {error}")));
                    return Ok(());
                }
                let service = command_actor.clone();
                let feedback = feedback_tx.clone();
                tokio::spawn(async move {
                    let message = service
                        .execute(command)
                        .await
                        .map(|_| "command completed".into())
                        .map_err(|e| e.to_string());
                    let _ = feedback.send(message);
                });
                Ok(())
            }
            latte_tui::UiAction::ReconcileUnknown { run_id, effect_id } => {
                let service = service.clone();
                let feedback = feedback_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let message = service
                        .reconcile_unknown_and_abort(run_id, &effect_id)
                        .map(|_| "unknown effect reconciled; run aborted".into())
                        .map_err(|e| e.to_string());
                    let _ = feedback.send(message);
                });
                Ok(())
            }
            latte_tui::UiAction::RefreshSnapshots | latte_tui::UiAction::Quit => Ok(()),
        },
        &feedback_rx,
    ) {
        Ok(()) => EXIT_COMPLETED,
        Err(error) => {
            eprintln!("{error}");
            if matches!(error, latte_tui::TuiError::Interrupted) {
                EXIT_INTERRUPTED
            } else if matches!(error, latte_tui::TuiError::NonTty) {
                EXIT_USAGE
            } else {
                EXIT_INTERNAL
            }
        }
    }
}

fn provider() -> Result<OpenAiProvider, String> {
    let endpoint =
        std::env::var("LATTE_OPENAI_ENDPOINT").map_err(|_| "missing LATTE_OPENAI_ENDPOINT")?;
    let model = std::env::var("LATTE_OPENAI_MODEL").map_err(|_| "missing LATTE_OPENAI_MODEL")?;
    let key = std::env::var("LATTE_OPENAI_API_KEY").map_err(|_| "missing LATTE_OPENAI_API_KEY")?;
    OpenAiProvider::new(endpoint, model, key, std::time::Duration::from_secs(60))
        .map_err(|error| error.to_string())
}

fn verification() -> Result<VerificationPlan, String> {
    let raw =
        std::env::var("LATTE_VERIFY_ARGV").map_err(|_| "missing LATTE_VERIFY_ARGV JSON array")?;
    let argv: Vec<String> = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid LATTE_VERIFY_ARGV: {error}"))?;
    if argv.is_empty() {
        return Err("LATTE_VERIFY_ARGV must not be empty".into());
    }
    Ok(VerificationPlan {
        argv,
        cwd: ".".into(),
        timeout_ms: 120_000,
        grace_ms: 250,
        stdout_cap: 16 * 1024,
        stderr_cap: 16 * 1024,
    })
}

fn render_run(run: &RunState, json_output: bool) -> i32 {
    let (status, code) = outcome(run);
    if json_output {
        emit_data(status, &json!({ "run": run }));
    } else {
        println!(
            "run {}: {:?} (revision {})",
            run.run_id, run.status, run.revision
        );
        if let Some(handoff) = &run.handoff {
            println!("{}", handoff.summary)
        }
    }
    code
}

fn outcome(run: &RunState) -> (&'static str, i32) {
    match run.status {
        RunStatus::Completed => ("completed", EXIT_COMPLETED),
        RunStatus::WaitingPermission | RunStatus::WaitingInput => ("waiting", EXIT_WAITING),
        RunStatus::Interrupted | RunStatus::Cancelling => ("interrupted", EXIT_INTERRUPTED),
        RunStatus::Failed
            if run
                .failure
                .as_ref()
                .is_some_and(|failure| failure.code == FailureCode::PermissionDenied) =>
        {
            ("denied", EXIT_DENIED)
        }
        RunStatus::Failed => ("failed", EXIT_FAILED),
        RunStatus::Queued | RunStatus::Running => ("internal", EXIT_INTERNAL),
    }
}

fn emit_data(status: &str, data: &Value) {
    println!(
        "{}",
        json!({ "version": JSON_VERSION, "status": status, "data": data })
    );
}

fn emit_error(
    json_output: bool,
    status: &str,
    code: &str,
    message: &str,
    exit: i32,
    show_help: bool,
) -> i32 {
    if json_output {
        println!(
            "{}",
            json!({
                "version": JSON_VERSION,
                "status": status,
                "error": { "code": code, "message": message }
            })
        );
    } else if show_help {
        eprintln!("{message}\n\n{HELP}");
    } else {
        eprintln!("{message}");
    }
    exit
}

#[cfg(test)]
mod tests {
    use super::{
        EXIT_COMPLETED, EXIT_DENIED, EXIT_FAILED, EXIT_INTERRUPTED, EXIT_WAITING,
        command_requires_configuration, outcome,
    };
    use latte_core::{
        FailureCode, IdSource, PermissionDecision, Retryability, RunFailure, RunId, RunState,
        RunStatus, RuntimeCommand, SystemIdSource,
    };

    fn state(status: RunStatus) -> RunState {
        let mut state =
            RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        state.status = status;
        state
    }

    #[test]
    fn run_statuses_have_stable_exit_codes() {
        assert_eq!(outcome(&state(RunStatus::Completed)).1, EXIT_COMPLETED);
        assert_eq!(
            outcome(&state(RunStatus::WaitingPermission)).1,
            EXIT_WAITING
        );
        assert_eq!(outcome(&state(RunStatus::Failed)).1, EXIT_FAILED);
        assert_eq!(outcome(&state(RunStatus::Interrupted)).1, EXIT_INTERRUPTED);
    }

    #[test]
    fn permission_denial_has_distinct_exit_code() {
        let mut run = state(RunStatus::Failed);
        run.failure = Some(RunFailure {
            code: FailureCode::PermissionDenied,
            message: "denied".into(),
            retryability: Retryability::Terminal,
        });
        assert_eq!(outcome(&run).1, EXIT_DENIED);
    }

    #[test]
    fn only_execution_capabilities_require_provider_configuration() {
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        assert!(command_requires_configuration(&RuntimeCommand::Run {
            prompt: "x".into()
        }));
        assert!(command_requires_configuration(
            &RuntimeCommand::ResolvePermission {
                run_id,
                request_id: "r".into(),
                expected_revision: 1,
                decision: PermissionDecision::Allow,
            }
        ));
        assert!(!command_requires_configuration(
            &RuntimeCommand::ResolvePermission {
                run_id,
                request_id: "r".into(),
                expected_revision: 1,
                decision: PermissionDecision::Deny,
            }
        ));
        assert!(!command_requires_configuration(&RuntimeCommand::Cancel {
            run_id,
            expected_revision: 1,
        }));
        assert!(!command_requires_configuration(&RuntimeCommand::Show {
            run_id
        }));
        assert!(!command_requires_configuration(&RuntimeCommand::List));
    }
}
