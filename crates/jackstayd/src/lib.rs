//! The daemon's parts, as a library so each can be tested on its own (docs/daemon.md). The binary
//! in `main.rs` puts them together.

pub mod client;
pub mod coexist;
pub mod config;
pub mod control;
pub mod daemon;
pub mod import;
pub mod notify;
pub mod paths;
pub mod session;
pub mod store;
pub mod summary;
pub mod tracker;
