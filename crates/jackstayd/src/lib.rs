//! The daemon's parts, as a library so each can be tested on its own (docs/daemon.md). The binary
//! in `main.rs` puts them together.

pub mod store;
pub mod tracker;
