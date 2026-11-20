use super::support::Scenario;
use latte_core::{
    CompactionStrategy, CompletionPolicy, ContextPolicy, EffectId, Evidence, FailureCode, Handoff,
    HarnessProfile, IdSource, ManualCompactionState, ManualCompactionTier, PendingInput,
    PendingPermission, ProfileVersion, ProjectKey, ProjectSummary, Retryability, RoundId,
    SessionCommand, SessionCommandEnvelope, SessionCommandId, SessionEffectKind, SessionLifecycle,
    SystemIdSource, SystemPromptSpec, Transition, TransitionError, TurnFailure, TurnId, TurnState,
    TurnStatus, VerificationStatus, redact_session_text, redact_session_value,
    valid_openai_chat_input_request_id, valid_openai_chat_opaque_id,
    valid_openai_chat_tool_call_id,
};
use latte_headless::{
    context, profile::ProfileCatalog, provider::Message, registry::ProviderRegistry,
};
use latte_tui::{
    command::{
        BuiltinCommand, CommandAvailability, CommandContext, SlashResolution, command_availability,
        resolve_slash, slash_suggestions,
    },
    session::SessionUiFeedback,
};
use std::path::Path;

fn turn_id() -> TurnId {
    TurnId::from_uuid(SystemIdSource::default().next_uuid_v7())
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
    let round_id = RoundId::from_u64(1);
    assert_eq!(round_id.as_u64(), 1);
    assert_eq!(round_id.to_string(), "1");
    let turn_for_effect = turn_id();
    let tool_effect_id = EffectId::for_tool_call(turn_for_effect, round_id, 0, "call_1").unwrap();
    assert!(!tool_effect_id.is_verification());
    assert_eq!(
        tool_effect_id.parse_session(),
        Some(SessionEffectKind::ToolCall {
            turn_id: turn_for_effect,
            round_id,
            call_ordinal: 0,
            tool_call_id: "call_1".to_string(),
        })
    );
    let verification_effect_id = EffectId::for_verification(turn_for_effect);
    assert!(verification_effect_id.is_verification());
    assert_eq!(
        verification_effect_id.parse_session(),
        Some(SessionEffectKind::Verification {
            turn_id: turn_for_effect,
        })
    );
    assert!(EffectId::new("").is_err());
    assert!(EffectId::new("bad\nid").is_err());
    assert!(EffectId::new("x".repeat(513)).is_err());
    assert!(EffectId::for_tool_call(turn_for_effect, round_id, 0, "bad id").is_err());
    let malformed_tool_effect =
        EffectId::new(format!("session-effect:{turn_for_effect}:1:0:bad id")).unwrap();
    assert_eq!(malformed_tool_effect.parse_session(), None);
    let parsed_effect: EffectId = "opaque-effect".parse().unwrap();
    assert_eq!(
        <EffectId as AsRef<str>>::as_ref(&parsed_effect),
        "opaque-effect"
    );
    let project_key = ProjectKey::new("proj_0123456789abcdef").unwrap();
    assert_eq!(project_key.as_str(), "proj_0123456789abcdef");
    assert_eq!(
        <ProjectKey as AsRef<str>>::as_ref(&project_key),
        "proj_0123456789abcdef"
    );
    assert_eq!(project_key.to_string(), "proj_0123456789abcdef");
    assert_eq!(
        serde_json::from_str::<ProjectKey>(r#""proj_0123456789abcdef""#).unwrap(),
        project_key
    );
    assert!(ProjectKey::new("").is_err());
    assert!(ProjectKey::new("bad/key").is_err());
    let mut ctx_policy = ContextPolicy::default();
    assert!(ctx_policy.validate().is_ok());
    let mut zero_req = ctx_policy.clone();
    zero_req.max_request_bytes = 0;
    assert!(zero_req.validate().is_err());
    let mut zero_timeout = ctx_policy.clone();
    zero_timeout.provider_timeout_ms = 0;
    assert!(zero_timeout.validate().is_err());
    let mut zero_rounds = ctx_policy.clone();
    zero_rounds.max_tool_rounds = Some(0);
    assert!(zero_rounds.validate().is_err());
    let mut zero_bpt = ctx_policy.clone();
    zero_bpt.token_estimate.bytes_per_token = 0;
    assert!(zero_bpt.validate().is_err());
    ctx_policy.compaction.strategy = CompactionStrategy::ElideToolResultsThenSummarize;
    let mut bad_trigger = ctx_policy.clone();
    bad_trigger.compaction.trigger_ratio = 0;
    assert!(bad_trigger.validate().is_err());
    let mut bad_retain = ctx_policy.clone();
    bad_retain.compaction.retain_ratio = 90;
    assert!(bad_retain.validate().is_err());
    let mut bad_source = ctx_policy.clone();
    bad_source.compaction.max_summary_source_bytes = 0;
    assert!(bad_source.validate().is_err());
    let mut bad_slot = ctx_policy.clone();
    bad_slot.compaction.summary_prompt_id.clear();
    assert!(bad_slot.validate().is_err());
    let mut harness_profile = HarnessProfile {
        profile_id: "default".into(),
        version: ProfileVersion::V1_0,
        context: ContextPolicy::default(),
        prompts: SystemPromptSpec::default(),
    };
    assert!(harness_profile.validate().is_ok());
    harness_profile.profile_id.clear();
    assert!(harness_profile.validate().is_err());
    let resolved_base = ProfileCatalog::without_registry(ContextPolicy::default())
        .resolve_base()
        .unwrap();
    assert_eq!(resolved_base.fingerprint().len(), 64);
    assert!(
        ProfileCatalog::without_registry(zero_req)
            .resolve_base()
            .is_err()
    );
    let sys_msg = Message::System {
        content: "sys".into(),
    };
    let user_msg = Message::User {
        content: "usr".into(),
    };
    let asst_msg = Message::Assistant {
        content: Some("ans".into()),
        tool_calls: Vec::new(),
    };
    let tool_msg = Message::Tool {
        tool_call_id: "c1".into(),
        name: None,
        content: "res".into(),
    };
    assert!(sys_msg.is_role("system") && sys_msg.content() == Some("sys"));
    assert!(user_msg.is_role("user") && user_msg.content() == Some("usr"));
    assert!(asst_msg.is_role("assistant") && asst_msg.content() == Some("ans"));
    assert!(tool_msg.is_role("tool") && tool_msg.content() == Some("res"));
    assert!(!tool_msg.is_role("user"));
    let summary = ProjectSummary {
        project_key,
        vcs_identity: "git:/tmp/project".into(),
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    assert_eq!(summary.project_key.as_str(), "proj_0123456789abcdef");
    let session_command_id = SessionCommandId::from_uuid(ids.next_uuid_v7());
    let session_envelope = SessionCommandEnvelope::new(
        session_command_id,
        SessionCommand::Cancel {
            session_id: latte_core::SessionId::from_uuid(ids.next_uuid_v7()),
            expected_session_revision: 1,
            expected_turn_revision: 1,
        },
    );
    assert_eq!(session_envelope.command_id, session_command_id);
    assert_eq!(
        session_envelope.protocol_version,
        latte_core::SESSION_PROTOCOL_VERSION
    );
    let compacted_state = ManualCompactionState::Compacted {
        tier: ManualCompactionTier::Elided,
        session_revision: 3,
    };
    assert_eq!(
        serde_json::to_value(compacted_state).unwrap()["compacted"]["session_revision"],
        3
    );
    let _ = SessionUiFeedback::compaction(Ok("compacted history".into()));
    assert!(SessionLifecycle::Ready.accepts_follow_up());
    assert!(!SessionLifecycle::Running.accepts_follow_up());
    for valid in ["opaque", "call_123", "req-1"] {
        assert!(valid_openai_chat_opaque_id(valid));
        assert!(valid_openai_chat_tool_call_id(valid));
        assert!(valid_openai_chat_input_request_id(valid));
    }
    let oversized = "x".repeat(257);
    for invalid in ["", "has space", "line\nbreak", oversized.as_str()] {
        assert!(!valid_openai_chat_opaque_id(invalid));
    }
    let redacted = redact_session_text("ok\x1b[31m token=secret-value-12345678901234567890\x07");
    assert!(!redacted.contains("secret-value"));
    assert!(!redacted.contains('\x1b'));
    let redacted_value = redact_session_value(serde_json::json!({
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
    assert_eq!(slash_suggestions("/co")[0].name, "compact");
    assert_eq!(slash_suggestions("/ca")[0].name, "cancel");
    assert!(matches!(
        resolve_slash("/compact"),
        SlashResolution::Command { descriptor, .. } if descriptor.id == BuiltinCommand::Compact
    ));
    assert!(matches!(
        resolve_slash("/cancel"),
        SlashResolution::Command { descriptor, .. } if descriptor.id == BuiltinCommand::Cancel
    ));
    assert!(matches!(
        resolve_slash("/compact bad"),
        SlashResolution::ValidationError(_)
    ));
    assert!(matches!(
        resolve_slash("/cancel bad"),
        SlashResolution::ValidationError(_)
    ));
    let draft_ctx = CommandContext {
        connected: true,
        ..CommandContext::default()
    };
    assert!(matches!(
        command_availability(BuiltinCommand::Compact, &draft_ctx),
        CommandAvailability::Disabled { .. }
    ));
    assert!(matches!(
        command_availability(BuiltinCommand::Cancel, &draft_ctx),
        CommandAvailability::Disabled { .. }
    ));
    let idle_ctx = CommandContext {
        connected: true,
        has_open_session: true,
        lifecycle: Some(SessionLifecycle::Ready),
        ..CommandContext::default()
    };
    assert_eq!(
        command_availability(BuiltinCommand::Compact, &idle_ctx),
        CommandAvailability::Enabled
    );
    let running_ctx = CommandContext {
        connected: true,
        has_open_session: true,
        lifecycle: Some(SessionLifecycle::Running),
        has_active_turn: true,
        ..CommandContext::default()
    };
    assert_eq!(
        command_availability(BuiltinCommand::Cancel, &running_ctx),
        CommandAvailability::Enabled
    );
    let disconnected_ctx = CommandContext::default();
    for cmd in [
        BuiltinCommand::Sessions,
        BuiltinCommand::Rename,
        BuiltinCommand::Fork,
        BuiltinCommand::Compact,
        BuiltinCommand::Cancel,
    ] {
        assert!(matches!(
            command_availability(cmd, &disconnected_ctx),
            CommandAvailability::Disabled { .. }
        ));
    }
    let compacting_ctx = CommandContext {
        has_pending_compaction: true,
        ..idle_ctx
    };
    assert!(matches!(
        command_availability(BuiltinCommand::Compact, &compacting_ctx),
        CommandAvailability::Disabled { .. }
    ));
    let waiting_perm_ctx = CommandContext {
        connected: true,
        has_open_session: true,
        lifecycle: Some(SessionLifecycle::WaitingPermission),
        has_pending_request: true,
        ..CommandContext::default()
    };
    assert!(matches!(
        command_availability(BuiltinCommand::Cancel, &waiting_perm_ctx),
        CommandAvailability::Disabled { .. }
    ));
    assert!(matches!(
        resolve_slash("/rename"),
        SlashResolution::ValidationError(_)
    ));

    let queued = TurnState::queued(turn_id());
    assert!(matches!(
        queued.transition(1, Transition::Start),
        Err(TransitionError::StaleRevision { .. })
    ));
    assert!(matches!(
        queued.transition(0, Transition::Interrupt),
        Err(TransitionError::Invalid {
            from: TurnStatus::Queued
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
    assert_eq!(denied.status, TurnStatus::Failed);
    assert_eq!(
        denied.failure.as_ref().unwrap().code,
        FailureCode::PermissionDenied
    );

    let retryable = running
        .transition(
            1,
            Transition::Fail(TurnFailure {
                code: FailureCode::RuntimeFailed,
                message: "retry".into(),
                retryability: Retryability::Retryable,
            }),
        )
        .unwrap();
    assert_eq!(
        retryable.transition(2, Transition::Resume).unwrap().status,
        TurnStatus::Queued
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
        TurnStatus::Interrupted
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
    assert_eq!(verification_failed.status, TurnStatus::Failed);
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
        .session_binding_for_model("primary", "local-a", &[])
        .unwrap();
    assert!(registry.resolve_session_bound(&bound, &[]).is_ok());
    let mut changed = bound;
    changed.credential_generation += 1;
    assert!(registry.resolve_session_bound(&changed, &[]).is_err());

    let mut missing_model_binding = resolved.binding.clone();
    missing_model_binding.model = "removed-model".into();
    assert!(registry.resolve_bound(&missing_model_binding, &[]).is_err());

    let provider_with_derived_session_scope = ProviderRegistry::parse_jsonc(
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
    let derived = provider_with_derived_session_scope
        .session_binding_for_default(&[])
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
    assert!(
        scenario
            .output(&["--json", "list"], |_| {})
            .status
            .success()
    );
}
