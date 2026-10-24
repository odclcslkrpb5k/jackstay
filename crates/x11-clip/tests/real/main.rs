//! The clipboard on a real X server (Xvfb), against real clipboard programs: xclip and xsel.
//! This is the core tier of the conformance matrix (plan §5.7).

mod adapter;
mod client;
mod harness;
mod xclip;
