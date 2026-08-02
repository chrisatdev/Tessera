//! X11 display layer for Tessera: an x11rb implementation of the core's
//! [`DisplayServer`] seam (design D1/D2), keeping the pure core X-free.
//!
//! Part A (U4-A): `connect` (T14), `claim_wm` (T15), the event loop and
//! event translation (T16). Part B (U4-B): frames (T17) and EWMH/keyboard
//! (T18).

pub mod display_server;
pub mod event_loop;
pub mod ewmh;
pub mod frames;
pub mod keyboard;
pub mod translate;

pub use display_server::X11Display;
