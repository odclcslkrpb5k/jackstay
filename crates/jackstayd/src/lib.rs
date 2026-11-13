//! The daemon's parts, as a library so each can be tested on its own (docs/daemon.md). The binary
//! in `main.rs` puts them together.

/// The commit a release was built from, as `tools/publish` sets it (`JACKSTAY_BUILD`, 12 hex
/// digits); `dev` for any other build. Each program prints it for `--version`, and `install.sh`
/// checks it.
pub const BUILD: &str = match option_env!("JACKSTAY_BUILD") {
    Some(build) => build,
    None => "dev",
};

pub mod client;
pub mod coexist;
pub mod config;
pub mod control;
pub mod daemon;
pub mod import;
pub mod input;
pub mod link;
pub mod notify;
pub mod paths;
pub mod session;
pub mod sharing;
pub mod store;
pub mod summary;
pub mod thumbnails;
pub mod tracker;
pub mod ui_socket;
