use serde::{Deserialize, Serialize};
use std::fmt;
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

typed_id!(RunId);
typed_id!(CommandId);
typed_id!(EventId);

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
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }
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
