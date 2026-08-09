//! Tessera binary: a minimal X11 tiling window manager.
//!
//! The binary assembles the pure core (`tessera-core`) and the x11rb display
//! layer (`tessera-x11`), then runs the core loop. Startup aborts with a
//! non-zero exit when the display is unreachable (REQ-x11-001) or the WM_S0
//! claim fails (REQ-x11-002).

mod app;
mod bar;

fn main() {
    let args = match app::CliArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("tessera: {msg}");
            eprintln!("usage: tessera [--config <path>] [--display <name>] [--version]");
            std::process::exit(2);
        }
    };
    // VER-1: `--version` prints and exits 0 BEFORE any config or display
    // work — the flag short-circuits in the parser, so this is the only
    // check needed here (design D5).
    if args.version {
        println!("tessera {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    if let Err(err) = app::run(&args) {
        eprintln!("tessera: {err}");
        std::process::exit(1);
    }
}
