//! Tessera binary: a minimal X11 tiling window manager.
//!
//! The binary assembles the pure core (`tessera-core`) and the x11rb display
//! layer (`tessera-x11`), then runs the core loop. Startup aborts with a
//! non-zero exit when the display is unreachable (REQ-x11-001) or the WM_S0
//! claim fails (REQ-x11-002).

mod app;
mod bar;

fn main() {
    let args = app::CliArgs::default();
    if let Err(err) = app::run(&args) {
        eprintln!("tessera: {err}");
        std::process::exit(1);
    }
}
