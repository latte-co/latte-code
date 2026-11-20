use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::{Timestamp, Uuid};

macro_rules! typed_id {
    ($name:ident) => {
        #[doc = concat!(stringify!($name), " is a strongly typed UUID identifier.")]
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates an identifier from an existing UUID.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }
            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

typed_id!(TurnId);
typed_id!(EventId);
// Session identifiers deliberately do not reuse the v1 turn identifiers. A
// session is a durable conversation which can contain several immutable turns.
typed_id!(SessionId);
typed_id!(SessionCommandId);
typed_id!(SessionEventId);
typed_id!(TranscriptEntryId);

/// Strongly typed identifier of one assistant tool-call round inside a [`TurnId`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoundId(u64);

impl RoundId {
    /// Creates a round identifier from its monotonic numeric value.
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying numeric value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RoundId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for RoundId {
    type Err = std::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<u64>().map(Self)
    }
}

/// Stable logical repository identity shared across Git worktrees of one project.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectKey(String);

impl ProjectKey {
    /// Validates a bounded ASCII catalog key (`[A-Za-z0-9_-]{1,128}`).
    ///
    /// # Errors
    ///
    /// Returns an explanation when `value` is empty, oversized, or contains
    /// unsafe characters.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("invalid project key".into());
        }
        Ok(Self(value))
    }

    /// Borrows the validated key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProjectKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for ProjectKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Structured decomposition of a session-scoped [`EffectId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEffectKind {
    /// A model-requested tool call within a specific turn, round, and call ordinal.
    ToolCall {
        turn_id: TurnId,
        round_id: RoundId,
        call_ordinal: usize,
        tool_call_id: String,
    },
    /// The engine-owned post-change verification process for `turn_id`.
    Verification { turn_id: TurnId },
}

/// Validated durable identifier for a privileged engine effect (`1..=512` bytes, no controls).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EffectId(String);

impl EffectId {
    /// Validates an opaque or structured effect identifier.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized (`> 512` bytes), or control-containing strings.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err("invalid session effect id".into());
        }
        Ok(Self(value))
    }

    /// Formats the canonical session tool-call effect identifier:
    /// `session-effect:{turn_id}:{round_id}:{call_ordinal}:{tool_call_id}`.
    ///
    /// # Errors
    ///
    /// Rejects malformed `tool_call_id` values or oversized composite identifiers.
    pub fn for_tool_call(
        turn_id: TurnId,
        round_id: RoundId,
        call_ordinal: usize,
        tool_call_id: &str,
    ) -> Result<Self, String> {
        if !crate::valid_openai_chat_tool_call_id(tool_call_id) {
            return Err("invalid session tool call id".into());
        }
        Self::new(format!(
            "session-effect:{turn_id}:{round_id}:{call_ordinal}:{tool_call_id}"
        ))
    }

    /// Formats the canonical session verification effect identifier:
    /// `session-verification:{turn_id}`.
    #[must_use]
    pub fn for_verification(turn_id: TurnId) -> Self {
        Self(format!("session-verification:{turn_id}"))
    }

    /// Parses session-scoped `session-effect:` / `session-verification:` identifiers
    /// (including legacy `thread-effect:` / `thread-verification:` prefixes).
    #[must_use]
    pub fn parse_session(&self) -> Option<SessionEffectKind> {
        if let Some(rest) = self
            .0
            .strip_prefix("session-verification:")
            .or_else(|| self.0.strip_prefix("thread-verification:"))
        {
            let turn_id = Uuid::parse_str(rest).map(TurnId::from_uuid).ok()?;
            return Some(SessionEffectKind::Verification { turn_id });
        }
        let rest = self
            .0
            .strip_prefix("session-effect:")
            .or_else(|| self.0.strip_prefix("thread-effect:"))?;
        let mut parts = rest.splitn(4, ':');
        let turn_id = Uuid::parse_str(parts.next()?).map(TurnId::from_uuid).ok()?;
        let round_id = parts.next()?.parse::<RoundId>().ok()?;
        let call_ordinal = parts.next()?.parse::<usize>().ok()?;
        let tool_call_id = parts.next()?;
        if !crate::valid_openai_chat_tool_call_id(tool_call_id) {
            return None;
        }
        Some(SessionEffectKind::ToolCall {
            turn_id,
            round_id,
            call_ordinal,
            tool_call_id: tool_call_id.to_owned(),
        })
    }

    /// Returns whether this identifier names a session verification effect.
    #[must_use]
    pub fn is_verification(&self) -> bool {
        matches!(
            self.parse_session(),
            Some(SessionEffectKind::Verification { .. })
        )
    }

    /// Borrows the underlying validated string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EffectId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EffectId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for EffectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Supplies wall-clock Unix milliseconds.
pub trait Clock: Send + Sync {
    /// Returns milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Supplies `UUIDv7` values. Tests can inject a deterministic implementation.
pub trait IdSource: Send + Sync {
    /// Returns the next `UUIDv7` value.
    fn next_uuid_v7(&self) -> Uuid;
}

/// Production wall clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        wall_time_ms()
    }
}

/// Returns the current wall-clock time as milliseconds since the Unix epoch.
#[must_use]
pub fn wall_time_ms() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Production `UUIDv7` source backed by an injected clock.
#[derive(Debug)]
pub struct SystemIdSource<C = SystemClock> {
    clock: C,
}

impl<C> SystemIdSource<C> {
    /// Creates a source using `clock`.
    pub const fn new(clock: C) -> Self {
        Self { clock }
    }
}

impl Default for SystemIdSource<SystemClock> {
    fn default() -> Self {
        Self::new(SystemClock)
    }
}

impl<C: Clock> IdSource for SystemIdSource<C> {
    fn next_uuid_v7(&self) -> Uuid {
        let now_ms = self.clock.now_ms();
        Uuid::new_v7(Timestamp::from_unix_time(
            now_ms / 1_000,
            (now_ms % 1_000) as u32 * 1_000_000,
            0,
            0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_id_as_uuid_round_trips() {
        let uuid = Uuid::now_v7();
        let id = SessionId::from_uuid(uuid);
        assert_eq!(id.as_uuid(), uuid);
        assert_eq!(id.to_string(), uuid.to_string());
    }

    #[test]
    fn round_project_and_effect_ids_enforce_grammar_and_round_trip() {
        let round = RoundId::from_u64(7);
        assert_eq!(round.as_u64(), 7);
        assert_eq!(round.to_string().parse::<RoundId>().unwrap(), round);
        assert!("bad".parse::<RoundId>().is_err());

        let project = ProjectKey::new("project-0123_abc").unwrap();
        assert_eq!(project.as_str(), "project-0123_abc");
        assert_eq!(project.as_ref(), "project-0123_abc");
        assert_eq!(project.to_string(), "project-0123_abc");
        let encoded = serde_json::to_string(&project).unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectKey>(&encoded).unwrap(),
            project
        );
        for invalid in ["", "bad/key", "bad key", &"x".repeat(129)] {
            assert!(ProjectKey::new(invalid).is_err(), "{invalid}");
            assert!(
                serde_json::from_value::<ProjectKey>(serde_json::json!(invalid)).is_err(),
                "{invalid}"
            );
        }

        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        let tool_effect = EffectId::for_tool_call(turn_id, round, 2, "call_abc-1").unwrap();
        assert!(!tool_effect.is_verification());
        assert_eq!(
            tool_effect.parse_session(),
            Some(SessionEffectKind::ToolCall {
                turn_id,
                round_id: round,
                call_ordinal: 2,
                tool_call_id: "call_abc-1".into(),
            })
        );
        let legacy_tool = EffectId::new(format!("thread-effect:{turn_id}:7:2:call_abc-1")).unwrap();
        assert_eq!(legacy_tool.parse_session(), tool_effect.parse_session());

        let verification = EffectId::for_verification(turn_id);
        assert!(verification.is_verification());
        assert_eq!(
            verification.parse_session(),
            Some(SessionEffectKind::Verification { turn_id })
        );
        let legacy_verification = EffectId::new(format!("thread-verification:{turn_id}")).unwrap();
        assert!(legacy_verification.is_verification());

        let opaque: EffectId = "internal-git-diff".parse().unwrap();
        assert_eq!(opaque.as_str(), "internal-git-diff");
        assert_eq!(opaque.as_ref(), "internal-git-diff");
        assert_eq!(opaque.to_string(), "internal-git-diff");
        assert_eq!(opaque.parse_session(), None);
        assert_eq!(
            serde_json::from_str::<EffectId>(&serde_json::to_string(&opaque).unwrap()).unwrap(),
            opaque
        );

        assert!(EffectId::for_tool_call(turn_id, round, 0, "bad space").is_err());
        for unparseable in [
            "session-verification:not-a-uuid",
            "session-effect:not-a-uuid:1:0:call_1",
            &format!("session-effect:{turn_id}:bad:0:call_1"),
            &format!("session-effect:{turn_id}:1:bad:call_1"),
            &format!("session-effect:{turn_id}:1:0:bad space"),
            &format!("session-effect:{turn_id}:1:0"),
        ] {
            assert_eq!(
                EffectId::new(unparseable).unwrap().parse_session(),
                None,
                "{unparseable}"
            );
        }
        for invalid in ["", "bad\nid", &"x".repeat(513)] {
            assert!(EffectId::new(invalid).is_err(), "{invalid:?}");
            assert!(
                serde_json::from_value::<EffectId>(serde_json::json!(invalid)).is_err(),
                "{invalid:?}"
            );
        }
    }
}
