//! Additive v2 conversation-thread protocol.
//!
//! This module intentionally does not alter any v1 command, event, or run
//! representation.  A decoder which only understands protocol v1 can keep
//! reading its existing records byte-for-byte, while v2 consumers use the
//! separate envelopes below.

use crate::{RunId, ThreadCommandId, ThreadEventId, ThreadId, TranscriptEntryId};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::LazyLock};

/// Version of the additive conversation protocol.
pub const THREAD_PROTOCOL_VERSION: u16 = 2;
/// Maximum durable display text accepted by the thread protocol.
pub const THREAD_TEXT_CAP_BYTES: usize = 64 * 1024;
/// Maximum number of bytes in an accepted `OpenAI` Chat-compatible tool-call ID.
pub const OPENAI_CHAT_TOOL_CALL_ID_MAX_BYTES: usize = 256;

/// Returns whether an untrusted provider-issued opaque identifier is safe to
/// use as a durable protocol identifier.
///
/// The accepted grammar is the deliberately small ASCII subset
/// `[A-Za-z0-9_-]{1,256}`. `OpenAI`'s normal `call_…` IDs fit this grammar, as
/// do safe IDs from compatible Chat backends. In particular, it excludes
/// assignment separators, whitespace, controls, and Unicode lookalikes. Those
/// characters can be transformed by transcript redaction, so accepting them
/// would make a provider identity unsuitable as a durable effect key.
///
/// This rule applies to provider-issued tool-call and input-request
/// identifiers. It does not classify ordinary code or tool input such as
/// `token=value` as invalid.
#[must_use]
pub fn valid_openai_chat_opaque_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= OPENAI_CHAT_TOOL_CALL_ID_MAX_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Returns whether a provider-issued Chat-compatible tool-call ID is safe.
#[must_use]
pub fn valid_openai_chat_tool_call_id(id: &str) -> bool {
    valid_openai_chat_opaque_id(id)
}

/// Returns whether a provider-issued Chat-compatible input-request ID is
/// safe. Input IDs become durable request, deduplication, and transcript keys
/// just like tool-call IDs, so they deliberately share the exact grammar.
#[must_use]
pub fn valid_openai_chat_input_request_id(id: &str) -> bool {
    valid_openai_chat_opaque_id(id)
}

/// Lifecycle of the conversation projection, distinct from its child run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadLifecycle {
    /// A thread may receive a follow-up. Its newest child either completed or
    /// failed with an explicitly retryable runtime error.
    Ready,
    /// The active child is talking to a provider or preparing work.
    Running,
    /// The active child requires an explicit permission decision.
    WaitingPermission,
    /// The active child requires non-secret user input.
    WaitingInput,
    /// Provider work was interrupted before an external effect started.
    Interrupted,
    /// A terminal child failure which cannot be continued.
    Failed,
    /// An external effect may have happened and must be reconciled first.
    ReconciliationRequired,
}

impl ThreadLifecycle {
    #[must_use]
    pub const fn accepts_follow_up(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Outcome of a session create operation.
#[derive(Clone, Debug, PartialEq)]
pub enum CreateOutcome<T = ThreadSnapshot> {
    /// A new session was created.
    Created(T),
    /// The session already existed (idempotent replay).
    Replayed(T),
}

/// Error class for a session-create acceptance failure, carried by the
/// durable-acceptance signal so the HTTP layer can map it to the right status
/// code without inspecting error strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateAcceptError {
    /// A durable command-id reuse with a different payload, or a non-replay
    /// create for an already-existing thread. Maps to 409 Conflict.
    Conflict(String),
    /// Any other acceptance failure. Maps to 500.
    Failed(String),
}

/// A serializable copy of every semantic provider-binding field.  Credential
/// *values* are intentionally absent; only the stable non-secret reference
/// and generation are durable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadProviderBindingV2 {
    pub version: u32,
    pub provider_name: String,
    pub provider_type: String,
    pub protocol: String,
    pub model: String,
    pub config_fingerprint: String,
    pub tools_fingerprint: String,
    pub aliases: BTreeMap<String, String>,
    pub credential_ref_id: String,
    pub data_scope_id: String,
    pub credential_generation: u64,
}

impl ThreadProviderBindingV2 {
    /// Validates fields before any credential resolution or history egress.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe explanation when a binding field is malformed.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("provider_name", &self.provider_name),
            ("provider_type", &self.provider_type),
            ("protocol", &self.protocol),
            ("model", &self.model),
            ("config_fingerprint", &self.config_fingerprint),
            ("tools_fingerprint", &self.tools_fingerprint),
            ("credential_ref_id", &self.credential_ref_id),
            ("data_scope_id", &self.data_scope_id),
        ] {
            if value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
            {
                return Err(format!("invalid {name}"));
            }
        }
        if self.aliases.len() > 256
            || self.aliases.iter().any(|(key, value)| {
                key.is_empty()
                    || value.is_empty()
                    || key.len() > 256
                    || value.len() > 256
                    || key.chars().any(char::is_control)
                    || value.chars().any(char::is_control)
            })
        {
            return Err("invalid provider aliases".into());
        }
        Ok(())
    }
}

/// A compact immutable child-run record shown in the session list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRunSummary {
    pub run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub ordinal: u64,
    pub status: ThreadRunStatus,
    pub run_revision: u64,
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<crate::protocol::FailureCode>,
}

/// Thread-safe projection of a v1 child status. It deliberately has no
/// transition API: `latte-engine` is the only writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRunStatus {
    Queued,
    Running,
    Cancelling,
    WaitingPermission,
    WaitingInput,
    Interrupted,
    Failed,
    Completed,
}

/// Typed transcript card kinds. The TUI never needs to parse debug strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    Permission,
    Input,
    Failure,
    Completion,
    System,
}

/// A redacted durable transcript card. `payload` is useful for structured
/// tool summaries, but is always passed through the protocol redactor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub entry_id: TranscriptEntryId,
    pub sequence: u64,
    pub run_id: Option<RunId>,
    pub kind: TranscriptKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    pub source_key: String,
    pub created_at_ms: u64,
}

/// A bounded transcript page. `next_after` is the last sequence returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptPage {
    pub entries: Vec<TranscriptEntry>,
    pub next_after: Option<u64>,
    pub has_more: bool,
}

/// Authoritative read projection for a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub thread_id: ThreadId,
    pub revision: u64,
    pub sequence: u64,
    pub lifecycle: ThreadLifecycle,
    pub binding: ThreadProviderBindingV2,
    pub latest_run_id: Option<RunId>,
    pub active_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<ThreadPendingRequest>,
    pub runs: Vec<ThreadRunSummary>,
    pub transcript: TranscriptPage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

/// Bounded, transcript-free metadata used by Session catalog discovery.
/// Provider credentials and executable effect data are intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSessionSummary {
    pub thread_id: ThreadId,
    pub title: String,
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    pub lifecycle: ThreadLifecycle,
    pub provider_name: String,
    pub model: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// The active request needed for explicit in-thread UI actions. Secret values
/// are never representable here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadPendingRequest {
    Permission {
        run_id: RunId,
        request_id: String,
        description: String,
        expected_run_revision: u64,
    },
    Input {
        run_id: RunId,
        request_id: String,
        prompt: String,
        expected_run_revision: u64,
    },
}

/// A versioned v2 command. It is isolated from `RuntimeCommand` so v1 JSON
/// and persisted envelopes remain exactly compatible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCommandEnvelope {
    pub protocol_version: u16,
    pub command_id: ThreadCommandId,
    pub command: ThreadCommand,
}

impl ThreadCommandEnvelope {
    #[must_use]
    pub const fn new(command_id: ThreadCommandId, command: ThreadCommand) -> Self {
        Self {
            protocol_version: THREAD_PROTOCOL_VERSION,
            command_id,
            command,
        }
    }
}

/// Commands accepted only by the thread composition boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadCommand {
    Start {
        thread_id: ThreadId,
        prompt: String,
        binding: ThreadProviderBindingV2,
        #[serde(default)]
        focus: Option<String>,
    },
    FollowUp {
        thread_id: ThreadId,
        expected_thread_revision: u64,
        prompt: String,
    },
    SwitchModel {
        thread_id: ThreadId,
        expected_thread_revision: u64,
        binding: ThreadProviderBindingV2,
    },
    Cancel {
        thread_id: ThreadId,
        expected_thread_revision: u64,
        expected_run_revision: u64,
    },
    ResolvePermission {
        thread_id: ThreadId,
        request_id: String,
        expected_thread_revision: u64,
        expected_run_revision: u64,
        allow: bool,
    },
    ProvideInput {
        thread_id: ThreadId,
        request_id: String,
        expected_thread_revision: u64,
        expected_run_revision: u64,
        value: String,
    },
}

/// A durable thread event, sequenced independently from v1 run events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadEventEnvelope {
    pub protocol_version: u16,
    pub event_id: ThreadEventId,
    pub thread_id: ThreadId,
    pub revision: u64,
    pub sequence: u64,
    pub event: ThreadEvent,
}

/// Events are typed wake-ups. Snapshots remain authoritative after a gap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadEvent {
    LifecycleChanged {
        lifecycle: ThreadLifecycle,
        run_id: Option<RunId>,
    },
    TranscriptAppended {
        entry: TranscriptEntry,
    },
    RunLinked {
        run: ThreadRunSummary,
    },
    BindingChanged {
        provider_name: String,
        model: String,
    },
    ReconciliationRequired {
        run_id: RunId,
        effect_id: String,
    },
}

/// Ephemeral progress shown only while connected. It is expressly not a
/// durable assistant message and must be cleared on snapshot/gap/reconnect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ThreadTransientProgress {
    ProviderAttempt {
        run_id: RunId,
        number: u32,
    },
    AssistantDelta {
        run_id: RunId,
        text: String,
    },
    ToolProgress {
        run_id: RunId,
        name: String,
        detail: String,
    },
}

/// Sanitizes control/ANSI text and caps it at a valid UTF-8 boundary.
#[must_use]
pub fn redact_thread_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(THREAD_TEXT_CAP_BYTES));
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        // Drop the whole CSI sequence rather than rendering terminal controls.
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        if out.len() + ch.len_utf8() > THREAD_TEXT_CAP_BYTES {
            out.push_str("…[truncated]");
            break;
        }
        out.push(ch);
    }
    redact_token_like_values(&out)
}

/// Recursively sanitizes untrusted structured data before persistence/UI use.
#[must_use]
pub fn redact_thread_value(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    let secret = [
                        "secret",
                        "token",
                        "password",
                        "api_key",
                        "authorization",
                        "credential",
                    ]
                    .iter()
                    .any(|needle| lower.contains(needle));
                    (
                        redact_thread_text(&key),
                        if secret {
                            Value::String("[REDACTED]".into())
                        } else {
                            redact_thread_value(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_thread_value).collect()),
        Value::String(value) => Value::String(redact_thread_text(&value)),
        other => other,
    }
}

fn redact_token_like_values(value: &str) -> String {
    // This runs after control filtering.  Prefer conservative redaction over
    // preserving a potentially reusable credential in a durable transcript
    // or a provider history replay. Assignments deliberately retain their
    // key/shape so the surrounding tool output stays intelligible.
    static NAMED_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?ix)\b(?P<name>(?:[a-z][a-z0-9_-]*?)?(?:api[_-]?key|access[_-]?token|auth[_-]?token|secret|token|password|credential)[a-z0-9_-]*)\s*(?P<separator>[:=])\s*(?:\"(?:\\.|[^\"])*\"|'(?:\\.|[^'])*'|[^\s,;\)\}\]]+)"#,
        )
        .expect("named secret assignment regex is valid")
    });
    static BEARER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\bbearer\s+(?:\"(?:\\.|[^\"])*\"|'(?:\\.|[^'])*'|[A-Za-z0-9._~+/=-]+)"#)
            .expect("bearer token regex is valid")
    });
    static OPENAI_KEY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bsk-[A-Za-z0-9_-]{6,}\b").expect("OpenAI key regex is valid")
    });
    let named = NAMED_ASSIGNMENT.replace_all(value, |captures: &regex::Captures<'_>| {
        format!("{}{}[REDACTED]", &captures["name"], &captures["separator"])
    });
    let bearer = BEARER.replace_all(&named, "Bearer [REDACTED]");
    OPENAI_KEY.replace_all(&bearer, "[REDACTED]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdSource, SystemIdSource};

    #[test]
    fn redactor_never_keeps_controls_or_obvious_secrets() {
        let text = redact_thread_text("ok\u{1b}[31m sk-this-should-not-survive-1234567890\u{7}");
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("sk-this"));
        let value =
            redact_thread_value(serde_json::json!({"api_key":"very-secret","ok":"\u{1b}[mtext"}));
        assert_eq!(value["api_key"], "[REDACTED]");
        assert_eq!(value["ok"], "text");
    }

    #[test]
    fn redactor_covers_assignment_quoted_bearer_and_provider_key_forms() {
        let secret = "sk-proj-0123456789abcdefghijklmnopqrstuvwxyz";
        let samples = [
            format!("OPENAI_API_KEY={secret}"),
            format!("anthropic_api_key = '{secret}'"),
            format!(r#"{{"apiKey": "{secret}"}}"#),
            format!("Authorization: Bearer {secret}"),
            format!(r#"Bearer "{secret}""#),
        ];
        for sample in samples {
            let redacted = redact_thread_text(&sample);
            assert!(!redacted.contains(secret), "{redacted}");
            assert!(redacted.contains("[REDACTED]"), "{redacted}");
        }

        // The named-assignment path also covers opaque provider values that
        // do not have a recognizable `sk-` or bearer shape.
        let named = redact_thread_text("GEMINI_API_KEY='short-provider-secret'");
        assert!(!named.contains("short-provider-secret"));
        assert!(named.contains("[REDACTED]"));
        let bare = redact_thread_text("token=short-provider-secret api_key=other-secret");
        assert!(!bare.contains("short-provider-secret"));
        assert!(!bare.contains("other-secret"));
        assert_eq!(bare.matches("[REDACTED]").count(), 2);

        let control = redact_thread_text(&format!("\u{1b}[31mOPENAI_API_KEY={secret}\u{7}"));
        assert!(!control.contains('\u{1b}'));
        assert!(!control.contains('\u{7}'));
        assert!(!control.contains(secret));
    }

    #[test]
    fn provider_tool_call_id_grammar_is_safe_without_reclassifying_code_input() {
        for id in ["call_abc123", "call_safe-opaque_42", "compatible-backend-1"] {
            assert!(valid_openai_chat_tool_call_id(id), "{id}");
        }
        for id in [
            "",
            "token=value",
            "call unsafe",
            "call:unsafe",
            "call/unsafe",
            "call\nunsafe",
            "call_✓",
            &"x".repeat(OPENAI_CHAT_TOOL_CALL_ID_MAX_BYTES + 1),
        ] {
            assert!(!valid_openai_chat_tool_call_id(id), "{id:?}");
        }

        // This is a protocol-ID boundary only. Tool input containing source
        // code remains valid and is handled by the normal redaction policy.
        assert_eq!(
            redact_thread_text("const token=value;"),
            "const token=[REDACTED];"
        );
    }

    #[test]
    fn thread_protocol_rejects_invalid_binding_and_keeps_commands_versioned() {
        let binding = ThreadProviderBindingV2 {
            version: 1,
            provider_name: "provider".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "model".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: BTreeMap::from([("read_file".into(), "rf".into())]),
            credential_ref_id: "keychain://main".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        assert!(binding.validate().is_ok());
        let mut invalid = binding.clone();
        invalid.model = "\n".into();
        assert!(invalid.validate().is_err());
        invalid = binding.clone();
        invalid.aliases = BTreeMap::from([("bad\nkey".into(), "rf".into())]);
        assert!(invalid.validate().is_err());

        let ids = SystemIdSource::default();
        let command = ThreadCommandEnvelope::new(
            ThreadCommandId::from_uuid(ids.next_uuid_v7()),
            ThreadCommand::Start {
                thread_id: ThreadId::from_uuid(ids.next_uuid_v7()),
                prompt: "hello".into(),
                binding,
                focus: None,
            },
        );
        assert_eq!(command.protocol_version, THREAD_PROTOCOL_VERSION);
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["command"]["type"], "start");
        assert_eq!(
            serde_json::from_value::<ThreadCommandEnvelope>(json).unwrap(),
            command
        );
        assert!(ThreadLifecycle::Ready.accepts_follow_up());
        for lifecycle in [
            ThreadLifecycle::Running,
            ThreadLifecycle::WaitingPermission,
            ThreadLifecycle::WaitingInput,
            ThreadLifecycle::Interrupted,
            ThreadLifecycle::Failed,
            ThreadLifecycle::ReconciliationRequired,
        ] {
            assert!(!lifecycle.accepts_follow_up());
        }
    }

    #[test]
    fn recursive_redaction_caps_text_and_preserves_safe_structure() {
        let long = "x".repeat(THREAD_TEXT_CAP_BYTES + 16);
        let redacted = redact_thread_text(&long);
        assert!(redacted.ends_with("…[truncated]"));
        assert!(redacted.len() > THREAD_TEXT_CAP_BYTES);
        let value = redact_thread_value(serde_json::json!({
            "nested": ["Bearer short", {"credential_id":"hidden", "safe":"ok"}],
            "authorization": "also hidden",
        }));
        assert_eq!(value["authorization"], "[REDACTED]");
        assert_eq!(value["nested"][1]["credential_id"], "[REDACTED]");
        assert_eq!(value["nested"][1]["safe"], "ok");
    }
}
