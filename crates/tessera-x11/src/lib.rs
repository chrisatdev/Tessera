//! X11 display layer for Tessera: an x11rb implementation of the core's
//! [`DisplayServer`] seam (design D1/D2), keeping the pure core X-free.
//!
//! Part A (U4-A): `connect` (T14), `claim_wm` (T15), the event loop and
//! event translation (T16). Part B (U4-B): frames (T17) and EWMH/keyboard
//! (T18).

pub mod bar_renderer;
pub mod display_server;
pub mod event_loop;
pub mod ewmh;
pub mod frames;
pub mod keyboard;
pub mod translate;

pub use bar_renderer::{BarPosition, BarRenderer};
pub use display_server::X11Display;
// The binary's bar wrapper takes these by value/Arc to hand the shared
// connection to the bar thread (task 2.7); re-exported so the binary never
// needs a direct x11rb dependency.
pub use x11rb::protocol::xproto::{Visualid, Window};
pub use x11rb::rust_connection::RustConnection;
