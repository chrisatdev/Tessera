//! TOML configuration with strict fields and safe reload (design D6).

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::event::KeyCombo;

/// Modifier mask for the "Super" (Mod4) key.
const MOD_SUPER: u32 = 1 << 3;
// Keysyms for the default keybindings (X11 keysym table).
const KEY_RETURN: u32 = 0xff0d;
const KEY_J: u32 = 0x006a;
const KEY_K: u32 = 0x006b;
const KEY_Q: u32 = 0x0071;
const KEY_SPACE: u32 = 0x0020;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub keybindings: Keybindings,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            general: GeneralConfig::default(),
            keybindings: Keybindings::default(),
        }
    }
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
        todo!()
    }

    /// Reads and parses the config file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        todo!()
    }

    /// Reloads from raw TOML, swapping via `Arc::swap` on success (D6).
    /// On a parse error the old config is kept (and logged); returns whether
    /// the shared config was replaced.
    pub fn reload(shared: &mut Arc<Config>, raw: &str) -> bool {
        todo!()
    }
}
