//! Headless runtime: provider registry, conversation context, and the
//! per-workspace thread runtime service shared by the TUI and the HTTP server.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::semicolon_if_nothing_returned
)]
pub mod context;
pub mod provider;
pub mod registry;
pub mod runtime;
pub mod service;
pub mod thread;
