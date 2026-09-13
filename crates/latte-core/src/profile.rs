//! Versioned harness profile types.
//!
//! A [`HarnessProfile`] is the per-model behavioral contract for the agent
//! loop: request budgets, compaction knobs, and prompt slots. It is plain
//! data by design — data can be snapshotted into session records, versioned,
//! and used as a deterministic contract-test fixture.
//!
//! Profiles are orthogonal to provider adapters: the adapter answers "how do
//! I talk to this model over the wire", the profile answers "how do I work
//! with this model". Both are selected by the same
//! [`crate::SessionProviderBinding`]; neither owns the other.
//!
//! Profiles never carry permission, effect, or policy fields. A profile can
//! only shape prompts and tighten budgets; authority stays in `latte-engine`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Identifier of the single built-in profile for OpenAI-chat-compatible
/// bindings. Its values are the historical `SessionHistoryPolicy` defaults
/// and system prompt, moved verbatim, so adopting profiles changes no
/// existing behavior.
pub const GENERIC_OPENAI_CHAT_PROFILE_ID: &str = "generic-openai-chat";

/// Slot holding the main system prompt rendered at every provider request.
pub const AGENT_SYSTEM_PROMPT_SLOT: &str = "agent.system";

/// Slot holding the summarization instruction used by context compaction.
/// Reserved: compaction is not implemented yet, so this slot is declared but
/// never rendered.
pub const AGENT_SUMMARIZE_PROMPT_SLOT: &str = "agent.summarize";

/// Structured profile version.
///
/// `minor` bumps add optional fields or adjust prompt wording; stored
/// snapshots stay compatible. `major` bumps change semantics (budget
/// derivation rules, slot meanings) and require an explicit migration
/// decision for existing sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProfileVersion {
    pub major: u32,
    pub minor: u32,
}

impl ProfileVersion {
    /// Version of the initial profile surface shipped with the profile
    /// abstraction itself.
    pub const V1_0: Self = Self { major: 1, minor: 0 };
}

/// Compaction configuration for one profile.
///
/// Declared as part of the v1 profile surface but not yet consumed: the
/// `enabled` flag is `false` in every shipped profile and the agent loop
/// keeps its current newest-first window behavior until compaction lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    pub enabled: bool,
    /// Compaction trigger as a percentage of the context cap estimate.
    /// Must be in `1..=100` when compaction is enabled.
    pub trigger_ratio: u8,
    /// Prompt slot used for the summarization request.
    pub summary_prompt_id: String,
    /// Byte bound on the history one summarization request may read.
    pub max_summary_source_bytes: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_ratio: 80,
            summary_prompt_id: AGENT_SUMMARIZE_PROMPT_SLOT.to_owned(),
            max_summary_source_bytes: 32 * 1024,
        }
    }
}

/// Token estimation parameters.
///
/// Byte budgets stay authoritative for every hard boundary; these estimates
/// only drive display and (later) compaction triggering, so no tokenizer
/// dependency is introduced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenEstimateParams {
    /// Average wire bytes per token assumed for this model family.
    pub bytes_per_token: usize,
}

impl Default for TokenEstimateParams {
    fn default() -> Self {
        Self { bytes_per_token: 4 }
    }
}

/// Context and request budgets for one profile. Field-for-field this is the
/// historical `latte_headless::session::SessionHistoryPolicy` plus the
/// compaction and estimation extensions; the loop consumes it through a
/// conversion so existing behavior is preserved exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicy {
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub reserved_output_bytes: usize,
    pub context_cap_bytes: usize,
    /// Optional hard bound on how many tool batches one turn may open;
    /// `None` means unlimited.
    pub max_tool_rounds: Option<u32>,
    /// Wall-clock budget for one provider request.
    pub provider_timeout_ms: u64,
    #[serde(default)]
    pub compaction: CompactionPolicy,
    #[serde(default)]
    pub token_estimate: TokenEstimateParams,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_request_bytes: 512 * 1024,
            max_input_bytes: 384 * 1024,
            reserved_output_bytes: 128 * 1024,
            context_cap_bytes: 64 * 1024,
            max_tool_rounds: None,
            provider_timeout_ms: 60_000,
            compaction: CompactionPolicy::default(),
            token_estimate: TokenEstimateParams::default(),
        }
    }
}

impl ContextPolicy {
    /// Validates budgets and compaction invariants without building a
    /// request.
    ///
    /// # Errors
    ///
    /// Returns a human-readable explanation when any bound is zero, when the
    /// reserved output is not smaller than the input budget, or when an
    /// enabled compaction policy is inconsistent.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_bytes == 0
            || self.max_input_bytes == 0
            || self.reserved_output_bytes >= self.max_input_bytes
            || self.context_cap_bytes == 0
        {
            return Err(
                "max_request_bytes/context cap must be nonzero and reserved output must be smaller than input budget"
                    .into(),
            );
        }
        if self.provider_timeout_ms == 0 {
            return Err("provider_timeout_ms must be nonzero".into());
        }
        if let Some(0) = self.max_tool_rounds {
            return Err("max_tool_rounds must be at least 1 when set".into());
        }
        if self.token_estimate.bytes_per_token == 0 {
            return Err("bytes_per_token must be nonzero".into());
        }
        if self.compaction.enabled {
            if self.compaction.trigger_ratio == 0 || self.compaction.trigger_ratio > 100 {
                return Err("compaction trigger_ratio must be in 1..=100 when enabled".into());
            }
            if self.compaction.max_summary_source_bytes == 0 {
                return Err("compaction max_summary_source_bytes must be nonzero".into());
            }
            if self.compaction.summary_prompt_id.trim().is_empty() {
                return Err("compaction summary_prompt_id must not be empty".into());
            }
        }
        Ok(())
    }
}

/// Prompt slot collection for one profile.
///
/// Slot ids are profile-scoped identifiers (`agent.system`,
/// `agent.summarize`); slot bodies are templates rendered by the loop. The
/// v1 catalog fills `agent.system` with the historical system prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemPromptSpec {
    pub slots: BTreeMap<String, String>,
}

impl SystemPromptSpec {
    /// Returns the rendered body of one slot, or `None` when the profile
    /// does not define it.
    #[must_use]
    pub fn render(&self, slot: &str, repository_context: &str) -> Option<String> {
        self.slots
            .get(slot)
            .map(|template| template.replace("{repository_context}", repository_context))
    }
}

/// The resolved per-model behavioral contract consumed by the agent loop.
///
/// `tools` and `semantics` are reserved dimensions declared by the design;
/// they stay `None` until a real consumer exists, and a `None` value must
/// never change loop behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessProfile {
    pub profile_id: String,
    pub version: ProfileVersion,
    pub context: ContextPolicy,
    pub prompts: SystemPromptSpec,
}

impl HarnessProfile {
    /// Validates the profile as a whole.
    ///
    /// # Errors
    ///
    /// Returns a human-readable explanation when the identity is malformed
    /// or the context policy is inconsistent.
    pub fn validate(&self) -> Result<(), String> {
        if self.profile_id.trim().is_empty() || self.profile_id.len() > 128 {
            return Err("profile_id must be 1..=128 non-whitespace characters".into());
        }
        self.context.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_context() -> ContextPolicy {
        ContextPolicy {
            max_request_bytes: 512 * 1024,
            max_input_bytes: 384 * 1024,
            reserved_output_bytes: 128 * 1024,
            context_cap_bytes: 64 * 1024,
            max_tool_rounds: None,
            provider_timeout_ms: 60_000,
            compaction: CompactionPolicy::default(),
            token_estimate: TokenEstimateParams::default(),
        }
    }

    #[test]
    fn context_policy_accepts_the_historical_defaults() {
        assert!(valid_context().validate().is_ok());
    }

    #[test]
    fn context_policy_rejects_zero_bounds() {
        let mut policy = valid_context();
        policy.max_request_bytes = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn context_policy_rejects_reserved_not_smaller_than_input() {
        let mut policy = valid_context();
        policy.reserved_output_bytes = policy.max_input_bytes;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn context_policy_accepts_request_smaller_than_input_plus_reserved() {
        // The historical policy computes the budget as
        // `min(max_request_bytes, max_input_bytes - reserved_output_bytes)`;
        // a small request byte bound is valid and simply wins the minimum.
        let mut policy = valid_context();
        policy.max_request_bytes = 1;
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn context_policy_rejects_zero_tool_round_bound() {
        let mut policy = valid_context();
        policy.max_tool_rounds = Some(0);
        assert!(policy.validate().is_err());
    }

    #[test]
    fn compaction_invariants_only_apply_when_enabled() {
        let mut policy = valid_context();
        policy.compaction.trigger_ratio = 0;
        assert!(
            policy.validate().is_ok(),
            "disabled compaction is not validated"
        );
        policy.compaction.enabled = true;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn compaction_rejects_enabled_with_empty_summary_slot() {
        let mut policy = valid_context();
        policy.compaction.enabled = true;
        policy.compaction.summary_prompt_id = "  ".into();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn token_estimate_must_be_nonzero() {
        let mut policy = valid_context();
        policy.token_estimate.bytes_per_token = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn prompt_spec_renders_repository_context_injection_point() {
        let spec = SystemPromptSpec {
            slots: BTreeMap::from([(
                AGENT_SYSTEM_PROMPT_SLOT.to_owned(),
                "head\n{repository_context}\ntail".to_owned(),
            )]),
        };
        assert_eq!(
            spec.render(AGENT_SYSTEM_PROMPT_SLOT, "REPO"),
            Some("head\nREPO\ntail".to_owned())
        );
        assert_eq!(spec.render("missing.slot", "REPO"), None);
    }

    #[test]
    fn profile_rejects_blank_identity() {
        let mut profile = HarnessProfile {
            profile_id: "  ".into(),
            version: ProfileVersion::V1_0,
            context: valid_context(),
            prompts: SystemPromptSpec::default(),
        };
        assert!(profile.validate().is_err());
        profile.profile_id = GENERIC_OPENAI_CHAT_PROFILE_ID.to_owned();
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn profile_types_round_trip_through_serde() {
        let profile = HarnessProfile {
            profile_id: GENERIC_OPENAI_CHAT_PROFILE_ID.to_owned(),
            version: ProfileVersion::V1_0,
            context: valid_context(),
            prompts: SystemPromptSpec::default(),
        };
        let json = serde_json::to_string(&profile).expect("serializable");
        let parsed: HarnessProfile = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(profile, parsed);
    }

    #[test]
    fn context_policy_denies_unknown_fields() {
        let json = r#"{"max_request_bytes":1,"max_input_bytes":1,"reserved_output_bytes":1,"context_cap_bytes":1,"max_tool_rounds":null,"provider_timeout_ms":1,"permission":"allow-all"}"#;
        assert!(serde_json::from_str::<ContextPolicy>(json).is_err());
    }
}
