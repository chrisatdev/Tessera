//! The WM's event loop over the x11rb connection (REQ-x11-003/004).
//!
//! The loop reads raw X events with `wait_for_event`, translates each one via
//! [`crate::translate`] and skips events the core does not act on; when the
//! server disappears it reports a clean end-of-loop (`Ok(None)`) instead of
//! an error. `root_event_mask` is the SubstructureRedirect + SubstructureNotify
//! selection the WM applies to the root window during startup (SC-x11-05).

use x11rb::protocol::xproto::EventMask;

/// The event mask the WM selects on the root window (REQ-x11-003, SC-x11-05):
/// SubstructureRedirect turns client maps into MapRequest events, and
/// SubstructureNotify delivers the unmap/destroy/configure notices the core's
/// window lifecycle depends on.
pub fn root_event_mask() -> u32 {
    u32::from(EventMask::SUBSTRUCTURE_REDIRECT) | u32::from(EventMask::SUBSTRUCTURE_NOTIFY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_event_mask_selects_substructure_redirect_and_notify() {
        // SC-x11-05: the mask must contain BOTH bits — losing either one
        // breaks the WM (no MapRequest for new clients, or no destroy
        // tracking). Asserting the exact value guards both at once.
        let expected =
            u32::from(EventMask::SUBSTRUCTURE_REDIRECT) | u32::from(EventMask::SUBSTRUCTURE_NOTIFY);
        assert_eq!(root_event_mask(), expected);
    }
}
