//! Gated end-to-end integration tests (T21): a REAL X server (Xvfb) drives
//! the full WM — the `tessera` binary as a subprocess, a second x11rb test
//! client for the windows and the assertions, and xdotool (or XTEST through
//! the test client) for the keypresses.
//!
//! Every test is `#[ignore]` so plain `cargo test` stays green headless; the
//! suite runs only on demand:
//!
//! ```text
//! xvfb-run -a -s "-screen 0 1280x1024x24" cargo test --test integration -- --ignored
//! ```
//!
//! Prerequisites (NOT installed by this change): `Xvfb` and `xdotool`. The
//! pinned screen size (1280x1024x24) deliberately differs from the old
//! hardcoded 1920x1080 tiling area, so the geometry assertions really prove
//! the real-screen wiring (T21) instead of matching the old constant by
//! coincidence.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcCommand, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, ConnectionExt, CreateWindowAux, GetGeometryReply, MapState, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// X11 core event codes used by the XTEST fallback driver.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
/// Keysyms the test drives: Super+J (focus_next, XK_j) and Super+2
/// (workspace 2, XK_2), matching the config defaults.
const KEY_J: u32 = 0x006a;
const KEY_2: u32 = 0x0032;
/// Mod4 keysym (XK_Super_L) — every default binding is Super-based.
const SUPER_L: u32 = 0xffeb;
/// Default frame border (`config.general.border_width`), baked into every
/// layout placement.
const BORDER: u16 = 2;
/// Time budget for every polled assertion.
const WAIT: Duration = Duration::from_secs(10);

// ------------------------------------------------------------------ harness

/// The display the test runs under (`xvfb-run` sets `$DISPLAY`).
fn display_name() -> String {
    std::env::var("DISPLAY").unwrap_or_else(|_| {
        panic!(
            "no DISPLAY set — run under `xvfb-run -a -s \"-screen 0 1280x1024x24\" \
             cargo test --test integration -- --ignored`"
        )
    })
}

/// The compiled `tessera` binary (the workspace root package). `CARGO_BIN_EXE_*`
/// is only set for the package that owns the binary, so the standard
/// workspace target layout is resolved instead.
fn wm_binary() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return Path::new(&dir).join("debug").join("tessera");
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
        .join("tessera")
}

/// Owns the WM subprocess; a `Drop` kill guarantees a panicking test can
/// never leave a WM grabbing the root (which would break the next test on the
/// same X server).
struct WmChild(Child);

impl WmChild {
    /// Spawns the binary on `display` with detached stdio (its eprintln
    /// chatter must not share the test harness pipes).
    fn spawn(display: &str) -> WmChild {
        let bin = wm_binary();
        assert!(
            bin.exists(),
            "build the binary first: cargo build (missing {})",
            bin.display()
        );
        let child = ProcCommand::new(&bin)
            .arg("--display")
            .arg(display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("cannot spawn {}: {err}", bin.display()));
        WmChild(child)
    }

    /// True while the WM process is still running.
    fn alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for WmChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls `cond` every 50 ms until it holds, panicking after [`WAIT`].
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// A second x11rb test client connection to the same X server.
fn connect(display: &str) -> RustConnection {
    x11rb::connect(Some(display))
        .unwrap_or_else(|err| panic!("test client cannot connect to {display}: {err}"))
        .0
}

/// The root window of the connected screen.
fn root_of(conn: &RustConnection) -> Window {
    conn.setup().roots[0].root
}

/// Interns `name` (a fresh atom when it does not exist yet).
fn intern(conn: &RustConnection, name: &[u8]) -> Atom {
    conn.intern_atom(false, name).unwrap().reply().unwrap().atom
}

/// Geometry of `w` (position is relative to its parent).
fn geom(conn: &RustConnection, w: Window) -> GetGeometryReply {
    conn.get_geometry(w).unwrap().reply().unwrap()
}

/// The parent of `w` — the reparenting frame once the WM manages it.
fn parent_of(conn: &RustConnection, w: Window) -> Window {
    conn.query_tree(w).unwrap().reply().unwrap().parent
}

/// The map state of `w` (Viewable when the WM mapped frame + client).
fn map_state(conn: &RustConnection, w: Window) -> MapState {
    conn.get_window_attributes(w)
        .unwrap()
        .reply()
        .unwrap()
        .map_state
}

/// Whether the geometry of `w` matches the expectation exactly.
fn frame_is(conn: &RustConnection, w: Window, expect: (i16, i16, u16, u16)) -> bool {
    let g = geom(conn, w);
    (g.x, g.y, g.width, g.height) == expect
}

/// Expected master/stack placement rects under the default layout (ratio 0.5,
/// border 2), derived from the REAL screen size — the assertion that makes
/// the root-geometry wiring (T21) observable: a WM still tiling the old
/// hardcoded 1920x1080 area places its frames elsewhere on the pinned
/// 1280x1024 Xvfb screen.
fn expected_placements(area_w: u16, area_h: u16) -> ((i16, i16, u16, u16), (i16, i16, u16, u16)) {
    // Mirrors MasterStack::arrange with ratio 0.5: the placements are already
    // border-inset (MasterStack::inset).
    let master_w = ((f64::from(area_w) * 0.5).round() as i32).clamp(0, i32::from(area_w)) as u16;
    let stack_w = area_w - master_w;
    let border_x = i16::try_from(BORDER).unwrap_or(i16::MAX);
    let master = (
        border_x,
        border_x,
        master_w - 2 * BORDER,
        area_h - 2 * BORDER,
    );
    let stack = (
        i16::try_from(i32::from(master_w) + i32::from(BORDER)).unwrap_or(i16::MAX),
        border_x,
        stack_w - 2 * BORDER,
        area_h - 2 * BORDER,
    );
    (master, stack)
}

/// Creates and maps a normal (non-override-redirect) client window, which
/// triggers a MapRequest the WM must reparent into a frame (SC-x11-07).
fn map_client(conn: &RustConnection, root: Window, depth: u8, visual: u32) -> Window {
    let win = conn.generate_id().unwrap();
    let aux = CreateWindowAux::default().background_pixel(0x4040_40);
    conn.create_window(
        depth,
        win,
        root,
        0,
        0,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &aux,
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(win).unwrap().check().unwrap();
    conn.flush().unwrap();
    win
}

/// The primary keycode mapping to `keysym` in the server's keyboard table.
fn keycode_for_keysym(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let min = conn.setup().min_keycode;
    let max = conn.setup().max_keycode;
    let count = max.saturating_sub(min).saturating_add(1);
    let reply = conn
        .get_keyboard_mapping(min, count)
        .unwrap()
        .reply()
        .unwrap();
    let per = usize::from(reply.keysyms_per_keycode.max(1));
    (min..=max).find(|&keycode| {
        let slot = usize::from(keycode - min) * per;
        reply.keysyms.get(slot).copied() == Some(keysym)
    })
}

/// Injects a Super+`keysym` press for the key named `keysym_name` (the
/// default bindings are all Super-based): xdotool when available, otherwise
/// XTEST fake input through the test client (Xvfb supports XTEST). The XTEST
/// fallback holds Super down while the key is pressed so the server reports
/// the Mod4 state the grab matches on.
fn press_super(conn: &RustConnection, keysym: u32, keysym_name: &str, what: &str) {
    let drove = ProcCommand::new("xdotool")
        .args(["key", "--clearmodifiers", &format!("super+{keysym_name}")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if drove {
        return;
    }
    let super_key = keycode_for_keysym(conn, SUPER_L)
        .unwrap_or_else(|| panic!("no keycode maps to Super_L (needed for {what})"));
    let key = keycode_for_keysym(conn, keysym)
        .unwrap_or_else(|| panic!("no keycode maps to {keysym_name} (needed for {what})"));
    let root = root_of(conn);
    let down = |keycode: u8| {
        conn.xtest_fake_input(KEY_PRESS, keycode, 0, root, 0, 0, 0)
            .unwrap();
    };
    let up = |keycode: u8| {
        conn.xtest_fake_input(KEY_RELEASE, keycode, 0, root, 0, 0, 0)
            .unwrap();
    };
    down(super_key);
    down(key);
    up(key);
    up(super_key);
    conn.flush().unwrap();
}

// -------------------------------------------------------------------- tests

#[test]
#[ignore]
fn wm_claims_wm_s0_and_keeps_running() {
    // SC-x11-01/03 at the real X server: the WM connects, claims WM_S0 (the
    // root window acts as the selection owner) and keeps running afterwards.
    let display = display_name();
    let mut wm = WmChild::spawn(&display);
    let conn = connect(&display);
    let root = root_of(&conn);
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0).unwrap().reply().unwrap().owner == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");
}

#[test]
#[ignore]
fn map_request_tiles_to_real_geometry_and_keys_drive_focus_and_switch() {
    // The full happy path on a real Xvfb server (SC-x11-07, the input side of
    // SC-ws-06, and the T21 geometry wiring):
    //   1. two clients map -> both are reparented into 2px-bordered frames,
    //   2. they are tiled master-stack against the REAL screen geometry,
    //   3. Super+J moves focus, so the focused client becomes the master,
    //   4. Super+2 (workspace 2, which v1 cannot create) is a safe no-op.
    let display = display_name();
    let mut wm = WmChild::spawn(&display);
    let conn = connect(&display);
    let root = root_of(&conn);
    let screen = &conn.setup().roots[0];

    // The WM must own WM_S0 before it manages anything (SC-x11-03/04).
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0).unwrap().reply().unwrap().owner == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    // Real screen geometry: the area the WM must tile against. The pinned
    // harness screen (1280x1024x24) is deliberately NOT 1920x1080 so these
    // expectations can only be met by the root-geometry wiring (T21).
    let root_geom = geom(&conn, root);
    assert!(
        (root_geom.width, root_geom.height) != (1920, 1080),
        "harness screen must differ from the old hardcoded 1920x1080 area"
    );
    let (master, stack) = expected_placements(root_geom.width, root_geom.height);

    // Two normal clients map on the focused workspace. The most recently
    // mapped window is focused, so B starts as master and A in the stack.
    let a = map_client(&conn, root, screen.root_depth, screen.root_visual);
    let b = map_client(&conn, root, screen.root_depth, screen.root_visual);
    let frame_of = |w: Window| parent_of(&conn, w);

    wait_until("both clients to be reparented into frames", || {
        frame_of(a) != root && frame_of(b) != root && frame_of(a) != frame_of(b)
    });

    // Reparent + tiled: B (master) left, A (stack) right, both clients
    // viewable and sitting at the border inset inside their frames.
    let fa = frame_of(a);
    let fb = frame_of(b);
    wait_until("master-stack placement at the real screen geometry", || {
        frame_is(&conn, fb, master) && frame_is(&conn, fa, stack)
    });
    assert_eq!(
        map_state(&conn, fb),
        MapState::VIEWABLE,
        "the WM must map the frame (and with it the client)"
    );
    assert_eq!(map_state(&conn, fa), MapState::VIEWABLE);
    let ga = geom(&conn, a);
    let gfa = geom(&conn, fa);
    assert_eq!(
        (ga.x, ga.y),
        (i16::try_from(BORDER).unwrap(), i16::try_from(BORDER).unwrap()),
        "the client must sit at the frame's border offset (SC-x11-07)"
    );
    assert_eq!(
        (ga.width, ga.height),
        (gfa.width - 2 * BORDER, gfa.height - 2 * BORDER),
        "the client must be inset by the 2px border on every side"
    );

    // Super+J (focus_next): focus moves to A, so A becomes the master — the
    // frames swap (SC-lay-03: the focus index selects the master). This is
    // the end-to-end proof of the input path: real keypress -> grab ->
    // translate -> command -> re-tile.
    press_super(&conn, KEY_J, "j", "focus_next");
    wait_until("focus cycle to move A into the master slot", || {
        frame_is(&conn, fa, master) && frame_is(&conn, fb, stack)
    });

    // Super+2 (SwitchWorkspace(2)): workspace 2 cannot exist in v1 (a
    // workspace is auto-opened only when none exist, and there is no command
    // to open an empty one), so the switch is a documented no-op — nothing
    // unmaps and the WM survives. The unmap/map/focus mechanics of SC-ws-06
    // itself are asserted at the core seam
    // (`switching_workspaces_unmaps_and_maps_frames`).
    press_super(&conn, KEY_2, "2", "workspace switch");
    wait_until("the WM to settle after the workspace-switch key", || {
        frame_is(&conn, fa, master) && frame_is(&conn, fb, stack)
    });
    assert_eq!(
        map_state(&conn, fa),
        MapState::VIEWABLE,
        "windows must stay mapped when switching to an unknown workspace"
    );
    assert_eq!(map_state(&conn, fb), MapState::VIEWABLE);
    assert!(wm.alive(), "the WM must survive an unknown-workspace switch");
}
