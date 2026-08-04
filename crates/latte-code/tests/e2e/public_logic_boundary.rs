use super::support::Scenario;
use latte_core::{
    CommandEnvelope, CommandId, CompletionPolicy, Evidence, FailureCode, Handoff, HeadlessOutcome,
    IdSource, PendingInput, PendingPermission, Retryability, RunFailure, RunId, RunState,
    RunStatus, RuntimeCommand, SystemIdSource, ThreadCommand, ThreadCommandEnvelope,
    ThreadCommandId, ThreadLifecycle, Transition, TransitionError, VerificationStatus,
    redact_thread_text, redact_thread_value, valid_openai_chat_input_request_id,
    valid_openai_chat_opaque_id, valid_openai_chat_tool_call_id,
};
use latte_headless::{
    HeadlessCommand, context, parse, registry::ProviderRegistry, render_placeholder,
};
use latte_tui::command::{SlashResolution, resolve_slash, slash_suggestions};
use std::path::Path;

fn run_id() -> RunId {
    RunId::from_uuid(SystemIdSource::default().next_uuid_v7())
}

fn handoff(status: VerificationStatus) -> Handoff {
    Handoff {
        summary: "public state transition handoff".into(),
        files_changed: vec!["src/public.rs".into()],
        evidence: vec![Evidence {
            name: "public-check".into(),
            status,
            summary: "checked through the public boundary".into(),
        }],
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_core_state_context_and_registry_boundaries_are_final_cli_compatible() {
    let ids = SystemIdSource::default();
    let command_id = CommandId::from_uuid(ids.next_uuid_v7());
    let envelope = CommandEnvelope::new(command_id, RuntimeCommand::List);
    assert_eq!(envelope.command_id.as_uuid(), command_id.as_uuid());
    assert_eq!(envelope.protocol_version, latte_core::PROTOCOL_VERSION);
    let thread_command_id = ThreadCommandId::from_uuid(ids.next_uuid_v7());
    let thread_envelope = ThreadCommandEnvelope::new(
        thread_command_id,
        ThreadCommand::Cancel {
            thread_id: latte_core::ThreadId::from_uuid(ids.next_uuid_v7()),
            expected_thread_revision: 1,
            expected_run_revision: 1,
        },
    );
    assert_eq!(thread_envelope.command_id, thread_command_id);
    assert_eq!(
        thread_envelope.protocol_version,
        latte_core::THREAD_PROTOCOL_VERSION
    );
    for (status, code) in [
        (HeadlessOutcome::Success, 0),
        (HeadlessOutcome::Failed, 1),
        (HeadlessOutcome::UsageError, 2),
        (HeadlessOutcome::Interrupted, 130),
        (HeadlessOutcome::Cancelled, 130),
        (HeadlessOutcome::InternalError, 70),
    ] {
        assert_eq!(status.exit_code(), code);
    }
    assert!(ThreadLifecycle::Ready.accepts_follow_up());
    assert!(!ThreadLifecycle::Running.accepts_follow_up());
    for valid in ["opaque", "call_123", "req-1"] {
        assert!(valid_openai_chat_opaque_id(valid));
        assert!(valid_openai_chat_tool_call_id(valid));
        assert!(valid_openai_chat_input_request_id(valid));
    }
    let oversized = "x".repeat(257);
    for invalid in ["", "has space", "line\nbreak", oversized.as_str()] {
        assert!(!valid_openai_chat_opaque_id(invalid));
    }
    let redacted = redact_thread_text("ok\x1b[31m token=secret-value-12345678901234567890\x07");
    assert!(!redacted.contains("secret-value"));
    assert!(!redacted.contains('\x1b'));
    let redacted_value = redact_thread_value(serde_json::json!({
        "authorization": "Bearer secret",
        "nested": [{"api_key":"secret"}],
        "normal": "visible"
    }));
    assert_eq!(redacted_value["authorization"], "[REDACTED]");
    assert_eq!(redacted_value["nested"][0]["api_key"], "[REDACTED]");
    assert_eq!(redacted_value["normal"], "visible");
    assert!(matches!(
        resolve_slash("ordinary prompt"),
        SlashResolution::Prompt
    ));
    assert!(matches!(resolve_slash("/"), SlashResolution::Prompt));
    assert!(matches!(resolve_slash("/New"), SlashResolution::Prompt));
    assert!(matches!(
        resolve_slash("/new unexpected"),
        SlashResolution::ValidationError(_)
    ));
    assert!(matches!(
        resolve_slash("/resume exact title"),
        SlashResolution::Command { argument, .. } if argument == "exact title"
    ));
    assert!(slash_suggestions("prompt").is_empty());
    assert!(slash_suggestions("/bad name").is_empty());
    assert!(slash_suggestions(&format!("/{}", "x".repeat(65))).is_empty());
    assert_eq!(slash_suggestions("/q")[0].name, "quit");

    let queued = RunState::queued(run_id());
    assert!(matches!(
        queued.transition(1, Transition::Start),
        Err(TransitionError::StaleRevision { .. })
    ));
    assert!(matches!(
        queued.transition(0, Transition::Interrupt),
        Err(TransitionError::Invalid {
            from: RunStatus::Queued
        })
    ));
    let running = queued.transition(0, Transition::Start).unwrap();
    let waiting_permission = running
        .transition(
            1,
            Transition::RequestPermission(PendingPermission {
                request_id: "permission-1".into(),
                operation_digest: "digest-1".into(),
                description: "public permission".into(),
            }),
        )
        .unwrap();
    assert!(matches!(
        waiting_permission.transition(
            2,
            Transition::ResolvePermission {
                request_id: "wrong".into(),
                allowed: true,
            }
        ),
        Err(TransitionError::MismatchedRequest)
    ));
    let refreshed = waiting_permission
        .transition(
            2,
            Transition::RefreshPermission(PendingPermission {
                request_id: "permission-2".into(),
                operation_digest: "digest-2".into(),
                description: "refreshed".into(),
            }),
        )
        .unwrap();
    let denied = refreshed
        .transition(
            3,
            Transition::ResolvePermission {
                request_id: "permission-2".into(),
                allowed: false,
            },
        )
        .unwrap();
    assert_eq!(denied.status, RunStatus::Failed);
    assert_eq!(
        denied.failure.as_ref().unwrap().code,
        FailureCode::PermissionDenied
    );

    let retryable = running
        .transition(
            1,
            Transition::Fail(RunFailure {
                code: FailureCode::RuntimeFailed,
                message: "retry".into(),
                retryability: Retryability::Retryable,
            }),
        )
        .unwrap();
    assert_eq!(
        retryable.transition(2, Transition::Resume).unwrap().status,
        RunStatus::Queued
    );
    let waiting_input = running
        .transition(
            1,
            Transition::RequestInput(PendingInput {
                request_id: "input-1".into(),
                prompt: "value?".into(),
            }),
        )
        .unwrap();
    assert!(matches!(
        waiting_input.transition(
            2,
            Transition::ProvideInput {
                request_id: "wrong".into(),
            }
        ),
        Err(TransitionError::MismatchedRequest)
    ));
    let running_again = waiting_input
        .transition(
            2,
            Transition::ProvideInput {
                request_id: "input-1".into(),
            },
        )
        .unwrap();
    let cancelling = running_again.transition(3, Transition::Cancel).unwrap();
    assert_eq!(
        cancelling
            .transition(4, Transition::Interrupt)
            .unwrap()
            .status,
        RunStatus::Interrupted
    );
    let verification_failed = running
        .transition(
            1,
            Transition::Complete {
                handoff: handoff(VerificationStatus::Failed),
                policy: CompletionPolicy::VerificationRequired,
            },
        )
        .unwrap();
    assert_eq!(verification_failed.status, RunStatus::Failed);
    let completed = running
        .transition(
            1,
            Transition::Complete {
                handoff: handoff(VerificationStatus::Passed),
                policy: CompletionPolicy::VerificationRequired,
            },
        )
        .unwrap();
    assert!(matches!(
        completed.transition(2, Transition::Cancel),
        Err(TransitionError::CompletedImmutable)
    ));

    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("AGENTS.md"), "root instructions").unwrap();
    std::fs::write(scenario.root().join("Cargo.toml"), "[workspace]").unwrap();
    std::fs::create_dir_all(scenario.root().join("src/deep")).unwrap();
    std::fs::write(scenario.root().join("src/AGENTS.md"), "nested instructions").unwrap();
    let bundle = context::build(
        scenario.root(),
        Some(Path::new("src/deep/missing.rs")),
        4_096,
    )
    .unwrap();
    assert_eq!(bundle.sources, ["AGENTS.md", "src/AGENTS.md", "Cargo.toml"]);
    assert!(context::build(scenario.root(), None, 7).unwrap().truncated);
    for invalid in [Path::new(""), Path::new("../outside"), Path::new("/tmp")] {
        assert_eq!(
            context::build(scenario.root(), Some(invalid), 100)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    let registry = ProviderRegistry::parse_jsonc(
        r#"{
            version: 1,
            default_model: "primary/local-a",
            providers: {
                primary: {
                    type: "openai-chat",
                    models: {
                        "local-a": { name: "backend-a", options: { context_window: 4096 } },
                        "local-b": { options: { reasoning_effort: "high", max_tokens: 512 } }
                    },
                    base_url: "http://127.0.0.1:9",
                    api_key: { source: "env", name: "PATH" }
                }
            }
        }"#,
    )
    .unwrap();
    assert_eq!(registry.default_name(), Some("primary"));
    assert_eq!(registry.default_model(), Some("local-a"));
    assert_eq!(registry.model_catalog().len(), 2);
    assert!(registry.resolve_default(&[]).is_ok());
    let resolved = registry.resolve_model("primary", "local-b", &[]).unwrap();
    assert!(registry.resolve_bound(&resolved.binding, &[]).is_ok());
    let mut mismatched = resolved.binding.clone();
    mismatched.config_fingerprint = "changed".into();
    assert!(registry.resolve_bound(&mismatched, &[]).is_err());
    mismatched.provider_name = "missing".into();
    assert!(registry.resolve_bound(&mismatched, &[]).is_err());
    assert!(registry.resolve_model("missing", "local-a", &[]).is_err());
    assert!(registry.resolve_model("primary", "missing", &[]).is_err());
    let bound = registry
        .thread_binding_for_model("primary", "local-a", &[])
        .unwrap();
    assert!(registry.resolve_thread_bound(&bound, &[]).is_ok());
    let mut changed = bound;
    changed.credential_generation += 1;
    assert!(registry.resolve_thread_bound(&changed, &[]).is_err());

    let mut missing_model_binding = resolved.binding.clone();
    missing_model_binding.model = "removed-model".into();
    assert!(registry.resolve_bound(&missing_model_binding, &[]).is_err());

    let provider_with_derived_thread_scope = ProviderRegistry::parse_jsonc(
        r#"{
            version: 1,
            default_model: "primary/model-a",
            providers: {
                primary: {
                    type: "openai-chat",
                    models: ["model-a"],
                    endpoint: "http://127.0.0.1:9/chat/completions",
                    api_key: { source: "env", name: "PATH" }
                }
            }
        }"#,
    )
    .unwrap();
    let derived = provider_with_derived_thread_scope
        .thread_binding_for_default(&[])
        .unwrap();
    assert_eq!(derived.credential_ref_id, "env:PATH");
    assert_eq!(derived.data_scope_id, "workspace");
    assert_eq!(derived.credential_generation, 1);

    let invalid_provider_configs = [
        r#"{version:2,default_model:"",providers:{}}"#,
        r#"{version:1,default_model:"x/m",providers:{"":{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"malformed",providers:{}}"#,
        r#"{version:1,default_model:"/m",providers:{}}"#,
        r#"{version:1,default_model:"missing/m",providers:{}}"#,
        r#"{version:1,default_model:"p/missing",providers:{p:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:[],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],base_url:"http://127.0.0.1:9",endpoint:"http://127.0.0.1:9/chat/completions",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"},timeout_ms:0}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"},max_attempts:11}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"},temperature:3}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:{m:{options:{context_window:0}}},endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:{m:{options:{context_window:16,max_tokens:16}}},endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:{m:{options:{reasoning_effort:""}}},endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
        r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:{m:{name:""}},endpoint:"http://127.0.0.1:9",api_key:{source:"env",name:"PATH"}}}}"#,
    ];
    for source in invalid_provider_configs {
        assert!(ProviderRegistry::parse_jsonc(source).is_err(), "{source}");
    }

    scenario.write_config("http://127.0.0.1:1", r#"["/bin/pwd"]"#);
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    assert_eq!(
        parse(&["run".into(), "--focus".into()]),
        Err("--focus requires a path".into())
    );
    assert!(parse(&["run".into(), "fix".into(), "--focus".into()]).is_err());
    assert!(parse(&["run".into()]).is_err());
    assert!(render_placeholder(&HeadlessCommand::List, &engine).contains("No runs"));
    assert!(
        render_placeholder(
            &HeadlessCommand::Run {
                prompt: "x".into(),
                focus: None,
            },
            &engine,
        )
        .contains("not implemented")
    );
    assert!(
        render_placeholder(&HeadlessCommand::Show { run_id: run_id() }, &engine).contains("lookup")
    );
    assert!(
        render_placeholder(
            &HeadlessCommand::Resume {
                run_id: run_id(),
                allow: true
            },
            &engine
        )
        .contains("resume")
    );
    drop(engine);
    assert!(
        scenario
            .output(&["--json", "list"], |_| {})
            .status
            .success()
    );
}
