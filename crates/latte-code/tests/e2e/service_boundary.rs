use super::support::{Scenario, json};
use latte_core::{
    FailureCode, IdSource, PermissionDecision, RunId, RunStatus, RuntimeCommand, SystemIdSource,
};
use latte_engine::EngineHandle;
use latte_headless::{
    provider::{
        FakeProvider, InputRequest, Provider, ProviderCapabilities, ProviderContext,
        ProviderFuture, ProviderRequest, ProviderResponse, ProviderUsage, ToolCall,
    },
    runtime::{AgentRuntime, RuntimeError, VerificationPlan},
    service::{CommandError, CommandResult, RuntimeCommandActor, RuntimeCommandService},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

fn build_engine(scenario: &Scenario) -> EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn verification() -> VerificationPlan {
    VerificationPlan {
        argv: vec!["/bin/pwd".into()],
        cwd: ".".into(),
        timeout_ms: 1_000,
        grace_ms: 50,
        stdout_cap: 1_024,
        stderr_cap: 1_024,
    }
}

fn response(message: &str) -> ProviderResponse {
    ProviderResponse {
        message: Some(message.into()),
        tool_calls: Vec::new(),
        input_request: None,
        usage: ProviderUsage::default(),
        finish_reason: None,
        provider_state: None,
    }
}

fn write_request(id: &str, path: &str) -> ProviderResponse {
    ProviderResponse {
        message: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": path,
                "content": "service boundary persisted\n",
                "create_intent": true
            }),
        }],
        input_request: None,
        usage: ProviderUsage::default(),
        finish_reason: None,
        provider_state: None,
    }
}

#[derive(Clone)]
struct InputProvider;

impl Provider for InputProvider {
    fn complete(&self, request: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
        Box::pin(async move {
            let answered = request
                .messages
                .iter()
                .any(|message| message.content() == Some("service-answer"));
            Ok(if answered {
                response("service consumed exact input")
            } else {
                ProviderResponse {
                    message: None,
                    tool_calls: Vec::new(),
                    input_request: Some(InputRequest {
                        id: "service-input".into(),
                        prompt: "service value?".into(),
                        secret: false,
                    }),
                    usage: ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                }
            })
        })
    }
}

#[derive(Clone)]
struct OutcomeProvider {
    response: ProviderResponse,
    capabilities: ProviderCapabilities,
}

impl Provider for OutcomeProvider {
    fn complete(&self, _: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }
}

fn run_from(result: CommandResult) -> Box<latte_core::RunState> {
    let CommandResult::Run(run) = result else {
        panic!("expected a run result")
    };
    run
}

fn final_show(scenario: &Scenario, run_id: RunId) -> std::process::Output {
    scenario.output(&["--json", "show", &run_id.to_string()], |command| {
        command.env("TEST_OPENAI_KEY", "service-boundary-secret");
    })
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_command_service_input_cancel_actor_and_error_matrix_is_final_cli_visible() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let service =
        RuntimeCommandService::new(engine.clone(), scenario.root(), verification(), || {
            Ok::<_, String>(InputProvider)
        });

    let run_id = match service
        .execute(RuntimeCommand::Run {
            prompt: "request input through the public service".into(),
        })
        .await
        .unwrap_err()
    {
        CommandError::Runtime(RuntimeError::InputRequired { run_id }) => run_id,
        error => panic!("unexpected input request result: {error}"),
    };
    let waiting = run_from(
        service
            .execute(RuntimeCommand::Show { run_id })
            .await
            .unwrap(),
    );
    assert_eq!(waiting.status, RunStatus::WaitingInput);
    assert!(matches!(
        service
            .execute(RuntimeCommand::ProvideInput {
                run_id,
                request_id: "wrong-request".into(),
                expected_revision: waiting.revision,
                value: "service-answer".into(),
            })
            .await,
        Err(CommandError::RequestMismatch)
    ));
    assert!(matches!(
        service
            .execute(RuntimeCommand::ProvideInput {
                run_id,
                request_id: "service-input".into(),
                expected_revision: waiting.revision.saturating_add(1),
                value: "service-answer".into(),
            })
            .await,
        Err(CommandError::Stale { .. })
    ));
    let completed = run_from(
        service
            .execute(RuntimeCommand::ProvideInput {
                run_id,
                request_id: "service-input".into(),
                expected_revision: waiting.revision,
                value: "service-answer".into(),
            })
            .await
            .unwrap(),
    );
    assert_eq!(completed.status, RunStatus::Completed);
    assert!(
        completed
            .handoff
            .as_ref()
            .unwrap()
            .summary
            .contains("service consumed exact input")
    );

    assert!(matches!(
        service.execute(RuntimeCommand::List).await.unwrap(),
        CommandResult::Runs(runs) if runs.iter().any(|run| run.run_id == run_id)
    ));
    assert!(matches!(
        service.execute(RuntimeCommand::Shutdown).await.unwrap(),
        CommandResult::Accepted
    ));
    assert!(matches!(
        service
            .execute(RuntimeCommand::Resume {
                run_id,
                expected_revision: waiting.revision,
            })
            .await,
        Err(CommandError::Stale { .. })
    ));

    let actor = RuntimeCommandActor::start(service.clone(), 0);
    assert!(matches!(
        actor.execute(RuntimeCommand::Show { run_id }).await.unwrap(),
        CommandResult::Run(run) if run.status == RunStatus::Completed
    ));
    assert!(matches!(
        actor.execute(RuntimeCommand::List).await.unwrap(),
        CommandResult::Runs(runs) if !runs.is_empty()
    ));
    assert!(matches!(
        actor.execute(RuntimeCommand::Shutdown).await.unwrap(),
        CommandResult::Accepted
    ));

    let queued_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let queued = engine
        .create_run(queued_id, latte_core::wall_time_ms())
        .unwrap();
    assert!(matches!(
        service
            .execute(RuntimeCommand::Cancel {
                run_id: queued_id,
                expected_revision: queued.revision,
            })
            .await,
        Err(CommandError::NotActive(id)) if id == queued_id
    ));
    assert!(matches!(
        service
            .execute(RuntimeCommand::Show {
                run_id: RunId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            })
            .await,
        Err(CommandError::Storage(_))
    ));
    assert!(matches!(
        service.reconcile_unknown_and_abort(queued_id, "missing-effect"),
        Err(CommandError::Storage(_))
    ));

    let unavailable = RuntimeCommandService::new(
        engine.clone(),
        scenario.root(),
        verification(),
        || -> Result<FakeProvider, String> { Err("provider factory unavailable".into()) },
    );
    assert!(matches!(
        unavailable
            .execute(RuntimeCommand::Run {
                prompt: "configuration must fail before mutation".into(),
            })
            .await,
        Err(CommandError::Storage(message)) if message.contains("provider configuration")
    ));

    drop(actor);
    drop(service);
    drop(engine);
    let shown = final_show(&scenario, run_id);
    assert!(shown.status.success());
    assert_eq!(json(&shown)["data"]["run"]["status"], "completed");
    let listed = scenario.output(&["--json", "list"], |command| {
        command.env("TEST_OPENAI_KEY", "service-boundary-secret");
    });
    assert!(listed.status.success());
    assert!(json(&listed)["data"]["runs"].as_array().unwrap().len() >= 2);

    let cancel_scenario = Scenario::new();
    cancel_scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let cancel_engine = build_engine(&cancel_scenario);
    let cancel_service = RuntimeCommandService::new(
        cancel_engine.clone(),
        cancel_scenario.root(),
        verification(),
        || Ok::<_, String>(InputProvider),
    );
    let cancel_id = match cancel_service
        .execute(RuntimeCommand::Run {
            prompt: "cancel this durable input request".into(),
        })
        .await
        .unwrap_err()
    {
        CommandError::Runtime(RuntimeError::InputRequired { run_id }) => run_id,
        error => panic!("unexpected cancel fixture result: {error}"),
    };
    let cancel_waiting = cancel_engine.show(cancel_id).unwrap();
    assert!(matches!(
        cancel_service
            .execute(RuntimeCommand::Cancel {
                run_id: cancel_id,
                expected_revision: cancel_waiting.revision,
            })
            .await
            .unwrap(),
        CommandResult::Accepted
    ));
    assert_eq!(
        cancel_engine.show(cancel_id).unwrap().failure.unwrap().code,
        FailureCode::Cancelled
    );
    assert!(
        cancel_engine
            .runtime_checkpoint(cancel_id)
            .unwrap()
            .is_none()
    );
    drop(cancel_service);
    drop(cancel_engine);
    let cancelled = final_show(&cancel_scenario, cancel_id);
    assert_eq!(cancelled.status.code(), Some(1));
    assert_eq!(
        json(&cancelled)["data"]["run"]["failure"]["code"],
        "cancelled"
    );
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_agent_runtime_provider_capability_and_outcome_matrix_is_final_cli_visible() {
    let base = response("unused completion");
    let cases = [
        (
            "tool capability disabled",
            base.clone(),
            ProviderCapabilities {
                tools: false,
                parallel_tool_calls: false,
                input_request: true,
            },
        ),
        (
            "provider state unsupported",
            ProviderResponse {
                provider_state: Some(serde_json::json!({"cursor":"unsupported"})),
                ..base.clone()
            },
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: true,
            },
        ),
        (
            "empty provider outcome",
            ProviderResponse {
                message: None,
                tool_calls: Vec::new(),
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: true,
            },
        ),
        (
            "undeclared input capability",
            ProviderResponse {
                message: None,
                tool_calls: Vec::new(),
                input_request: Some(InputRequest {
                    id: "public-input".into(),
                    prompt: "value?".into(),
                    secret: false,
                }),
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: false,
            },
        ),
        (
            "invalid input request",
            ProviderResponse {
                message: None,
                tool_calls: Vec::new(),
                input_request: Some(InputRequest {
                    id: "bad id".into(),
                    prompt: String::new(),
                    secret: false,
                }),
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: true,
            },
        ),
    ];
    for (prompt, outcome, capabilities) in cases {
        let scenario = Scenario::new();
        scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
        let engine = build_engine(&scenario);
        let runtime = AgentRuntime::new(
            engine,
            OutcomeProvider {
                response: outcome,
                capabilities,
            },
            scenario.root(),
            verification(),
        )
        .with_verification(verification());
        let state = runtime.run(prompt).await.unwrap();
        assert_eq!(state.status, RunStatus::Failed, "{prompt}");
        assert!(matches!(
            runtime.provide_input(state.run_id, "none", "").await,
            Err(RuntimeError::Engine(message)) if message.contains("1..=16384")
        ));
        runtime.cancel();
        let listed = scenario.output(&["--json", "list"], |_| {});
        assert!(listed.status.success());
        assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 1);
    }

    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let cancellation = latte_engine::CancellationToken::new();
    cancellation.cancel();
    let cancelled = AgentRuntime::new(
        engine.clone(),
        OutcomeProvider {
            response: response("must not complete"),
            capabilities: ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: true,
            },
        },
        scenario.root(),
        verification(),
    )
    .with_cancellation(cancellation)
    .run("cancel before provider completion")
    .await
    .unwrap();
    assert_eq!(cancelled.status, RunStatus::Interrupted);

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_command_service_permission_allow_and_deny_are_final_cli_visible() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let scripts = Arc::new(Mutex::new(VecDeque::from([
        vec![write_request("service-allow", "service-allow.txt")],
        vec![response("service permission allow completed")],
    ])));
    let factory_scripts = Arc::clone(&scripts);
    let service =
        RuntimeCommandService::new(engine.clone(), scenario.root(), verification(), move || {
            factory_scripts
                .lock()
                .unwrap()
                .pop_front()
                .map(FakeProvider::scripted)
                .ok_or_else(|| "provider script exhausted".into())
        });

    let allow_id = match service
        .execute(RuntimeCommand::Run {
            prompt: "exercise service permission allow".into(),
        })
        .await
        .unwrap_err()
    {
        CommandError::Runtime(RuntimeError::PermissionRequired { run_id }) => run_id,
        error => panic!("unexpected allow fixture result: {error}"),
    };
    let allow_waiting = engine.show(allow_id).unwrap();
    let allow_request = allow_waiting.pending_permission.as_ref().unwrap();
    assert!(matches!(
        service
            .execute(RuntimeCommand::ResolvePermission {
                run_id: allow_id,
                request_id: "wrong-effect".into(),
                expected_revision: allow_waiting.revision,
                decision: PermissionDecision::Allow,
            })
            .await,
        Err(CommandError::RequestMismatch)
    ));
    let allowed = run_from(
        service
            .execute(RuntimeCommand::ResolvePermission {
                run_id: allow_id,
                request_id: allow_request.request_id.clone(),
                expected_revision: allow_waiting.revision,
                decision: PermissionDecision::Allow,
            })
            .await
            .unwrap(),
    );
    assert_eq!(allowed.status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("service-allow.txt")).unwrap(),
        "service boundary persisted\n"
    );

    assert!(scripts.lock().unwrap().is_empty());

    drop(service);
    drop(engine);
    let allowed = final_show(&scenario, allow_id);
    assert!(allowed.status.success());
    assert_eq!(json(&allowed)["data"]["run"]["status"], "completed");

    let deny_scenario = Scenario::new();
    deny_scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let deny_engine = build_engine(&deny_scenario);
    let deny_service = RuntimeCommandService::new(
        deny_engine.clone(),
        deny_scenario.root(),
        verification(),
        || {
            Ok::<_, String>(FakeProvider::scripted([write_request(
                "service-deny",
                "service-deny.txt",
            )]))
        },
    );
    let deny_id = match deny_service
        .execute(RuntimeCommand::Run {
            prompt: "exercise service permission deny".into(),
        })
        .await
        .unwrap_err()
    {
        CommandError::Runtime(RuntimeError::PermissionRequired { run_id }) => run_id,
        error => panic!("unexpected deny fixture result: {error}"),
    };
    let deny_waiting = deny_engine.show(deny_id).unwrap();
    let deny_request = deny_waiting.pending_permission.as_ref().unwrap();
    let denied = run_from(
        deny_service
            .execute(RuntimeCommand::ResolvePermission {
                run_id: deny_id,
                request_id: deny_request.request_id.clone(),
                expected_revision: deny_waiting.revision,
                decision: PermissionDecision::Deny,
            })
            .await
            .unwrap(),
    );
    assert_eq!(denied.status, RunStatus::Failed);
    assert_eq!(
        denied.failure.as_ref().unwrap().code,
        FailureCode::PermissionDenied
    );
    assert!(!deny_scenario.root().join("service-deny.txt").exists());
    drop(deny_service);
    drop(deny_engine);
    let denied = final_show(&deny_scenario, deny_id);
    assert_eq!(denied.status.code(), Some(11));
    assert_eq!(
        json(&denied)["data"]["run"]["failure"]["code"],
        "permission_denied"
    );
}

#[derive(Clone)]
struct SlowProvider;

impl Provider for SlowProvider {
    fn complete(&self, _: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(response("late service response"))
        })
    }
}

#[tokio::test]
async fn public_command_actor_cancel_lane_interrupts_active_runtime_durably() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = build_engine(&scenario);
    let service =
        RuntimeCommandService::new(engine.clone(), scenario.root(), verification(), || {
            Ok::<_, String>(SlowProvider)
        });
    let actor = RuntimeCommandActor::start(service, 1);
    let run_actor = actor.clone();
    let running = tokio::spawn(async move {
        run_actor
            .execute(RuntimeCommand::Run {
                prompt: "cancel active public actor request".into(),
            })
            .await
    });
    let mut observed_running = false;
    for _ in 0..300 {
        if engine
            .list()
            .unwrap()
            .iter()
            .any(|run| run.status == RunStatus::Running)
        {
            observed_running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(observed_running, "active service run never became durable");
    let state = engine
        .list()
        .unwrap()
        .into_iter()
        .find(|run| run.status == RunStatus::Running)
        .unwrap();
    assert!(matches!(
        actor
            .execute(RuntimeCommand::Cancel {
                run_id: state.run_id,
                expected_revision: state.revision,
            })
            .await
            .unwrap(),
        CommandResult::Accepted
    ));
    let interrupted = run_from(running.await.unwrap().unwrap());
    assert_eq!(interrupted.status, RunStatus::Interrupted);
    assert_eq!(
        engine.show(state.run_id).unwrap().status,
        RunStatus::Interrupted
    );
}
