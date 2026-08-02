//! Binary wiring (T20): assemble the X display layer, the core [`App`] and
//! the bar placeholder, then run the loop (REQ-x11-001/002, SC-x11-01).

use std::path::PathBuf;

use tessera_core::{DErr, EventBus};

/// Minimal CLI: `tessera [--config <path>] [--display <name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// Optional TOML config file path (`--config`).
    pub config_path: Option<PathBuf>,
    /// Optional X display name (`--display`); `None` means `$DISPLAY`.
    pub display: Option<String>,
}

/// Wires core + x11 and runs the loop (T20).
pub fn run(args: &CliArgs) -> Result<(), DErr> {
    // T20: assemble X11Display + EventBus + App. T19 seam proof: a Bar built
    // from a live bus consumes the WmState watch (REQ-bus-004) — the real
    // wiring replaces this throwaway bus with the App's bus.
    let _ = args.config_path.as_deref();
    let _ = args.display.as_deref();
    let bus = EventBus::new(Default::default());
    let mut bar = crate::bar::Bar::new(bus.state_rx());
    bar.refresh();
    let _ = (bar.latest(), bar.render());
    todo!("T20: assemble X11Display + EventBus + App")
}
