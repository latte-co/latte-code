//! Stable protocol and pure runtime state model for Latte Code.

mod ids;
mod protocol;
mod session;
mod state;

pub use ids::{
    Clock, CommandId, EventId, IdSource, RunId, SessionCommandId, SessionEventId, SessionId,
    SystemClock, SystemIdSource, TranscriptEntryId, wall_time_ms,
};
pub use protocol::*;
pub use session::*;
pub use state::*;

/// Version of the protocol encoded by this crate.
pub const PROTOCOL_VERSION: u16 = 1;
