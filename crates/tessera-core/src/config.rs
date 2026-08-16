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
/// Modifier mask for the "Shift" key (X11 bit 0, `1 << 0`). Combined with
/// `MOD_SUPER` for the default `move_to_workspace` bindings (Super+Shift+N);
/// disjoint from every other default mask.
const MOD_SHIFT: u32 = 1 << 0;
// Keysyms for the default keybindings (X11 keysym table).
const KEY_RETURN: u32 = 0xff0d;
const KEY_J: u32 = 0x006a;
const KEY_K: u32 = 0x006b;
const KEY_Q: u32 = 0x0071;
const KEY_SPACE: u32 = 0x0020;
const KEY_H: u32 = 0x0068;
const KEY_L: u32 = 0x006c;

// No `Eq`: `bar.font_size` is an `f32` (see [`BarConfig`]).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
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
/// Gap applied by the layout on every side of every cell (REQ-lay-004).
/// Cells touch, so `3` renders as 6px between two windows and 3px against
/// the screen edge — a visible default now that the layout actually reads
/// this value (it was parsed but dead before).
fn default_gaps() -> u32 {
    3
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
///
/// `font`/`font_size` are plain config keys here — the core never reads a
/// font file and never links a rasteriser (design D1: `tessera-core` stays
/// free of X AND of font code). `tessera-x11` owns the loading, the glyph
/// cache and the fallback.
///
/// No `Eq`: `font_size` is an `f32`, so only `PartialEq` is derivable.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    /// ABSOLUTE path to a TTF/OTF file used for the tag glyphs — deliberately
    /// a path and NOT a family name like `"Hack Nerd Font Mono"`. Resolving a
    /// family needs fontconfig (a C dependency) or shelling out to `fc-match`
    /// on every start; neither is worth it for one bar font, and a path is
    /// also exactly reproducible. An unreadable or unparseable file never
    /// aborts: the renderer warns once and falls back to the X core font.
    #[serde(default = "default_bar_font")]
    pub font: String,
    /// Glyph size in pixels-per-em for `font`.
    #[serde(default = "default_bar_font_size")]
    pub font_size: f32,
}

impl Default for BarConfig {
    fn default() -> Self {
        BarConfig {
            position: BarPosition::Top,
            thickness: None,
            bg_color: default_bar_bg_color(),
            fg_color: default_bar_fg_color(),
            visible: default_bar_visible(),
            font: default_bar_font(),
            font_size: default_bar_font_size(),
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
/// The Nerd Font shipped as the bar default: `fc-match 'Hack Nerd Font Mono'`
/// resolves to exactly this path, so the default is what a user asking for
/// that family would get — without linking fontconfig to find out.
fn default_bar_font() -> String {
    "/usr/share/fonts/TTF/HackNerdFontMono-Regular.ttf".to_string()
}
fn default_bar_font_size() -> f32 {
    12.0
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
    /// Steps to the previous/next workspace in ascending numeric-id order,
    /// wrapping at both ends (WS-1, D11). Default Super+H / Super+L; `h`/`l`
    /// must be in `KEY_NAMES` (D13) for these defaults to be nameable.
    #[serde(default = "default_workspace_prev")]
    pub workspace_prev: KeyCombo,
    #[serde(default = "default_workspace_next")]
    pub workspace_next: KeyCombo,
    /// Super+Shift+{1..9,0} sends the focused window to workspace {1..10}
    /// without following it (MV-1/2, D6/D7). Index `i` maps to workspace
    /// `i + 1`, mirroring `workspace` exactly.
    #[serde(default = "default_move_to_workspace")]
    pub move_to_workspace: [KeyCombo; 10],
    /// Moves focus to the geometrically adjacent window in that direction
    /// (DF-1, D6/D7/D8); with no candidate this is a silent no-op (DF-2 —
    /// unlike workspace stepping, it never wraps). Reuses the `h`/`j`/`k`/`l`
    /// keysyms u1 already made nameable and mapped (D13) — no new keysym, no
    /// new keycode.
    #[serde(default = "default_focus_left")]
    pub focus_left: KeyCombo,
    #[serde(default = "default_focus_down")]
    pub focus_down: KeyCombo,
    #[serde(default = "default_focus_up")]
    pub focus_up: KeyCombo,
    #[serde(default = "default_focus_right")]
    pub focus_right: KeyCombo,
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
            workspace_prev: default_workspace_prev(),
            workspace_next: default_workspace_next(),
            move_to_workspace: default_move_to_workspace(),
            focus_left: default_focus_left(),
            focus_down: default_focus_down(),
            focus_up: default_focus_up(),
            focus_right: default_focus_right(),
        }
    }
}

impl Keybindings {
    /// The claim-log name paired with every configured binding (KBR-3, D8).
    ///
    /// The destructure below is EXHAUSTIVE and deliberately has no `..` rest
    /// pattern: adding a field to `Keybindings` must fail to compile here
    /// until that field is given a claim-log name (`E0027`).
    /// `#[deny(unused_variables)]` closes the other half of the same guard —
    /// binding a field without emitting it into `pairs` is a build error too.
    /// Together these are the entire point of this registry, which exists
    /// because a `zip` of two hand-maintained lists (the config field array
    /// and a parallel names `Vec`) used to truncate silently on any length
    /// mismatch, leaving a binding ungrabbed, unnamed, AND still counted as
    /// healthy in `GrabStats.bindings` (D2, D7).
    ///
    /// DO NOT add `..` to silence a compile error here after adding a field.
    /// That error IS the feature: it is telling you a binding would
    /// otherwise be grabbed under no name, or not grabbed at all, while the
    /// claim line still reports it healthy. Give the new field a name in
    /// BOTH the destructure below AND the `pairs` list, in the position
    /// where it should actually be grabbed.
    ///
    /// Emission order is WRITTEN, not derived from struct-declaration order
    /// (D4): `workspace` is the 5th declared field but grabs LAST here,
    /// because the ratified grab-order test pins the first eight grab calls
    /// to the terminal binding's lock-variant masks. Workspace names are
    /// GENERATED from the array's own index (D5) via `enumerate`, never a
    /// second parallel list, so they cannot truncate or shift independently.
    /// `focus_left`/`focus_down`/`focus_up`/`focus_right` (u2,
    /// tessera-navigation-bindings) are appended as scalars right after
    /// `workspace_next`, keeping the same "new scalars after the previous
    /// last scalar" placement u1 used for `workspace_prev`/`workspace_next`
    /// — `workspace` and `move_to_workspace` still extend last, in that
    /// order, so `calls[..8]` (the terminal binding) is untouched.
    #[deny(unused_variables)]
    pub fn named_bindings(&self) -> Vec<(String, KeyCombo)> {
        let Keybindings {
            terminal,
            focus_next,
            focus_prev,
            close,
            workspace,
            toggle_layout,
            launcher,
            workspace_prev,
            workspace_next,
            move_to_workspace,
            focus_left,
            focus_down,
            focus_up,
            focus_right,
        } = self;
        let mut pairs: Vec<(String, KeyCombo)> = vec![
            ("terminal".to_string(), *terminal),
            ("focus_next".to_string(), *focus_next),
            ("focus_prev".to_string(), *focus_prev),
            ("close".to_string(), *close),
            ("toggle_layout".to_string(), *toggle_layout),
            ("launcher".to_string(), *launcher),
            ("workspace_prev".to_string(), *workspace_prev),
            ("workspace_next".to_string(), *workspace_next),
            ("focus_left".to_string(), *focus_left),
            ("focus_down".to_string(), *focus_down),
            ("focus_up".to_string(), *focus_up),
            ("focus_right".to_string(), *focus_right),
        ];
        pairs.extend(
            workspace
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("workspace-{}", i + 1), *c)),
        );
        pairs.extend(
            move_to_workspace
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("move-to-workspace-{}", i + 1), *c)),
        );
        pairs
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
fn default_workspace_prev() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_H,
    }
}
fn default_workspace_next() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER,
        key: KEY_L,
    }
}
/// Super+Shift+1..9 send to workspaces 1..9; index 9 (Super+Shift+0) sends to
/// workspace 10 — identical numbering to `default_workspace` (D12).
fn default_move_to_workspace() -> [KeyCombo; 10] {
    std::array::from_fn(|i| KeyCombo {
        mods: MOD_SUPER | MOD_SHIFT,
        key: if i == 9 { 0x0030 } else { 0x0031 + i as u32 },
    })
}
/// Super+Shift+h/j/k/l (DF-1): reuses the same keysyms as `focus_prev`
/// (`KEY_K`)/`focus_next` (`KEY_J`) and `workspace_prev`/`workspace_next`
/// (`KEY_H`/`KEY_L`) — only the modifier mask differs, so u2 needs no new
/// keysym and no new keycode (D13).
fn default_focus_left() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER | MOD_SHIFT,
        key: KEY_H,
    }
}
fn default_focus_down() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER | MOD_SHIFT,
        key: KEY_J,
    }
}
fn default_focus_up() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER | MOD_SHIFT,
        key: KEY_K,
    }
}
fn default_focus_right() -> KeyCombo {
    KeyCombo {
        mods: MOD_SUPER | MOD_SHIFT,
        key: KEY_L,
    }
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
    fn defaults_are_border_2_gaps_3_alacritty() {
        let c = Config::default();
        assert_eq!(c.general.border_width, 2);
        assert_eq!(c.general.gaps, 3);
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

    // === Named keysyms + named modifiers — WU1 (tessera-test-debugging) ===

    #[test]
    fn named_key_and_mod_equal_legacy_ints() {
        // NKB-1 "Named key equals legacy int": `mods = "super", key =
        // "Return"` must parse to exactly the legacy `mods = 64, key = 65293`
        // combo (0xff0d).
        let named = Config::parse("[keybindings.terminal]\nmods = \"super\"\nkey = \"Return\"\n")
            .expect("named values must parse");
        let legacy =
            Config::parse("[keybindings.terminal]\nmods = 64\nkey = 65293\n").expect("legacy ints");
        assert_eq!(
            named.keybindings.terminal, legacy.keybindings.terminal,
            "named values must resolve to the legacy int combo"
        );
        assert_eq!(
            named.keybindings.terminal,
            KeyCombo {
                mods: MOD_SUPER,
                key: KEY_RETURN,
            }
        );
    }

    #[test]
    fn mod_combo_string_ors_masks() {
        // NKB-1 "Mod combo OR": `"super+control"` (64 | 4) must become 68.
        let c =
            Config::parse("[keybindings.terminal]\nmods = \"super+control\"\nkey = \"Return\"\n")
                .expect("a+b mod list must parse");
        assert_eq!(c.keybindings.terminal.mods, 68);
        // Triangulation: a second combo exercises a different mask set.
        let d = Config::parse("[keybindings.close]\nmods = \"mod1+lock\"\nkey = \"q\"\n")
            .expect("mod1+lock must parse");
        assert_eq!(d.keybindings.close.mods, 8 | 2);
    }

    #[test]
    fn single_named_mod_maps_to_its_mask() {
        // NKB-1 "Single named mod": `"ctrl"` (the control alias) is 4.
        let c = Config::parse("[keybindings.launcher]\nmods = \"ctrl\"\nkey = \"space\"\n")
            .expect("ctrl alias must parse");
        assert_eq!(c.keybindings.launcher.mods, MOD_CONTROL);
        assert_eq!(c.keybindings.launcher.key, KEY_SPACE);
    }

    #[test]
    fn int_only_config_parses_unchanged() {
        // NKB-1 "Ints still parse": an existing int-only config keeps
        // working byte-for-byte — the named forms are additive.
        let c = Config::parse(
            "[keybindings.terminal]\nmods = 64\nkey = 65293\n\
             [keybindings.close]\nmods = 64\nkey = 113\n",
        )
        .expect("legacy ints must keep parsing");
        assert_eq!(
            c.keybindings.terminal,
            KeyCombo {
                mods: 64,
                key: 0xff0d,
            }
        );
        assert_eq!(
            c.keybindings.close,
            KeyCombo {
                mods: 64,
                key: 0x0071,
            }
        );
    }

    #[test]
    fn enter_esc_aliases_resolve_to_the_canonical_keysyms() {
        // NKB-3: the lowercase aliases `enter`/`esc` resolve to the same
        // keysyms as the canonical `Return`/`Escape`.
        let c = Config::parse("[keybindings.terminal]\nmods = \"super\"\nkey = \"enter\"\n")
            .expect("enter alias must parse");
        assert_eq!(c.keybindings.terminal.key, KEY_RETURN);
        let d = Config::parse("[keybindings.close]\nmods = \"super\"\nkey = \"esc\"\n")
            .expect("esc alias must parse");
        assert_eq!(d.keybindings.close.key, 0xff1b);
    }

    #[test]
    fn unknown_key_name_errors_naming_field_and_accepted_list() {
        // NKB-2 "Unknown key name": a typo'd key name fails strict parsing,
        // naming `key` and listing the accepted dictionary. Lookup is
        // exact-case, so lowercase "return" is rejected too.
        for bad in ["Entery", "return"] {
            let raw = format!("[keybindings.terminal]\nmods = \"super\"\nkey = \"{bad}\"\n");
            let err = Config::parse(&raw).expect_err("unknown key name must be rejected");
            let msg = format!("{err:?}");
            assert!(msg.contains("key"), "error must name the field: {msg}");
            assert!(
                msg.contains("accepted"),
                "error must list accepted names: {msg}"
            );
            assert!(
                msg.contains("Return"),
                "accepted list must include the canonical name: {msg}"
            );
            assert!(
                msg.contains(bad),
                "error must echo the offending name: {msg}"
            );
        }
    }

    // === Named-bindings registry (KBR-3, D8) — tessera-keybinding-registry ===

    #[test]
    fn named_bindings_pairs_every_field_in_grab_order() {
        // T-A: the exact 32-name sequence in registry (grab) order (16 base +
        // 12 WU1 [workspace_prev/next + move_to_workspace] + 4 WU2
        // [focus_left/down/up/right], tessera-navigation-bindings), and each
        // pair's combo equals the field read through ITS OWN path — never
        // the registry itself. This is the one test that can catch the D4
        // grab-order trap: `workspace` is Keybindings' 5th declared field but
        // must land after toggle_layout, launcher, workspace_prev,
        // workspace_next and the four focus_* scalars here, and
        // `move_to_workspace` must land LAST of all.
        let k = Keybindings::default();
        let pairs = k.named_bindings();
        let expected_names: Vec<String> = [
            "terminal",
            "focus_next",
            "focus_prev",
            "close",
            "toggle_layout",
            "launcher",
            "workspace_prev",
            "workspace_next",
            "focus_left",
            "focus_down",
            "focus_up",
            "focus_right",
        ]
        .iter()
        .map(|n| (*n).to_string())
        .chain((1..=10).map(|i| format!("workspace-{i}")))
        .chain((1..=10).map(|i| format!("move-to-workspace-{i}")))
        .collect();
        let names: Vec<String> = pairs.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, expected_names);
        assert_eq!(pairs.len(), 32);
        assert_eq!(pairs[0], ("terminal".to_string(), k.terminal));
        assert_eq!(pairs[1], ("focus_next".to_string(), k.focus_next));
        assert_eq!(pairs[2], ("focus_prev".to_string(), k.focus_prev));
        assert_eq!(pairs[3], ("close".to_string(), k.close));
        assert_eq!(pairs[4], ("toggle_layout".to_string(), k.toggle_layout));
        assert_eq!(pairs[5], ("launcher".to_string(), k.launcher));
        assert_eq!(pairs[6], ("workspace_prev".to_string(), k.workspace_prev));
        assert_eq!(pairs[7], ("workspace_next".to_string(), k.workspace_next));
        assert_eq!(pairs[8], ("focus_left".to_string(), k.focus_left));
        assert_eq!(pairs[9], ("focus_down".to_string(), k.focus_down));
        assert_eq!(pairs[10], ("focus_up".to_string(), k.focus_up));
        assert_eq!(pairs[11], ("focus_right".to_string(), k.focus_right));
        for i in 0..10 {
            assert_eq!(
                pairs[12 + i],
                (format!("workspace-{}", i + 1), k.workspace[i]),
                "workspace-{} must pair with k.workspace[{}], read directly from the field",
                i + 1,
                i
            );
        }
        for i in 0..10 {
            assert_eq!(
                pairs[22 + i],
                (
                    format!("move-to-workspace-{}", i + 1),
                    k.move_to_workspace[i]
                ),
                "move-to-workspace-{} must pair with k.move_to_workspace[{}], read directly from the field",
                i + 1,
                i
            );
        }
    }

    #[test]
    fn named_bindings_names_the_directional_focus_bindings() {
        // D12/D6/D7/D8 (u2, tessera-navigation-bindings): focus_left/down/
        // up/right are named SCALARS appended right after workspace_next —
        // the same "append after the previous last scalar" placement u1
        // used, so `workspace`/`move_to_workspace` keep extending last.
        let k = Keybindings::default();
        let pairs = k.named_bindings();
        assert_eq!(pairs.len(), 32);
        assert_eq!(pairs[8], ("focus_left".to_string(), k.focus_left));
        assert_eq!(pairs[9], ("focus_down".to_string(), k.focus_down));
        assert_eq!(pairs[10], ("focus_up".to_string(), k.focus_up));
        assert_eq!(pairs[11], ("focus_right".to_string(), k.focus_right));
    }

    #[test]
    fn named_bindings_names_are_unique() {
        // T-B: no two entries share a claim-log name. This closes the one
        // hole the compiler guards (D2) cannot see — a field emitted under
        // the WRONG or a DUPLICATE label, which would make `missing:`
        // misname a binding at claim time.
        let pairs = Keybindings::default().named_bindings();
        let mut names: Vec<&String> = pairs.iter().map(|(n, _)| n).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "every binding name must be unique");
    }

    // === Workspace ring + move-to-workspace bindings — WU1 (tessera-navigation-bindings) ===

    #[test]
    fn named_bindings_names_the_workspace_step_and_move_bindings() {
        // D12: workspace_prev/workspace_next are named SCALARS appended after
        // launcher; move_to_workspace is GENERATED via enumerate and extended
        // LAST (after the workspace extend, and after u2's focus_* scalars),
        // so calls[..8] (grab order) still resolves to the terminal binding
        // (KBR-3 gotcha).
        let k = Keybindings::default();
        let pairs = k.named_bindings();
        assert_eq!(pairs.len(), 32);
        assert_eq!(pairs[6], ("workspace_prev".to_string(), k.workspace_prev));
        assert_eq!(pairs[7], ("workspace_next".to_string(), k.workspace_next));
        for i in 0..10 {
            assert_eq!(
                pairs[22 + i],
                (
                    format!("move-to-workspace-{}", i + 1),
                    k.move_to_workspace[i]
                ),
                "move-to-workspace-{} must pair with k.move_to_workspace[{}]",
                i + 1,
                i
            );
        }
    }

    #[test]
    fn unknown_mod_name_errors_naming_field_and_accepted_list() {
        // NKB-2 "Unknown mod name": "hyper" is a valid KEYSYM name but not a
        // named modifier — the error names `mods` and lists the accepted
        // modifiers (exact-case, including the ctrl alias).
        let err = Config::parse("[keybindings.terminal]\nmods = \"hyper\"\nkey = \"Return\"\n")
            .expect_err("unknown mod name must be rejected");
        let msg = format!("{err:?}");
        assert!(msg.contains("mods"), "error must name the field: {msg}");
        assert!(
            msg.contains("hyper"),
            "error must echo the offending name: {msg}"
        );
        for accepted in [
            "super", "control", "ctrl", "shift", "alt", "mod1", "mod2", "mod3", "mod4", "mod5",
            "lock",
        ] {
            assert!(
                msg.contains(accepted),
                "accepted list must include {accepted}: {msg}"
            );
        }
    }
}
