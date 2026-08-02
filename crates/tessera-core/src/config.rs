//! TOML configuration with strict fields and safe reload (design D6).

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::event::KeyCombo;

/// Modifier mask for the "Super" (Mod4) key.
///
/// NOTE: X11 modifier bits are Shift=1, Lock=2, Control=4, Mod1=8, Mod2=16,
/// Mod3=32, Mod4=64, Mod5=128 — Mod4 is bit 6, NOT `1 << 3` (that would be
/// Mod1/Alt). A wrong mask silently breaks every Super binding: the grab
/// never matches the real Mod4 event state (caught by the Xvfb E2E).
const MOD_SUPER: u32 = 1 << 6;
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
}

impl Default for GeneralConfig {
    fn default() -> Self {
        GeneralConfig {
            border_width: default_border_width(),
            gaps: default_gaps(),
            terminal: default_terminal(),
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
}
