//! TOML configuration with strict fields and safe reload (design D6).

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, de};

use crate::event::KeyCombo;
use crate::theme::Color;

/// Modifier mask for the "Super" (Mod4) key.
///
/// NOTE: X11 modifier bits are Shift=1, Lock=2, Control=4, Mod1=8, Mod2=16,
/// Mod3=32, Mod4=64, Mod5=128 — Mod4 is bit 6, NOT `1 << 3` (that would be
/// Mod1/Alt). A wrong mask silently breaks every Super binding: the grab
/// never matches the real Mod4 event state (caught by the Xvfb E2E).
const MOD_SUPER: u32 = 1 << 6;
/// Modifier mask for the "Control" key (X11 bit 2, `1 << 2`). Ctrl+Space is
/// the default launcher binding; mask 4 is disjoint from Super (64), so the
/// two default sets never collide in grab-variant space.
const MOD_CONTROL: u32 = 1 << 2;
// Keysyms for the default keybindings (X11 keysym table).
const KEY_RETURN: u32 = 0xff0d;
const KEY_J: u32 = 0x006a;
const KEY_K: u32 = 0x006b;
const KEY_Q: u32 = 0x0071;
const KEY_SPACE: u32 = 0x0020;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub keybindings: Keybindings,
    /// Status-bar configuration (`[bar]` table, design D5). Additive: a config
    /// without the table uses `BarConfig::default()` (top edge, visible).
    #[serde(default)]
    pub bar: BarConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    #[serde(default = "default_border_width")]
    pub border_width: u32,
    #[serde(default = "default_gaps")]
    pub gaps: u32,
    #[serde(default = "default_terminal")]
    pub terminal: String,
    /// Launcher program run by the Ctrl+Space keybinding (design D5, ALA-2).
    /// Defaults to `["rofi", "-show", "drun"]`; an explicit empty array is a
    /// parse error — a launcher that silently does nothing is never accepted.
    #[serde(
        default = "default_launcher",
        deserialize_with = "deserialize_launcher"
    )]
    pub launcher: Vec<String>,
    /// Optional path to a `theme.toml` (REQ-thm-003). `None` -> embedded
    /// ayu_dark, no file read; `Some(path)` is resolved at startup.
    #[serde(default)]
    pub theme: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        GeneralConfig {
            border_width: default_border_width(),
            gaps: default_gaps(),
            terminal: default_terminal(),
            launcher: default_launcher(),
            theme: None,
        }
    }
}

fn default_border_width() -> u32 {
    2
}
fn default_gaps() -> u32 {
    0
}
fn default_terminal() -> String {
    "alacritty".to_string()
}
fn default_launcher() -> Vec<String> {
    vec!["rofi".to_string(), "-show".to_string(), "drun".to_string()]
}

/// Screen edge the status bar is drawn on (`[bar] position`).
///
/// Deserializes from the lowercase strings `"top" | "bottom" | "left" |
/// "right"`; any other value is a strict-TOML parse error (spec scenario
/// "Invalid position string").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BarPosition {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// Status-bar configuration (`[bar]` table, design D5/D6).
///
/// Strict-TOML: unknown keys are rejected, matching `GeneralConfig`. Colors
/// reuse the shared `#RRGGBB`-only `Color` deserializer (design OQ4).
/// `thickness` is `None` when omitted, letting the renderer resolve the
/// per-edge default (22px top/bottom, 6px left/right) from `position`; an
/// explicit value is uniform and MUST be in `1..=200` (spec scenario
/// "Thickness bounds").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarConfig {
    #[serde(default)]
    pub position: BarPosition,
    #[serde(default, deserialize_with = "deserialize_thickness")]
    pub thickness: Option<u16>,
    #[serde(default = "default_bar_bg_color")]
    pub bg_color: Color,
    #[serde(default = "default_bar_fg_color")]
    pub fg_color: Color,
    #[serde(default = "default_bar_visible")]
    pub visible: bool,
}

impl Default for BarConfig {
    fn default() -> Self {
        BarConfig {
            position: BarPosition::Top,
            thickness: None,
            bg_color: default_bar_bg_color(),
            fg_color: default_bar_fg_color(),
            visible: default_bar_visible(),
        }
    }
}

fn default_bar_bg_color() -> Color {
    Color {
        r: 0x22,
        g: 0x22,
        b: 0x22,
    }
}
fn default_bar_fg_color() -> Color {
    Color {
        r: 0xEE,
        g: 0xEE,
        b: 0xEE,
    }
}
fn default_bar_visible() -> bool {
    true
}

/// Validates an explicit `[bar] thickness` at parse time.
///
/// A missing field keeps `None` (per-edge default, design D6). A present value
/// out of `1..=200` aborts startup with a config-validation error naming the
/// field (spec scenario "Thickness bounds": `0` and `> 200` are rejected).
fn deserialize_thickness<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<u16>::deserialize(deserializer)?;
    match raw {
        Some(0) => Err(de::Error::custom("bar.thickness must be in 1..=200")),
        Some(n) if n > 200 => Err(de::Error::custom("bar.thickness must be in 1..=200")),
        other => Ok(other),
    }
}

/// Validates an explicit `[general] launcher` at parse time.
///
/// A missing field keeps the rofi default (design D5). A present empty array
/// is a misconfiguration: it would leave Ctrl+Space silently inert, the same
/// class of failure this change exists to fix, so it aborts startup with a
/// config-validation error naming the field.
fn deserialize_launcher<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    if raw.is_empty() {
        return Err(de::Error::custom("general.launcher must not be empty"));
    }
    Ok(raw)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybindings {
    #[serde(default = "default_terminal_combo")]
    pub terminal: KeyCombo,
    #[serde(default = "default_focus_next")]
    pub focus_next: KeyCombo,
    #[serde(default = "default_focus_prev")]
    pub focus_prev: KeyCombo,
    #[serde(default = "default_close")]
    pub close: KeyCombo,
    #[serde(default = "default_workspace")]
    pub workspace: [KeyCombo; 10],
    #[serde(default = "default_toggle_layout")]
    pub toggle_layout: KeyCombo,
    /// Launcher keybinding (design D5, ALA-2): Ctrl+Space, the 16th default
    /// binding. Ctrl (bit 2) is disjoint from Super (bit 6), so it can never
    /// collide with the existing Super+Space/Super+Enter defaults.
    #[serde(default = "default_launcher_combo")]
    pub launcher: KeyCombo,
}

impl Default for Keybindings {
    fn default() -> Self {
        Keybindings {
            terminal: default_terminal_combo(),
            focus_next: default_focus_next(),
            focus_prev: default_focus_prev(),
            close: default_close(),
            workspace: default_workspace(),
            toggle_layout: default_toggle_layout(),
            launcher: default_launcher_combo(),
        }
    }
}

fn default_terminal_combo() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_RETURN,
    }
}
fn default_focus_next() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_J,
    }
}
fn default_focus_prev() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_K,
    }
}
fn default_close() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_Q,
    }
}
fn default_toggle_layout() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_SPACE,
    }
}
fn default_launcher_combo() -> KeyCombo {
    KeyCombo {
        mods: MOD_CONTROL,
        key: KEY_SPACE,
    }
}
/// Super+1..9 map to workspaces 1..9; index 9 (Super+0) maps to workspace 10.
fn default_workspace() -> [KeyCombo; 10] {
    std::array::from_fn(|i| KeyCombo {
        mods: MOD_SUPER,
        key: if i == 9 { 0x0030 } else { 0x0031 + i as u32 },
    })
}

/// Errors while reading or parsing the configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Io(String),
    Parse(String),
}

impl Config {
    /// Parses raw TOML into a [`Config`]; unknown keys and malformed input
    /// are rejected so a bad file can never silently replace a good config.
    pub fn parse(raw: &str) -> Result<Config, ConfigError> {
        toml::from_str(raw).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Reads and parses the config file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        Config::parse(&raw)
    }

    /// Reloads from raw TOML, replacing the shared config on success (D6).
    /// `Arc::swap` no longer exists in std, so the caller's `&mut Arc` slot is
    /// reassigned instead — equivalent for the single-threaded design, and
    /// every other holder of the old `Arc` keeps working with the old config.
    /// On a parse error the old config is kept (and logged); returns whether
    /// the shared config was replaced.
    pub fn reload(shared: &mut Arc<Config>, raw: &str) -> bool {
        match Config::parse(raw) {
            Ok(new) => {
                *shared = Arc::new(new);
                true
            }
            Err(err) => {
                eprintln!("tessera: config reload failed, keeping old config: {err:?}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn defaults_are_border_2_gaps_0_alacritty() {
        let c = Config::default();
        assert_eq!(c.general.border_width, 2);
        assert_eq!(c.general.gaps, 0);
        assert_eq!(c.general.terminal, "alacritty");
    }

    #[test]
    fn default_keybindings_cover_super_combos() {
        let c = Config::default();
        assert_eq!(
            c.keybindings.terminal,
            KeyCombo {
                mods: MOD_SUPER,
                key: KEY_RETURN,
            }
        );
        assert_eq!(
            c.keybindings.focus_next,
            KeyCombo {
                mods: MOD_SUPER,
                key: KEY_J,
            }
        );
        assert_eq!(
            c.keybindings.workspace[0],
            KeyCombo {
                mods: MOD_SUPER,
                key: 0x0031,
            }
        );
        assert_eq!(
            c.keybindings.workspace[9],
            KeyCombo {
                mods: MOD_SUPER,
                key: 0x0030,
            }
        );
    }

    #[test]
    fn parses_valid_toml_overriding_defaults() {
        let raw = "[general]\nborder_width = 4\ngaps = 6\nterminal = \"foot\"\n";
        let c = Config::parse(raw).expect("valid toml");
        assert_eq!(c.general.border_width, 4);
        assert_eq!(c.general.gaps, 6);
        assert_eq!(c.general.terminal, "foot");
        // Unspecified sections keep their defaults.
        assert_eq!(
            c.keybindings.terminal,
            KeyCombo {
                mods: MOD_SUPER,
                key: KEY_RETURN,
            }
        );
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(Config::parse("general = { border_width = ").is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::parse("[general]\nbogus = 1\n").is_err());
        assert!(Config::parse("bogus_section = 1\n").is_err());
    }

    #[test]
    fn reload_keeps_old_config_on_parse_error() {
        let mut shared = Arc::new(Config::default());
        let before = Arc::clone(&shared);
        assert!(!Config::reload(&mut shared, "not [valid toml"));
        assert!(Arc::ptr_eq(&shared, &before));
        assert_eq!(shared.general.border_width, 2);
    }

    #[test]
    fn reload_swaps_in_new_config() {
        let mut shared = Arc::new(Config::default());
        assert!(Config::reload(
            &mut shared,
            "[general]\nborder_width = 12\n"
        ));
        assert_eq!(shared.general.border_width, 12);
        assert_eq!(shared.general.terminal, "alacritty"); // unset stays default
    }

    #[test]
    fn default_theme_option_is_none() {
        // REQ-thm-003: absent `theme` -> embedded ayu_dark, no file read.
        let c = Config::default();
        assert_eq!(c.general.theme, None);
    }

    #[test]
    fn theme_path_round_trips_through_parse() {
        // REQ-thm-003: a `theme = "path"` in [general] survives a round-trip.
        let c = Config::parse("[general]\ntheme = \"themes/ayu.toml\"\n").expect("valid toml");
        assert_eq!(c.general.theme, Some("themes/ayu.toml".to_string()));
        // An explicit theme while other general keys are set keeps its value.
        let d =
            Config::parse("[general]\nborder_width = 4\ntheme = \"dark.toml\"\n").expect("valid");
        assert_eq!(d.general.theme, Some("dark.toml".to_string()));
        // Absent theme stays None even when other general keys are present.
        let e = Config::parse("[general]\ngaps = 6\n").expect("valid");
        assert_eq!(e.general.theme, None);
    }

    // === Bar config ([bar] table) — PR1 / Work Unit 1 (tessera-bar-login) ===

    #[test]
    fn bar_defaults_when_table_absent() {
        // Spec: no `[bar]` table (or `position` omitted) -> top edge, default
        // colors, visible; thickness `None` resolves to the per-edge default
        // at draw time (design D6).
        let c = Config::parse("[general]\n").expect("valid toml");
        assert_eq!(c.bar.position, BarPosition::Top);
        assert_eq!(c.bar.thickness, None);
        assert_eq!(c.bar.bg_color, Color::parse_hex("#222222").expect("hex"));
        assert_eq!(c.bar.fg_color, Color::parse_hex("#eeeeee").expect("hex"));
        assert!(c.bar.visible);
        // The serde defaults agree with the manual Default constructor, so
        // `Config::default()` and a parsed empty config cannot drift apart.
        assert_eq!(Config::default().bar, BarConfig::default());
        assert_eq!(Config::default().bar, c.bar);
    }

    #[test]
    fn bar_position_parses_all_lowercase_variants() {
        // Triangulation: each enum variant must deserialize from its exact
        // lowercase string (serde rename_all = "lowercase").
        for (raw, want) in [
            ("top", BarPosition::Top),
            ("bottom", BarPosition::Bottom),
            ("left", BarPosition::Left),
            ("right", BarPosition::Right),
        ] {
            let c = Config::parse(&format!("[bar]\nposition = \"{raw}\"\n")).expect("valid toml");
            assert_eq!(c.bar.position, want);
        }
    }

    #[test]
    fn bar_full_explicit_config_applies() {
        // Spec scenario "Full explicit config": all five fields parsed.
        let raw = "[bar]\nposition = \"bottom\"\nthickness = 28\nbg_color = \"#112233\"\nfg_color = \"#445566\"\nvisible = true\n";
        let c = Config::parse(raw).expect("valid toml");
        assert_eq!(c.bar.position, BarPosition::Bottom);
        assert_eq!(c.bar.thickness, Some(28));
        assert_eq!(c.bar.bg_color, Color::parse_hex("#112233").expect("hex"));
        assert_eq!(c.bar.fg_color, Color::parse_hex("#445566").expect("hex"));
        assert!(c.bar.visible);
    }

    #[test]
    fn bar_rejects_unknown_field_naming_key() {
        // Spec scenario "Unknown field rejected": strict-TOML error names the
        // offending key and the accepted field set. (toml 0.9's serde error
        // for a nested `deny_unknown_fields` table names the key but not the
        // parent table path; the key is identified precisely.)
        let err = Config::parse("[bar]\nflavor = \"cherry\"\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("flavor"),
            "unexpected err: {err:?}"
        );
        assert!(
            format!("{err:?}").contains("expected one of"),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn bar_rejects_thickness_zero() {
        let err = Config::parse("[bar]\nthickness = 0\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("thickness"),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn bar_rejects_thickness_above_200() {
        for n in [201u16, 1000] {
            let raw = format!("[bar]\nthickness = {n}\n");
            let err = Config::parse(&raw).expect_err("must reject");
            assert!(
                format!("{err:?}").contains("thickness"),
                "unexpected err: {err:?}"
            );
        }
    }

    #[test]
    fn bar_visible_false_parses() {
        // Spec scenario "Visible=false hides bar": parses, and only visibility
        // flips; the remaining fields keep their defaults.
        let c = Config::parse("[bar]\nvisible = false\n").expect("valid toml");
        assert!(!c.bar.visible);
        assert_eq!(c.bar.position, BarPosition::Top);
        assert_eq!(c.bar.thickness, None);
    }

    #[test]
    fn bar_rejects_invalid_position_string() {
        // Spec scenario "Invalid position string": "diagonal" is not a variant.
        let err = Config::parse("[bar]\nposition = \"diagonal\"\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("position"),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn bar_rejects_non_rrggbb_colors() {
        // Design OQ4: colors are restricted to `#RRGGBB`; named X colors and
        // malformed hex are rejected by the shared `Color` deserializer.
        let err = Config::parse("[bar]\nbg_color = \"red\"\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("bg_color"),
            "unexpected err: {err:?}"
        );
        let err = Config::parse("[bar]\nfg_color = \"#12345\"\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("fg_color"),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn bar_types_reexported_from_crate_root() {
        // Task 1.2: `BarConfig` and `BarPosition` are visible at the crate
        // root, not only via `config::`.
        assert_eq!(crate::BarPosition::Top, BarPosition::Top);
        let _ = crate::BarConfig::default();
    }

    // === Launcher config + Ctrl+Space binding — PR1 / WU1 (tessera-keybinds-launcher) ===

    #[test]
    fn launcher_defaults_to_rofi_drun() {
        // ALA-2: `[general] launcher` defaults to ["rofi","-show","drun"].
        // The serde default must agree with the manual Default constructor,
        // so `Config::default()` and a parsed empty config cannot drift apart.
        let c = Config::default();
        assert_eq!(c.general.launcher, vec!["rofi", "-show", "drun"]);
        let d = Config::parse("[general]\n").expect("valid toml");
        assert_eq!(c.general.launcher, d.general.launcher);
    }

    #[test]
    fn launcher_override_replaces_default() {
        // ALA-2 scenario "Launcher is configurable": an explicit array wins.
        let c = Config::parse("[general]\nlauncher = [\"dmenu_run\"]\n").expect("valid toml");
        assert_eq!(c.general.launcher, vec!["dmenu_run"]);
    }

    #[test]
    fn launcher_empty_array_is_rejected_naming_field() {
        // Design D5 (Open Question 2): `launcher = []` is a misconfiguration
        // that would leave Ctrl+Space silently inert, so the strict-TOML
        // parse error must name the field.
        let err = Config::parse("[general]\nlauncher = []\n").expect_err("must reject");
        assert!(
            format!("{err:?}").contains("launcher"),
            "unexpected err: {err:?}"
        );
    }

    #[test]
    fn default_launcher_keybinding_is_ctrl_space_and_round_trips() {
        // ALA-2: the launcher keybinding defaults to Ctrl+Space (mods=4,
        // key=0x0020); an explicit table with the same values parses to the
        // identical combo, proving the default round-trips through TOML.
        let c = Config::default();
        assert_eq!(
            c.keybindings.launcher,
            KeyCombo {
                mods: MOD_CONTROL,
                key: KEY_SPACE,
            }
        );
        let d = Config::parse("[keybindings.launcher]\nmods = 4\nkey = 32\n").expect("valid toml");
        assert_eq!(d.keybindings.launcher, c.keybindings.launcher);
        assert_eq!(
            d.keybindings.launcher,
            KeyCombo {
                mods: 4,
                key: 0x0020
            }
        );
    }
}
