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

mod announce;
mod error;
mod frame;
mod reader;

pub use announce::{
    Announce, EntryId, MAX_INLINE_LEN, MAX_TARGET_NAME_LEN, MAX_TARGETS, Target, TargetName,
};
pub use error::{DecodeError, LimitError};
pub use frame::{FRAME_HEADER_LEN, MAX_PAYLOAD_LEN, Message, decode, encode};
pub use reader::Reader;
