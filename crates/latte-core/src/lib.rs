//! Stable protocol and pure runtime state model for Latte Code.

mod ids;
mod protocol;
mod state;
mod thread;

pub use ids::{
    Clock, CommandId, EventId, IdSource, RunId, SystemClock, SystemIdSource, ThreadCommandId,
    ThreadEventId, ThreadId, TranscriptEntryId, wall_time_ms,
};
pub use protocol::*;
pub use state::*;
pub use thread::*;

/// Version of the protocol encoded by this crate.
pub const PROTOCOL_VERSION: u16 = 1;
