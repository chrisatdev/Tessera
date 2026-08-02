//! Binary wiring (T20): assemble the X display layer, the core [`App`] and
//! the bar placeholder, then run the loop (REQ-x11-001/002, SC-x11-01).

use std::path::PathBuf;

use tessera_core::DErr;

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
    // T20: assemble X11Display + EventBus + App. The bar placeholder (T19)
    // is referenced so its module stays live until the wiring lands.
    let _ = args.config_path.as_deref();
    let _ = args.display.as_deref();
    let _ = std::mem::size_of::<crate::bar::Bar>();
    todo!("T20: assemble X11Display + EventBus + App")
}
