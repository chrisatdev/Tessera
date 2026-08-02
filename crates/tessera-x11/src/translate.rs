//! Pure translation of raw x11rb events into the core's [`Event`] enum (D2).
//!
//! The event loop feeds every raw event through [`translate_event`]; events
//! the core does not act on yet yield `None` and are skipped. Translation is
//! deliberately connection-free (a pure function) so the full mapping is
//! testable headless.

use tessera_core::Event;

/// Translates one raw X event, or `None` when the core has no use for it yet.
pub fn translate_event(raw: &x11rb::protocol::Event) -> Option<Event> {
    let _ = raw;
    todo!("T16: raw x11rb event -> core Event mapping")
}

/// Translates a batch of raw events in arrival order, dropping the ones the
/// core ignores (REQ-x11-004 / SC-x11-06: each X event is translated and
/// published in the order it arrived).
pub fn translate_events<'a>(
    raw: impl IntoIterator<Item = &'a x11rb::protocol::Event>,
) -> Vec<Event> {
    let _ = raw;
    todo!("T16: ordered batch translation")
}
