//! Input sharing's logic, with no X in it (docs/input.md): layouts and their edges, points on an
//! edge as fractions, the secondary's virtual pointer, held keys and buttons, the translation
//! between two keymaps, and the secondary's spans.
//!
//! Layouts, keymaps and every message here come from another machine, so none of this code may
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

mod keys;
mod layout;
mod pointer;
mod secondary;

pub use keys::{Keymap, Set, Syms, Translation};
pub use layout::{BadLayout, Edge, Layout, Rect};
pub use pointer::{Pointer, Sides, Step};
pub use secondary::{Action, Secondary};
