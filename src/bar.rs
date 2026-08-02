//! Status-bar placeholder (T19, REQ-bus-004 / SC-bus-04).
//!
//! The real bar (X drawing) is a later change; this placeholder owns the
//! snapshot-consumer seam — it subscribes to the WmState watch and renders
//! the complete current snapshot as plain text. It has zero X dependencies:
//! it consumes [`WmState`] only, so it can be promoted to its own crate later
//! (design: a `src/bar.rs` module in the binary crate for now).

/// Snapshot consumer for the status bar (placeholder; T19).
pub struct Bar;
