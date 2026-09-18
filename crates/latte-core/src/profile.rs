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
/// Rendered as the system message of a dedicated summary request when the
/// resolved compaction strategy needs one.
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

/// Data-shaped compaction strategy selector. The loop `match`es this value
/// and runs the corresponding algorithm; the strategy implementation never
/// lives in the profile, so profiles stay snapshotable, versionable data.
/// See `docs/design/context-design.md` §3.3 for the evolution contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// History a request window discards is silently superseded — the
    /// pre-compaction behavior.
    #[default]
    Off,
    /// Older history is summarized through the `agent.summarize` prompt
    /// slot and replaced in the window by a durable `compact_summary`
    /// card. Summarization runs both reactively — when the newest-first
    /// window has to discard older history — and proactively, once the
    /// estimated fill reaches `trigger_ratio` before any discard happens.
    /// A suffix of the most recent segments always travels verbatim
    /// (`retain_ratio`); the card's `retain_from_sequence` payload records
    /// the boundary so future windows replay it raw after the summary.
    SummarizeOnDiscard,
    /// Deterministic first tier, then the same summary tier: before any
    /// model call, old tool results in the would-be-superseded prefix are
    /// replaced by a secret-free skeleton (tool name, original byte size,
    /// ok/error status). The full results stay in the transcript; only an
    /// append-only `tool_result_elision` card records the boundary. When
    /// skeletonizing alone brings the window back under the trigger/byte
    /// wall no summary request is made; otherwise the flow continues
    /// exactly like `SummarizeOnDiscard`. The same forced elision is the
    /// one-shot recovery for a provider context-overflow rejection.
    ElideToolResultsThenSummarize,
}

/// Compaction configuration for one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPolicy {
    pub strategy: CompactionStrategy,
    /// Proactive trigger as a percentage of the estimated request-budget
    /// use. The loop summarizes the oldest segments once the next window's
    /// estimated fill reaches this ratio, instead of waiting for the hard
    /// byte wall; the read-only [`ContextUsage`] projection reports the
    /// same condition as `proactive_compaction_due`.
    pub trigger_ratio: u8,
    /// Percentage of the exact request budget reserved for the most recent
    /// segments, which travel verbatim after the summary instead of being
    /// summarized. The retained suffix is built from whole user segments
    /// newest-first, so tool-call/result pairs are never split.
    pub retain_ratio: u8,
    /// Prompt slot used for the summarization request.
    pub summary_prompt_id: String,
    /// Byte bound on the history one summarization request may read.
    pub max_summary_source_bytes: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            strategy: CompactionStrategy::Off,
            trigger_ratio: 90,
            retain_ratio: 20,
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

impl TokenEstimateParams {
    /// Estimates the token count for `bytes` wire bytes. The estimate rounds
    /// up, so a nonzero byte length never reports zero tokens. Estimates only
    /// drive display and proactive compaction; they never participate in a
    /// fail-closed byte decision.
    #[must_use]
    pub fn estimate_tokens(&self, bytes: usize) -> usize {
        bytes.div_ceil(self.bytes_per_token)
    }
}

/// Read-only projection of how full the next request window is under one
/// resolved profile. Plain data so it can cross the server/client boundary and
/// be used as a contract-test fixture; it carries no credentials.
///
/// Byte fields stay exact (`wire_bytes` against the exact request budget);
/// token fields are estimates from [`TokenEstimateParams`] and must never be
/// used for a hard boundary. `used_bytes` measures the durable history window
/// the next request would carry *without* the not-yet-submitted prompt: the
/// prompt is always admitted and a window that cannot fit it fails the turn
/// instead of being reported here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    /// Exact per-request byte budget the window is fitted against
    /// (`min(max_request_bytes, max_input_bytes - reserved_output_bytes)`).
    pub request_budget_bytes: usize,
    /// Exact wire bytes of system plus the kept history segments.
    pub used_bytes: usize,
    /// Exact bytes still free before the window starts discarding segments.
    pub remaining_bytes: usize,
    /// Byte cap on the L1 static repository context, carried for display.
    pub context_cap_bytes: usize,
    /// Estimated tokens behind `used_bytes`.
    pub estimated_used_tokens: usize,
    /// Estimated tokens behind `request_budget_bytes`.
    pub estimated_budget_tokens: usize,
    /// Estimated tokens behind `remaining_bytes`.
    pub estimated_remaining_tokens: usize,
    /// Segments the newest-first fit dropped; the range a compaction summary
    /// would supersede on the next request.
    pub discarded_segments: usize,
    /// Resolved compaction strategy for this binding.
    pub compaction_strategy: CompactionStrategy,
    /// Configured proactive trigger percentage (1..=100 when compaction is
    /// enabled; otherwise inert).
    pub trigger_ratio: u8,
    /// `true` when the estimated fill ratio reaches `trigger_ratio` and the
    /// resolved strategy compacts proactively. Always `false` while
    /// compaction is `Off`: the ratio alone never arms a trigger.
    pub proactive_compaction_due: bool,
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
        if self.compaction.strategy != CompactionStrategy::Off {
            if self.compaction.trigger_ratio == 0 || self.compaction.trigger_ratio > 100 {
                return Err("compaction trigger_ratio must be in 1..=100 when enabled".into());
            }
            if self.compaction.retain_ratio == 0 || self.compaction.retain_ratio > 80 {
                return Err("compaction retain_ratio must be in 1..=80 when enabled".into());
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

    /// The exact per-request byte budget the newest-first window fits
    /// against, matching the loop's historical computation:
    /// `min(max_request_bytes, max_input_bytes - reserved_output_bytes)`.
    #[must_use]
    pub fn request_budget_bytes(&self) -> usize {
        self.max_request_bytes.min(
            self.max_input_bytes
                .saturating_sub(self.reserved_output_bytes),
        )
    }

    /// Builds the read-only [`ContextUsage`] projection for one fitted
    /// window. `used_bytes` is the exact wire size of system plus the kept
    /// history segments; `discarded_segments` counts the segments the
    /// newest-first fit could not admit.
    ///
    /// The proactive trigger compares estimated tokens (cross-multiplied to
    /// stay integer-exact) against `trigger_ratio` and only arms while an
    /// active compaction strategy is resolved. Byte values remain exact; the
    /// estimate never enters a fail-closed decision.
    #[must_use]
    pub fn usage(&self, used_bytes: usize, discarded_segments: usize) -> ContextUsage {
        let budget = self.request_budget_bytes();
        let remaining = budget.saturating_sub(used_bytes);
        let estimated_used = self.token_estimate.estimate_tokens(used_bytes);
        let estimated_budget = self.token_estimate.estimate_tokens(budget);
        let estimated_remaining = self.token_estimate.estimate_tokens(remaining);
        let proactive_compaction_due = self.compaction.strategy != CompactionStrategy::Off
            && self.compaction.trigger_ratio > 0
            && estimated_used * 100
                >= estimated_budget * usize::from(self.compaction.trigger_ratio);
        ContextUsage {
            request_budget_bytes: budget,
            used_bytes,
            remaining_bytes: remaining,
            context_cap_bytes: self.context_cap_bytes,
            estimated_used_tokens: estimated_used,
            estimated_budget_tokens: estimated_budget,
            estimated_remaining_tokens: estimated_remaining,
            discarded_segments,
            compaction_strategy: self.compaction.strategy,
            trigger_ratio: self.compaction.trigger_ratio,
            proactive_compaction_due,
        }
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
    fn compaction_invariants_only_apply_to_active_strategies() {
        let mut policy = valid_context();
        policy.compaction.trigger_ratio = 0;
        assert!(
            policy.validate().is_ok(),
            "an Off strategy is not validated"
        );
        policy.compaction.strategy = CompactionStrategy::SummarizeOnDiscard;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn compaction_rejects_active_strategy_with_empty_summary_slot() {
        let mut policy = valid_context();
        policy.compaction.strategy = CompactionStrategy::SummarizeOnDiscard;
        policy.compaction.summary_prompt_id = "  ".into();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn compaction_strategy_round_trips_through_serde() {
        for strategy in [
            CompactionStrategy::Off,
            CompactionStrategy::SummarizeOnDiscard,
        ] {
            let json = serde_json::to_string(&strategy).expect("serializable");
            let parsed: CompactionStrategy = serde_json::from_str(&json).expect("parses");
            assert_eq!(strategy, parsed);
        }
        assert!(
            serde_json::from_str::<CompactionStrategy>("\"elide_everything\"").is_err(),
            "unknown strategies are rejected at the schema boundary"
        );
    }

    #[test]
    fn token_estimate_must_be_nonzero() {
        let mut policy = valid_context();
        policy.token_estimate.bytes_per_token = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn token_estimate_rounds_up_and_never_reports_zero_for_bytes() {
        let params = TokenEstimateParams { bytes_per_token: 4 };
        assert_eq!(params.estimate_tokens(0), 0);
        assert_eq!(params.estimate_tokens(1), 1);
        assert_eq!(params.estimate_tokens(4), 1);
        assert_eq!(params.estimate_tokens(5), 2);
        assert_eq!(params.estimate_tokens(400), 100);
    }

    #[test]
    fn request_budget_is_the_minimum_of_request_and_reserved_input() {
        let policy = valid_context();
        let expected = policy
            .max_request_bytes
            .min(policy.max_input_bytes - policy.reserved_output_bytes);
        assert_eq!(policy.request_budget_bytes(), expected);
        let mut tight = valid_context();
        tight.max_request_bytes = 1;
        assert_eq!(tight.request_budget_bytes(), 1);
    }

    #[test]
    fn usage_reports_exact_bytes_estimated_tokens_and_saturating_remaining() {
        let policy = valid_context();
        let budget = policy.request_budget_bytes();
        let usage = policy.usage(1_000, 2);
        assert_eq!(usage.request_budget_bytes, budget);
        assert_eq!(usage.used_bytes, 1_000);
        assert_eq!(usage.remaining_bytes, budget - 1_000);
        assert_eq!(usage.estimated_used_tokens, 250);
        assert_eq!(
            usage.estimated_budget_tokens,
            policy.token_estimate.estimate_tokens(budget)
        );
        assert_eq!(
            usage.estimated_remaining_tokens,
            policy.token_estimate.estimate_tokens(budget - 1_000)
        );
        assert_eq!(usage.discarded_segments, 2);
        assert_eq!(usage.context_cap_bytes, policy.context_cap_bytes);
        // Over-use saturates at zero instead of underflowing.
        let overflow = policy.usage(budget + 500, 0);
        assert_eq!(overflow.remaining_bytes, 0);
        assert_eq!(overflow.estimated_remaining_tokens, 0);
    }

    #[test]
    fn proactive_trigger_never_arms_while_compaction_is_off() {
        let policy = valid_context();
        // 90% fill with the default 80% ratio would be due if enabled...
        let budget = policy.request_budget_bytes();
        let usage = policy.usage(budget * 9 / 10, 0);
        assert!(!usage.proactive_compaction_due);
        assert_eq!(usage.compaction_strategy, CompactionStrategy::Off);
    }

    #[test]
    fn proactive_trigger_arms_at_and_above_the_ratio_only_when_enabled() {
        let mut policy = valid_context();
        policy.compaction.strategy = CompactionStrategy::SummarizeOnDiscard;
        policy.compaction.trigger_ratio = 50;
        let budget = policy.request_budget_bytes();
        // The estimate rounds bytes up to whole tokens, so pick boundaries at
        // token granularity: the default estimate is 4 bytes/token and the
        // budget is 4-aligned, making the arithmetic exact.
        let threshold_bytes = budget / 2;
        assert!(
            !policy
                .usage(threshold_bytes - 4, 0)
                .proactive_compaction_due
        );
        assert!(policy.usage(threshold_bytes, 0).proactive_compaction_due);
        assert!(policy.usage(budget * 9 / 10, 3).proactive_compaction_due);
        policy.compaction.trigger_ratio = 100;
        assert!(!policy.usage(budget - 4, 0).proactive_compaction_due);
        assert!(policy.usage(budget, 0).proactive_compaction_due);
    }

    #[test]
    fn context_usage_round_trips_through_serde_and_denies_unknown_fields() {
        let usage = valid_context().usage(10, 1);
        let json = serde_json::to_string(&usage).expect("serializable");
        let parsed: ContextUsage = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(usage, parsed);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["bogus"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ContextUsage>(value).is_err());
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
