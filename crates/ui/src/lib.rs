//! `jackstay-ui`: the desktop side of Jackstay (docs/ui.md). It holds no history; everything it
//! shows comes from `jackstayd` over the control socket.

pub mod listen;
pub mod model;
pub mod popup;
pub mod tray;
pub mod worker;
