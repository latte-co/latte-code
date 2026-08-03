use super::support::{PtySession, Scenario};
use latte_core::{IdSource, SystemIdSource, ThreadId, ThreadLifecycle, ThreadProviderBindingV2};
use latte_headless::{
    provider::{FakeProvider, InputRequest, ProviderResponse, ProviderUsage, ToolCall},
    registry::{ProviderBinding, ResolvedProvider},
    runtime::VerificationPlan,
    thread::{
        ThreadHistoryPolicy, ThreadProviderFactory, ThreadRuntimeError, ThreadRuntimeService,
    },
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";

fn binding(provider: &str, model: &str) -> ThreadProviderBindingV2 {
    ThreadProviderBindingV2 {
        version: 1,
        provider_name: provider.into(),
        provider_type: "openai-chat".into(),
        protocol: "openai-chat-completions-v1".into(),
        model: model.into(),
        config_fingerprint: format!("config-{provider}"),
        tools_fingerprint: "tools-none".into(),
        aliases: BTreeMap::new(),
        credential_ref_id: format!("env:{}_KEY", provider.to_ascii_uppercase()),
        data_scope_id: "workspace".into(),
        credential_generation: 1,
    }
}

fn completion(text: &str) -> ProviderResponse {
    ProviderResponse {
        message: Some(text.into()),
        tool_calls: Vec::new(),
        input_request: None,
        usage: ProviderUsage::default(),
        finish_reason: None,
        provider_state: None,
    }
}

fn factory(responses: impl IntoIterator<Item = ProviderResponse>) -> ThreadProviderFactory {
    let provider = Arc::new(FakeProvider::scripted(responses));
    Arc::new(move |selected| {
        Ok(ResolvedProvider {
            provider: provider.clone(),
            binding: ProviderBinding {
                version: selected.version,
                provider_name: selected.provider_name.clone(),
                provider_type: selected.provider_type.clone(),
                protocol: selected.protocol.clone(),
                model: selected.model.clone(),
                config_fingerprint: selected.config_fingerprint.clone(),
                tools_fingerprint: selected.tools_fingerprint.clone(),
                aliases: selected.aliases.clone(),
            },
        })
    })
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_thread_service_state_and_configuration_matrix_is_final_cli_visible() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let service = ThreadRuntimeService::new(
        engine.clone(),
        scenario.root(),
        ThreadHistoryPolicy::default(),
        factory([
            completion("first public child completed"),
            completion("follow-up public child completed"),
        ]),
    )
    .with_progress_sink(Arc::new(|_| {}))
    .with_verification(VerificationPlan {
        argv: vec!["/bin/pwd".into()],
        cwd: ".".into(),
        timeout_ms: 1_000,
        grace_ms: 50,
        stdout_cap: 1_024,
        stderr_cap: 1_024,
    })
    .with_lease_ttl_ms(120_000);

    let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let ready = service
        .start(
            thread_id,
            "exercise public thread service states".into(),
            binding("primary", "model-a"),
        )
        .await
        .unwrap();
    assert_eq!(ready.lifecycle, ThreadLifecycle::Ready);
    assert_eq!(
        service
            .switch_model(thread_id, ready.revision, &ready.binding)
            .unwrap()
            .revision,
        ready.revision
    );

    let mut invalid = ready.binding.clone();
    invalid.provider_name.clear();
    assert!(matches!(
        service.switch_model(thread_id, ready.revision, &invalid),
        Err(ThreadRuntimeError::ProviderConfiguration(_))
    ));
    let secondary = binding("secondary", "model-b");
    assert!(matches!(
        service.switch_model(thread_id, ready.revision + 1, &secondary),
        Err(ThreadRuntimeError::InvalidState)
    ));
    let switched = service
        .switch_model(thread_id, ready.revision, &secondary)
        .unwrap();
    assert_eq!(switched.binding, secondary);
    assert!(matches!(
        service
            .follow_up(thread_id, ready.revision, "stale follow-up".into())
            .await,
        Err(ThreadRuntimeError::InvalidState)
    ));
    let followed = service
        .follow_up(
            thread_id,
            switched.revision,
            "accepted follow-up through switched model".into(),
        )
        .await
        .unwrap();
    assert_eq!(followed.lifecycle, ThreadLifecycle::Ready);
    assert_eq!(followed.runs.len(), 2);

    assert!(matches!(
        service
            .provide_input(
                thread_id,
                followed.revision,
                "not-waiting".into(),
                "value".into(),
            )
            .await,
        Err(ThreadRuntimeError::InvalidState)
    ));
    assert!(matches!(
        service
            .resolve_permission(thread_id, followed.revision, "not-waiting".into(), false,)
            .await,
        Err(ThreadRuntimeError::InvalidState)
    ));
    assert!(matches!(
        service.reconcile_unknown_effect(thread_id, "not-unknown"),
        Err(ThreadRuntimeError::InvalidState)
    ));
    service.cancel(thread_id);
    assert!(matches!(
        service.cancel_durable(thread_id),
        Err(ThreadRuntimeError::InvalidState)
    ));
    let missing = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    assert!(
        service
            .follow_up(missing, 0, "missing".into())
            .await
            .is_err()
    );
    assert!(service.cancel_durable(missing).is_err());

    let constrained = ThreadRuntimeService::new(
        engine.clone(),
        scenario.root(),
        ThreadHistoryPolicy {
            max_request_bytes: 1,
            max_input_bytes: 2,
            reserved_output_bytes: 1,
            context_cap_bytes: 1,
        },
        factory([completion("unreachable")]),
    );
    assert!(matches!(
        constrained
            .start(
                ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                "too large".into(),
                binding("primary", "model-a"),
            )
            .await,
        Err(ThreadRuntimeError::History(_))
    ));

    let unavailable: ThreadProviderFactory =
        Arc::new(|_| Err("configured provider unavailable".into()));
    let unavailable_service = ThreadRuntimeService::new(
        engine.clone(),
        scenario.root(),
        ThreadHistoryPolicy::default(),
        unavailable,
    );
    let failed = unavailable_service
        .start(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            "durable provider configuration failure".into(),
            binding("primary", "model-a"),
        )
        .await
        .unwrap();
    assert_eq!(failed.lifecycle, ThreadLifecycle::Ready);
    assert!(failed.transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Failure
            && entry.text.contains("selected model could not be started")
    }));

    let no_verification = ThreadRuntimeService::new(
        engine.clone(),
        scenario.root(),
        ThreadHistoryPolicy::default(),
        factory([
            ProviderResponse {
                message: None,
                tool_calls: vec![ToolCall {
                    id: "public-service-write".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path":"service-write.txt",
                        "content":"changed without verification\n",
                        "create_intent":true
                    }),
                }],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            completion("must not complete without verification"),
        ]),
    );
    let waiting = no_verification
        .start(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            "workspace change without verification".into(),
            binding("primary", "model-a"),
        )
        .await
        .unwrap();
    assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
    let request_id = match waiting.pending.as_ref().unwrap() {
        latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
        latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
    };
    let failed_verification = no_verification
        .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
        .await
        .unwrap();
    assert_eq!(failed_verification.lifecycle, ThreadLifecycle::Failed);
    assert!(scenario.root().join("service-write.txt").exists());

    let invalid_outcomes = [
        ProviderResponse {
            message: None,
            tool_calls: Vec::new(),
            input_request: None,
            usage: ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        },
        ProviderResponse {
            message: None,
            tool_calls: Vec::new(),
            input_request: Some(InputRequest {
                id: "secret-input".into(),
                prompt: "secret?".into(),
                secret: true,
            }),
            usage: ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        },
        ProviderResponse {
            message: Some("invalid tool envelope".into()),
            tool_calls: vec![ToolCall {
                id: "bad id".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path":"missing"}),
            }],
            input_request: None,
            usage: ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        },
    ];
    for (index, outcome) in invalid_outcomes.into_iter().enumerate() {
        let invalid_service = ThreadRuntimeService::new(
            engine.clone(),
            scenario.root(),
            ThreadHistoryPolicy::default(),
            factory([outcome]),
        );
        let invalid = invalid_service
            .start(
                ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
                format!("invalid provider outcome {index}"),
                binding("primary", "model-a"),
            )
            .await
            .unwrap();
        assert_eq!(invalid.lifecycle, ThreadLifecycle::Failed);
    }

    std::fs::create_dir(scenario.root().join("unreadable-as-file")).unwrap();
    let uncertain_tool = ThreadRuntimeService::new(
        engine.clone(),
        scenario.root(),
        ThreadHistoryPolicy::default(),
        factory([ProviderResponse {
            message: None,
            tool_calls: vec![ToolCall {
                id: "missing-read".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path":"unreadable-as-file"}),
            }],
            input_request: None,
            usage: ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        }]),
    );
    let reconciliation = uncertain_tool
        .start(
            ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            "uncertain filesystem result requires reconciliation".into(),
            binding("primary", "model-a"),
        )
        .await
        .unwrap();
    assert_eq!(
        reconciliation.lifecycle,
        ThreadLifecycle::ReconciliationRequired
    );
    assert!(reconciliation.transcript.entries.iter().any(|entry| {
        entry.kind == latte_core::TranscriptKind::Failure
            && entry
                .payload
                .as_ref()
                .is_some_and(|payload| payload["status"] == "unknown")
    }));

    drop(service);
    drop(engine);
    let mut tui = PtySession::spawn(scenario.command(&["tui"]));
    assert!(tui.wait_for_output(TUI_READY, Duration::from_secs(5)));
    tui.write(format!("/resume {}\r", reconciliation.thread_id).as_bytes());
    assert!(tui.wait_for_output(b"Reconciliation required", Duration::from_secs(5)));
    tui.write(F10);
    assert!(tui.finish(Duration::from_secs(5)).0.success());
}
