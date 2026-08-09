//! Binary wiring (T20): assemble the X display layer, the core [`App`] and
//! the bar placeholder, then run the loop (REQ-x11-001/002, SC-x11-01).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use tessera_core::{App, Config, DErr, DisplayServer, Theme};
use tessera_x11::X11Display;
use tessera_x11::bar_renderer::tiling_area;

use crate::bar::Bar;

/// Minimal CLI: `tessera [--config <path>] [--display <name>] [--version]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// Optional TOML config file path (`--config`).
    pub config_path: Option<PathBuf>,
    /// Optional X display name (`--display`); `None` means `$DISPLAY`.
    pub display: Option<String>,
    /// `--version`: print the version to stdout and exit 0 before any
    /// config or display work (VER-1). The parse loop short-circuits on it,
    /// so no argument after it is ever validated (design D5).
    pub version: bool,
}

impl CliArgs {
    /// Parses the CLI arguments (`--config <path>`, `--display <name>`).
    /// Unknown flags and missing values are rejected; `--version` wins over
    /// everything after it (VER-1) — once seen, parsing stops and returns
    /// immediately.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<CliArgs, String> {
        let mut args = args.into_iter();
        let mut config_path = None;
        let mut display = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--version" => {
                    // VER-1: the winning flag — skip all later validation
                    // (a trailing `--config` with a missing value must not
                    // turn a version query into an error, design D5).
                    return Ok(CliArgs {
                        version: true,
                        ..Default::default()
                    });
                }
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
            version: false,
        })
    }
}

/// Default configuration written to the auto-detected path on first run
/// (CFG-3, design D6/D7): every value IS a [`Config::default()`] value, so
/// `Config::parse(TEMPLATE) == Config::default()` is locked by
/// `template_round_trips_to_default` (CFG-6). Hand-written, commented, and
/// deliberately WITHOUT `Serialize` derives — the file is never re-emitted
/// after editing, so the comments survive.
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# Tessera default configuration — created on first run. Edit and restart (or SIGHUP) to apply.
# mods = X11 modifier mask: Shift=1, Control=4, Mod1/Alt=8, Mod4/Super=64. key = keysym in decimal
# (Return=65293, space=32, j=106, k=107, q=113, 1..9=49..57, 0=48).

[general]
border_width = 2
gaps = 0
terminal = "alacritty"
launcher = ["rofi", "-show", "drun"]

[keybindings.terminal]     # Super+Enter: open a terminal
mods = 64
key = 65293

[keybindings.focus_next]   # Super+J
mods = 64
key = 106

[keybindings.focus_prev]   # Super+K
mods = 64
key = 107

[keybindings.close]        # Super+Q
mods = 64
key = 113

[keybindings.toggle_layout]  # Super+Space
mods = 64
key = 32

[keybindings.launcher]     # Ctrl+Space: run [general] launcher
mods = 4
key = 32

# Workspace 1..9, Super+0 = workspace 10. To rebind off Super (e.g. in a VM whose
# host captures Super — see README "Super in VM guests"): change the mods of a binding to 4 (Control).
[[keybindings.workspace]]  # workspace-1 (Super+1)
mods = 64
key = 49
[[keybindings.workspace]]  # workspace-2 (Super+2)
mods = 64
key = 50
[[keybindings.workspace]]  # workspace-3 (Super+3)
mods = 64
key = 51
[[keybindings.workspace]]  # workspace-4 (Super+4)
mods = 64
key = 52
[[keybindings.workspace]]  # workspace-5 (Super+5)
mods = 64
key = 53
[[keybindings.workspace]]  # workspace-6 (Super+6)
mods = 64
key = 54
[[keybindings.workspace]]  # workspace-7 (Super+7)
mods = 64
key = 55
[[keybindings.workspace]]  # workspace-8 (Super+8)
mods = 64
key = 56
[[keybindings.workspace]]  # workspace-9 (Super+9)
mods = 64
key = 57
[[keybindings.workspace]]  # workspace-10 (Super+0)
mods = 64
key = 48
"#;

/// Resolves the auto-detected config path (CFG-1, design D6): an absolute
/// `$XDG_CONFIG_HOME` wins; an empty, relative or absent XDG falls back to
/// `$HOME/.config`; with no usable `$HOME` there is no candidate at all
/// (the caller warns and uses defaults). Pure — takes the env values as
/// parameters so the whole table is testable without touching the process
/// environment.
fn auto_config_path(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let xdg = xdg.filter(|v| !v.is_empty() && Path::new(v).is_absolute());
    match xdg {
        Some(xdg) => Some(PathBuf::from(xdg).join("Tessera").join("tessera.toml")),
        None => {
            let home = home.filter(|v| !v.is_empty())?;
            Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("Tessera")
                    .join("tessera.toml"),
            )
        }
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

/// The env wrapper around [`auto_config_path`] (CFG-1, design D6): reads
/// `$XDG_CONFIG_HOME` and `$HOME` and resolves the auto-detected candidate.
fn resolve_auto_config_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(|v| v.to_string_lossy().into_owned());
    let home = std::env::var_os("HOME").map(|v| v.to_string_lossy().into_owned());
    auto_config_path(xdg.as_deref(), home.as_deref())
}

/// Writes the default template at `path`, creating parent directories.
fn create_default_config(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
}

/// Bootstraps the auto-detected config path (design D6) — the LENIENT path:
/// a missing file is created from the template and logged (CFG-3); an
/// existing file is loaded silently (CFG-3); a malformed existing file
/// (CFG-5) or a failed creation (CFG-4) warns and falls back to defaults.
/// Never aborts — only an explicit `--config` stays strict (CFG-2).
fn bootstrap_config(path: &Path) -> Config {
    // CFG-3: an existing NON-EMPTY file loads silently. A zero-byte file is
    // treated as absent: it parses as empty TOML (all-defaults) yet carries
    // no template, so the user would never get a commented config to edit.
    // Empty files are almost always editor/touch accidents, not deliberate
    // configs — regenerate the template instead (empty-config bugfix).
    let non_empty = std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);
    if non_empty {
        return match Config::load(path) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("tessera: warning: cannot parse {path:?} ({err:?}); using defaults");
                Config::default()
            }
        };
    }
    if let Err(err) = create_default_config(path) {
        eprintln!(
            "tessera: warning: cannot create default config at {path:?} ({err:?}); using defaults"
        );
        return Config::default();
    }
    eprintln!("tessera: created default config at {path:?}");
    match Config::load(path) {
        Ok(config) => config,
        // Unreachable in practice: the template is locked to the defaults
        // by template_round_trips_to_default (CFG-6).
        Err(err) => {
            eprintln!("tessera: warning: cannot parse {path:?} ({err:?}); using defaults");
            Config::default()
        }
    }
}

/// Resolves the startup theme from `config.general.theme` (REQ-thm-003).
///
/// `None` -> the embedded ayu_dark palette, and no theme file is read
/// (SC-thm-06). `Some(path)` -> the file at `path` is loaded and its values
/// take effect (SC-thm-07). A missing or unparseable file NEVER aborts
/// startup: the WM warns and falls back to the embedded ayu_dark palette
/// (ratified decision #1281 — overrides SC-thm-08/D6 abort semantics).
pub fn resolve_theme(config: &Config) -> Theme {
    match config.general.theme.as_deref() {
        None => Theme::default(),
        Some(path) => match Theme::load(Path::new(path)) {
            Ok(theme) => theme,
            Err(err) => {
                eprintln!(
                    "tessera: warning: cannot load theme {path:?} ({err:?}); \
                     falling back to the embedded ayu_dark theme"
                );
                Theme::default()
            }
        },
    }
}

/// Wires core + x11 + the bar and runs the loop (SC-x11-01).
///
/// Startup aborts with `Err` when the display is unreachable (REQ-x11-001,
/// SC-x11-02) or the WM_S0 claim fails (REQ-x11-002, SC-x11-04); [`main`]
/// maps that to a non-zero exit. The bar consumes the WmState watch
/// (REQ-bus-004, T19): its renderer lives on a dedicated thread (task 2.7)
/// drawing once per recompute (design D4).
pub fn run(args: &CliArgs) -> Result<(), DErr> {
    // Config precedence (CFG-1/2, design D6): an explicit `--config` is
    // strict and never auto-created (missing/malformed aborts, unchanged);
    // the auto-detected path is lenient — first run creates the commented
    // template, a malformed file falls back to defaults; with neither
    // $HOME nor $XDG_CONFIG_HOME there is no candidate at all, so defaults
    // are used with a warning.
    let config = Arc::new(match args.config_path.as_deref() {
        Some(path) => load_config(Some(path)).map_err(DErr::X)?,
        None => match resolve_auto_config_path() {
            Some(path) => bootstrap_config(&path),
            None => {
                eprintln!("tessera: warning: $HOME and $XDG_CONFIG_HOME unset; using defaults");
                Config::default()
            }
        },
    });
    // REQ-thm-003: the theme is resolved ONCE at startup (T8). A custom
    // `theme = "path"` that is missing or unparseable falls back to the
    // embedded ayu_dark with a warning — startup never aborts (T9, ratified
    // decision #1281, overrides SC-thm-08).
    let theme = Arc::new(resolve_theme(&config));
    // The X layer needs the real keybindings (and border) BEFORE claim_wm
    // grabs them (U4-B note); defaults match anyway, but an explicit config
    // must win.
    let mut x11 = X11Display::new(args.display.as_deref());
    x11.set_config(Arc::clone(&config));
    // The X layer paints the resolved theme's borders (T9, D4): set_theme
    // must run before claim_wm so managed frames use the right pixels.
    x11.set_theme(Arc::clone(&theme));
    // REQ-x11-001: an unreachable display aborts startup (SC-x11-02).
    x11.connect()?;
    // REQ-x11-002: another WM owning WM_S0 aborts startup (SC-x11-04).
    x11.claim_wm()?;
    // The bar's monitor (design D10: primary RandR output, else first
    // connected, else full screen with a once-only warning) and the tiling
    // area it leaves once the bar is subtracted (task 2.6).
    let bar_area = x11.bar_area()?;
    let area = tiling_area(bar_area, &config.bar);
    // The bar thread shares the X connection (task 2.7); grab the pieces
    // before `x11` is moved into the core App.
    let conn = x11.connection()?;
    let root = x11.root();
    let depth = x11.depth();
    let visual = x11.visual();
    let bar_config = config.bar.clone();
    // SC-x11-01: connected and claimed -> the core loop runs.
    // T4 seam (D4): the resolved theme rides through App::new into the
    // WmState watch; the X layer already holds the same Arc (dual injection).
    let mut app = App::new(Box::new(x11), config, theme, area);
    // T19/task 2.7: the bar subscribes to the WmState watch and catches up to
    // the complete current snapshot (SC-bus-04), then renders it on its own
    // thread. `Rc<RefCell<_>>` keeps the bar alive after the hook below moves
    // a clone into the App; the worker thread joins when the last clone drops.
    let bar = Rc::new(RefCell::new(Bar::spawn(
        conn,
        root,
        depth,
        visual,
        bar_area,
        &bar_config,
        app.bus().state_rx(),
    )?));
    // D4: draw exactly once per recompute — never on idle event polling, so
    // an idle WM issues no X traffic from the bar. `recompute()` -> the hook
    // (fires after `publish_state`, so the snapshot is fresh).
    let draw_bar = Rc::clone(&bar);
    let snapshot_rx = app.bus().state_rx();
    app.set_on_recompute(Box::new(move || {
        if let Err(err) = draw_bar.borrow_mut().draw(&snapshot_rx.borrow()) {
            eprintln!("tessera: {err}");
        }
    }));
    app.run();
    // The watch is live during run(); refresh() after the loop reads the
    // final snapshot it carried.
    bar.borrow_mut().refresh();
    eprintln!("tessera: bar: {}", bar.borrow().render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;

    /// Serializes env-mutating tests (design D8): `std::env::set_var` /
    /// `remove_var` are `unsafe` in edition 2024 precisely because
    /// concurrent mutation is a data race; holding this mutex for the whole
    /// read-modify-restore window makes the `unsafe` blocks sound.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Sets `key` to `value` for the duration of `f`, then restores the
    /// previous value (or removes the var when it was unset). The ENV_MUTEX
    /// guard is held across the whole call, so no two env-mutating tests can
    /// interleave (D8).
    fn with_env<K: AsRef<OsStr>, V: AsRef<OsStr>, R>(key: K, value: V, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let key = key.as_ref();
        let previous = std::env::var_os(key);
        // SAFETY: the ENV_MUTEX guard serializes every mutation across test
        // threads, so no other code can read or write this variable while we
        // hold the lock (design D8).
        unsafe { std::env::set_var(key, value) };
        let result = f();
        match previous {
            Some(previous) => unsafe { std::env::set_var(key, previous) },
            None => unsafe { std::env::remove_var(key) },
        }
        result
    }

    /// Writes `contents` to a uniquely named temp config file and returns
    /// its path (the caller removes it after the assertion).
    fn config_path(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}.toml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Writes `contents` to a uniquely named temp theme file and returns
    /// its path (the caller removes it after the assertion).
    fn theme_path(name: &str, contents: &str) -> std::path::PathBuf {
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
                version: false,
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
                version: false,
            }
        );
    }

    #[test]
    fn parse_version_short_circuits() {
        // VER-1: `--version` wins — once seen, no other argument validation
        // runs (a trailing `--config` with a missing value, or any later
        // unknown flag, must not error). VER-2: without `--version` first,
        // unknown flags still fail.
        for args in [
            vec!["--version".to_string()],
            vec!["--version".to_string(), "--config".to_string()],
            vec!["--version".to_string(), "--bogus".to_string()],
        ] {
            let parsed = CliArgs::parse(args).expect("--version must short-circuit");
            assert!(
                parsed.version,
                "--version must set version: true (got {parsed:?})"
            );
            assert_eq!(parsed.config_path, None);
            assert_eq!(parsed.display, None);
        }
        assert!(
            CliArgs::parse(["--bogus".to_string()].into_iter()).is_err(),
            "an unknown flag without a preceding --version must still error"
        );
    }

    #[test]
    fn parse_rejects_unknown_flags_and_missing_values() {
        assert!(CliArgs::parse(["--bogus".to_string()]).is_err());
        assert!(CliArgs::parse(["--config".to_string()]).is_err()); // value missing
    }

    #[test]
    fn auto_config_path_prefers_absolute_xdg() {
        // CFG-1 scenario "XDG_CONFIG_HOME set": an absolute XDG wins.
        assert_eq!(
            auto_config_path(Some("/tmp/cfg"), Some("/home/u")),
            Some(PathBuf::from("/tmp/cfg/Tessera/tessera.toml"))
        );
    }

    #[test]
    fn auto_config_path_falls_back_to_home() {
        // CFG-1 scenario "XDG empty or relative": empty or relative XDG (or
        // none at all) resolves under $HOME/.config instead.
        let want = Some(PathBuf::from("/home/u/.config/Tessera/tessera.toml"));
        for xdg in [Some(""), Some("rel/cfg"), None] {
            assert_eq!(
                auto_config_path(xdg, Some("/home/u")),
                want,
                "xdg={xdg:?} must fall back to $HOME/.config"
            );
        }
    }

    #[test]
    fn auto_config_path_returns_none_without_home() {
        // CFG-1 scenario "HOME and XDG unset": no HOME, no candidate — the
        // caller (run()) warns and uses defaults. An empty HOME is treated
        // as unset: a relative $HOME would resolve to a cwd-relative path.
        assert_eq!(auto_config_path(None, None), None);
        assert_eq!(auto_config_path(Some(""), None), None);
        assert_eq!(auto_config_path(None, Some("")), None);
    }

    #[test]
    fn bootstrap_config_first_run_creates_the_template_and_loads_defaults() {
        // CFG-3 scenario "First run": a missing auto-detected file is
        // created verbatim from the template (dirs included) and loads to
        // Config::default().
        let dir =
            std::env::temp_dir().join(format!("tessera-bootstrap-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        with_env("XDG_CONFIG_HOME", dir.to_str().unwrap(), || {
            let path = resolve_auto_config_path().expect("xdg set -> a candidate");
            let config = bootstrap_config(&path);
            let raw = std::fs::read_to_string(&path).unwrap_or_else(|err| {
                panic!("template must be written at {}: {err}", path.display())
            });
            assert_eq!(
                raw, DEFAULT_CONFIG_TEMPLATE,
                "the written file must be the template"
            );
            assert_eq!(
                config,
                Config::default(),
                "the template must load to the defaults"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_config_regenerates_template_on_an_empty_existing_file() {
        // Empty-config bugfix: a zero-byte file is treated as absent, so the
        // commented template is written (it parses as all-defaults and would
        // otherwise leave the user with an empty config nobody can edit).
        let dir =
            std::env::temp_dir().join(format!("tessera-bootstrap-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Tessera").join("tessera.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let config = bootstrap_config(&path);
        assert_eq!(config, Config::default());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw, DEFAULT_CONFIG_TEMPLATE,
            "an empty existing file must be regenerated to the template"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_config_loads_an_existing_file_without_rewriting() {
        // CFG-3 scenario "Existing file loads": a valid file is loaded, and
        // never touched (no creation, no rewrite to the template).
        let dir =
            std::env::temp_dir().join(format!("tessera-bootstrap-existing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Tessera").join("tessera.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[general]\nborder_width = 7\nterminal = \"foot\"\n").unwrap();
        let config = bootstrap_config(&path);
        assert_eq!(config.general.border_width, 7);
        assert_eq!(config.general.terminal, "foot");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("border_width = 7"),
            "the existing file must not be rewritten to the template"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_config_warns_and_uses_defaults_on_a_malformed_auto_config() {
        // CFG-5: a malformed AUTO-detected file is lenient — defaults, and
        // the garbage file is left untouched (only the log differs).
        let dir = std::env::temp_dir().join(format!(
            "tessera-bootstrap-malformed-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Tessera").join("tessera.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not [valid toml").unwrap();
        let config = bootstrap_config(&path);
        assert_eq!(config, Config::default());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw, "not [valid toml",
            "a malformed file must not be overwritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bootstrap_config_warns_and_uses_defaults_when_creation_fails() {
        // CFG-4: when the parent directory cannot be created (here: the XDG
        // parent is a FILE), creation failure is never fatal — defaults.
        let dir =
            std::env::temp_dir().join(format!("tessera-bootstrap-blocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("xdg");
        std::fs::write(&blocker, "i am a file, not a directory").unwrap();
        with_env("XDG_CONFIG_HOME", blocker.to_str().unwrap(), || {
            let path = resolve_auto_config_path().expect("xdg set -> a candidate");
            let config = bootstrap_config(&path);
            assert_eq!(config, Config::default());
            assert!(
                !path.exists(),
                "a failed create must not leave a file behind"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_round_trips_to_default() {
        // CFG-6: the hand-written commented template (D6/D7) must parse
        // EXACTLY to Config::default() — [general] + the six nested
        // [keybindings.<name>] tables + the ten [[keybindings.workspace]]
        // array-of-tables blocks, all carrying the default values.
        let parsed = Config::parse(DEFAULT_CONFIG_TEMPLATE)
            .expect("the default template must parse as strict TOML");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn resolve_theme_uses_embedded_ayu_dark_without_a_theme_path() {
        // SC-thm-06: config without `theme` -> the embedded ayu_dark is used
        // and NO file is read (the None branch never touches the filesystem).
        let cfg = Config::default();
        assert_eq!(cfg.general.theme, None);
        let theme = resolve_theme(&cfg);
        assert_eq!(theme, Theme::default());
        // The borders derive from the ayu palette (SC-thm-09).
        assert_eq!(
            (
                theme.active_border().r,
                theme.active_border().g,
                theme.active_border().b
            ),
            (0xFF, 0x8F, 0x40),
            "focused border must derive from the ayu accent"
        );
        assert_eq!(
            (
                theme.inactive_border().r,
                theme.inactive_border().g,
                theme.inactive_border().b
            ),
            (0x62, 0x6A, 0x73),
            "unfocused border must derive from the ayu comment"
        );
    }

    #[test]
    fn resolve_theme_loads_the_file_at_the_configured_path() {
        // SC-thm-07: `theme = "path"` -> the file is loaded and its values
        // take effect; fields the file leaves unset stay on ayu_dark.
        let theme_file = theme_path("tessera-theme-ok", "red = \"#F07178\"\n");
        let mut cfg = Config::default();
        cfg.general.theme = Some(theme_file.to_string_lossy().into_owned());
        let theme = resolve_theme(&cfg);
        let _ = std::fs::remove_file(&theme_file);
        assert_eq!(
            (theme.red.r, theme.red.g, theme.red.b),
            (0xF0, 0x71, 0x78),
            "the file's red must override the ayu_dark default"
        );
        assert_eq!(
            theme.background,
            Theme::default().background,
            "an unset field must stay on the ayu_dark default"
        );
        assert_eq!(
            theme.active_border(),
            Theme::default().active_border(),
            "an unset border must derive from the ayu accent"
        );
    }

    #[test]
    fn resolve_theme_falls_back_to_ayu_dark_on_a_missing_theme_file() {
        // Ratified decision #1281 (overrides SC-thm-08/D6): a custom theme
        // file that is missing must NOT abort startup — the WM warns and
        // uses the embedded ayu_dark palette.
        let mut cfg = Config::default();
        cfg.general.theme = Some("/nonexistent/tessera-theme-gone.toml".to_string());
        let theme = resolve_theme(&cfg);
        assert_eq!(theme, Theme::default());
        assert_eq!(
            (theme.red.r, theme.red.g, theme.red.b),
            (0xF2, 0x53, 0x58),
            "the fallback must be the full ayu_dark palette"
        );
    }

    #[test]
    fn resolve_theme_falls_back_to_ayu_dark_on_an_unparseable_theme_file() {
        // Ratified decision #1281: a theme file that cannot be parsed must
        // NOT abort startup either — warn and fall back to ayu_dark (the
        // "parseable file, missing keys" and "file broken" cases are two
        // different leniency levels; both are lenient).
        let theme_file = theme_path("tessera-theme-bad", "not [valid toml");
        let mut cfg = Config::default();
        cfg.general.theme = Some(theme_file.to_string_lossy().into_owned());
        let theme = resolve_theme(&cfg);
        let _ = std::fs::remove_file(&theme_file);
        assert_eq!(theme, Theme::default());
        assert_eq!(
            (
                theme.active_border().r,
                theme.active_border().g,
                theme.active_border().b
            ),
            (0xFF, 0x8F, 0x40),
            "the fallback must keep the ayu derived borders"
        );
    }
}
