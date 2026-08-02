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
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::xproto::{
    Atom, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::event_loop::{next_x11_event, root_event_mask};

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

/// Maps a connection-level failure from a request into [`DErr::X`].
fn map_conn_error(err: ConnectionError) -> DErr {
    DErr::X(format!("x11 request failed: {err}"))
}

/// Maps a request/reply failure into [`DErr::X`].
fn map_reply_error(err: ReplyError) -> DErr {
    DErr::X(format!("x11 request failed: {err}"))
}

/// The minimal X surface the WM_S0 claim needs (REQ-x11-002), abstracted so
/// the claim flow is scriptable headless. [`RustConnection`] implements it
/// directly; tests use a recording fake.
pub(crate) trait X11Startup {
    /// Interns a named atom and returns its id.
    fn intern(&self, name: &str) -> Result<Atom, DErr>;
    /// Returns the current owner of `atom` (`0` = unowned).
    fn owner_of(&self, atom: Atom) -> Result<Window, DErr>;
    /// Sets `owner` as the owner of `atom` at time `time` (`0` = CurrentTime).
    fn take_ownership(&self, owner: Window, atom: Atom, time: u32) -> Result<(), DErr>;
    /// Selects the event `mask` on `root`.
    fn select_root_events(&self, root: Window, mask: u32) -> Result<(), DErr>;
}

impl X11Startup for RustConnection {
    fn intern(&self, name: &str) -> Result<Atom, DErr> {
        let cookie = self
            .intern_atom(false, name.as_bytes())
            .map_err(map_conn_error)?;
        cookie
            .reply()
            .map(|reply| reply.atom)
            .map_err(map_reply_error)
    }
    fn owner_of(&self, atom: Atom) -> Result<Window, DErr> {
        let cookie = self.get_selection_owner(atom).map_err(map_conn_error)?;
        cookie
            .reply()
            .map(|reply| reply.owner)
            .map_err(map_reply_error)
    }
    fn take_ownership(&self, owner: Window, atom: Atom, time: u32) -> Result<(), DErr> {
        let cookie = self
            .set_selection_owner(owner, atom, time)
            .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn select_root_events(&self, root: Window, mask: u32) -> Result<(), DErr> {
        let aux = ChangeWindowAttributesAux::default().event_mask(EventMask::from(mask));
        let cookie = self
            .change_window_attributes(root, &aux)
            .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
}

/// Claims `WM_S0` for this WM and selects the root event mask (REQ-x11-002,
/// REQ-x11-003). `root` acts as the selection owner — a WM has no other
/// dedicated window at this stage of startup.
///
/// Order matters (SC-x11-03/04): abort BEFORE taking ownership when another
/// WM owns the selection (so startup never manages a window), then claim, then
/// re-check that the claim survived the round trip (a concurrent WM that
/// raced us now owns it — abort), and only then select root events.
pub(crate) fn startup_claim(conn: &impl X11Startup, root: Window) -> Result<(), DErr> {
    let wm_s0 = conn.intern("WM_S0")?;
    let current = conn.owner_of(wm_s0)?;
    if current != 0 {
        return Err(DErr::X(format!(
            "another window manager already owns WM_S0 (owner {current:#x}); aborting"
        )));
    }
    conn.take_ownership(root, wm_s0, x11rb::CURRENT_TIME)?;
    let after = conn.owner_of(wm_s0)?;
    if after != root {
        return Err(DErr::X(
            "WM_S0 claim lost to a concurrent window manager; aborting".to_string(),
        ));
    }
    conn.select_root_events(root, root_event_mask())?;
    Ok(())
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
        // REQ-x11-002: claim WM_S0 (abort on conflict, SC-x11-04) and select
        // the root event mask (REQ-x11-003, SC-x11-05) before any window is
        // managed. The claim itself is scripted headless through startup_claim.
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DErr::X("connect() must succeed before claim_wm()".to_string()))?;
        startup_claim(conn, self.root)
    }

    fn next_event(&mut self) -> Result<Option<Event>, DErr> {
        // REQ-x11-004: a single-threaded loop over the X connection; D2 — raw
        // events are translated here (event_loop + translate), so the core
        // only ever sees typed Events. Ok(None) = connection closed.
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DErr::X("connect() must succeed before next_event()".to_string()))?;
        next_x11_event(conn)
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
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Scripted `WM_S0` server state backing the claim flow tests: records
    /// every call in order and answers `owner_of` from a simulated owner.
    struct FakeStartup {
        calls: RefCell<Vec<FakeCall>>,
        /// The owner `owner_of` reports (`0` = nobody owns the selection).
        owner: Cell<Window>,
        /// When set, `take_ownership` makes `owner` become this value instead
        /// of the requested owner — simulates losing the claim to a
        /// concurrent WM between the claim and the verification round trip.
        claim_lost_to: Option<Window>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeCall {
        Intern(String),
        OwnerOf(Atom),
        TakeOwnership(Window, Atom, u32),
        SelectEvents(Window, u32),
    }

    impl FakeStartup {
        fn free() -> Self {
            FakeStartup {
                calls: RefCell::new(Vec::new()),
                owner: Cell::new(0),
                claim_lost_to: None,
            }
        }

        fn owned_by(other: Window) -> Self {
            FakeStartup {
                calls: RefCell::new(Vec::new()),
                owner: Cell::new(other),
                claim_lost_to: None,
            }
        }

        fn calls(&self) -> Vec<FakeCall> {
            self.calls.borrow().clone()
        }
    }

    impl X11Startup for FakeStartup {
        fn intern(&self, name: &str) -> Result<Atom, DErr> {
            self.calls
                .borrow_mut()
                .push(FakeCall::Intern(name.to_string()));
            Ok(WM_S0_ATOM)
        }
        fn owner_of(&self, atom: Atom) -> Result<Window, DErr> {
            self.calls.borrow_mut().push(FakeCall::OwnerOf(atom));
            Ok(self.owner.get())
        }
        fn take_ownership(&self, owner: Window, atom: Atom, time: u32) -> Result<(), DErr> {
            self.calls
                .borrow_mut()
                .push(FakeCall::TakeOwnership(owner, atom, time));
            self.owner.set(self.claim_lost_to.unwrap_or(owner));
            Ok(())
        }
        fn select_root_events(&self, root: Window, mask: u32) -> Result<(), DErr> {
            self.calls
                .borrow_mut()
                .push(FakeCall::SelectEvents(root, mask));
            Ok(())
        }
    }

    const WM_S0_ATOM: Atom = 0x1000;
    const ROOT: Window = 0x0000_0010;

    #[test]
    fn startup_claim_claims_free_selection_then_selects_root_events() {
        // SC-x11-03 + SC-x11-05: free WM_S0 → ownership acquired (claim +
        // verification round trip) and the SubstructureRedirect|Notify mask is
        // selected on the root window, in that order.
        let fake = FakeStartup::free();
        startup_claim(&fake, ROOT).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                FakeCall::Intern("WM_S0".to_string()),
                FakeCall::OwnerOf(WM_S0_ATOM),
                FakeCall::TakeOwnership(ROOT, WM_S0_ATOM, 0), // CurrentTime
                FakeCall::OwnerOf(WM_S0_ATOM),                // verify the claim survived
                FakeCall::SelectEvents(ROOT, root_event_mask()),
            ]
        );
    }

    #[test]
    fn startup_claim_aborts_when_selection_already_owned() {
        // SC-x11-04: another WM owns WM_S0 → abort BEFORE taking ownership and
        // before selecting anything, so startup never manages a window.
        let fake = FakeStartup::owned_by(0x0020_0001);
        let err = startup_claim(&fake, ROOT).unwrap_err();
        assert!(
            matches!(err, DErr::X(ref msg) if msg.contains("WM_S0")),
            "expected an error naming WM_S0, got {err:?}"
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, FakeCall::TakeOwnership(..))),
            "must not claim a selection another WM already owns"
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, FakeCall::SelectEvents(..))),
            "must not select root events when startup aborts"
        );
    }

    #[test]
    fn startup_claim_aborts_when_claim_is_lost_to_a_race() {
        // The post-claim verification catches a concurrent WM that steals
        // WM_S0 between our claim and the check: abort, and never select.
        let fake = FakeStartup {
            calls: RefCell::new(Vec::new()),
            owner: Cell::new(0),
            claim_lost_to: Some(0x0030_0000),
        };
        let err = startup_claim(&fake, ROOT).unwrap_err();
        assert!(matches!(err, DErr::X(_)), "got {err:?}");
        assert!(
            fake.calls()
                .iter()
                .any(|c| matches!(c, FakeCall::TakeOwnership(..))),
            "the claim must have been attempted before the race was detected"
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, FakeCall::SelectEvents(..))),
            "must not select root events when the claim failed"
        );
    }

    #[test]
    fn claim_wm_requires_a_connection_first() {
        // The trait entry point must not dereference a missing connection:
        // calling claim_wm before connect is a programming error surfaced as
        // DErr::X, never a panic.
        let mut d = X11Display::new(None);
        let err = d.claim_wm().unwrap_err();
        assert!(
            matches!(err, DErr::X(ref msg) if msg.contains("connect")),
            "expected an error mentioning connect, got {err:?}"
        );
    }

    #[test]
    fn next_event_requires_a_connection_first() {
        // Same guard for the event loop: next_event before connect is an
        // error, never a panic on a missing connection.
        let mut d = X11Display::new(None);
        let err = d.next_event().unwrap_err();
        assert!(
            matches!(err, DErr::X(ref msg) if msg.contains("connect")),
            "expected an error mentioning connect, got {err:?}"
        );
    }

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
