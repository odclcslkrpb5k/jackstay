//! Jackstay's files, as bytes: the history index and its blob names (docs/daemon.md, "Storage"),
//! and clipman's saved history (docs/daemon.md, "clipman import"). No I/O; the daemon reads and
//! writes the files.
//!
//! Both formats are read back from disk, where anything may have happened to them, so no decoder
//! here may panic on any input: the lints below make indexing, unchecked arithmetic and the
//! panicking helpers compile errors in this crate.
#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod blob;
pub mod clipman;
mod escape;
mod index;
#[doc(hidden)]
pub mod invariants;

pub use blob::BlobHash;
pub use index::{
    HeaderError, Index, MAX_ID, Parsed, SavedData, SavedEntry, SavedTarget, VERSION, encode, parse,
};
