//! Gated end-to-end integration tests (T21): a REAL X server (Xvfb) drives
//! the full WM — the `tessera` binary as a subprocess, a second x11rb test
//! client for the windows and the assertions, and xdotool (or XTEST through
//! the test client) for the keypresses.
//!
//! Every test is `#[ignore]` so plain `cargo test` stays green headless; the
//! suite runs only on demand. `--test-threads=1` is REQUIRED: every test
//! spawns its own WM and WM_S0 is an exclusive root selection — two WMs on
//! the same Xvfb display cannot coexist, and the second would abort its
//! claim (SC-x11-04) before its assertions start:
//!
//! ```text
//! xvfb-run -a -s "-screen 0 1280x1024x24" cargo test --test integration -- --ignored --test-threads=1
//! ```
//!
//! Prerequisites (NOT installed by this change): `Xvfb` and `xdotool`. The
//! pinned screen size (1280x1024x24) deliberately differs from the old
//! hardcoded 1920x1080 tiling area, so the geometry assertions really prove
//! the real-screen wiring (T21) instead of matching the old constant by
//! coincidence.
//!
//! The lock-tolerance and launcher tests (PR4) drive keys purely through
//! XTEST fake input — never xdotool `--clearmodifiers`, which clears the
//! lock bits those tests exist to prove — and run the terminal/launcher
//! through test-only probe fixtures under the workspace `tests/` directory
//! (plain executables on a PATH given ONLY to the WM under test; never
//! installed, never compiled by cargo).

use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcCommand, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::Connection as RandRConnection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xkb::{ConnectionExt as _, Group, ID};
use x11rb::protocol::xproto::{
    Atom, ConnectionExt, CreateWindowAux, EventMask, GetGeometryReply, ImageFormat, MapState,
    ModMask, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use tessera_core::{BarConfig, Rect};
use tessera_x11::bar_renderer::tiling_area;

/// X11 core event codes used by the XTEST fallback driver.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
/// Keysyms the test drives: Super+J (focus_next, XK_j) and Super+2
/// (workspace 2, XK_2), matching the config defaults.
const KEY_J: u32 = 0x006a;
const KEY_2: u32 = 0x0032;
/// Mod4 keysym (XK_Super_L) — every default binding is Super-based.
const SUPER_L: u32 = 0xffeb;
/// Keysyms the launcher/lock E2E (PR4) drives: Return (terminal), Space
/// (Ctrl+Space launcher), Control_L, and the two lock keys XTEST can toggle
/// under Xvfb (NumLock, CapsLock). ScrollLock's Mod3 bit has no toggleable
/// key under Xvfb's default keymap, so [`set_locks`] sets it through the
/// XKB LatchLockState request instead.
const KEY_RETURN: u32 = 0xff0d;
const KEY_SPACE: u32 = 0x0020;
const CONTROL_L: u32 = 0xffe3;
const NUM_LOCK: u32 = 0xff7f;
const CAPS_LOCK: u32 = 0xffe5;
/// The three lock-modifier bits (XKB locked mods): Lock (CapsLock, 2),
/// Mod2 (NumLock, 16) and Mod3 (ScrollLock, 32) — the bits keyboard.rs
/// strips (LOCK_STRIP=178) before the command lookup, so a locked press
/// still matches the base combo.
const LOCK_BITS: u16 = 2 | 16 | 32;
/// Default frame border (`config.general.border_width`), baked into every
/// layout placement.
const BORDER: u16 = 2;
/// ayu_dark (the embedded default theme): focused-frame border pixel =
/// accent `#FF8F40`, unfocused = comment `#626A73`, packed into the low
/// 24 bits (SC-thm-09).
const AYU_ACTIVE_PIXEL: u32 = 0x00FF_8F40;
const AYU_INACTIVE_PIXEL: u32 = 0x0062_6A73;
/// A window geometry `(x, y, width, height)` as reported by `get_geometry`.
type Geom = (i16, i16, u16, u16);
/// Time budget for every polled assertion.
const WAIT: Duration = Duration::from_secs(10);

// ------------------------------------------------------------------ harness

/// The display the test runs under (`xvfb-run` sets `$DISPLAY`).
fn display_name() -> String {
    std::env::var("DISPLAY").unwrap_or_else(|_| {
        panic!(
            "no DISPLAY set — run under `xvfb-run -a -s \"-screen 0 1280x1024x24\" \
             cargo test --test integration -- --ignored --test-threads=1`"
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
    /// Spawns the binary on `display` with the default config.
    fn spawn(display: &str) -> WmChild {
        Self::spawn_with_config(display, None)
    }

    /// Spawns the binary on `display`, optionally passing `--config <path>`
    /// (T10: the themed-border tests drive a custom theme through a config
    /// file). Detached stdio keeps the WM's eprintln chatter out of the test
    /// harness pipes.
    fn spawn_with_config(display: &str, config: Option<&Path>) -> WmChild {
        Self::spawn_full(display, config, &[], false)
    }

    /// Spawns the binary on `display` with extra env entries (e.g. a
    /// probe-only PATH and the probe sentinel path — applied ONLY to the WM
    /// child, never to the harness) and optional stderr capture (the claim
    /// log's KBR-3 "16 bindings" line). With `capture_stderr`, the pipe must
    /// be drained via [`Self::stop_and_read_stderr`] after the child has
    /// stopped — reading while the WM still runs would block on the open
    /// pipe forever (the WM never exits on its own).
    fn spawn_full(
        display: &str,
        config: Option<&Path>,
        envs: &[(&str, &str)],
        capture_stderr: bool,
    ) -> WmChild {
        // The previous test's WM must have released WM_S0 before a new WM
        // claims it (the server clears the selection asynchronously).
        wait_for_wm_s0_release(display);
        let bin = wm_binary();
        assert!(
            bin.exists(),
            "build the binary first: cargo build (missing {})",
            bin.display()
        );
        let mut cmd = ProcCommand::new(&bin);
        cmd.arg("--display").arg(display);
        if let Some(path) = config {
            cmd.arg("--config").arg(path);
        }
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        if capture_stderr {
            cmd.stderr(Stdio::piped());
        } else {
            cmd.stderr(Stdio::null());
        }
        let child = cmd
            .spawn()
            .unwrap_or_else(|err| panic!("cannot spawn {}: {err}", bin.display()));
        WmChild(child)
    }

    /// Kills and reaps the WM (no stderr drain).
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    /// Kills and reaps the WM, then returns everything it wrote to stderr
    /// ("" when stderr was not captured). Only meaningful after the child
    /// has stopped — the WM is killed here first, so the pipe reaches EOF
    /// and drains without blocking.
    fn stop_and_read_stderr(&mut self) -> String {
        let _ = self.0.kill();
        let _ = self.0.wait();
        match self.0.stderr.take() {
            Some(mut pipe) => {
                let mut out = String::new();
                use std::io::Read;
                let _ = pipe.read_to_string(&mut out);
                out
            }
            None => String::new(),
        }
    }

    /// True while the WM process is still running.
    fn alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }

    /// The WM's OS pid, used by the idle-CPU test to read its `/proc/<pid>`
    /// tick counters.
    fn pid(&self) -> u32 {
        self.0.id()
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
///
/// Retried with a short backoff: Xvfb can transiently reset a fresh
/// connection while it is still tearing down the previous test's killed WM
/// (a pre-existing gated-suite flake — the handshake succeeds on retry).
fn connect(display: &str) -> RustConnection {
    let mut last_error = None;
    for _ in 0..5 {
        match x11rb::connect(Some(display)) {
            Ok((conn, _)) => return conn,
            Err(err) => {
                last_error = Some(err);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    panic!("test client cannot connect to {display}: {last_error:?}");
}

/// Waits until no client owns WM_S0 — i.e. the previous test's WM has been
/// fully torn down. The server clears a killed client's selection
/// asynchronously with its connection close, and a WM spawned into that
/// window would abort its claim (SC-x11-04) and exit before its assertions
/// start (a pre-existing gated-suite flake: "timed out waiting for the WM
/// to own WM_S0").
fn wait_for_wm_s0_release(display: &str) {
    let probe = connect(display);
    let wm_s0 = intern(&probe, b"WM_S0");
    wait_until("the previous WM to release WM_S0", || {
        probe
            .get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == 0
    });
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
fn frame_is(conn: &RustConnection, w: Window, expect: Geom) -> bool {
    let g = geom(conn, w);
    (g.x, g.y, g.width, g.height) == expect
}

/// The border pixel of `frame` as it is actually drawn on the root — the
/// true "themed border visible on screen" check (REQ-x11-005 modified). The
/// X11 GetWindowAttributes reply does not carry the border colour, and a
/// window's border is not part of its own drawable image, so the root image
/// is sampled at the frame's outer corner (frames are direct root children,
/// so their geometry is root-relative). ZPixmap at 24-bit depth is 32bpp,
/// LSB-first `[B, G, R, pad]`.
fn border_pixel(conn: &RustConnection, frame: Window) -> u32 {
    let g = geom(conn, frame);
    let img = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root_of(conn),
            g.x,
            g.y,
            1,
            1,
            0xFFFF_FFFF,
        )
        .unwrap()
        .reply()
        .unwrap();
    let (b, g_, r) = (img.data[0] as u32, img.data[1] as u32, img.data[2] as u32);
    (r << 16) | (g_ << 8) | b
}

/// Expected master/stack placement rects under the default layout (ratio 0.5,
/// border 2), derived from the tiling area — the REAL screen minus the default
/// top bar (22px, task 2.6) — so the assertions prove both the root-geometry
/// wiring (T21) and the bar-shrunk area: a WM still tiling the old hardcoded
/// 1920x1080 area (or the full screen) places its frames elsewhere on the
/// pinned 1280x1024 Xvfb screen.
fn expected_placements(area: Rect) -> (Geom, Geom) {
    // Mirrors MasterStack::arrange with ratio 0.5: the placements are already
    // border-inset (MasterStack::inset), offset by the tiling area's origin.
    let area_w = area.w;
    let area_h = area.h;
    let master_w = ((f64::from(area_w) * 0.5).round() as i32).clamp(0, i32::from(area_w)) as u16;
    let stack_w = area_w - master_w;
    let border_x = i16::try_from(BORDER).unwrap_or(i16::MAX);
    let area_x = i16::try_from(area.x).unwrap_or(i16::MAX);
    let area_y = i16::try_from(area.y).unwrap_or(i16::MAX);
    let master = (
        area_x + border_x,
        area_y + border_x,
        master_w - 2 * BORDER,
        area_h - 2 * BORDER,
    );
    let stack = (
        area_x + i16::try_from(i32::from(master_w) + i32::from(BORDER)).unwrap_or(i16::MAX),
        area_y + border_x,
        stack_w - 2 * BORDER,
        area_h - 2 * BORDER,
    );
    (master, stack)
}

/// Creates and maps a normal (non-override-redirect) client window, which
/// triggers a MapRequest the WM must reparent into a frame (SC-x11-07).
fn map_client(conn: &RustConnection, root: Window, depth: u8, visual: u32) -> Window {
    let win = conn.generate_id().unwrap();
    let aux = CreateWindowAux::default().background_pixel(0x0040_4040);
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

/// Raw XTEST press+release of `keycode`, no modifiers touched. This is the
/// ONLY input path the lock tests use — xdotool's `--clearmodifiers` would
/// clear the exact lock bits these tests prove survive, so they must never
/// go through it.
fn xtest_key(conn: &RustConnection, keycode: u8) {
    let root = root_of(conn);
    conn.xtest_fake_input(KEY_PRESS, keycode, 0, root, 0, 0, 0).unwrap();
    conn.xtest_fake_input(KEY_RELEASE, keycode, 0, root, 0, 0, 0)
        .unwrap();
    conn.flush().unwrap();
}

/// Ensures the XKB extension is usable on `conn` (x11rb's xkb feature gate
/// requires an explicit xkb_use_extension handshake before any XKB request).
fn init_xkb(conn: &RustConnection) {
    conn.xkb_use_extension(1, 0)
        .unwrap()
        .reply()
        .unwrap_or_else(|err| panic!("xkb_use_extension failed: {err}"));
}

/// The core keyboard's currently locked modifier bits (bit 1 = CapsLock,
/// bit 4 = NumLock, bit 5 = ScrollLock).
fn locks_on(conn: &RustConnection) -> u16 {
    init_xkb(conn);
    conn.xkb_get_state(ID::USE_CORE_KBD.into())
        .unwrap()
        .reply()
        .unwrap()
        .locked_mods
        .into()
}

/// Drives the three lock modifiers to the requested state by injecting the
/// REAL key presses a user would use — XTEST toggles for the keys that are
/// toggleable under Xvfb's default keymap (NumLock=77 locks Mod2, CapsLock=66
/// locks Lock) — and, for the ScrollLock bit (Mod3, which maps to an inert
/// keycode 203 under Xvfb), the XKB LatchLockState request. Afterwards the
/// core keyboard's locked_mods must equal `target & LOCK_BITS`.
fn set_locks(conn: &RustConnection, target: u16) {
    let current = locks_on(conn) & LOCK_BITS;
    let want = target & LOCK_BITS;
    let toggle_towards = |conn: &RustConnection, keycode: u8, bit: u16| {
        let active = current & bit != 0;
        let wanted = want & bit != 0;
        if active != wanted {
            xtest_key(conn, keycode);
        }
    };
    toggle_towards(conn, keycode_for_keysym(conn, NUM_LOCK).unwrap(), 16);
    toggle_towards(conn, keycode_for_keysym(conn, CAPS_LOCK).unwrap(), 2);
    let locked = locks_on(conn) & LOCK_BITS;
    let need_latch = want & 32 != 0;
    let already_latched = locked & 32 != 0;
    if need_latch != already_latched {
        init_xkb(conn);
        conn.xkb_latch_lock_state(
            ID::USE_CORE_KBD.into(),
            ModMask::from(32u16), // affect_mod_locks
            ModMask::from(32u16), // mod_locks
            false,                // lock_group
            Group::from(0u8),     // group_lock
            ModMask::from(0u16),  // affect_mod_latches
            false,                // latch_group
            0u16,                 // group_latch
        )
        .unwrap();
        conn.flush().unwrap();
    }
    let after = locks_on(conn) & LOCK_BITS;
    assert_eq!(
        after, want,
        "lock bits after set_locks: got {after:#06x}, want {want:#06x}"
    );
}

/// Drives a chord via pure XTEST: `mods` then `key`, released in reverse
/// order. Never clears modifiers — the lock tests rely on the modifier state
/// being untouched and reported as-is.
fn press_combo(conn: &RustConnection, mods: &[u8], key: u8) {
    let root = root_of(conn);
    for &keycode in mods {
        conn.xtest_fake_input(KEY_PRESS, keycode, 0, root, 0, 0, 0).unwrap();
    }
    conn.xtest_fake_input(KEY_PRESS, key, 0, root, 0, 0, 0).unwrap();
    conn.xtest_fake_input(KEY_RELEASE, key, 0, root, 0, 0, 0).unwrap();
    for &keycode in mods.iter().rev() {
        conn.xtest_fake_input(KEY_RELEASE, keycode, 0, root, 0, 0, 0).unwrap();
    }
    conn.flush().unwrap();
}

/// The `keysym`'s keycode, looked up against the server's table.
fn keycode_for(conn: &RustConnection, keysym: u32, name: &str) -> u8 {
    keycode_for_keysym(conn, keysym)
        .unwrap_or_else(|| panic!("no keycode maps to {name} (keysym {keysym:#06x})"))
}

/// Full keyboard state for the "every lock modifier on" scenario: the press
/// must carry locked Lock/Mod2/Mod3 with it.
fn all_locks_on(conn: &RustConnection) {
    set_locks(conn, LOCK_BITS);
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
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
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
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    // Real screen geometry: the area the WM must tile against. The pinned
    // harness screen (1280x1024x24) is deliberately NOT 1920x1080 so these
    // expectations can only be met by the root-geometry wiring (T21). The
    // default top bar (22px) shrinks the tiling area (task 2.6).
    let root_geom = geom(&conn, root);
    assert!(
        (root_geom.width, root_geom.height) != (1920, 1080),
        "harness screen must differ from the old hardcoded 1920x1080 area"
    );
    let monitor = Rect {
        x: 0,
        y: 0,
        w: root_geom.width,
        h: root_geom.height,
    };
    let (master, stack) = expected_placements(tiling_area(monitor, &BarConfig::default()));

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
        (
            i16::try_from(BORDER).unwrap(),
            i16::try_from(BORDER).unwrap()
        ),
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
    assert!(
        wm.alive(),
        "the WM must survive an unknown-workspace switch"
    );
}

#[test]
#[ignore]
fn themed_borders_paint_active_and_inactive_and_repaint_on_focus() {
    // SC-thm-09 + SC-x11-13 at the real X server: with the embedded ayu_dark
    // default (no --config), the focused frame's border is the accent pixel
    // and the unfocused frame's is the comment pixel; moving focus repaints
    // the old frame inactive and the new frame active — the pixels swap.
    let display = display_name();
    let mut wm = WmChild::spawn(&display);
    let conn = connect(&display);
    let root = root_of(&conn);
    let screen = &conn.setup().roots[0];

    // The WM must own WM_S0 before it manages anything (SC-x11-03/04).
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    // The most recently mapped window is focused, so B's frame is the active
    // one and A's is inactive (REQ-x11-005 modified).
    let a = map_client(&conn, root, screen.root_depth, screen.root_visual);
    let b = map_client(&conn, root, screen.root_depth, screen.root_visual);
    wait_until("both clients to be reparented into frames", || {
        parent_of(&conn, a) != root
            && parent_of(&conn, b) != root
            && parent_of(&conn, a) != parent_of(&conn, b)
    });
    let fa = parent_of(&conn, a);
    let fb = parent_of(&conn, b);

    // The default ayu_dark borders reach the server: focused = accent,
    // unfocused = comment, and they differ (SC-thm-09).
    wait_until("the frames to carry the themed border pixels", || {
        border_pixel(&conn, fb) == AYU_ACTIVE_PIXEL && border_pixel(&conn, fa) == AYU_INACTIVE_PIXEL
    });

    // Super+J moves focus to A: A's frame repaints active and B's inactive
    // (SC-x11-13 through the real keypress + grab + repaint path).
    press_super(&conn, KEY_J, "j", "focus_next");
    wait_until("the focus change to repaint both border pixels", || {
        border_pixel(&conn, fa) == AYU_ACTIVE_PIXEL && border_pixel(&conn, fb) == AYU_INACTIVE_PIXEL
    });
}

#[test]
#[ignore]
fn custom_theme_file_overrides_frame_border_pixels() {
    // SC-thm-07/10 at the real X server: a config `theme = "path"` makes the
    // WM paint the explicit border keys from that file, replacing the
    // ayu_dark derived defaults (focused = #FF0000, unfocused = #00FF00).
    let display = display_name();
    let theme = std::env::temp_dir().join(format!(
        "tessera-e2e-custom-theme-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &theme,
        "active_border = \"#FF0000\"\ninactive_border = \"#00FF00\"\n",
    )
    .expect("write the custom theme file");
    let config = std::env::temp_dir().join(format!(
        "tessera-e2e-custom-config-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &config,
        format!("[general]\ntheme = {:?}\n", theme.to_string_lossy()),
    )
    .expect("write the config referencing the custom theme");

    let mut wm = WmChild::spawn_with_config(&display, Some(&config));
    let conn = connect(&display);
    let root = root_of(&conn);
    let screen = &conn.setup().roots[0];

    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    let a = map_client(&conn, root, screen.root_depth, screen.root_visual);
    let b = map_client(&conn, root, screen.root_depth, screen.root_visual);
    wait_until("both clients to be reparented into frames", || {
        parent_of(&conn, a) != root && parent_of(&conn, b) != root
    });
    let fa = parent_of(&conn, a);
    let fb = parent_of(&conn, b);

    // The custom theme's explicit border keys replace the derived defaults
    // (SC-thm-10): focused = #FF0000, unfocused = #00FF00 — NOT the ayu
    // accent/comment pixels.
    wait_until("the frames to carry the custom border pixels", || {
        border_pixel(&conn, fb) == 0x00FF_0000 && border_pixel(&conn, fa) == 0x0000_FF00
    });
    assert!(
        border_pixel(&conn, fb) != AYU_ACTIVE_PIXEL,
        "the custom active border must replace the ayu default"
    );

    let _ = std::fs::remove_file(&theme);
    let _ = std::fs::remove_file(&config);
}

// ------------------------------------------------------------------- bar E2E
//
// Task 5.1 (`x11::bar-position::*` tags): the status bar must be drawn along
// exactly the configured screen edge (default top), and stay idle-cheap. The
// tests run only under an Xvfb E2E session (same ignore harness as the other
// suites); selection:
// ```text
// xvfb-run -a -s "-screen 0 1280x1024x24" cargo test --test integration -- --ignored bar_position --test-threads=1
// ```

/// The status bar's default `[bar] thickness` for `Top`/`Bottom` (design D6).
///
/// Kept independent of the renderer constants on purpose: the E2E asserts a
/// concrete, spec-derived number, so a regression in the renderer's default
/// breaks this test instead of silently matching a copied value.
const BAR_TOP_BOTTOM_THICKNESS: u16 = 22;
/// Default `[bar] thickness` for `Left`/`Right` (design D6).
const BAR_SIDE_THICKNESS: u16 = 6;
/// The idle window the low-CPU budget is measured over (spec: 60 seconds).
const CPU_IDLE_WINDOW: Duration = Duration::from_secs(60);
/// The idle budget: below 5% of one core (spec "Low Idle CPU Overhead").
const CPU_BUDGET_FRACTION: f64 = 0.05;

/// The status bar's root child window, or `None` before the WM maps it.
///
/// The bar and the reparented client frames are both override-redirect, but
/// frames select `SubstructureNotify` on themselves (frames.rs) while the bar
/// window requests no events (`BarRenderer::new`), so the two are
/// distinguishable by `GetWindowAttributes` even when clients are present.
fn find_bar(conn: &RustConnection, root: Window) -> Option<Window> {
    conn.query_tree(root)
        .unwrap()
        .reply()
        .unwrap()
        .children
        .into_iter()
        .find(|&w| {
            conn.get_window_attributes(w)
                .unwrap()
                .reply()
                .is_ok_and(|attrs| {
                    attrs.override_redirect
                        && !attrs
                            .your_event_mask
                            .contains(EventMask::SUBSTRUCTURE_NOTIFY)
                        && attrs.map_state == MapState::VIEWABLE
                })
        })
}

/// Writes a `[bar] position = "<position>"` config into a unique temp file
/// and returns its path.
fn bar_position_config(position: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "tessera-e2e-bar-position-{position}-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, format!("[bar]\nposition = \"{position}\"\n"))
        .expect("write the bar-position config");
    path
}

/// Spawns the WM (with `config`, or defaults when `None`) and waits for the
/// WM_S0 claim, returning the harness pieces the caller asserts over.
fn spawn_wm(display: &str, config: Option<&Path>) -> (WmChild, RustConnection, Window) {
    let wm = WmChild::spawn_with_config(display, config);
    let conn = connect(display);
    let root = root_of(&conn);
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    (wm, conn, root)
}

/// Asserts `[bar] position = <position>` renders a Viewable bar exactly along
/// that edge at the default thickness (22px top/bottom, 6px left/right).
fn assert_bar_position(display: &str, position: &str) {
    let config = bar_position_config(position);
    let (_wm, conn, root) = spawn_wm(display, Some(&config));
    wait_until("the bar window to be mapped", || {
        find_bar(&conn, root).is_some()
    });
    let bar = find_bar(&conn, root).expect("the WM must create the bar window");
    let bar_geom = geom(&conn, bar);
    let root_geom = geom(&conn, root);
    let (rw, rh) = (i32::from(root_geom.width), i32::from(root_geom.height));
    let expected: Geom = match position {
        "top" => (0, 0, rw as u16, BAR_TOP_BOTTOM_THICKNESS),
        "bottom" => (
            0,
            i16::try_from(rh - i32::from(BAR_TOP_BOTTOM_THICKNESS)).unwrap_or(i16::MAX),
            rw as u16,
            BAR_TOP_BOTTOM_THICKNESS,
        ),
        "left" => (0, 0, BAR_SIDE_THICKNESS, rh as u16),
        "right" => (
            i16::try_from(rw - i32::from(BAR_SIDE_THICKNESS)).unwrap_or(i16::MAX),
            0,
            BAR_SIDE_THICKNESS,
            rh as u16,
        ),
        other => panic!("unknown bar position {other:?}"),
    };
    assert!(
        frame_is(&conn, bar, expected),
        "the {position} bar must sit at the {position} edge {expected:?}, \
         got {bar_geom:?} on a {root_geom:?} root"
    );
    assert_eq!(
        map_state(&conn, bar),
        MapState::VIEWABLE,
        "the {position} bar must be mapped and visible"
    );
    let _ = std::fs::remove_file(&config);
}

#[test]
#[ignore]
fn bar_position_top() {
    assert_bar_position(&display_name(), "top");
}

#[test]
#[ignore]
fn bar_position_bottom() {
    assert_bar_position(&display_name(), "bottom");
}

#[test]
#[ignore]
fn bar_position_left() {
    assert_bar_position(&display_name(), "left");
}

#[test]
#[ignore]
fn bar_position_right() {
    assert_bar_position(&display_name(), "right");
}

#[test]
#[ignore]
fn bar_position_idle_cpu_within_budget_of_one_core() {
    // SC "Low Idle CPU Overhead": with a visible bar under Xvfb, a 60s idle
    // window keeps the WM's whole-process CPU (including the dedicated bar
    // thread) under 5% of one core — the proof that the event loop blocks on
    // `wait_for_event` (no idle polling) and the bar only draws on recompute
    // (design D4/D11). The budget is measured as the WM's own tick delta per
    // the system tick delta across all cores, normalized to ONE core, so the
    // host's background load cannot inflate the WM's number.
    let display = display_name();
    let (mut wm, conn, root) = spawn_wm(&display, None);
    wait_until("the bar window to be mapped", || {
        find_bar(&conn, root).is_some()
    });
    assert!(
        wm.alive(),
        "the WM must still run once the default-config bar is up"
    );

    // A client opens a workspace, so the bar really paints once; afterwards
    // nothing touches the WM for the whole idle window.
    let screen = &conn.setup().roots[0];
    let client = map_client(&conn, root, screen.root_depth, screen.root_visual);
    wait_until("the client to be reparented into a frame", || {
        parent_of(&conn, client) != root
    });

    let pid = wm.pid();
    let start = Instant::now();
    let start_proc = process_cpu_ticks(pid);
    let start_sys = system_cpu_ticks();
    while start.elapsed() < CPU_IDLE_WINDOW {
        std::thread::sleep(Duration::from_secs(5));
    }
    let end_proc = process_cpu_ticks(pid);
    let end_sys = system_cpu_ticks();

    let sys_delta = end_sys.saturating_sub(start_sys);
    assert!(
        sys_delta > 0,
        "the system CPU clock must advance over the idle window"
    );
    let proc_delta = end_proc.saturating_sub(start_proc);
    let cores = system_core_count().max(1);
    let fraction_of_one_core = proc_delta as f64 * cores as f64 / sys_delta as f64;

    assert!(wm.alive(), "the WM must survive the whole idle window");
    assert!(
        fraction_of_one_core < CPU_BUDGET_FRACTION,
        "idle WM+bar used {:.2}% of one core over {:?} (budget {:.0}%), \
         proc {proc_delta} ticks vs {sys_delta} system ticks on {cores} cores",
        fraction_of_one_core * 100.0,
        CPU_IDLE_WINDOW,
        CPU_BUDGET_FRACTION * 100.0
    );
}

#[test]
#[ignore]
fn bar_position_two_outputs_render_only_the_primary() {
    // Design D10's "bar on the RandR primary output only" needs a REAL
    // multi-head RandR server. Xvfb exposes exactly one output per screen
    // (extra `-screen` args create separate X screens — different roots, not
    // two outputs of one root — and generic RandR has no way to fabricate an
    // output+CRTC pair headlessly), so under a single-output server this test
    // records the skip honestly and returns; on a genuine dual-monitor setup
    // it proves the bar stays inside the primary output and draws no bar that
    // overlaps a non-primary output.
    let display = display_name();
    let (_wm, conn, root) = spawn_wm(&display, None);
    wait_until("the bar window to be mapped", || {
        find_bar(&conn, root).is_some()
    });

    let resources = conn
        .randr_get_screen_resources_current(root)
        .unwrap()
        .reply()
        .unwrap();
    let primary_id = conn
        .randr_get_output_primary(root)
        .unwrap()
        .reply()
        .unwrap()
        .output;

    let mut connected: Vec<(u32, Rect)> = Vec::new();
    for &out in resources.outputs.iter() {
        let info = conn
            .randr_get_output_info(out, x11rb::CURRENT_TIME)
            .unwrap()
            .reply();
        let info = match info {
            Ok(reply)
                if reply.connection == RandRConnection::CONNECTED && reply.crtc != x11rb::NONE =>
            {
                reply
            }
            _ => continue,
        };
        let crtc = conn
            .randr_get_crtc_info(info.crtc, x11rb::CURRENT_TIME)
            .unwrap()
            .reply()
            .unwrap();
        if crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        connected.push((
            out,
            Rect {
                x: i32::from(crtc.x),
                y: i32::from(crtc.y),
                w: crtc.width,
                h: crtc.height,
            },
        ));
    }

    if connected.len() < 2 {
        eprintln!(
            "skip: two-output primary-only assertion needs a real multi-head \
             display ({} connected output(s) found)",
            connected.len()
        );
        return;
    }

    let bar = find_bar(&conn, root).expect("the bar window");
    let g = geom(&conn, bar);
    let bar_rect = Rect {
        x: i32::from(g.x),
        y: i32::from(g.y),
        w: g.width,
        h: g.height,
    };
    let primary_rect = connected
        .iter()
        .find(|(id, _)| *id == primary_id)
        .map(|(_, rect)| *rect)
        .unwrap_or(connected[0].1);
    assert!(
        rect_contains(primary_rect, bar_rect),
        "the bar {bar_rect:?} must sit inside the primary output {primary_rect:?}"
    );
    for (id, rect) in connected {
        if rect == primary_rect {
            continue;
        }
        assert!(
            !rects_overlap(bar_rect, rect),
            "the bar {bar_rect:?} must not overlap the non-primary \
             output {id} ({rect:?})"
        );
    }
}

/// Sum of `utime`+`stime` (fields 14/15 of each thread's stat file) across
/// every thread of `pid` — the whole-process CPU the bar renderer thread
/// contributes to. Reading only `/proc/<pid>/stat` would miss the dedicated
/// bar thread (that file reports the thread-group leader only).
fn process_cpu_ticks(pid: u32) -> u64 {
    let task_dir = Path::new("/proc").join(pid.to_string()).join("task");
    let mut total = 0u64;
    for entry in std::fs::read_dir(&task_dir)
        .unwrap_or_else(|err| panic!("cannot read {task_dir:?} for pid {pid}: {err}"))
    {
        let stat = std::fs::read_to_string(entry.unwrap().path().join("stat"))
            .unwrap_or_else(|err| panic!("cannot read thread stat for pid {pid}: {err}"));
        // `pid (comm) state ppid ... utime stime ...`: after the closing `)`
        // the first token is field 3 (state), so utime (field 14) is token 11
        // and stime (field 15) is token 12.
        let after_comm = stat.rsplit(')').next().expect("a stat line ends with ')'");
        let mut fields = after_comm.split_whitespace();
        let utime: u64 = fields
            .nth(11)
            .unwrap_or_else(|| panic!("no utime field in stat {stat:?}"))
            .parse()
            .unwrap_or_else(|err| panic!("utime parse: {err}"));
        let stime: u64 = fields
            .next()
            .unwrap_or_else(|| panic!("no stime field in stat {stat:?}"))
            .parse()
            .unwrap_or_else(|err| panic!("stime parse: {err}"));
        total += utime + stime;
    }
    total
}

/// Total system CPU ticks across all cores (the first `/proc/stat` line), in
/// the same USER_HZ unit as the process tick fields.
fn system_cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/stat").expect("cannot read /proc/stat");
    let cpu = stat.lines().next().expect("cpu line");
    cpu.split_whitespace()
        .skip(1)
        .take(8)
        .map(|token| token.parse::<u64>().unwrap_or(0))
        .sum()
}

/// Number of online CPUs (the `cpuN` lines in `/proc/stat`).
fn system_core_count() -> usize {
    let stat = std::fs::read_to_string("/proc/stat").expect("cannot read /proc/stat");
    stat.lines()
        .filter(|line| {
            line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(|b| b.is_ascii_digit())
        })
        .count()
}

/// Whether rect `a` lies entirely inside rect `b` (i64 math so negative
/// CRT-relative coordinates on real multi-head roots cannot overflow).
fn rect_contains(outer: Rect, inner: Rect) -> bool {
    let (ox, oy, ow, oh) = (
        i64::from(outer.x),
        i64::from(outer.y),
        i64::from(outer.w),
        i64::from(outer.h),
    );
    let (ix, iy, iw, ih) = (
        i64::from(inner.x),
        i64::from(inner.y),
        i64::from(inner.w),
        i64::from(inner.h),
    );
    ix >= ox && iy >= oy && ix + iw <= ox + ow && iy + ih <= oy + oh
}

/// Whether rects `a` and `b` share at least one pixel.
fn rects_overlap(a: Rect, b: Rect) -> bool {
    let (ax, ay, aw, ah) = (
        i64::from(a.x),
        i64::from(a.y),
        i64::from(a.w),
        i64::from(a.h),
    );
    let (bx, by, bw, bh) = (
        i64::from(b.x),
        i64::from(b.y),
        i64::from(b.w),
        i64::from(b.h),
    );
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

// ----------------------------------------------------------------------
// Lock-variant + launcher E2E (PR4, tessera-keybinds-launcher)
// ----------------------------------------------------------------------

/// The workspace `tests/` directory holding the test-only probe fixtures
/// (task 4.2: plain executables, never installed — the Makefile installs
/// only the binary and the desktop entry, and the fixtures are only reachable
/// through a PATH handed to the WM under test).
fn probes_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../tests")
        .canonicalize()
        .unwrap_or_else(|err| panic!("cannot resolve tests/ dir: {err}"))
}

/// The inherited PATH with `probes` prepended — the WM's spawned programs
/// resolve the probe fixtures first, but a real launcher like rofi (found
/// later on the inherited PATH) still works.
fn path_with_probes(probes: &Path) -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", probes.display(), inherited)
}

/// A unique sentinel path for `what`'s probe output (per-process so parallel
/// harnesses never collide; the probe writes its full argv there).
fn sentinel_path(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tessera-e2e-{what}-{}.probe",
        std::process::id()
    ))
}

/// Whether `name` resolves through the current PATH (plain `Command::new`
/// lookup semantics: a `+x` regular file in any PATH entry).
fn probe_on_path(name: &str) -> bool {
    std::env::var("PATH").is_ok_and(|path| {
        path.split(':').any(|dir| {
            let candidate = Path::new(dir).join(name);
            candidate.is_file() && {
                use std::os::unix::fs::PermissionsExt;
                candidate.metadata().is_ok_and(|m| {
                    m.permissions().mode() & 0o111 != 0
                })
            }
        })
    })
}

/// PIDs of every live process whose comm is exactly `name` (rofi's is
/// "rofi", unlike xdotool's transient "xdotool").
fn process_pids(name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if std::fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|comm| comm.trim() == name)
            {
                pids.push(pid);
            }
        }
    }
    pids
}

#[test]
#[ignore]
fn lock_variant_press_with_locks_on_spawns_the_terminal_probe() {
    // KBR-3 at the real X server, the scenario xdotool --clearmodifiers
    // would destroy: with CapsLock+NumLock+ScrollLock ALL on, Super+Enter
    // must still spawn the terminal. The press is injected through pure
    // XTEST (never --clearmodifiers), so the lock bits survive, the WM's
    // lock-variant grab matches, and the locked modifier state is proven to
    // reach the command lookup untouched.
    let display = display_name();
    let probes = probes_dir();
    let config = std::env::temp_dir().join(format!(
        "tessera-e2e-lock-terminal-{}.toml",
        std::process::id()
    ));
    std::fs::write(&config, "[general]\nterminal = \"probe_terminal\"\n")
        .expect("write the terminal-probe config");
    let sentinel = sentinel_path("lock-terminal");
    let _ = std::fs::remove_file(&sentinel);
    let path = path_with_probes(&probes);

    let mut wm = WmChild::spawn_full(
        &display,
        Some(&config),
        &[
            ("PATH", &path),
            ("TESSERA_TEST_SENTINEL", sentinel.to_str().unwrap()),
        ],
        false,
    );
    let conn = connect(&display);
    let root = root_of(&conn);
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    all_locks_on(&conn);
    assert_eq!(
        locks_on(&conn) & LOCK_BITS,
        LOCK_BITS,
        "precondition: every lock modifier on"
    );

    let super_kc = keycode_for(&conn, SUPER_L, "Super_L");
    let enter_kc = keycode_for(&conn, KEY_RETURN, "Return");
    press_combo(&conn, &[super_kc], enter_kc);

    wait_until("the terminal probe to record its argv", || {
        sentinel.exists()
    });
    let argv = std::fs::read_to_string(&sentinel)
        .unwrap_or_else(|err| panic!("read {}: {err}", sentinel.display()));
    assert_eq!(
        argv, "",
        "terminal probe must receive no arguments beyond argv[0] (got {argv:?})"
    );
    assert_eq!(
        locks_on(&conn) & LOCK_BITS,
        LOCK_BITS,
        "the press must not clear the lock modifiers (no --clearmodifiers)"
    );

    wm.kill();
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&sentinel);
}

#[test]
#[ignore]
fn ctrl_space_spawns_the_launcher_probe_and_claims_16_bindings() {
    // ALA-2/D4 end-to-end: Ctrl+Space (the default launcher combo) must make
    // the WM spawn the configured launcher with the args passed VERBATIM
    // (no shell, no interpretation — the probe records its full argument
    // list), and the WM's claim log must prove the 16-binding lock-variant
    // grab table reached the server (KBR-3's "16 bindings" line on stderr).
    let display = display_name();
    let probes = probes_dir();
    let config = std::env::temp_dir().join(format!(
        "tessera-e2e-launcher-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &config,
        "[general]\nlauncher = [\"probe_launcher\", \"-show\", \"drun\"]\n",
    )
    .expect("write the launcher-probe config");
    let sentinel = sentinel_path("launcher");
    let _ = std::fs::remove_file(&sentinel);
    let path = path_with_probes(&probes);

    let mut wm = WmChild::spawn_full(
        &display,
        Some(&config),
        &[
            ("PATH", &path),
            ("TESSERA_TEST_SENTINEL", sentinel.to_str().unwrap()),
        ],
        true,
    );
    let conn = connect(&display);
    let root = root_of(&conn);
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    let ctrl_kc = keycode_for(&conn, CONTROL_L, "Control_L");
    let space_kc = keycode_for(&conn, KEY_SPACE, "space");
    press_combo(&conn, &[ctrl_kc], space_kc);

    wait_until("the launcher probe to record its argv", || {
        sentinel.exists()
    });
    let argv = std::fs::read_to_string(&sentinel)
        .unwrap_or_else(|err| panic!("read {}: {err}", sentinel.display()));
    assert_eq!(
        argv,
        "-show\ndrun\n",
        "launcher probe must receive the configured argv verbatim (got {argv:?})"
    );

    let stderr = wm.stop_and_read_stderr();
    assert!(
        stderr.contains("16 bindings"),
        "claim log must report the 16-binding grab table, got stderr: {stderr:?}"
    );

    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&sentinel);
}

#[test]
#[ignore]
fn configured_rofi_launcher_spawns_or_skips_when_missing() {
    // ALA-3 at the real X server: the DEFAULT launcher (`rofi -show drun`)
    // must actually spawn rofi on Ctrl+Space when rofi is on PATH. When rofi
    // is not installed the test records the honest skip and returns — the
    // path-missing behavior itself is unit-proven in tessera-core
    // (launcher_failure_is_logged_and_the_loop_survives).
    if !probe_on_path("rofi") {
        eprintln!("skip: rofi is not on PATH");
        return;
    }
    let display = display_name();
    let mut wm = WmChild::spawn(&display);
    let conn = connect(&display);
    let root = root_of(&conn);
    let wm_s0 = intern(&conn, b"WM_S0");
    wait_until("the WM to own WM_S0", || {
        conn.get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == root
    });
    assert!(wm.alive(), "the WM must keep running after claiming WM_S0");

    let before = process_pids("rofi");
    let ctrl_kc = keycode_for(&conn, CONTROL_L, "Control_L");
    let space_kc = keycode_for(&conn, KEY_SPACE, "space");
    press_combo(&conn, &[ctrl_kc], space_kc);

    wait_until("rofi to appear", || {
        process_pids("rofi").iter().any(|pid| !before.contains(pid))
    });
    for pid in process_pids("rofi") {
        if !before.contains(&pid) {
            let _ = ProcCommand::new("kill").arg(pid.to_string()).status();
        }
    }

    wm.kill();
}
