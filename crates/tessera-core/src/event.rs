//! Typed events published on the [`EventBus`](crate::bus::EventBus).

use std::sync::Arc;

use serde::{Deserializer, de};

use crate::command::Command;
use crate::config::Config;
use crate::geometry::{LayoutKind, Placement, Rect, WindowId, WorkspaceId};

/// A bound key combination. `mods` is the X11 modifier mask and `key` the
/// keysym; both are raw integers so the core stays X-free. Parsing accepts
/// the legacy integer forms plus the committed dictionaries in this module
/// (NKB-1): `key` deserializes from an integer keysym or an exact-case
/// `KEY_NAMES` entry; `mods` from an integer mask, a single `MOD_NAMES`
/// entry, or an `"a+b"` list OR-ed into one mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct KeyCombo {
    #[serde(deserialize_with = "deserialize_mods")]
    pub mods: u32,
    #[serde(deserialize_with = "deserialize_key")]
    pub key: u32,
}

/// Committed keysym dictionary (NKB-3): 39 canonical names plus the
/// lowercase aliases `enter`/`esc`. Lookup is exact-case; any other name is
/// an NKB-2 strict-parse error naming the field and the accepted list. The
/// table covers every default binding and the keysyms the X layer names
/// (design D2: a separate parse dict in core, never merged with the x11
/// display hints in `keysym_name`).
pub(crate) const KEY_NAMES: &[(&str, u32)] = &[
    // Editing + navigation keys.
    ("Return", 0xff0d),
    ("Escape", 0xff1b),
    ("Tab", 0xff09),
    ("BackSpace", 0xff08),
    ("Delete", 0xffff),
    ("space", 0x0020),
    ("j", 0x006a),
    ("k", 0x006b),
    ("q", 0x0071),
    ("1", 0x0031),
    ("2", 0x0032),
    ("3", 0x0033),
    ("4", 0x0034),
    ("5", 0x0035),
    ("6", 0x0036),
    ("7", 0x0037),
    ("8", 0x0038),
    ("9", 0x0039),
    ("0", 0x0030),
    ("Up", 0xff52),
    ("Down", 0xff54),
    ("Left", 0xff51),
    ("Right", 0xff53),
    ("Home", 0xff50),
    ("End", 0xff57),
    ("PgUp", 0xff55),
    ("PgDn", 0xff56),
    // Modifier keysyms (named for completeness; press-time grabs resolve
    // them through the x11 display hints, never through this table).
    ("Shift_L", 0xffe1),
    ("Shift_R", 0xffe2),
    ("Control_L", 0xffe3),
    ("Control_R", 0xffe4),
    ("Meta_L", 0xffe7),
    ("Meta_R", 0xffe8),
    ("Alt_L", 0xffe9),
    ("Alt_R", 0xffea),
    ("Super_L", 0xffeb),
    ("Super_R", 0xffec),
    ("Hyper_L", 0xffed),
    ("Hyper_R", 0xffee),
    // Lowercase aliases (NKB-3).
    ("enter", 0xff0d),
    ("esc", 0xff1b),
];

/// Committed named-modifier dictionary (NKB-3): exact-case names mapping to
/// X11 modifier mask bits. `"a+b"` lists OR their masks; `ctrl` is the
/// accepted alias for `control`.
pub(crate) const MOD_NAMES: &[(&str, u32)] = &[
    ("super", 64),
    ("control", 4),
    ("ctrl", 4),
    ("shift", 1),
    ("alt", 8),
    ("mod1", 8),
    ("mod2", 16),
    ("mod3", 32),
    ("mod4", 64),
    ("mod5", 128),
    ("lock", 2),
];

/// Shared visitor for [`deserialize_key`] and [`deserialize_mods`]: accepts
/// an integer, or a string that the `names` dictionary maps to a u32
/// (mods additionally accepts `"a+b"` OR combos). Unknown names fail with an
/// error naming the field and listing every accepted name (NKB-2).
struct DictVisitor {
    /// Field name echoed in NKB-2 errors (`"key"` | `"mods"`).
    field: &'static str,
    /// Kind of value, for error wording (`"keysym"` | `"modifier"`).
    what: &'static str,
    /// The committed dictionary to look names up in.
    names: &'static [(&'static str, u32)],
    /// When true (mods only), `"a+b"` strings OR their named masks.
    allow_combo: bool,
}

impl DictVisitor {
    fn unknown(&self, name: &str) -> String {
        let accepted = self
            .names
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}: unknown {} '{name}'; accepted: {accepted}",
            self.field, self.what
        )
    }
}

impl<'de> de::Visitor<'de> for DictVisitor {
    type Value = u32;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "an integer {} or a named {} from the committed dictionary",
            self.what, self.what
        )
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<u32, E> {
        u32::try_from(v).map_err(|_| {
            E::custom(format!(
                "{}: {} must be a non-negative u32, got {v}",
                self.field, self.what
            ))
        })
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<u32, E> {
        u32::try_from(v).map_err(|_| {
            E::custom(format!(
                "{}: {} must fit in u32, got {v}",
                self.field, self.what
            ))
        })
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<u32, E> {
        if let Some(&(_, value)) = self.names.iter().find(|(name, _)| *name == v) {
            return Ok(value);
        }
        if self.allow_combo && v.contains('+') {
            let mut mask = 0u32;
            for part in v.split('+') {
                let Some(&(_, m)) = self.names.iter().find(|(name, _)| *name == part) else {
                    return Err(E::custom(self.unknown(part)));
                };
                mask |= m;
            }
            return Ok(mask);
        }
        Err(E::custom(self.unknown(v)))
    }
}

/// Deserializes a `KeyCombo.key`: an integer keysym (unchanged) or an
/// exact-case `KEY_NAMES` entry (NKB-1). Unknown names fail with an error
/// naming the field and listing the accepted dictionary (NKB-2).
fn deserialize_key<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(DictVisitor {
        field: "key",
        what: "keysym",
        names: KEY_NAMES,
        allow_combo: false,
    })
}

/// Deserializes a `KeyCombo.mods`: an integer mask, a single `MOD_NAMES`
/// entry, or an `"a+b"` list OR-ed into one mask (NKB-1). Unknown names fail
/// with an error naming the field and listing the accepted modifiers
/// (NKB-2).
fn deserialize_mods<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(DictVisitor {
        field: "mods",
        what: "modifier",
        names: MOD_NAMES,
        allow_combo: true,
    })
}

/// Every event flowing through the bus (design D4, all 17 variants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    WindowMapRequested(WindowId),
    WindowConfigureRequested(WindowId, Rect),
    WindowUnmapNotify(WindowId),
    WindowDestroyNotify(WindowId),
    WindowManaged(WindowId),
    WindowUnmapped(WindowId),
    WindowFocusChanged(Option<WindowId>),
    WindowTitleChanged(WindowId, String),
    WorkspaceOpened(WorkspaceId),
    WorkspaceClosed(WorkspaceId),
    WorkspaceChanged(WorkspaceId),
    WorkspaceLayoutChanged(WorkspaceId, LayoutKind),
    PlacementsChanged(WorkspaceId, Vec<Placement>),
    KeyPressed(KeyCombo),
    Command(Command),
    ConfigReloaded(Arc<Config>),
    Shutdown,
}
