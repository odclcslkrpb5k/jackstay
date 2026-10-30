//! The input thread's X side (docs/input.md): its own X connection, separate from the
//! clipboard's, with `RandR` for the monitors, `XFixes` pointer barriers, `XInput` 2 grabs and raw
//! events, `XTest` injection and XKB's lock state. Pure Rust over x11rb's `RustConnection`: no C,
//! no `unsafe`.
//!
//! What the primary and the secondary decide lives in `jackstay-input`; this crate only reads
//! and changes the X server.

mod display;
mod watchdog;

pub use display::{Display, Error, Event, Grab, KeyboardMap};
pub use watchdog::Watchdog;
