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
        tools: latte_core::ToolPresentationStyle::Canonical,
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
        reasoning_content: None,
        thinking_blocks: Vec::new(),
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

#[test]
#[allow(clippy::too_many_lines)]
fn anthropic_provider_registry_profile_and_edit_error_boundaries() {
    use latte_core::{
        CompactionPolicy, CompactionStrategy, SessionProviderBinding, ToolPresentationStyle,
    };
    use latte_engine::{CancellationToken, EngineBuilder};
    use latte_headless::provider::{
        AnthropicProvider, FinishReason, Provider, ProviderContext, ProviderError, ProviderRequest,
        ThinkingBlock, ToolCall,
    };
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // 1. Registry + ProfileCatalog boundaries for `anthropic-messages` & `tool_style: "claude_code"`.
    assert!(ProviderRegistry::parse_jsonc(
        r#"{version:1,default_model:"c/m",providers:{c:{type:"anthropic-messages",models:["m"],base_url:"https://api.anthropic.com/v1",api_key:"k",temperature:1.5}}}"#
    ).is_err());
    let bad_hdr_reg = ProviderRegistry::parse_jsonc(
        r#"{version:1,default_model:"c/m",providers:{c:{type:"anthropic-messages",models:["m"],base_url:"https://api.anthropic.com/v1/",api_key:"k",headers:{"x-api-key":"override"}}}}"#
    ).unwrap();
    assert!(bad_hdr_reg.resolve_model("c", "m", &[]).is_err());

    let scenario = Scenario::new();
    let engine = EngineBuilder::new()
        .workspace_root(scenario.root())
        .build()
        .unwrap();
    let descriptors = engine.tool_descriptors();

    let dual_reg = std::sync::Arc::new(ProviderRegistry::parse_jsonc(
        r#"{
            version: 1,
            default_model: "claude/m",
            providers: {
                claude: {
                    type: "anthropic-messages",
                    models: { m: { options: { context_window: 128000, reasoning_effort: "low", max_tokens: 4096 } } },
                    base_url: "https://api.anthropic.com/v1/",
                    api_key: "secret-anth",
                    prompt_caching: false
                },
                oai_claude: {
                    type: "openai-chat",
                    models: ["m2"],
                    endpoint: "http://127.0.0.1:9/v1/chat/completions",
                    api_key: "secret-oai",
                    tool_style: "claude_code"
                }
            }
        }"#
    ).unwrap());
    let base_ctx = ContextPolicy {
        compaction: CompactionPolicy {
            strategy: CompactionStrategy::ElideToolResultsThenSummarize,
            ..CompactionPolicy::default()
        },
        ..ContextPolicy::default()
    };
    let catalog = ProfileCatalog::new(base_ctx, std::sync::Arc::clone(&dual_reg)).unwrap();
    let anth_binding: SessionProviderBinding = dual_reg
        .session_binding_for_model("claude", "m", &descriptors)
        .unwrap();
    let anth_prof = catalog.resolve(&anth_binding).unwrap();
    assert_eq!(
        anth_prof.tool_presentation(),
        ToolPresentationStyle::ClaudeCode
    );
    assert!(anth_prof.compaction().reuse_prefix);
    assert!(anth_prof.system_prompt("").unwrap().contains("`Edit`"));

    let oai_binding = dual_reg
        .session_binding_for_model("oai_claude", "m2", &descriptors)
        .unwrap();
    let oai_prof = catalog.resolve(&oai_binding).unwrap();
    assert_eq!(
        oai_prof.tool_presentation(),
        ToolPresentationStyle::ClaudeCode
    );
    assert!(oai_prof.system_prompt("").unwrap().contains("`Bash`"));

    // 2. AnthropicProvider transport, fallback, error classification, and budget variants.
    let serve_seq = |replies: Vec<(&'static str, &'static str, &'static str)>| -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, ctype, body) in replies {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 16 * 1024];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nrequest-id: req_test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}/v1/messages")
    };

    rt.block_on(async {
        let ctx = || ProviderContext {
            deadline: Instant::now() + Duration::from_secs(3),
            cancellation: CancellationToken::new(),
            events: None,
        };

        // Empty-body 400 stream fallback -> inline JSON with redacted_thinking & max_tokens,
        // plus exercising assistant-first turn, overlong tool_id, unsigned thinking drop, and prompt_caching=false.
        let ep_fallback = serve_seq(vec![
            ("400 Bad Request", "application/json", ""),
            (
                "200 OK",
                "application/json",
                r#"{"type":"message","content":[{"type":"redacted_thinking","data":"enc1"},{"type":"text","text":"fb-ok"}],"stop_reason":"max_tokens","usage":{"output_tokens":7}}"#,
            ),
        ]);
        let prov = AnthropicProvider::new(ep_fallback, "m", "secret-token", Duration::from_secs(2))
            .unwrap()
            .with_prompt_caching(false)
            .with_sampling_options(Some(0.2), None)
            .with_reasoning_effort(Some("off".into()))
            .with_headers(BTreeMap::from([
                ("x-api-key".into(), "ignored".into()),
                ("anthropic-beta".into(), "custom-beta".into()),
            ]));
        assert!(format!("{prov:?}").contains("[REDACTED]"));
        assert!(!prov.capabilities().input_request);
        let overlong = "call_".to_owned() + &"z".repeat(80);
        let fb_out = prov
            .complete(
                ProviderRequest {
                    session_ref: "s1".into(),
                    tools: vec![],
                    messages: vec![
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![ThinkingBlock::Thinking {
                                thinking: "unsigned".into(),
                                signature: None,
                            }],
                            tool_calls: vec![ToolCall {
                                id: overlong.clone(),
                                name: "Read".into(),
                                input: serde_json::json!("non-object"),
                            }],
                        },
                        Message::Tool {
                            tool_call_id: overlong,
                            name: Some("Read".into()),
                            content: "ok".into(),
                        },
                    ],
                },
                ctx(),
            )
            .await
            .unwrap();
        assert_eq!(fb_out.message.as_deref(), Some("fb-ok"));
        assert_eq!(fb_out.finish_reason, Some(FinishReason::Length));
        assert_eq!(fb_out.thinking_blocks().len(), 1);

        // Exercise remaining reasoning_effort budget tiers ("minimal", "low", "medium", "xhigh", "2048", small max_tokens).
        for (effort, max_tok, stop, expected_reason) in [
            ("minimal", None, "refusal", FinishReason::ContentFilter),
            ("low", Some(1024), "stop_sequence", FinishReason::Stop),
            ("medium", None, "custom_stop", FinishReason::Other("custom_stop".into())),
            ("xhigh", Some(65_536), "end_turn", FinishReason::Stop),
            ("2048", Some(8_192), "end_turn", FinishReason::Stop),
        ] {
            let body = format!(
                r#"{{"type":"message","content":[{{"type":"text","text":"ok"}}],"stop_reason":{stop:?},"usage":{{"input_tokens":5}}}}"#
            );
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 8192];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            });
            let res = AnthropicProvider::new(
                format!("http://{addr}/v1/messages"),
                "m",
                "k",
                Duration::from_secs(2),
            )
            .unwrap()
            .with_streaming(false)
            .with_sampling_options(Some(0.5), max_tok)
            .with_reasoning_effort(Some(effort.into()))
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![Message::User { content: String::new() }],
                },
                ctx(),
            )
            .await
            .unwrap();
            assert_eq!(res.finish_reason, Some(expected_reason));
        }

        // HTTP 400 "prompt is too long" -> ContextOverflow; SSE overloaded, SSE overflow, SSE generic error, truncated SSE.
        let ep_err = serve_seq(vec![
            (
                "400 Bad Request",
                "application/json",
                r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 250000 > 200000"}}"#,
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"prompt is too long\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            ),
        ]);
        let err_prov = AnthropicProvider::new(ep_err, "m", "k", Duration::from_secs(2)).unwrap();
        let req = || ProviderRequest {
            session_ref: "s".into(),
            tools: vec![],
            messages: vec![Message::User { content: "u".into() }],
        };
        assert!(matches!(
            err_prov.complete(req(), ctx()).await,
            Err(ProviderError::ContextOverflow { .. })
        ));
        assert!(matches!(
            err_prov.complete(req(), ctx()).await,
            Err(ProviderError::Http { status: 529, retryable: true, .. })
        ));
        assert!(matches!(
            err_prov.complete(req(), ctx()).await,
            Err(ProviderError::ContextOverflow { .. })
        ));
        assert!(matches!(
            err_prov.complete(req(), ctx()).await,
            Err(ProviderError::Malformed(_))
        ));
        assert!(matches!(
            err_prov.complete(req(), ctx()).await,
            Err(ProviderError::Malformed(_))
        ));

        // ClaudeCode AliasedProvider outbound/inbound translation across all canonical & edge forms,
        // plus OpenAiProvider Debug redaction and SSE malformed block guards.
        let oai_dbg = latte_headless::provider::OpenAiProvider::new(
            "http://127.0.0.1:9/v1/chat/completions",
            "m",
            "super-secret-oai",
            Duration::from_secs(1),
        )
        .unwrap();
        let oai_rendered = format!("{oai_dbg:?}");
        assert!(oai_rendered.contains("[REDACTED]"));
        assert!(!oai_rendered.contains("super-secret-oai"));
        assert!(!oai_dbg.capabilities().input_request);

        let ep_sse_malformed = serve_seq(vec![
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"Read\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"seed\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\n",
            ),
        ]);
        let sse_prov =
            AnthropicProvider::new(ep_sse_malformed, "m", "k", Duration::from_secs(2)).unwrap();
        for _ in 0..6 {
            assert!(matches!(
                sse_prov
                    .complete(
                        ProviderRequest {
                            session_ref: "s".into(),
                            tools: vec![],
                            messages: vec![
                                Message::User { content: "u".into() },
                                Message::Assistant {
                                    content: None,
                                    reasoning_content: None,
                                    thinking_blocks: vec![],
                                    tool_calls: vec![],
                                },
                            ],
                        },
                        ctx(),
                    )
                    .await,
                Err(ProviderError::Malformed(_))
            ));
        }

        let ep_claude = serve_seq(vec![(
            "200 OK",
            "application/json",
            r#"{"type":"message","content":[
                {"type":"tool_use","id":"c1","name":"GitDiff","input":{}},
                {"type":"tool_use","id":"c2","name":"ReadManifest","input":{}},
                {"type":"tool_use","id":"c3","name":"Grep","input":{"pattern":"a+b","literal":true}},
                {"type":"tool_use","id":"c4","name":"Write","input":{"file_path":"w.txt","content":"hi","create_intent":true}},
                {"type":"tool_use","id":"c5","name":"Bash","input":{"argv":["ls"],"cwd":".","timeout_ms":500}},
                {"type":"tool_use","id":"c6","name":"Bash","input":{"command":"FOO=1 env"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":4}}"#,
        )]);
        let reg = ProviderRegistry::parse_jsonc(&format!(
            r#"{{version:1,default_model:"c/m",providers:{{c:{{type:"anthropic-messages",models:["m"],endpoint:{ep_claude:?},api_key:"k",streaming:false}}}}}}"#
        ))
        .unwrap();
        assert!(format!("{reg:?}").contains("[REDACTED]"));
        let resolved = reg.resolve_default(&descriptors).unwrap();
        let out = resolved
            .provider
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: descriptors.clone(),
                    messages: vec![
                        Message::User { content: "check".into() },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![],
                            tool_calls: vec![
                                ToolCall {
                                    id: "h1".into(),
                                    name: "search".into(),
                                    input: serde_json::json!({"query": "lit", "regex": false, "glob": "*.rs"}),
                                },
                                ToolCall {
                                    id: "h2".into(),
                                    name: "process".into(),
                                    input: serde_json::json!({"shell": "echo a | cat", "cwd": ".", "timeout_ms": 1000}),
                                },
                                ToolCall {
                                    id: "h3".into(),
                                    name: "read_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "offset": 1, "limit": 10}),
                                },
                                ToolCall {
                                    id: "h4".into(),
                                    name: "edit_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "before": "x", "after": "y", "replace_all": true}),
                                },
                                ToolCall {
                                    id: "h5".into(),
                                    name: "write_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "content": "z"}),
                                },
                                ToolCall {
                                    id: "h6".into(),
                                    name: "list_directory".into(),
                                    input: serde_json::json!({"path": "src"}),
                                },
                                ToolCall {
                                    id: "h7".into(),
                                    name: "git_diff".into(),
                                    input: serde_json::json!("raw"),
                                },
                            ],
                        },
                        Message::Tool {
                            tool_call_id: "h1".into(),
                            name: Some("search".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h2".into(),
                            name: Some("process".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h3".into(),
                            name: Some("read_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h4".into(),
                            name: Some("edit_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h5".into(),
                            name: Some("write_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h6".into(),
                            name: Some("list_directory".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h7".into(),
                            name: Some("git_diff".into()),
                            content: "ok".into(),
                        },
                    ],
                },
                ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.tool_calls[0].name, "git_diff");
        assert_eq!(out.tool_calls[1].name, "read_project_manifest");
        assert_eq!(out.tool_calls[2].name, "search");
        assert_eq!(out.tool_calls[2].input["regex"], false);
        assert_eq!(out.tool_calls[3].name, "write_file");
        assert_eq!(out.tool_calls[3].input["create_intent"], true);
        assert_eq!(out.tool_calls[4].name, "process");
        assert_eq!(out.tool_calls[4].input["argv"], serde_json::json!(["ls"]));
        assert_eq!(out.tool_calls[5].name, "process");
        assert_eq!(out.tool_calls[5].input["shell"], "FOO=1 env");
    });
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn engine_reissue_verification_legacy_import_and_mid_turn_compaction_boundaries() {
    use latte_core::{CompactionPolicy, CompactionStrategy, PermissionMode, SessionId, Transition};
    use latte_engine::{
        CancellationToken, EffectStatus, EngineBuilder, ProcessDecision, ProcessError,
        ProcessInvocation, ToolError, ToolInvocation, classify,
    };
    use latte_headless::{
        provider::{FakeProvider, ProviderError, ProviderResponse, ProviderUsage, ToolCall},
        registry::ResolvedProvider,
        session::{SessionHistoryPolicy, SessionRuntimeService},
    };
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    struct SeqProv(
        std::sync::Arc<
            std::sync::Mutex<std::collections::VecDeque<Result<ProviderResponse, ProviderError>>>,
        >,
    );
    impl latte_headless::provider::Provider for SeqProv {
        fn complete(
            &self,
            _req: latte_headless::provider::ProviderRequest,
            _ctx: latte_headless::provider::ProviderContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send>,
        > {
            let res = self.0.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { res })
        }
    }

    struct LeaseStealProv {
        engine: latte_engine::EngineHandle,
        session_id: SessionId,
    }
    impl latte_headless::provider::Provider for LeaseStealProv {
        fn complete(
            &self,
            _req: latte_headless::provider::ProviderRequest,
            _ctx: latte_headless::provider::ProviderContext,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send>,
        > {
            let engine = self.engine.clone();
            let sid = self.session_id;
            Box::pin(async move {
                let _ = engine.acquire_session_lease(sid, u64::MAX / 4, 60_000);
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(ProviderResponse {
                    message: Some("late".into()),
                    tool_calls: vec![],
                    input_request: None,
                    usage: ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                })
            })
        }
    }

    // 1. `classify` edge matrix (`--recursive`, `--force`, `--no-preserve-root`, `--`, `git` flags).
    let empty_env = BTreeMap::new();
    let classify_argv = |argv: &[String]| {
        classify(&ProcessInvocation {
            argv,
            shell: None,
            cwd: ".",
            env: &empty_env,
            timeout_ms: 1_000,
            grace_ms: 50,
            stdout_cap: 1024,
            stderr_cap: 1024,
            turn_revision: 1,
            effect_id: "eff",
            attempt: 1,
            approval_digest: None,
            lease_owner: "o",
            lease_token: 1,
        })
    };
    let s = |items: &[&str]| items.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        classify_argv(&s(&["rm", "--recursive", "--force", "/"])),
        ProcessDecision::Deny
    );
    assert_eq!(
        classify_argv(&s(&["rm", "--no-preserve-root", "tmp"])),
        ProcessDecision::Deny
    );
    assert_eq!(
        classify_argv(&s(&["rm", "--", "-rf", "local_file"])),
        ProcessDecision::Ask
    );
    assert_eq!(
        classify_argv(&s(&[
            "git",
            "status",
            "-s",
            "-b",
            "--porcelain",
            "-u",
            "--untracked-files=no"
        ])),
        ProcessDecision::Allow
    );
    assert_eq!(
        classify_argv(&s(&["git", "rev-parse", "HEAD"])),
        ProcessDecision::Allow
    );
    assert_eq!(classify_argv(&s(&["git"])), ProcessDecision::Ask);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // 3. Non-linked v1 EngineHandle: tool/process permission reissue, execute_verification,
        //    failed approved tool (`finish_effect(false)`), and IO-failed approved tool (`mark_effect_unknown` + `unknown_effects_for_turn`).
        let scenario = Scenario::new();
        std::fs::write(scenario.root().join("a.txt"), "hello\n").unwrap();
        let ro_dir = scenario.root().join("ro");
        std::fs::create_dir_all(&ro_dir).unwrap();
        std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let engine = EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(scenario.database_path())
            .build()
            .unwrap();
        let tid = turn_id();
        engine.create_turn(tid, 1).unwrap();
        let old_lease = engine.acquire_turn_lease(tid, "owner-1", 2, 50).unwrap();
        let mut sub = engine.subscribe();
        assert!(sub.try_recv().unwrap().is_none());
        let running = engine
            .apply_transition(tid, 0, Transition::Start, 3, &old_lease)
            .unwrap();
        let _ = sub.recv().await.unwrap();
        let mut cur_rev = running.revision;

        let read_in = serde_json::json!({"path": "a.txt"});
        let sha = engine
            .execute_tool(
                tid,
                &old_lease,
                4,
                &ToolInvocation {
                    name: "read_file",
                    input: &read_in,
                    turn_revision: cur_rev,
                    effect_id: "r1",
                    attempt: 1,
                    precondition: None,
                    timeout_ms: 0,
                    output_cap: 1024,
                    approval_digest: None,
                    lease_owner: old_lease.owner(),
                    lease_token: old_lease.fencing_token(),
                },
            )
            .unwrap()
            .value["sha256"]
            .as_str()
            .unwrap()
            .to_owned();

        let write_in = serde_json::json!({"path": "a.txt", "content": "world\n"});
        let old_write = ToolInvocation {
            name: "write_file",
            input: &write_in,
            turn_revision: cur_rev + 2,
            effect_id: "old-w",
            attempt: 1,
            precondition: Some(&sha),
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: old_lease.owner(),
            lease_token: old_lease.fencing_token(),
        };
        assert!(matches!(
            engine.execute_tool(tid, &old_lease, 5, &old_write),
            Err(ToolError::PermissionRequired { .. })
        ));

        let echo_argv = s(&["/bin/echo", "reissued"]);
        let old_proc = ProcessInvocation {
            argv: &echo_argv,
            shell: None,
            cwd: ".",
            env: &empty_env,
            timeout_ms: 2_000,
            grace_ms: 50,
            stdout_cap: 1024,
            stderr_cap: 1024,
            turn_revision: cur_rev + 2,
            effect_id: "old-p",
            attempt: 1,
            approval_digest: None,
            lease_owner: old_lease.owner(),
            lease_token: old_lease.fencing_token(),
        };
        assert!(matches!(
            engine
                .execute_process(tid, &old_lease, 6, &old_proc, &CancellationToken::new())
                .await,
            Err(ProcessError::PermissionRequired { .. })
        ));

        let fresh = engine.acquire_turn_lease(tid, "owner-2", 100, 1_000).unwrap();
        let fresh_write = ToolInvocation {
            effect_id: "fresh-w",
            attempt: 2,
            lease_owner: fresh.owner(),
            lease_token: fresh.fencing_token(),
            ..old_write
        };
        let w_digest = engine
            .reissue_tool_permission("old-w", tid, &fresh, 101, &fresh_write)
            .unwrap();
        assert!(!w_digest.is_empty());
        assert_eq!(engine.effect_status("old-w").unwrap(), EffectStatus::ObservedFailed);

        let fresh_proc = ProcessInvocation {
            effect_id: "fresh-p",
            attempt: 2,
            lease_owner: fresh.owner(),
            lease_token: fresh.fencing_token(),
            ..old_proc
        };
        let p_digest = engine
            .reissue_process_permission("old-p", tid, &fresh, 102, &fresh_proc)
            .unwrap();
        engine
            .apply_transition(
                tid,
                cur_rev,
                Transition::RequestPermission(PendingPermission {
                    request_id: "req-p".into(),
                    operation_digest: p_digest.clone(),
                    description: "echo".into(),
                }),
                103,
                &fresh,
            )
            .unwrap();
        cur_rev = engine
            .apply_transition(
                tid,
                cur_rev + 1,
                Transition::ResolvePermission {
                    request_id: "req-p".into(),
                    allowed: true,
                },
                104,
                &fresh,
            )
            .unwrap()
            .revision;
        let approved_proc = ProcessInvocation {
            approval_digest: Some(&p_digest),
            turn_revision: cur_rev,
            ..fresh_proc
        };
        let proc_out = engine
            .execute_process(tid, &fresh, 105, &approved_proc, &CancellationToken::new())
            .await
            .unwrap();
        assert!(proc_out.command_succeeded());

        let ver_argv = s(&["/bin/pwd"]);
        let ver_out = engine
            .execute_verification(
                tid,
                cur_rev,
                &fresh,
                106,
                &ProcessInvocation {
                    argv: &ver_argv,
                    shell: None,
                    cwd: ".",
                    env: &empty_env,
                    timeout_ms: 2_000,
                    grace_ms: 50,
                    stdout_cap: 1024,
                    stderr_cap: 1024,
                    turn_revision: cur_rev,
                    effect_id: "ver-1",
                    attempt: 1,
                    approval_digest: None,
                    lease_owner: fresh.owner(),
                    lease_token: fresh.fencing_token(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(ver_out.command_succeeded());

        // Approved `edit_file` failing `MatchCount(0)` -> `finish_effect(false)`.
        let bad_edit_in = serde_json::json!({"path": "a.txt", "before": "missing", "after": "x"});
        let bad_edit_prep = ToolInvocation {
            name: "edit_file",
            input: &bad_edit_in,
            turn_revision: cur_rev + 2,
            effect_id: "edit-fail",
            attempt: 1,
            precondition: Some(&sha),
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: fresh.owner(),
            lease_token: fresh.fencing_token(),
        };
        let Err(ToolError::PermissionRequired { digest: edit_dig, .. }) =
            engine.execute_tool(tid, &fresh, 107, &bad_edit_prep)
        else {
            panic!("expected PermissionRequired");
        };
        engine
            .apply_transition(
                tid,
                cur_rev,
                Transition::RequestPermission(PendingPermission {
                    request_id: "req-e".into(),
                    operation_digest: edit_dig.clone(),
                    description: "edit".into(),
                }),
                108,
                &fresh,
            )
            .unwrap();
        cur_rev = engine
            .apply_transition(
                tid,
                cur_rev + 1,
                Transition::ResolvePermission {
                    request_id: "req-e".into(),
                    allowed: true,
                },
                109,
                &fresh,
            )
            .unwrap()
            .revision;
        assert!(engine
            .execute_tool(
                tid,
                &fresh,
                110,
                &ToolInvocation {
                    approval_digest: Some(&edit_dig),
                    turn_revision: cur_rev,
                    ..bad_edit_prep
                }
            )
            .is_err());

        // Approved `write_file` inside `0o555` readonly directory -> `mark_effect_unknown` + `unknown_effects_for_turn`.
        let io_write_in =
            serde_json::json!({"path": "ro/blocked.txt", "content": "x", "create_intent": true});
        let io_write_prep = ToolInvocation {
            name: "write_file",
            input: &io_write_in,
            turn_revision: cur_rev + 2,
            effect_id: "io-unk",
            attempt: 1,
            precondition: None,
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: fresh.owner(),
            lease_token: fresh.fencing_token(),
        };
        let Err(ToolError::PermissionRequired { digest: io_dig, .. }) =
            engine.execute_tool(tid, &fresh, 111, &io_write_prep)
        else {
            panic!("expected PermissionRequired");
        };
        engine
            .apply_transition(
                tid,
                cur_rev,
                Transition::RequestPermission(PendingPermission {
                    request_id: "req-io".into(),
                    operation_digest: io_dig.clone(),
                    description: "write".into(),
                }),
                112,
                &fresh,
            )
            .unwrap();
        cur_rev = engine
            .apply_transition(
                tid,
                cur_rev + 1,
                Transition::ResolvePermission {
                    request_id: "req-io".into(),
                    allowed: true,
                },
                113,
                &fresh,
            )
            .unwrap()
            .revision;
        assert!(engine
            .execute_tool(
                tid,
                &fresh,
                114,
                &ToolInvocation {
                    approval_digest: Some(&io_dig),
                    turn_revision: cur_rev,
                    ..io_write_prep
                }
            )
            .is_err());
        assert_eq!(engine.unknown_effects_for_turn(tid).unwrap(), vec!["io-unk".to_owned()]);
        std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // 4. `import_legacy_workspace_database` from a separate SQLite file into a fresh global DB.
        let legacy_src = Scenario::new();
        let legacy_db = legacy_src.root().join("legacy.db");
        let legacy_engine = EngineBuilder::new()
            .workspace_root(scenario.root())
            .database_path(&legacy_db)
            .build()
            .unwrap();
        let leg_reg = ProviderRegistry::parse_jsonc(
            r#"{version:1,default_model:"p/m",providers:{p:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9/v1/chat/completions",api_key:"k"}}}"#,
        )
        .unwrap();
        let leg_binding = leg_reg
            .session_binding_for_default(&legacy_engine.tool_descriptors())
            .unwrap();
        let leg_sid = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
        legacy_engine
            .create_session_v2(leg_sid, turn_id(), leg_binding.clone(), "legacy imported title", 50)
            .unwrap();
        drop(legacy_engine);

        assert!(engine.import_legacy_workspace_database(&legacy_db, 200).is_err());
        assert!(
            !engine
                .import_legacy_workspace_database(scenario.root().join("missing.db"), 201)
                .unwrap()
        );
        engine
            .create_session_v2(
                leg_sid,
                turn_id(),
                leg_binding.clone(),
                "legacy imported title",
                202,
            )
            .unwrap();

        // 5. In-process `latte_server` + `WorkspaceInstance` + `ServerClient` SessionServer methods.
        let srv_reg = Arc::new(leg_reg);
        let srv_engine = engine.clone();
        let builder: latte_server::WorkspaceRuntimeBuilder = Arc::new(move |root| {
            let reg = Arc::clone(&srv_reg);
            let reg_for_factory = Arc::clone(&srv_reg);
            let descriptors = srv_engine.tool_descriptors();
            let runtime = SessionRuntimeService::new(
                srv_engine.clone(),
                root,
                SessionHistoryPolicy::default(),
                Arc::new(move |selected| {
                    let resolved = reg_for_factory
                        .resolve_session_bound(selected, &descriptors)
                        .map_err(|e| e.to_string())?;
                    Ok(ResolvedProvider {
                        provider: Arc::new(FakeProvider::scripted([
                            ProviderResponse {
                                message: Some("server-ok".into()),
                                tool_calls: vec![],
                                input_request: None,
                                usage: ProviderUsage::default(),
                                finish_reason: None,
                                provider_state: None,
                            },
                            ProviderResponse {
                                message: Some("server-followup-ok".into()),
                                tool_calls: vec![],
                                input_request: None,
                                usage: ProviderUsage::default(),
                                finish_reason: None,
                                provider_state: None,
                            },
                        ])),
                        binding: resolved.binding,
                    })
                }),
            );
            Ok(latte_server::BuiltWorkspace {
                engine: srv_engine.clone(),
                runtime,
                registry: reg,
            })
        });
        let root_for_loc = scenario.root().to_path_buf();
        let locator: latte_server::SessionLocator = Arc::new(move |_| Some(root_for_loc.clone()));
        let state = latte_server::new_state("tok-test".into(), builder, locator);
        let ws_inst = state.workspaces.get_or_create(scenario.root()).await.unwrap();
        assert!(!ws_inst.list_sessions().unwrap().is_empty());
        assert!(!ws_inst.search_sessions("legacy", 10).unwrap().is_empty());
        assert!(!ws_inst
            .find_sessions_by_exact_title("legacy imported title", 10)
            .unwrap()
            .is_empty());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let srv_task = tokio::spawn(async move {
            let _ = latte_server::serve_with_shutdown(state, listener, async {
                let _ = stop_rx.await;
            })
            .await;
        });

        let srv_url = format!("http://{srv_addr}");
        let srv_list = scenario.output(
            &["--json", "list", "--server", &srv_url, "--token", "tok-test"],
            |_| {},
        );
        assert!(srv_list.status.success());
        let srv_run = scenario.output(
            &[
                "--json",
                "run",
                "--server",
                &srv_url,
                "--token",
                "tok-test",
                "--custom-flag",
                "hello",
            ],
            |_| {},
        );
        assert!(srv_run.status.success());
        let srv_sid = super::support::json(&srv_run)["data"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let srv_resume = scenario.output(
            &[
                "--json",
                "resume",
                "--server",
                &srv_url,
                "--token",
                "tok-test",
                &srv_sid,
                "again",
            ],
            |_| {},
        );
        assert!(srv_resume.status.success());
        let http = reqwest::Client::new();
        let exact_resp = http
            .get(format!(
                "{srv_url}/v1/workspaces/{}/sessions/exact-title?q=legacy%20imported%20title&limit=10",
                ws_inst.id
            ))
            .bearer_auth("tok-test")
            .send()
            .await
            .unwrap();
        assert!(exact_resp.status().is_success());
        let _ = stop_tx.send(());
        let _ = srv_task.await;

        // 6. Mid-turn `ElideToolResultsThenSummarize` (`reuse_prefix: false`) — elision cure, failing summary audit, succeeding cold-prefix summary, and lease-loss recovery.
        let mid_ws = Scenario::new();
        std::fs::write(mid_ws.root().join("big.txt"), "X".repeat(3_600)).unwrap();
        let mid_engine = EngineBuilder::new()
            .workspace_root(mid_ws.root())
            .permission_mode(PermissionMode::FullAuto)
            .build()
            .unwrap();
        let mid_desc = mid_engine.tool_descriptors();
        let mid_reg = Arc::new(
            ProviderRegistry::parse_jsonc(
                r#"{version:1,default_model:"oai/m",providers:{oai:{type:"openai-chat",models:["m"],endpoint:"http://127.0.0.1:9/v1/chat/completions",api_key:"k"}}}"#,
            )
            .unwrap(),
        );
        let mid_binding = mid_reg.session_binding_for_default(&mid_desc).unwrap();
        let mid_policy = SessionHistoryPolicy {
            max_request_bytes: 9_216,
            max_input_bytes: 9_216,
            reserved_output_bytes: 512,
            context_cap_bytes: 1_024,
            provider_timeout_ms: 2_000,
            max_tool_rounds: Some(8),
        };
        let mid_compaction = CompactionPolicy {
            strategy: CompactionStrategy::ElideToolResultsThenSummarize,
            max_summary_source_bytes: 16_384,
            trigger_ratio: 45,
            retain_ratio: 1,
            reuse_prefix: false,
            ..CompactionPolicy::default()
        };
        let mid_cat = Arc::new(
            ProfileCatalog::new(
                ContextPolicy {
                    max_request_bytes: 9_216,
                    max_input_bytes: 9_216,
                    reserved_output_bytes: 512,
                    context_cap_bytes: 1_024,
                    max_tool_rounds: Some(8),
                    provider_timeout_ms: 2_000,
                    compaction: mid_compaction,
                    ..ContextPolicy::default()
                },
                Arc::clone(&mid_reg),
            )
            .unwrap(),
        );

        let calls = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
            // Turn 1: small read + 1_600-byte assistant text (stays right below 45% * 8_704 = 3_916 before Turn 2).
            Ok(ProviderResponse {
                message: None,
                tool_calls: vec![ToolCall {
                    id: "t1-l".into(),
                    name: "list_directory".into(),
                    input: serde_json::json!({"path": "."}),
                }],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
            Ok(ProviderResponse {
                message: Some("A".repeat(1_800)),
                tool_calls: vec![],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
            // Turn 2 Round 1: read big.txt (3_600 bytes) -> pushes over 45%; eliding Turn 1's tiny list_directory does NOT cure it,
            // so `mid_turn_summarize` runs! First attempt fails -> `compact-summary-failed:round` audit card; Round 2 runs another
            // list_directory -> `mid_turn_summarize` succeeds with `reuse_prefix: false` -> Round 3 completes!
            Ok(ProviderResponse {
                message: None,
                tool_calls: vec![ToolCall {
                    id: "t2-r".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "big.txt"}),
                }],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
            Err(ProviderError::Transport("mid-turn summary transient".into())),
            Ok(ProviderResponse {
                message: None,
                tool_calls: vec![ToolCall {
                    id: "t2-l2".into(),
                    name: "list_directory".into(),
                    input: serde_json::json!({"path": "."}),
                }],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
            Ok(ProviderResponse {
                message: Some("cold-prefix mid-turn summary".into()),
                tool_calls: vec![],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
            Ok(ProviderResponse {
                message: Some("turn2 done".into()),
                tool_calls: vec![],
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }),
        ])));
        let seq_prov: Arc<dyn latte_headless::provider::Provider> = Arc::new(SeqProv(calls));
        let reg_for_mid = Arc::clone(&mid_reg);
        let desc_for_mid = mid_desc.clone();
        let mid_svc = SessionRuntimeService::new(
            mid_engine.clone(),
            mid_ws.root(),
            mid_policy,
            Arc::new(move |sel| {
                let res = reg_for_mid
                    .resolve_session_bound(sel, &desc_for_mid)
                    .map_err(|e| e.to_string())?;
                Ok(ResolvedProvider {
                    provider: Arc::clone(&seq_prov),
                    binding: res.binding,
                })
            }),
        )
        .with_profile_catalog(mid_cat);

        let m_sid = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let s1 = mid_svc
            .start(m_sid, "seed history".into(), mid_binding.clone(), None)
            .await
            .unwrap();
        let s2 = mid_svc
            .follow_up(m_sid, s1.session_revision, "trigger mid-turn summarize".into())
            .await
            .unwrap();
        assert_eq!(s2.lifecycle, SessionLifecycle::Ready);
        assert!(s2
            .transcript
            .entries
            .iter()
            .any(|e| e.kind == latte_core::TranscriptKind::CompactSummary));

        // Lease-loss heartbeat recovery during provider request (`recover_lease_loss`).
        let steal_sid = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let steal_prov: Arc<dyn latte_headless::provider::Provider> = Arc::new(LeaseStealProv {
            engine: mid_engine.clone(),
            session_id: steal_sid,
        });
        let reg_for_steal = Arc::clone(&mid_reg);
        let steal_svc = SessionRuntimeService::new(
            mid_engine,
            mid_ws.root(),
            SessionHistoryPolicy::default(),
            Arc::new(move |sel| {
                let res = reg_for_steal
                    .resolve_session_bound(sel, &mid_desc)
                    .map_err(|e| e.to_string())?;
                Ok(ResolvedProvider {
                    provider: Arc::clone(&steal_prov),
                    binding: res.binding,
                })
            }),
        )
        .with_lease_ttl_ms(20);
        assert!(
            steal_svc
                .start(steal_sid, "lose lease".into(), mid_binding, None)
                .await
                .is_err()
        );
    });

    // 7. Human-readable CLI `context`, `compact` (when disabled), and CLI usage error arms.
    let cli_ws = Scenario::new();
    for bad_args in [
        &["list", "--auto"][..],
        &["list", "--permission-mode", "ask"][..],
        &["context"][..],
        &["compact"][..],
    ] {
        assert!(!cli_ws.output(bad_args, |_| {}).status.success());
    }
    let provider =
        super::support::ScriptedProvider::start([super::support::ProviderReply::completion(
            "cli-human-ok",
        )]);
    std::fs::create_dir_all(cli_ws.home().join(".latte")).unwrap();
    let ep = provider.endpoint();
    std::fs::write(
        cli_ws.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"p/m",providers:{{p:{{type:"openai-chat",models:["m"],endpoint:{ep:?},api_key:"k"}}}},session:{{compaction:{{enabled:false}}}}}}"#
        ),
    )
    .unwrap();
    let run_out = cli_ws.output(&["--json", "run", "hello"], |_| {});
    assert!(run_out.status.success());
    let run_sid = super::support::json(&run_out)["data"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ctx_out = cli_ws.output(&["context", &run_sid], |_| {});
    assert!(ctx_out.status.success());
    let cmp_out = cli_ws.output(&["compact", &run_sid], |_| {});
    assert!(cmp_out.status.success());
    assert!(
        String::from_utf8_lossy(&cmp_out.stdout)
            .contains("compaction is disabled in the resolved profile")
    );
}
