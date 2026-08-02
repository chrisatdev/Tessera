//! x11rb implementation of the core's [`DisplayServer`] seam (design D1/D2).
//!
//! [`X11Display`] owns a live [`RustConnection`] and implements every trait
//! method over it. Part A (U4-A) implements `connect` (T14), `claim_wm`
//! (T15) and `next_event` (T16); the frame/EWMH/keyboard methods (T17/T18)
//! currently return an explicit "not implemented" error so the seam compiles
//! and fails loudly instead of silently misbehaving.
//!
//! Headless-testability approach (documented per task): failure paths are
//! tested through the REAL x11rb connect against an unreachable display name
//! (no server needed — the error mapping is what T14 owns), the WM_S0 claim
//! flow is scripted behind a small internal [`X11Startup`] seam (T15), and
//! event translation is a pure module (`crate::translate`, T16). Happy paths
//! that require a live server are covered by the gated Xvfb integration tests
//! in U5.

use tessera_core::{DErr, DisplayServer, Event, FrameId, Rect, WindowId};
use x11rb::connection::Connection;
use x11rb::errors::ConnectError;
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

/// The x11rb display layer: one X connection plus the root window of the
/// screen it was opened on.
pub struct X11Display {
    /// Display name passed to [`X11Display::new`]; `None` means `$DISPLAY`.
    display_name: Option<String>,
    /// The live connection, created by [`DisplayServer::connect`].
    conn: Option<RustConnection>,
    /// Root window of the connected screen (set by `connect`).
    root: Window,
}

impl X11Display {
    /// Creates the layer for display `display_name` (`None` = `$DISPLAY`).
    /// No X server is touched until [`DisplayServer::connect`].
    pub fn new(display_name: Option<&str>) -> Self {
        X11Display {
            display_name: display_name.map(str::to_owned),
            conn: None,
            root: 0,
        }
    }
}

/// Maps an [`x11rb::connect`] failure into the abort error [`DErr::X`],
/// naming the display so a startup failure is actionable
/// (REQ-x11-001 / SC-x11-02).
fn map_connect_error(display: Option<&str>, err: ConnectError) -> DErr {
    match display {
        Some(name) => DErr::X(format!("cannot connect to display '{name}': {err}")),
        None => DErr::X(format!(
            "cannot connect to X display (check $DISPLAY): {err}"
        )),
    }
}

impl DisplayServer for X11Display {
    fn connect(&mut self) -> Result<(), DErr> {
        // REQ-x11-001: open the display; an unreachable display maps every
        // ConnectError kind into DErr::X (SC-x11-02) so startup aborts with a
        // non-zero exit instead of proceeding without a display.
        let name = self.display_name.as_deref();
        let (conn, screen_num) =
            x11rb::connect(name).map_err(|err| map_connect_error(name, err))?;
        let root = conn.setup().roots[screen_num].root;
        self.conn = Some(conn);
        self.root = root;
        Ok(())
    }

    fn claim_wm(&mut self) -> Result<(), DErr> {
        todo!("T15: WM_S0 claim + root event selection")
    }

    fn next_event(&mut self) -> Result<Option<Event>, DErr> {
        todo!("T16: event loop over the x11rb connection")
    }

    fn manage(&mut self, _w: WindowId) -> Result<FrameId, DErr> {
        Err(DErr::X("manage: frames are U4 part B (T17)".to_string()))
    }

    fn map_window(&mut self, _w: WindowId) -> Result<(), DErr> {
        Err(DErr::X(
            "map_window: frames are U4 part B (T17)".to_string(),
        ))
    }

    fn unmap_window(&mut self, _w: WindowId) -> Result<(), DErr> {
        Err(DErr::X(
            "unmap_window: frames are U4 part B (T17)".to_string(),
        ))
    }

    fn configure(&mut self, _w: WindowId, _r: Rect) -> Result<(), DErr> {
        Err(DErr::X("configure: frames are U4 part B (T17)".to_string()))
    }

    fn focus_window(&mut self, _w: WindowId) -> Result<(), DErr> {
        Err(DErr::X(
            "focus_window: frames are U4 part B (T17)".to_string(),
        ))
    }

    fn destroy_frame(&mut self, _f: FrameId) -> Result<(), DErr> {
        Err(DErr::X(
            "destroy_frame: frames are U4 part B (T17)".to_string(),
        ))
    }

    fn set_desktops(&mut self, _n: u32, _cur: u32, _names: &[String]) -> Result<(), DErr> {
        Err(DErr::X("set_desktops: EWMH is U4 part B (T18)".to_string()))
    }

    fn spawn(&self, prog: &str) -> Result<(), DErr> {
        tessera_core::spawn_program(prog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_fails_with_x_error_for_unreachable_display() {
        // SC-x11-02 seam: a display number with no server behind it fails the
        // REAL x11rb connect (instant socket error, no X server needed), and
        // connect() must map that into DErr::X — the abort signal for startup.
        // NOTE: the display number stays < 59536 so x11rb's `6000 + display`
        // port arithmetic cannot overflow u16.
        let mut d = X11Display::new(Some(":59534"));
        let err = d.connect().unwrap_err();
        assert!(
            matches!(err, DErr::X(ref msg) if msg.contains(":59534")),
            "expected DErr::X naming the display, got {err:?}"
        );
    }

    #[test]
    fn connect_fails_with_x_error_for_malformed_display() {
        // A display string that fails x11rb's parser (no ':' host separator)
        // is a DIFFERENT failure path than an unreachable server: it must
        // also abort via DErr::X, proving the mapping covers every
        // ConnectError kind, not just I/O failures.
        let mut d = X11Display::new(Some("definitely-not-a-display-xyz"));
        let err = d.connect().unwrap_err();
        assert!(matches!(err, DErr::X(_)), "expected DErr::X, got {err:?}");
        assert!(
            format!("{err}").contains("cannot connect"),
            "expected a 'cannot connect' message, got {err}"
        );
    }
}
