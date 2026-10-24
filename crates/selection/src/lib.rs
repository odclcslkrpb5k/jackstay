//! The X selection protocol (ICCCM §2, with `XFixes`) as a pure state machine. `docs/selection.md`
//! is its specification; section names below refer to it.
//!
//! Every property value this crate reads was written by another program, so none of its code may
//! panic on any input: the lints below make indexing, unchecked arithmetic and the panicking
//! helpers compile errors here.
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

mod atoms;
mod capture;
mod engine;
mod pool;
mod port;
mod serve;
mod text;
mod types;

pub use capture::Limits;
pub use engine::{Config, Engine, Report};
pub use port::{Port, Property, Read};
pub use text::{compound_text, latin1};
pub use types::{Atom, Event, Millis, OwnerChange, PropertyState, Timestamp, Window};
