//! Harness profile resolution.
//!
//! [`ProfileCatalog`] resolves a [`latte_core::SessionProviderBinding`] into a
//! [`ResolvedProfile`]: the per-model behavioral contract (request budgets,
//! prompt slots) consumed by the agent loop.
//!
//! Resolution layers, later overrides earlier:
//!
//! 1. the built-in profile for the binding's provider type — the historical
//!    `SessionHistoryPolicy` defaults and system prompt, moved verbatim;
//! 2. the application `session` configuration section, which overrides a
//!    builtin budget only where it differs from the config-layer default
//!    (`layer_budget`); untouched fields keep following the builtin;
//! 3. the model's declared `context_window`, which can only *tighten* the
//!    repository-context cap.
//!
//! Resolution is fail-closed: an unknown provider type with no matching
//! built-in profile is an error, never a silent fallback. Profiles carry no
//! permission, effect, or policy fields, so no resolution result can widen
//! authority.

use std::sync::Arc;

use latte_core::{
    AGENT_SUMMARIZE_PROMPT_SLOT, AGENT_SYSTEM_PROMPT_SLOT, ANTHROPIC_MESSAGES_PROFILE_ID,
    CompactionPolicy, CompactionStrategy, ContextPolicy, GENERIC_OPENAI_CHAT_PROFILE_ID,
    HarnessProfile, ProfileVersion, SessionProviderBinding, SystemPromptSpec, TokenEstimateParams,
    ToolPresentationStyle,
};
use sha2::{Digest, Sha256};

use crate::registry::ProviderRegistry;
use crate::session::SessionHistoryPolicy;

/// Failures of profile resolution. All messages are secret-free.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(
        "no harness profile for provider type `{provider_type}`; refusing to start without an explicit profile"
    )]
    UnknownProviderType { provider_type: String },
    #[error("invalid harness profile for `{profile_id}`: {reason}")]
    Invalid { profile_id: String, reason: String },
}

/// The built-in system prompt template, identical to the pre-profile
/// hardcoded prompt. `{repository_context}` is the injection point for the
/// collected workspace context.
const AGENT_SYSTEM_PROMPT_TEMPLATE: &str = "You are Latte Code, a coding agent making scoped changes to the repository described below. Work only within it: paths outside the workspace are rejected.\n\nRead before you write. To change an existing file, first call `read_file` on it (use `offset` and `limit` to paginate large files). The engine automatically records the `sha256` snapshot you read across consecutive edits, and also accepts that `sha256` as an optional `precondition` to `edit_file` or `write_file`; if an external process modifies the file on disk, re-read it first. Prefer `edit_file`, whose `before` must match the file verbatim and occur exactly once; use `write_file` to create new files (missing parent directories are created automatically) or replace an entire file.\n\nUnderstand before you change. Locate relevant code with `search` (`path`, `glob`) and `list_directory` (`max_depth`, `glob`), and read enough context that your edit matches existing conventions. `read_project_manifest` shows project manifests, and `process` runs commands inside the workspace (prefer `argv` over `shell`). Do not invent APIs, dependencies, or file paths you have not observed.\n\nFinish what you start. After editing, inspect your changes with `git_diff`. When a verification command is configured it must pass before the task is complete; a failing, missing, or unrun verification means the work is unfinished. Report concisely what you changed and verified. If a tool call returns an error, read the error and adjust your next call rather than repeating it unchanged. If the task is ambiguous or blocked, state why clearly instead of guessing.\n{repository_context}";

/// The built-in Claude/Anthropic system prompt template aligned with the
/// `ClaudeCode` tool presentation (`Read`, `Edit`, `Write`, `Glob`, `Grep`,
/// `Bash`, `GitDiff`, `ReadManifest`).
const AGENT_CLAUDE_SYSTEM_PROMPT_TEMPLATE: &str = "You are Latte Code, a coding agent making scoped changes to the repository described below. Work only within it: paths outside the workspace are rejected.\n\nRead before you write. To change an existing file, first call `Read` on it (`file_path`, with optional `offset` and `limit` to paginate large files). The engine automatically records the `sha256` snapshot you read across consecutive edits; if an external process modifies the file on disk, call `Read` again first. Prefer `Edit` (`file_path`, `old_string`, `new_string`, optional `replace_all`), where `old_string` must match uniquely unless `replace_all` is true; use `Write` (`file_path`, `content`) to create new files (missing parent directories are created automatically) or replace an entire file.\n\nUnderstand before you change. Locate relevant code with `Grep` (`pattern`, `path`, `glob`) and `Glob` (`pattern`, `path`), and read enough context that your edit matches existing conventions. `ReadManifest` shows project manifests, and `Bash` (`command`, optional `cwd`, `timeout_ms`) runs commands inside the workspace. Do not invent APIs, dependencies, or file paths you have not observed.\n\nFinish what you start. After editing, inspect your changes with `GitDiff`. When a verification command is configured it must pass before the task is complete; a failing, missing, or unrun verification means the work is unfinished. Report concisely what you changed and verified. If a tool call returns an error, read the error and adjust your next call rather than repeating it unchanged. If the task is ambiguous or blocked, state why clearly instead of guessing.\n{repository_context}";

/// Provider types recognized by the built-in catalog. `embedded` covers the
/// in-process test fixture binding and `test` covers the crate-local session
/// fixtures — both are fixture identities, not production protocols. The
/// whitelist is also the upgrade-compat boundary: bindings are persisted
/// verbatim (including `provider_type`), so every type that could have been
/// persisted by any previous release must resolve here or old sessions fail
/// to reopen.
const BUILTIN_PROVIDER_TYPES: [&str; 4] = ["openai-chat", "anthropic-messages", "embedded", "test"];

/// The provider type used for eager, binding-free pre-checks (prompt budget
/// validation before enqueue). Every built-in profile shares the same prompt
/// catalog today, so the concrete value only names the builtin family.
const DEFAULT_BUILTIN_PROVIDER_TYPE: &str = "openai-chat";

/// The builtin summarization instruction used when context compaction
/// replaces discarded history with a durable summary. It travels as the
/// system message of a dedicated provider request whose user message is the
/// discarded transcript plain text.
const AGENT_SUMMARIZE_PROMPT_TEMPLATE: &str = "You are compacting the earlier history of a coding-agent conversation that no longer fits its request budget. Produce a terse factual summary of the transcript below so a fresh model instance can continue the work without the original turns.\n\nPreserve, in this priority order: the user's goal and any corrections to it; every decision made and its reason; every file path, command, and verification result that is still relevant; open questions and unfinished work. Omit small talk, retries, and superseded attempts. Do not invent anything that is not in the transcript. The transcript may contain an earlier `[compacted summary]` block: treat it as a prior checkpoint, merge it into one consolidated summary — preserve still-true facts and drop stale ones — and never copy it verbatim. Reply with the summary only.\n\nTranscript:\n";

fn prompts_for_style(style: ToolPresentationStyle) -> SystemPromptSpec {
    let system_template = match style {
        ToolPresentationStyle::Canonical => AGENT_SYSTEM_PROMPT_TEMPLATE,
        ToolPresentationStyle::ClaudeCode => AGENT_CLAUDE_SYSTEM_PROMPT_TEMPLATE,
    };
    SystemPromptSpec {
        slots: [
            (
                AGENT_SYSTEM_PROMPT_SLOT.to_owned(),
                system_template.to_owned(),
            ),
            (
                AGENT_SUMMARIZE_PROMPT_SLOT.to_owned(),
                AGENT_SUMMARIZE_PROMPT_TEMPLATE.to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
    }
}

fn builtin_profile(provider_type: &str) -> Result<HarnessProfile, ProfileError> {
    if !BUILTIN_PROVIDER_TYPES.contains(&provider_type) {
        return Err(ProfileError::UnknownProviderType {
            provider_type: provider_type.to_owned(),
        });
    }
    if provider_type == "anthropic-messages" {
        let mut context = ContextPolicy::default();
        context.compaction.reuse_prefix = true;
        return Ok(HarnessProfile {
            profile_id: ANTHROPIC_MESSAGES_PROFILE_ID.to_owned(),
            version: ProfileVersion::V1_1,
            context,
            prompts: prompts_for_style(ToolPresentationStyle::ClaudeCode),
            tools: ToolPresentationStyle::ClaudeCode,
        });
    }
    Ok(HarnessProfile {
        profile_id: GENERIC_OPENAI_CHAT_PROFILE_ID.to_owned(),
        version: ProfileVersion::V1_0,
        context: ContextPolicy::default(),
        prompts: prompts_for_style(ToolPresentationStyle::Canonical),
        tools: ToolPresentationStyle::Canonical,
    })
}

/// Layers the application `session` configuration over one builtin profile's
/// budgets: a configured field overrides the builtin only when it differs
/// from the historical default (the config layer's own fallback). Fields the
/// user never touched keep following the builtin profile, so a builtin
/// budget change takes effect for default configurations instead of being
/// silently replaced by config defaults.
///
/// The known edge: a user who explicitly sets a field *to the default value*
/// also tracks future builtin changes. Distinguishing that needs per-field
/// presence tracking in the config layer, which v1 does not have; documented
/// in the design doc instead.
#[must_use]
fn layer_budget(builtin: &ContextPolicy, base: &ContextPolicy) -> ContextPolicy {
    let config_default = ContextPolicy::from(SessionHistoryPolicy::default());
    ContextPolicy {
        max_request_bytes: if base.max_request_bytes == config_default.max_request_bytes {
            builtin.max_request_bytes
        } else {
            base.max_request_bytes
        },
        max_input_bytes: if base.max_input_bytes == config_default.max_input_bytes {
            builtin.max_input_bytes
        } else {
            base.max_input_bytes
        },
        reserved_output_bytes: if base.reserved_output_bytes == config_default.reserved_output_bytes
        {
            builtin.reserved_output_bytes
        } else {
            base.reserved_output_bytes
        },
        context_cap_bytes: if base.context_cap_bytes == config_default.context_cap_bytes {
            builtin.context_cap_bytes
        } else {
            base.context_cap_bytes
        },
        max_tool_rounds: if base.max_tool_rounds == config_default.max_tool_rounds {
            builtin.max_tool_rounds
        } else {
            base.max_tool_rounds
        },
        provider_timeout_ms: if base.provider_timeout_ms == config_default.provider_timeout_ms {
            builtin.provider_timeout_ms
        } else {
            base.provider_timeout_ms
        },
        // Compaction: a non-Off config strategy wins (and brings its own
        // bounds); an Off config keeps the builtin strategy. The token
        // estimate has no configuration surface and stays a builtin concern.
        compaction: if matches!(base.compaction.strategy, CompactionStrategy::Off) {
            builtin.compaction.clone()
        } else {
            CompactionPolicy {
                reuse_prefix: base.compaction.reuse_prefix || builtin.compaction.reuse_prefix,
                ..base.compaction.clone()
            }
        },
        token_estimate: builtin.token_estimate.clone(),
    }
}

impl From<SessionHistoryPolicy> for ContextPolicy {
    fn from(policy: SessionHistoryPolicy) -> Self {
        Self {
            max_request_bytes: policy.max_request_bytes,
            max_input_bytes: policy.max_input_bytes,
            reserved_output_bytes: policy.reserved_output_bytes,
            context_cap_bytes: policy.context_cap_bytes,
            max_tool_rounds: policy.max_tool_rounds,
            provider_timeout_ms: policy.provider_timeout_ms,
            compaction: CompactionPolicy::default(),
            token_estimate: TokenEstimateParams::default(),
        }
    }
}

/// Resolves bindings into harness profiles.
///
/// The catalog is immutable after construction; the agent loop holds it for
/// the process lifetime and resolves per binding at turn boundaries.
#[derive(Debug)]
pub struct ProfileCatalog {
    base: ContextPolicy,
    registry: Option<Arc<ProviderRegistry>>,
}

impl ProfileCatalog {
    /// Builds a catalog over the application session configuration (already
    /// merged over built-in defaults by the binary) and the provider
    /// registry used for per-model declarations.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Invalid`] when the base policy fails context
    /// validation; refusing here keeps misconfiguration at startup instead
    /// of at first request.
    pub fn new(base: ContextPolicy, registry: Arc<ProviderRegistry>) -> Result<Self, ProfileError> {
        base.validate().map_err(|reason| ProfileError::Invalid {
            profile_id: GENERIC_OPENAI_CHAT_PROFILE_ID.to_owned(),
            reason,
        })?;
        Ok(Self {
            base,
            registry: Some(registry),
        })
    }

    /// Builds a registry-free catalog over one base policy. Resolution uses
    /// the built-in profile and the base context; per-model `context_window`
    /// tightening is unavailable because no registry is attached. Merged
    /// results are still validated at resolve time.
    #[must_use]
    pub fn without_registry(base: ContextPolicy) -> Self {
        Self {
            base,
            registry: None,
        }
    }

    /// Resolves the base policy without a binding. Used only for eager
    /// prompt pre-validation where the authoritative, binding-aware check
    /// happens later at the child boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Invalid`] when the base policy is
    /// inconsistent.
    pub fn resolve_base(&self) -> Result<ResolvedProfile, ProfileError> {
        let builtin = builtin_profile(DEFAULT_BUILTIN_PROVIDER_TYPE)?;
        let context = self.base.clone();
        context.validate().map_err(|reason| ProfileError::Invalid {
            profile_id: builtin.profile_id.clone(),
            reason,
        })?;
        Ok(ResolvedProfile {
            profile: HarnessProfile {
                profile_id: builtin.profile_id,
                version: builtin.version,
                context,
                prompts: builtin.prompts,
                tools: builtin.tools,
            },
        })
    }

    /// Resolves the profile for one binding.
    ///
    /// # Errors
    ///
    /// Fail-closed: unknown provider types and inconsistent merged policies
    /// are errors, never silent fallbacks.
    pub fn resolve(
        &self,
        binding: &SessionProviderBinding,
    ) -> Result<ResolvedProfile, ProfileError> {
        let builtin = builtin_profile(&binding.provider_type)?;
        // Three-layer merge: the builtin profile provides the budget
        // baseline, the application `session` configuration overrides fields
        // it explicitly sets, and a declared context window (below) can only
        // tighten the repository-context cap.
        let mut context = layer_budget(&builtin.context, &self.base);
        // A declared context window may only tighten the repository-context
        // cap; a larger window never widens the configured budget.
        if let Some(window) = self.registry.as_ref().and_then(|registry| {
            registry.model_context_window(&binding.provider_name, &binding.model)
        }) {
            let derived_cap =
                (window as usize).saturating_mul(context.token_estimate.bytes_per_token);
            context.context_cap_bytes = context.context_cap_bytes.min(derived_cap);
        }
        let tools = self
            .registry
            .as_ref()
            .and_then(|registry| registry.provider_tool_presentation(&binding.provider_name))
            .unwrap_or(builtin.tools);
        let prompts = if tools == builtin.tools {
            builtin.prompts
        } else {
            prompts_for_style(tools)
        };
        context.validate().map_err(|reason| ProfileError::Invalid {
            profile_id: builtin.profile_id.clone(),
            reason,
        })?;
        Ok(ResolvedProfile {
            profile: HarnessProfile {
                profile_id: builtin.profile_id,
                version: builtin.version,
                context,
                prompts,
                tools,
            },
        })
    }
}

/// A binding-resolved profile ready for loop consumption.
#[derive(Clone, Debug)]
pub struct ResolvedProfile {
    profile: HarnessProfile,
}

impl ResolvedProfile {
    /// The full profile data.
    #[must_use]
    pub fn profile(&self) -> &HarnessProfile {
        &self.profile
    }

    /// The loop-facing history policy projected from the profile.
    #[must_use]
    pub fn history_policy(&self) -> SessionHistoryPolicy {
        SessionHistoryPolicy {
            max_request_bytes: self.profile.context.max_request_bytes,
            max_input_bytes: self.profile.context.max_input_bytes,
            reserved_output_bytes: self.profile.context.reserved_output_bytes,
            context_cap_bytes: self.profile.context.context_cap_bytes,
            max_tool_rounds: self.profile.context.max_tool_rounds,
            provider_timeout_ms: self.profile.context.provider_timeout_ms,
        }
    }

    /// Renders the `agent.system` prompt slot with the collected repository
    /// context.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Invalid`] when the profile lacks the slot;
    /// every built-in profile defines it, so a miss means catalog corruption.
    pub fn system_prompt(&self, repository_context: &str) -> Result<String, ProfileError> {
        self.profile
            .prompts
            .render(AGENT_SYSTEM_PROMPT_SLOT, repository_context)
            .ok_or_else(|| ProfileError::Invalid {
                profile_id: self.profile.profile_id.clone(),
                reason: format!("missing prompt slot `{AGENT_SYSTEM_PROMPT_SLOT}`"),
            })
    }

    /// The profile's compaction policy.
    #[must_use]
    pub fn compaction(&self) -> &CompactionPolicy {
        &self.profile.context.compaction
    }

    /// Renders the `agent.summarize` prompt slot used as the system message
    /// of a compaction summary request.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Invalid`] when the profile lacks the slot.
    pub fn summarize_prompt(&self) -> Result<String, ProfileError> {
        self.profile
            .prompts
            .render(AGENT_SUMMARIZE_PROMPT_SLOT, "")
            .ok_or_else(|| ProfileError::Invalid {
                profile_id: self.profile.profile_id.clone(),
                reason: format!("missing prompt slot `{AGENT_SUMMARIZE_PROMPT_SLOT}`"),
            })
    }

    /// The profile's wire-level tool presentation style.
    #[must_use]
    pub fn tool_presentation(&self) -> ToolPresentationStyle {
        self.profile.tools
    }

    /// Stable content fingerprint of the resolved profile data. Equal
    /// fingerprints guarantee equal behavioral parameters.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let canonical = serde_json::json!({
            "profile_id": self.profile.profile_id,
            "version": {
                "major": self.profile.version.major,
                "minor": self.profile.version.minor,
            },
            "context": self.profile.context,
            "tools": self.profile.tools,
        });
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(text: &str) -> Arc<ProviderRegistry> {
        Arc::new(ProviderRegistry::parse_jsonc(text).expect("valid registry"))
    }

    fn minimal_registry() -> Arc<ProviderRegistry> {
        registry(
            "{version:1,default_model:'main/m',providers:{main:{type:'openai-chat',models:['m'],endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        )
    }

    fn binding(provider_type: &str, model: &str) -> SessionProviderBinding {
        SessionProviderBinding {
            binding_version: 2,
            provider_name: "main".into(),
            provider_type: provider_type.into(),
            protocol: "openai-chat".into(),
            model: model.into(),
            config_fingerprint: "cfg".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::default(),
            credential_ref_id: "cred".into(),
            data_scope_id: "scope".into(),
            credential_generation: 1,
        }
    }

    fn catalog(base: ContextPolicy, registry: Arc<ProviderRegistry>) -> ProfileCatalog {
        ProfileCatalog::new(base, registry).expect("valid catalog")
    }

    #[test]
    fn builtin_resolution_matches_historical_defaults_exactly() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        // `ContextPolicy::default` is `SessionHistoryPolicy::default`
        // projected through `From`; a bare-`Ids` model declares no window,
        // so the resolved policy must equal the historical default bit for
        // bit. This is the zero-behavior-change migration proof.
        assert_eq!(resolved.history_policy(), SessionHistoryPolicy::default());
        assert_eq!(
            resolved.profile().profile_id,
            GENERIC_OPENAI_CHAT_PROFILE_ID
        );
        assert_eq!(resolved.profile().version, ProfileVersion::V1_0);
    }

    #[test]
    fn resolution_is_deterministic_for_equal_bindings() {
        let catalog = catalog(ContextPolicy::default(), minimal_registry());
        let a = catalog
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let b = catalog
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.profile(), b.profile());
    }

    #[test]
    fn declared_context_window_only_tightens_the_context_cap() {
        let registry = registry(
            "{version:1,default_model:'main/small',providers:{main:{type:'openai-chat',models:{small:{options:{context_window:1024}},large:{options:{context_window:1000000}}},endpoint:'https://x',api_key:{source:'env',name:'K'}}}}",
        );
        let catalog = catalog(ContextPolicy::default(), registry);
        let small = catalog
            .resolve(&binding("openai-chat", "small"))
            .expect("resolves");
        // 1024 tokens * 4 bytes/token = 4096 < 64 KiB default: tightened.
        assert_eq!(small.history_policy().context_cap_bytes, 4096);
        let large = catalog
            .resolve(&binding("openai-chat", "large"))
            .expect("resolves");
        // 1_000_000 tokens * 4 > 128 KiB default: cap stays, never widens.
        assert_eq!(large.history_policy().context_cap_bytes, 128 * 1024);
    }

    #[test]
    fn base_configuration_overrides_built_in_budgets() {
        let base = ContextPolicy {
            max_request_bytes: 256 * 1024,
            max_input_bytes: 128 * 1024,
            reserved_output_bytes: 64 * 1024,
            provider_timeout_ms: 30_000,
            ..ContextPolicy::default()
        };
        let resolved = catalog(base, minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let policy = resolved.history_policy();
        assert_eq!(policy.max_request_bytes, 256 * 1024);
        assert_eq!(policy.provider_timeout_ms, 30_000);
    }

    /// The builtin profile's budgets are live, not dead code: a budget the
    /// config layer never touched follows the builtin value even when it
    /// drifts from the historical default. Without this guarantee a future
    /// builtin budget change would silently do nothing — the exact
    /// global-one-size-fits-all behavior the profile abstraction exists to
    /// remove.
    #[test]
    fn builtin_budget_changes_take_effect_for_untouched_config_fields() {
        let builtin = builtin_profile("openai-chat").expect("builtin");
        let drifted = ContextPolicy {
            max_request_bytes: 1024 * 1024,
            context_cap_bytes: 32 * 1024,
            ..builtin.context.clone()
        };
        let layered = layer_budget(&drifted, &ContextPolicy::default());
        assert_eq!(layered.max_request_bytes, 1024 * 1024);
        assert_eq!(layered.context_cap_bytes, 32 * 1024);
        // Untouched builtin fields flow through unchanged.
        assert_eq!(layered.max_input_bytes, builtin.context.max_input_bytes);
        assert_eq!(
            layered.provider_timeout_ms,
            builtin.context.provider_timeout_ms
        );
    }

    /// Explicit config overrides win over the builtin even when the builtin
    /// drifts: only fields equal to the config-layer default fall back to
    /// the builtin.
    #[test]
    fn explicit_config_override_wins_over_drifted_builtin() {
        let builtin = builtin_profile("openai-chat").expect("builtin");
        let drifted = ContextPolicy {
            max_request_bytes: 1024 * 1024,
            ..builtin.context.clone()
        };
        let base = ContextPolicy {
            max_request_bytes: 256 * 1024,
            ..ContextPolicy::default()
        };
        assert_eq!(layer_budget(&drifted, &base).max_request_bytes, 256 * 1024);
    }

    /// The builtin profile carries both prompt slots; the summarize slot is
    /// the system message of compaction summary requests.
    #[test]
    fn builtin_profile_defines_the_summarize_slot() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let prompt = resolved.summarize_prompt().expect("slot exists");
        assert!(prompt.contains("compacting the earlier history"));
        assert_eq!(
            resolved.compaction().strategy,
            CompactionStrategy::Off,
            "builtin compaction is off"
        );
        assert!(resolved.compaction().max_summary_source_bytes > 0);
    }

    /// The config layer can activate a compaction strategy; the layering
    /// must not drop an active strategy the way it drops untouched budget
    /// overrides.
    #[test]
    fn layer_budget_carries_an_active_compaction_strategy() {
        let builtin = builtin_profile("openai-chat").expect("builtin");
        let base = ContextPolicy {
            compaction: CompactionPolicy {
                strategy: CompactionStrategy::SummarizeOnDiscard,
                max_summary_source_bytes: 4_096,
                ..CompactionPolicy::default()
            },
            ..ContextPolicy::default()
        };
        let layered = layer_budget(&builtin.context, &base);
        assert_eq!(
            layered.compaction.strategy,
            CompactionStrategy::SummarizeOnDiscard
        );
        assert_eq!(layered.compaction.max_summary_source_bytes, 4_096);
        // An Off base keeps the builtin policy (which is also Off).
        let layered_default = layer_budget(&builtin.context, &ContextPolicy::default());
        assert_eq!(layered_default.compaction.strategy, CompactionStrategy::Off);
    }

    /// Fixture bindings with `provider_type = "test"` are part of the
    /// persisted-binding compat surface: the crate-local session fixtures
    /// and any historical binary that persisted such a binding must keep
    /// resolving.
    #[test]
    fn test_fixture_provider_type_resolves() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("test", "m"))
            .expect("fixture provider type must resolve");
        assert_eq!(
            resolved.profile().profile_id,
            GENERIC_OPENAI_CHAT_PROFILE_ID
        );
    }

    #[test]
    fn unknown_provider_type_fails_closed() {
        let error = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("anthropic", "m"))
            .expect_err("unknown provider type must fail");
        assert!(matches!(
            error,
            ProfileError::UnknownProviderType { ref provider_type } if provider_type == "anthropic"
        ));
    }

    #[test]
    fn invalid_base_policy_is_rejected_at_catalog_construction() {
        let base = ContextPolicy {
            max_request_bytes: 0,
            ..ContextPolicy::default()
        };
        let error = ProfileCatalog::new(base, minimal_registry())
            .expect_err("invalid base must be rejected");
        assert!(matches!(error, ProfileError::Invalid { .. }));
    }

    #[test]
    fn embedded_fixture_bindings_resolve_to_the_generic_profile() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("embedded", "embedded"))
            .expect("resolves");
        assert_eq!(
            resolved.profile().profile_id,
            GENERIC_OPENAI_CHAT_PROFILE_ID
        );
    }

    /// The read-before-mutate rule spans two tools, so no single tool
    /// description can carry it. Losing it here makes a real model omit
    /// `precondition` and every mutation is rejected as stale.
    #[test]
    fn system_prompt_states_the_read_before_mutate_rule() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let prompt = resolved.system_prompt("").expect("slot exists");
        for needle in [
            "read_file",
            "sha256",
            "precondition",
            "edit_file",
            "write_file",
        ] {
            assert!(prompt.contains(needle), "system prompt lost `{needle}`");
        }
        assert!(
            prompt.contains("re-read"),
            "a second edit to the same file needs the fresh digest"
        );
    }

    /// Verification is a completion bar, not a suggestion; the engine rejects
    /// completion without it, so the model must know before it claims success.
    #[test]
    fn system_prompt_states_the_completion_bar() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let prompt = resolved.system_prompt("").expect("slot exists");
        assert!(prompt.contains("verification"));
        assert!(prompt.contains("unfinished"));
    }

    #[test]
    fn system_prompt_render_injects_repository_context_after_final_rule() {
        let resolved = catalog(ContextPolicy::default(), minimal_registry())
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        let prompt = resolved
            .system_prompt("REPOSITORY-CONTEXT")
            .expect("slot exists");
        assert!(prompt.ends_with("\nREPOSITORY-CONTEXT"));
        // The read-before-mutate rule must survive the move into the slot.
        assert!(prompt.contains("first call `read_file` on it"));
        assert!(prompt.contains("`precondition`"));
        assert!(!prompt.contains("{repository_context}"));
        assert_eq!(
            resolved.system_prompt("").expect("slot exists"),
            prompt.strip_suffix("REPOSITORY-CONTEXT").expect("suffix")
        );
    }

    #[test]
    fn fingerprint_stable_across_process_orderings() {
        let catalog = catalog(ContextPolicy::default(), minimal_registry());
        let first = catalog
            .resolve(&binding("openai-chat", "m"))
            .expect("resolves");
        // Serialize-by-value via serde_json::json! is canonical for our
        // field set (no maps with order-dependent iteration), so two equal
        // profiles yield equal hex digests; sanity-check the shape.
        let fingerprint = first.fingerprint();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn anthropic_messages_profile_aligns_claude_tool_presentation_and_warm_prefix_compaction() {
        let reg = registry(
            "{version:1,default_model:'claude/seed',providers:{claude:{type:'anthropic-messages',models:['seed'],base_url:'http://127.0.0.1:8317',api_key:'k'},oai_claude:{type:'openai-chat',models:['m'],endpoint:'http://127.0.0.1:9',api_key:'k',tool_style:'claude_code'}}}",
        );
        let base = ContextPolicy {
            compaction: CompactionPolicy {
                strategy: CompactionStrategy::ElideToolResultsThenSummarize,
                ..CompactionPolicy::default()
            },
            ..ContextPolicy::default()
        };
        let cat = catalog(base, reg);
        let mut anth_binding = binding("anthropic-messages", "seed");
        anth_binding.provider_name = "claude".into();
        let resolved = cat.resolve(&anth_binding).expect("resolves");
        assert_eq!(resolved.profile().profile_id, ANTHROPIC_MESSAGES_PROFILE_ID);
        assert_eq!(resolved.profile().version, ProfileVersion::V1_1);
        assert_eq!(
            resolved.tool_presentation(),
            ToolPresentationStyle::ClaudeCode
        );
        assert!(resolved.compaction().reuse_prefix);
        let prompt = resolved.system_prompt("").expect("slot exists");
        for needle in ["`Read`", "`Edit`", "`Write`", "`Glob`", "`Grep`", "`Bash`"] {
            assert!(prompt.contains(needle), "missing {needle}");
        }

        // An openai-chat provider with explicit `tool_style: "claude_code"` swaps prompt + tools coherently.
        let mut oai_binding = binding("openai-chat", "m");
        oai_binding.provider_name = "oai_claude".into();
        let overridden = cat.resolve(&oai_binding).expect("resolves");
        assert_eq!(
            overridden.tool_presentation(),
            ToolPresentationStyle::ClaudeCode
        );
        assert!(overridden.system_prompt("").unwrap().contains("`Edit`"));
    }
}
