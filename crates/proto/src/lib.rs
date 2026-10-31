//! Jackstay's wire format: how messages between machines are framed, encoded and decoded.
//!
//! Every decoder here reads bytes from another machine, so none of them may panic on any input. The
//! lints below make indexing, unchecked arithmetic and the panicking helpers compile errors in this
//! crate; a decoder reads through [`Reader`], whose every read is checked against the bytes left.
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

pub mod control;
mod error;
pub mod link;
mod reader;
pub mod sharing;

pub use error::{DecodeError, LimitError};
pub use reader::Reader;

/// Frame header: payload length (u32, big-endian), then message kind (u8).
pub const FRAME_HEADER_LEN: usize = 5;
/// The largest payload a control frame may carry.
pub const MAX_PAYLOAD_LEN: u32 = 1024 * 1024;
