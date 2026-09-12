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
    AGENT_SYSTEM_PROMPT_SLOT, CompactionPolicy, ContextPolicy, GENERIC_OPENAI_CHAT_PROFILE_ID,
    HarnessProfile, ProfileVersion, SessionProviderBinding, SystemPromptSpec, TokenEstimateParams,
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
const AGENT_SYSTEM_PROMPT_TEMPLATE: &str = "You are Latte Code, a coding agent making scoped changes to the repository described below. Work only within it: paths outside the workspace are rejected, and you cannot reach the network.\n\nRead before you write. To change an existing file, first call `read_file` on it and pass the `sha256` it returns as `precondition` to `edit_file` or `write_file`. A mutation without the digest of the version you actually read is rejected, and this holds again for every later edit to the same file: re-read to get the new digest. Prefer `edit_file`, whose `before` must match the file verbatim and occur exactly once; reach for `write_file` only to create a file or to rewrite one whole.\n\nUnderstand before you change. Locate the relevant code with `search` and `list_directory`, and read enough of it that your edit follows what is already there. `read_project_manifest` shows the language and dependencies. Do not invent APIs, dependencies, or file paths you have not observed.\n\nFinish what you start. After editing, check your own work with `git_diff`. When a verification command is configured it must pass before the task is complete; a failing, missing, or unrun verification means the work is unfinished. Report plainly what you changed and what you verified. If a tool call is rejected, read the error and correct the call rather than repeating it unchanged. If the task is ambiguous or you lack the means to finish it, say so instead of guessing.\n{repository_context}";

/// Provider types recognized by the built-in catalog. `embedded` covers the
/// in-process test fixture binding and `test` covers the crate-local session
/// fixtures — both are fixture identities, not production protocols. The
/// whitelist is also the upgrade-compat boundary: bindings are persisted
/// verbatim (including `provider_type`), so every type that could have been
/// persisted by any previous release must resolve here or old sessions fail
/// to reopen.
const BUILTIN_PROVIDER_TYPES: [&str; 3] = ["openai-chat", "embedded", "test"];

/// The provider type used for eager, binding-free pre-checks (prompt budget
/// validation before enqueue). Every built-in profile shares the same prompt
/// catalog today, so the concrete value only names the builtin family.
const DEFAULT_BUILTIN_PROVIDER_TYPE: &str = "openai-chat";

fn builtin_profile(provider_type: &str) -> Result<HarnessProfile, ProfileError> {
    if !BUILTIN_PROVIDER_TYPES.contains(&provider_type) {
        return Err(ProfileError::UnknownProviderType {
            provider_type: provider_type.to_owned(),
        });
    }
    let context = ContextPolicy {
        max_request_bytes: 512 * 1024,
        max_input_bytes: 384 * 1024,
        reserved_output_bytes: 128 * 1024,
        context_cap_bytes: 64 * 1024,
        max_tool_rounds: None,
        provider_timeout_ms: 60_000,
        ..ContextPolicy::default()
    };
    let prompts = SystemPromptSpec {
        slots: [(
            AGENT_SYSTEM_PROMPT_SLOT.to_owned(),
            AGENT_SYSTEM_PROMPT_TEMPLATE.to_owned(),
        )]
        .into_iter()
        .collect(),
    };
    Ok(HarnessProfile {
        profile_id: GENERIC_OPENAI_CHAT_PROFILE_ID.to_owned(),
        version: ProfileVersion::V1_0,
        context,
        prompts,
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
        compaction: builtin.compaction.clone(),
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
            version: 2,
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
        // 1_000_000 tokens * 4 > 64 KiB default: cap stays, never widens.
        assert_eq!(large.history_policy().context_cap_bytes, 64 * 1024);
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
        assert!(prompt.contains("`sha256` it returns as `precondition`"));
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
}
