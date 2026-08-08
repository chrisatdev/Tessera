// Reproduction harness for "close one of N terminal windows -> freeze?".
// Lives OUTSIDE the tessera workspace members so it doesn't disturb build.

use std::path::PathBuf;
use std::process::{Child, Command as P, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt, CreateWindowAux, GetGeometryReply, MapState, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

const WAIT: Duration = Duration::from_secs(6);

fn wm_binary() -> PathBuf {
    PathBuf::from("/home/chris/dev/Tessera/target/debug/tessera")
}

fn connect(d: &str) -> RustConnection {
    for _ in 0..5 {
        if let Ok((c, _)) = x11rb::connect(Some(d)) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("cannot connect to {d}");
}

fn root(c: &RustConnection) -> Window {
    c.setup().roots[0].root
}

fn parent(c: &RustConnection, w: Window) -> Window {
    c.query_tree(w).unwrap().reply().unwrap().parent
}

fn geom(c: &RustConnection, w: Window) -> GetGeometryReply {
    c.get_geometry(w).unwrap().reply().unwrap()
}

fn map_state(c: &RustConnection, w: Window) -> MapState {
    c.get_window_attributes(w).unwrap().reply().unwrap().map_state
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let dl = Instant::now() + WAIT;
    while Instant::now() < dl {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn map_client(c: &RustConnection, root: Window) -> Window {
    let s = &c.setup().roots[0];
    let w = c.generate_id().unwrap();
    c.create_window(
        s.root_depth,
        w,
        root,
        0,
        0,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        s.root_visual,
        &CreateWindowAux::default().background_pixel(0x0040_4040),
    )
    .unwrap()
    .check()
    .unwrap();
    c.map_window(w).unwrap().check().unwrap();
    c.flush().unwrap();
    w
}

fn destroy(c: &RustConnection, w: Window) {
    c.destroy_window(w).unwrap().check().unwrap();
    c.flush().unwrap();
}

struct Wm(Child);
impl Drop for Wm {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() {
    let display = ":99";
    let bin = wm_binary();
    assert!(bin.exists(), "build first: cargo build --workspace");

    // Wait no WM owns WM_S0 (leftover from a killed WM).
    let probe = connect(display);
    let wm_s0 = probe
        .intern_atom(false, b"WM_S0")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    wait("WM_S0 released", || {
        probe
            .get_selection_owner(wm_s0)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == 0
    });
    drop(probe);

    let mut wm = Wm(
        P::new(&bin)
            .arg("--display")
            .arg(display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let c = connect(display);
    let r = root(&c);
    let wm_s0_2 = c
        .intern_atom(false, b"WM_S0")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    wait("WM owns WM_S0", || {
        c.get_selection_owner(wm_s0_2)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            == r
    });
    println!("[repro] WM running and owns WM_S0");

    // Map 3 clients. MRU-first: C is focused (master), B and A in stack.
    let a = map_client(&c, r);
    let b = map_client(&c, r);
    let cc = map_client(&c, r);

    wait("3 clients reparented into frames", || {
        parent(&c, a) != r && parent(&c, b) != r && parent(&c, cc) != r
            && parent(&c, a) != parent(&c, b)
            && parent(&c, b) != parent(&c, cc)
    });
    let fa = parent(&c, a);
    let fb = parent(&c, b);
    let fc = parent(&c, cc);
    println!("[repro] frames: a={fa:#x} b={fb:#x} c={fc:#x}");

    // Capture pre-close geometry + map-state of all three frames.
    let g_a_pre = geom(&c, fa);
    let g_b_pre = geom(&c, fb);
    let g_c_pre = geom(&c, fc);
    let s_a_pre = map_state(&c, fa);
    let s_b_pre = map_state(&c, fb);
    let s_c_pre = map_state(&c, fc);
    println!(
        "[repro] PRE-close: a={g_a_pre:?} (state {s_a_pre:?}), b={g_b_pre:?} (state {s_b_pre:?}), c={g_c_pre:?} (state {s_c_pre:?})"
    );

    // Sanity: all three should be VIEWABLE after mapping.
    assert_eq!(s_a_pre, MapState::VIEWABLE);
    assert_eq!(s_b_pre, MapState::VIEWABLE);
    assert_eq!(s_c_pre, MapState::VIEWABLE);

    // CLOSE the middle client (b). Then we poll: does the WM retile the two
    // survivors (a, c) to master-stack, AND stay alive? If "freeze" is real,
    // this either times out (the WM is stuck) or the survivors keep the
    // old 3-up geometry (the WM never re-tiled).
    println!("[repro] destroying middle client b (client window {b:#x})");
    destroy(&c, b);
    println!("[repro] destroyed; waiting for retile of survivors a,c");

    // We expect exactly 2 frames VIEWABLE after retile, and the two survivors
    // reconfigured to master+stack (one master-wide-left, one stack-right).
    let start = Instant::now();
    let mut retiled = false;
    let dl = start + WAIT;
    while Instant::now() < dl {
        let sa = map_state(&c, fa);
        let sc = map_state(&c, fc);
        let ga = geom(&c, fa);
        let gc = geom(&c, fc);
        // After retile: a and c VIEWABLE, b NOT VIEWABLE, and the two
        // survivors arrange side-by-side without overlap (master-stack).
        if sa == MapState::VIEWABLE
            && sc == MapState::VIEWABLE
            && map_state(&c, fb) != MapState::VIEWABLE
        {
            let a_x2 = i32::from(ga.x) + i32::from(ga.width);
            let c_x = i32::from(gc.x);
            if a_x2 <= c_x || i32::from(gc.x) + i32::from(gc.width) <= i32::from(ga.x) {
                retiled = true;
                println!(
                    "[repro] retiled in {:?}: a={ga:?} c={gc:?}; b frame now {:?}",
                    start.elapsed(),
                    map_state(&c, fb)
                );
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let alive = matches!(wm.0.try_wait(), Ok(None));
    println!("[repro] WM alive after close: {alive}");
    println!("[repro] retiled: {retiled}");

    if retiled && alive {
        println!("[repro] *** PASS: closing the middle window did NOT freeze the WM; retile happened.");
    } else if !retiled && alive {
        let ga = geom(&c, fa);
        let gc = geom(&c, fc);
        let sb = map_state(&c, fb);
        println!(
            "[repro] *** SOFT-FAIL: WM alive but no retile in {WAIT:?}. final: a={ga:?} c={gc:?} b state={sb:?}"
        );
    } else {
        println!("[repro] *** FAIL: WM frozen/dead after close.");
    }
}
