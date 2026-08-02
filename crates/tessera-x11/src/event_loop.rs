//! The WM's event loop over the x11rb connection (REQ-x11-003/004).
//!
//! The loop reads raw X events with `wait_for_event`, translates each one via
//! [`crate::translate`] and skips events the core does not act on; when the
//! server disappears it reports a clean end-of-loop (`Ok(None)`) instead of
//! an error. `root_event_mask` is the SubstructureRedirect + SubstructureNotify
//! selection the WM applies to the root window during startup (SC-x11-05).

use tessera_core::{DErr, Event};
use x11rb::connection::Connection;
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::EventMask;
use x11rb::rust_connection::RustConnection;

use crate::translate::translate_event;

/// The event mask the WM selects on the root window (REQ-x11-003, SC-x11-05):
/// SubstructureRedirect turns client maps into MapRequest events, and
/// SubstructureNotify delivers the unmap/destroy/configure notices the core's
/// window lifecycle depends on.
pub fn root_event_mask() -> u32 {
    u32::from(EventMask::SUBSTRUCTURE_REDIRECT) | u32::from(EventMask::SUBSTRUCTURE_NOTIFY)
}

/// Classifies a `wait_for_event` failure: `Ok(())` means the connection
/// closed (a clean end of loop, surfaced as `Ok(None)` by
/// [`DisplayServer::next_event`]); `Err` is a fatal error to report.
pub(crate) fn classify_wait_error(err: ConnectionError) -> Result<(), DErr> {
    let _ = err;
    todo!("T16: closed-connection detection")
}

/// Blocks for the next translated event. Raw events the core ignores are
/// skipped and the loop keeps waiting; a dead connection ends the loop
/// cleanly with `Ok(None)` (REQ-x11-004 / SC-x11-06).
pub(crate) fn next_x11_event(conn: &RustConnection) -> Result<Option<Event>, DErr> {
    let _ = conn;
    todo!("T16: wait_for_event loop")
}

#[cfg(test)]
mod tests {
    use x11rb::protocol::xproto::EventMask;

    use super::*;

    #[test]
    fn root_event_mask_selects_substructure_redirect_and_notify() {
        // SC-x11-05: the mask must contain BOTH bits — losing either one
        // breaks the WM (no MapRequest for new clients, or no destroy
        // tracking). Asserting the exact value guards both at once.
        let expected = u32::from(EventMask::SUBSTRUCTURE_REDIRECT)
            | u32::from(EventMask::SUBSTRUCTURE_NOTIFY);
        assert_eq!(root_event_mask(), expected);
    }

    #[test]
    fn closed_connection_is_reported_as_clean_end_of_loop() {
        // wait_for_event fails with IoError when the server disappears: the
        // loop must surface that as the trait's "connection closed" signal
        // (Ok(None)), not as a fatal error.
        let err = ConnectionError::IoError(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "server closed the connection",
        ));
        assert_eq!(classify_wait_error(err), Ok(()));
    }

    #[test]
    fn fatal_connection_errors_surface_as_x_errors() {
        // Any other wait_for_event failure is a real error the caller must
        // see and log, not a silent stop.
        let result = classify_wait_error(ConnectionError::UnknownError);
        assert!(matches!(result, Err(DErr::X(_))), "got {result:?}");
    }
}
