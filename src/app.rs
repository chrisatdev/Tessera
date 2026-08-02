//! Binary wiring (T20): assemble the X display layer, the core [`App`] and
//! the bar placeholder, then run the loop (REQ-x11-001/002, SC-x11-01).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessera_core::{App, Config, DErr, DisplayServer};
use tessera_x11::X11Display;

use crate::bar::Bar;

/// Minimal CLI: `tessera [--config <path>] [--display <name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// Optional TOML config file path (`--config`).
    pub config_path: Option<PathBuf>,
    /// Optional X display name (`--display`); `None` means `$DISPLAY`.
    pub display: Option<String>,
}

impl CliArgs {
    /// Parses the CLI arguments (`--config <path>`, `--display <name>`).
    /// Unknown flags and missing values are rejected.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<CliArgs, String> {
        let mut args = args.into_iter();
        let mut config_path = None;
        let mut display = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    let value = args.next().ok_or("--config needs a config file path")?;
                    config_path = Some(PathBuf::from(value));
                }
                "--display" => {
                    let value = args.next().ok_or("--display needs a display name")?;
                    display = Some(value);
                }
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        Ok(CliArgs {
            config_path,
            display,
        })
    }
}

/// Loads the config from `path`, or the defaults when no path is given.
///
/// A config file that cannot be read or parsed aborts startup: at boot there
/// is no previous config to fall back on (D6's keep-old-on-error applies to
/// reloads, not to the first load).
pub fn load_config(path: Option<&Path>) -> Result<Config, String> {
    match path {
        Some(path) => Config::load(path).map_err(|err| format!("cannot load config: {err:?}")),
        None => Ok(Config::default()),
    }
}

/// Wires core + x11 (+ the bar placeholder) and runs the loop (SC-x11-01).
///
/// Startup aborts with `Err` when the display is unreachable (REQ-x11-001,
/// SC-x11-02) or the WM_S0 claim fails (REQ-x11-002, SC-x11-04); [`main`]
/// maps that to a non-zero exit. The bar consumes the WmState watch
/// (REQ-bus-004, T19); its live per-iteration rendering lands with the real
/// bar (later change).
pub fn run(args: &CliArgs) -> Result<(), DErr> {
    // Config: explicit file, or defaults. A bad file aborts startup (there is
    // no previous config at boot to keep — D6 covers reloads only).
    let config = Arc::new(load_config(args.config_path.as_deref()).map_err(DErr::X)?);
    // The X layer needs the real keybindings (and border) BEFORE claim_wm
    // grabs them (U4-B note); defaults match anyway, but an explicit config
    // must win.
    let mut x11 = X11Display::new(args.display.as_deref());
    x11.set_config(Arc::clone(&config));
    // REQ-x11-001: an unreachable display aborts startup (SC-x11-02).
    x11.connect()?;
    // REQ-x11-002: another WM owning WM_S0 aborts startup (SC-x11-04).
    x11.claim_wm()?;
    // T21: the tiling area is the real screen geometry, queried from the root
    // window after connect (replaces the hardcoded 1920x1080 const).
    let area = x11.root_size()?;
    // SC-x11-01: connected and claimed -> the core loop runs.
    let mut app = App::new(Box::new(x11), config, area);
    // T19: the bar subscribes to the WmState watch and catches up to the
    // complete current snapshot (SC-bus-04). The watch is live during run();
    // refresh() after the loop reads the final snapshot it carried.
    let mut bar = Bar::new(app.bus().state_rx());
    app.run();
    bar.refresh();
    eprintln!("tessera: bar: {}", bar.render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Writes `contents` to a uniquely named temp config file and returns
    /// its path (the caller removes it after the assertion).
    fn config_path(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}.toml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn load_config_reads_the_file_at_the_given_path() {
        let path = config_path(
            "tessera-config-ok",
            "[general]\nborder_width = 7\nterminal = \"foot\"\n",
        );
        let cfg = load_config(Some(&path)).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.general.border_width, 7);
        assert_eq!(cfg.general.terminal, "foot");
        assert_eq!(cfg.general.gaps, 0); // unset keeps the default
    }

    #[test]
    fn load_config_falls_back_to_defaults_without_a_path() {
        let cfg = load_config(None).unwrap();
        assert_eq!(cfg.general.border_width, 2);
        assert_eq!(cfg.general.terminal, "alacritty");
    }

    #[test]
    fn load_config_aborts_on_a_missing_file() {
        let err =
            load_config(Some(Path::new("/nonexistent/tessera-does-not-exist.toml"))).unwrap_err();
        assert!(err.contains("config"), "expected a config error, got {err}");
    }

    #[test]
    fn load_config_aborts_on_a_malformed_file() {
        let path = config_path("tessera-config-bad", "general = { border_width = ");
        let err = load_config(Some(&path)).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("config"), "expected a config error, got {err}");
    }

    #[test]
    fn parse_accepts_no_args_and_full_args() {
        assert_eq!(
            CliArgs::parse(std::iter::empty::<String>()).unwrap(),
            CliArgs {
                config_path: None,
                display: None,
            }
        );
        assert_eq!(
            CliArgs::parse(
                [
                    "--config".to_string(),
                    "/tmp/tessera.toml".to_string(),
                    "--display".to_string(),
                    ":9".to_string(),
                ]
                .into_iter()
            )
            .unwrap(),
            CliArgs {
                config_path: Some("/tmp/tessera.toml".into()),
                display: Some(":9".into()),
            }
        );
    }

    #[test]
    fn parse_rejects_unknown_flags_and_missing_values() {
        assert!(CliArgs::parse(["--bogus".to_string()]).is_err());
        assert!(CliArgs::parse(["--config".to_string()]).is_err()); // value missing
    }
}
