//! Stable protocol and pure runtime state model for Lattecode.

mod ids;
mod protocol;
mod state;

pub use ids::{Clock, CommandId, EventId, IdSource, RunId, SystemClock, SystemIdSource};
pub use protocol::*;
pub use state::*;

/// Version of the protocol encoded by this crate.
pub const PROTOCOL_VERSION: u16 = 1;
