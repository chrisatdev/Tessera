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
        let err = load_config(Some(Path::new(
            "/nonexistent/tessera-does-not-exist.toml",
        )))
        .unwrap_err();
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
