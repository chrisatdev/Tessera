//! x11rb implementation of the core's [`DisplayServer`] seam (design D1/D2).
//!
//! [`X11Display`] owns a live [`RustConnection`] and implements every trait
//! method over it. Part A (U4-A) implements `connect` (T14), `claim_wm`
//! (T15) and `next_event` (T16); part B (U4-B) wires the frame methods to
//! [`crate::frames`] (T17), EWMH desktop sync to [`crate::ewmh`] and the
//! keyboard grab + keysym translation to [`crate::keyboard`] (T18).
//!
//! Headless-testability approach (documented per task): failure paths are
//! tested through the REAL x11rb connect against an unreachable display name
//! (no server needed — the error mapping is what T14 owns), the WM_S0 claim
//! flow is scripted behind a small internal [`X11Startup`] seam (T15), event
//! translation is a pure module (`crate::translate`, T16), and the frame /
//! EWMH / keyboard mechanics are scripted behind their own seams (T17/T18).
//! Happy paths that require a live server are covered by the gated Xvfb
//! integration tests in U5.

use std::collections::HashMap;
use std::sync::{Arc, Once};

use tessera_core::{Config, DErr, DisplayServer, Event, FrameId, Rect, Theme, WindowId};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::randr::{Connection as RandRConnection, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Visualid, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::event_loop::{next_x11_event, root_event_mask};
use crate::{ewmh, frames, keyboard};

/// The x11rb display layer: one X connection plus the root window of the
/// screen it was opened on.
pub struct X11Display {
    /// Display name passed to [`X11Display::new`]; `None` means `$DISPLAY`.
    display_name: Option<String>,
    /// The live connection, created by [`DisplayServer::connect`]. Wrapped in
    /// an [`Arc`] so the bar's dedicated renderer thread (task 2.7) can share
    /// it with the event loop.
    conn: Option<Arc<RustConnection>>,
    /// Root window of the connected screen (set by `connect`).
    root: Window,
    /// Root depth of the screen (frame windows use it).
    depth: u8,
    /// Root visual of the screen (frame windows use it).
    visual: Visualid,
    /// The config the X layer needs: frame border width (T17) and the
    /// keybindings to grab (T18). Defaults until [`X11Display::set_config`].
    config: Arc<Config>,
    /// The theme the frame layer paints from (T6/T7): active/inactive border
    /// pixels. Defaults to the embedded ayu_dark until
    /// [`X11Display::set_theme`] (REQ-x11-005 modified).
    theme: Arc<Theme>,
    /// Frame window id created per managed client (`manage`, T17).
    frames: HashMap<WindowId, FrameId>,
    /// The currently focused client, if any — repaint target for the
    /// previous frame when focus moves (D5, SC-x11-13).
    focused: Option<WindowId>,
    /// Cached keycode → keysym mapping, loaded during `claim_wm` (T18).
    keyboard: Option<keyboard::Keymap>,
}

impl X11Display {
    /// Creates the layer for display `display_name` (`None` = `$DISPLAY`).
    /// No X server is touched until [`DisplayServer::connect`].
    pub fn new(display_name: Option<&str>) -> Self {
        X11Display {
            display_name: display_name.map(str::to_owned),
            conn: None,
            root: 0,
            depth: 0,
            visual: 0,
            config: Arc::new(Config::default()),
            theme: Arc::new(Theme::default()),
            frames: HashMap::new(),
            focused: None,
            keyboard: None,
        }
    }

    /// Replaces the config the X layer uses: frame border width (T17) and the
    /// keybindings grabbed during `claim_wm` (T18). Defaults until called.
    pub fn set_config(&mut self, config: Arc<Config>) {
        self.config = config;
    }

    /// Replaces the theme the X layer paints from (T7): the active/inactive
    /// border pixels for frame creation and focus repaints (D4 — mirrors
    /// [`X11Display::set_config`]; the [`DisplayServer`] trait stays theme-free
    /// so the pure core never depends on X). Defaults to ayu_dark until called.
    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
    }

    /// The X pixel of the focused-frame border (REQ-x11-005 modified):
    /// the theme's active border colour packed into the low 24 bits (D2).
    fn active_border_pixel(&self) -> u32 {
        frames::pixel(self.theme.active_border())
    }

    /// The X pixel of the unfocused-frame border (REQ-x11-005 modified):
    /// the theme's inactive border colour packed into the low 24 bits (D2).
    fn inactive_border_pixel(&self) -> u32 {
        frames::pixel(self.theme.inactive_border())
    }

    /// The frame of client `w`, or an error when the client was never managed
    /// (a programming error surfaced as `DErr::X`, never a panic).
    fn frame_of(&self, w: WindowId) -> Result<FrameId, DErr> {
        self.frames
            .get(&w)
            .copied()
            .ok_or_else(|| DErr::X(format!("no frame for client {w}; manage() must run first")))
    }

    /// Queries the root window's current geometry (T21): the tiling area the
    /// binary passes to the core is the REAL screen size, not a hardcoded
    /// constant. Called after `connect` (guarded like every method here).
    pub fn root_size(&self) -> Result<Rect, DErr> {
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before root_size()".to_string()))?;
        let cookie = conn.get_geometry(self.root).map_err(map_conn_error)?;
        let geom = cookie.reply().map_err(map_reply_error)?;
        Ok(Rect {
            x: 0,
            y: 0,
            w: geom.width,
            h: geom.height,
        })
    }

    /// The shared connection, so the binary can pass it to the bar's
    /// dedicated renderer thread (task 2.7). Call after `connect`.
    pub fn connection(&self) -> Result<Arc<RustConnection>, DErr> {
        self.conn
            .clone()
            .ok_or_else(|| DErr::X("connect() must succeed before connection()".to_string()))
    }

    /// The root window of the connected screen (call after `connect`).
    pub fn root(&self) -> Window {
        self.root
    }

    /// The root depth, used by the bar window (call after `connect`).
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// The root visual, used by the bar window (call after `connect`).
    pub fn visual(&self) -> Visualid {
        self.visual
    }

    /// The monitor the bar renders on (design D10): the primary RandR output,
    /// else the first connected output (lowest id), else the root geometry.
    /// Never refuses to start — any RandR failure falls back to the full
    /// screen, logging a warning once.
    pub fn bar_area(&self) -> Result<Rect, DErr> {
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before bar_area()".to_string()))?;
        match bar_monitor_rect(conn, self.root) {
            Ok(Some(rect)) => Ok(rect),
            Ok(None) | Err(_) => {
                log_bar_fallback_once();
                self.root_size()
            }
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
/// `pub(crate)`: also used by frames/ewmh/keyboard (U4-B).
pub(crate) fn map_conn_error(err: ConnectionError) -> DErr {
    DErr::X(format!("x11 request failed: {err}"))
}

/// Maps a request/reply failure into [`DErr::X`].
/// `pub(crate)`: also used by frames/ewmh/keyboard (U4-B).
pub(crate) fn map_reply_error(err: ReplyError) -> DErr {
    DErr::X(format!("x11 request failed: {err}"))
}

/// D10: the output the bar renders on — the primary RandR output, else the
/// first connected output (lowest RandR id), else `None` (the caller falls
/// back to the root geometry and never refuses to start).
///
/// `outputs` is the ordered list of `(RandR output id, connected)` pairs as
/// returned by `get_screen_resources_current`. A stale primary id that is no
/// longer in the list cannot win over a real connected output.
fn pick_bar_output(primary: u32, outputs: &[(u32, bool)]) -> Option<u32> {
    if primary != x11rb::NONE && outputs.iter().any(|&(id, _)| id == primary) {
        return Some(primary);
    }
    outputs
        .iter()
        .find(|(_, connected)| *connected)
        .map(|(id, _)| *id)
}

/// Resolves the bar monitor rect over a live connection per D10. `Ok(None)`
/// (no RandR extension, no output, or an inactive CRTC) and any `Err` both
/// mean "fall back to the root geometry" for the caller.
fn bar_monitor_rect(conn: &RustConnection, root: Window) -> Result<Option<Rect>, DErr> {
    // Register the RANDR extension first: the generated protocol functions
    // resolve their major opcode through the extension cache.
    let randr = conn
        .extension_information("RANDR")
        .map_err(map_conn_error)?;
    if randr.is_none() {
        return Ok(None);
    }
    let resources = conn
        .randr_get_screen_resources_current(root)
        .map_err(map_conn_error)?
        .reply()
        .map_err(map_reply_error)?;
    let primary = conn
        .randr_get_output_primary(root)
        .map_err(map_conn_error)?
        .reply()
        .map_err(map_reply_error)?
        .output;
    let outputs: Vec<(u32, bool)> = resources
        .outputs
        .iter()
        .map(|&id| {
            let connected = conn
                .randr_get_output_info(id, x11rb::CURRENT_TIME)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.connection == RandRConnection::CONNECTED)
                .unwrap_or(false);
            (id, connected)
        })
        .collect();
    let Some(output) = pick_bar_output(primary, &outputs) else {
        return Ok(None);
    };
    let info = conn
        .randr_get_output_info(output, x11rb::CURRENT_TIME)
        .map_err(map_conn_error)?
        .reply()
        .map_err(map_reply_error)?;
    if info.crtc == x11rb::NONE {
        return Ok(None);
    }
    let crtc = conn
        .randr_get_crtc_info(info.crtc, x11rb::CURRENT_TIME)
        .map_err(map_conn_error)?
        .reply()
        .map_err(map_reply_error)?;
    if crtc.width == 0 || crtc.height == 0 {
        return Ok(None);
    }
    Ok(Some(Rect {
        x: crtc.x as i32,
        y: crtc.y as i32,
        w: crtc.width,
        h: crtc.height,
    }))
}

/// Warns once when the bar falls back to the full screen because no usable
/// RandR monitor could be resolved (D10: never refuses to start).
static BAR_FALLBACK_WARNED: Once = Once::new();
fn log_bar_fallback_once() {
    BAR_FALLBACK_WARNED.call_once(|| {
        eprintln!(
            "tessera: warning: no usable RandR bar monitor found; \
             drawing the bar on the full screen"
        );
    });
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
        let screen = &conn.setup().roots[screen_num];
        self.root = screen.root;
        self.depth = screen.root_depth;
        self.visual = screen.root_visual;
        self.conn = Some(Arc::new(conn));
        Ok(())
    }

    fn claim_wm(&mut self) -> Result<(), DErr> {
        // REQ-x11-002: claim WM_S0 (abort on conflict, SC-x11-04) and select
        // the root event mask (REQ-x11-003, SC-x11-05) before any window is
        // managed. The claim itself is scripted headless through startup_claim.
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before claim_wm()".to_string()))?;
        startup_claim(conn, self.root)?;
        // REQ-x11-008: becoming the WM also means grabbing the configured
        // keybindings on the root and caching the keycode -> keysym mapping so
        // next_event can translate raw KeyPress events (T18, SC-x11-12). A
        // failed grab aborts startup loudly rather than silently dropping a
        // binding.
        let keymap = keyboard::Keymap::load(conn)?;
        let stats = keyboard::grab_keybindings(conn, &keymap, self.root, &self.config)?;
        // KBR-3 (D6): the claim log surfaces the grab stats and any mapping
        // holes — fewer grabs than `bindings * 8` (unresolved/duplicate
        // bindings) or keycodes stuck at NoSymbol is never silent.
        let holes = keymap.nosymbol_keycodes().len();
        self.keyboard = Some(keymap);
        eprintln!(
            "tessera: grabbed {} lock-variant grabs for {} bindings; {} keycodes with NoSymbol keysym",
            stats.grabs, stats.bindings, holes
        );
        if stats.grabs == 0 {
            eprintln!("tessera: no keybinding grabbed from config");
        }
        Ok(())
    }

    fn next_event(&mut self) -> Result<Option<Event>, DErr> {
        // REQ-x11-004: a single-threaded loop over the X connection; D2 — raw
        // events are translated here (event_loop + translate), so the core
        // only ever sees typed Events. Ok(None) = connection closed.
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before next_event()".to_string()))?;
        loop {
            match next_x11_event(conn)? {
                Some(Event::KeyPressed(raw)) => match &self.keyboard {
                    // translate.rs carries the raw keyCODE (T16); resolve it
                    // to the bound keysym through the cached keymap so the
                    // core's command_for_key can match it (T18, SC-x11-12).
                    Some(keymap) => {
                        if let Some(ev) = keyboard::translate_key_press(keymap, raw) {
                            return Ok(Some(ev));
                        }
                        // NoSymbol (unbound) key: skip it, keep waiting.
                    }
                    None => return Ok(Some(Event::KeyPressed(raw))),
                },
                other => return Ok(other),
            }
        }
    }

    fn manage(&mut self, w: WindowId) -> Result<FrameId, DErr> {
        // REQ-x11-005 / SC-x11-07: reparent `w` into a fresh border-only frame
        // and remember the client -> frame mapping (frames.rs, T17). The frame
        // is created with the theme's active border pixel (REQ-x11-005
        // modified) so a freshly managed client starts focused-coloured.
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before manage()".to_string()))?;
        let border = u16::try_from(self.config.general.border_width).unwrap_or(u16::MAX);
        let active_pixel = self.active_border_pixel();
        let frame = frames::create_frame(
            conn,
            self.root,
            w,
            border,
            self.depth,
            self.visual,
            active_pixel,
        )?;
        self.frames.insert(w, frame);
        Ok(frame)
    }

    fn map_window(&mut self, w: WindowId) -> Result<(), DErr> {
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before map_window()".to_string()))?;
        let frame = self.frame_of(w)?;
        frames::map_frame(conn, frame.0, w)
    }

    fn unmap_window(&mut self, w: WindowId) -> Result<(), DErr> {
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before unmap_window()".to_string()))?;
        let frame = self.frame_of(w)?;
        frames::unmap_frame(conn, frame.0)
    }

    fn configure(&mut self, w: WindowId, r: Rect) -> Result<(), DErr> {
        // REQ-x11-006 / SC-x11-09: the client is re-tiled to its layout
        // placement — the frame takes `r`, the client the frame interior.
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before configure()".to_string()))?;
        let frame = self.frame_of(w)?;
        let border = u16::try_from(self.config.general.border_width).unwrap_or(u16::MAX);
        frames::configure_frame(conn, frame.0, w, r, border)
    }

    fn focus_window(&mut self, w: WindowId) -> Result<(), DErr> {
        // REQ-x11-005 (modified) / SC-x11-13: on a focus change the previously
        // focused frame repaints inactive and the newly focused frame repaints
        // active, then input focus moves to the client (D5 — X11Display owns
        // `focused`, so the repaint is local, no trait/bus change).
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before focus_window()".to_string()))?;
        let previous = self.focused;
        let active = self.active_border_pixel();
        let inactive = self.inactive_border_pixel();
        frames::repaint_focus(conn, &self.frames, previous, w, active, inactive)?;
        self.focused = Some(w);
        frames::focus_client(conn, w)
    }

    fn destroy_frame(&mut self, f: FrameId) -> Result<(), DErr> {
        // REQ-x11-007: the client already died (DestroyNotify); destroy the
        // orphaned frame window.
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before destroy_frame()".to_string()))?;
        frames::destroy_frame(conn, f.0)
    }

    fn set_desktops(&mut self, n: u32, cur: u32, names: &[String]) -> Result<(), DErr> {
        // REQ-ws-003 / SC-ws-05: sync the three _NET_* desktop root
        // properties through ewmh.rs (T18).
        let conn = self
            .conn
            .as_deref()
            .ok_or_else(|| DErr::X("connect() must succeed before set_desktops()".to_string()))?;
        ewmh::set_desktops(conn, self.root, n, cur, names)
    }

    fn spawn(&self, prog: &str) -> Result<(), DErr> {
        tessera_core::spawn_program(prog)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    use tessera_core::{Color, Theme};

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
    fn root_size_requires_a_connection_first() {
        // T21: the tiling area must come from the real root window geometry.
        // Querying it before connect is an error, never a panic on a missing
        // connection (same guard shape as claim_wm/next_event).
        let d = X11Display::new(None);
        let err = d.root_size().unwrap_err();
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

    #[test]
    fn default_theme_pixels_are_ayu_dark_active_and_inactive() {
        // SC-thm-09 at the X boundary: a fresh display (no set_theme) derives
        // its border pixels from the embedded ayu_dark theme — active from
        // accent (#FF8F40), inactive from comment (#626A73) — and they differ.
        let d = X11Display::new(None);
        assert_eq!(d.active_border_pixel(), 0x00FF_8F40);
        assert_eq!(d.inactive_border_pixel(), 0x0062_6A73);
        assert_ne!(d.active_border_pixel(), d.inactive_border_pixel());
    }

    #[test]
    fn set_theme_replaces_the_border_pixels() {
        // REQ-x11-005 (modified): set_theme mirrors set_config — the stored
        // theme's explicit border overrides (SC-thm-10) become the pixels the
        // frame layer uses, replacing the ayu_dark derived defaults.
        let mut d = X11Display::new(None);
        let custom = Theme {
            active_border: Some(Color::parse_hex("#112233").expect("valid hex")),
            inactive_border: Some(Color::parse_hex("#445566").expect("valid hex")),
            ..Theme::default()
        };
        d.set_theme(Arc::new(custom));
        assert_eq!(d.active_border_pixel(), 0x0011_2233);
        assert_eq!(d.inactive_border_pixel(), 0x0044_5566);
    }

    #[test]
    fn bar_area_requires_a_connection_first() {
        // Same guard as root_size: the bar monitor is resolved over the live
        // connection, so a missing connection is an error, never a panic.
        let d = X11Display::new(None);
        let err = d.bar_area().unwrap_err();
        assert!(
            matches!(err, DErr::X(ref msg) if msg.contains("connect")),
            "expected an error mentioning connect, got {err:?}"
        );
    }

    #[test]
    fn pick_bar_output_prefers_the_primary_output() {
        // D10: with a primary set, the bar uses exactly that output.
        assert_eq!(
            pick_bar_output(5, &[(5, true), (7, true)]),
            Some(5),
            "the primary output wins"
        );
    }

    #[test]
    fn pick_bar_output_falls_back_to_the_first_connected_output() {
        // D10: no primary (NONE) -> the first connected output by RandR id.
        assert_eq!(
            pick_bar_output(x11rb::NONE, &[(1, false), (2, true), (3, true)]),
            Some(2),
            "the first connected output (lowest id) is the fallback"
        );
    }

    #[test]
    fn pick_bar_output_ignores_a_primary_not_in_the_output_list() {
        // A stale primary id must not win over a real connected output.
        assert_eq!(
            pick_bar_output(99, &[(1, true)]),
            Some(1),
            "an unknown primary must not shadow the connected output"
        );
    }

    #[test]
    fn pick_bar_output_returns_none_with_no_connected_output() {
        // D10: nothing usable -> None, and the caller falls back to the root
        // geometry (never refuses to start).
        assert_eq!(
            pick_bar_output(x11rb::NONE, &[(1, false), (3, false)]),
            None,
            "no connected output means no bar monitor"
        );
    }
}
