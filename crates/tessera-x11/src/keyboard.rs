//! Keyboard: keycode → keysym mapping and grabbing of the configured
//! keybindings (T18, REQ-x11-008, SC-x11-12).
//!
//! Raw `KeyPress` events carry a keyCODE; the core's `KeyCombo.key` is a
//! keysym. [`Keymap`] holds the server's keycode → keysym table so
//! [`translate_key_press`] can resolve a raw press into an
//! [`Event::KeyPressed`] the core's `command_for_key` understands, and
//! [`grab_keybindings`] can convert the config's keysym bindings into
//! keycode grabs on the root window. Every X side effect goes through the
//! [`KeyboardOps`] seam so both directions are scriptable headless.
//!
//! Grabs are lock-tolerant (KBR-1): every binding is expanded into 8
//! lock-variant masks so CapsLock/NumLock/ScrollLock cannot kill a binding,
//! and [`translate_key_press`] strips those lock bits from the press state
//! (KBR-2) so `command_for_key` matches on the meaningful modifiers only.

use tessera_core::{Config, DErr, Event, KeyCombo};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    GrabMode, ModMask, Window, get_keyboard_mapping, get_modifier_mapping, grab_key,
};
use x11rb::rust_connection::RustConnection;

use crate::display_server::{map_conn_error, map_reply_error};

/// Lock modifier bits stripped from KeyPress state before the core's
/// `command_for_key` lookup (KBR-2, D2): Lock(2) | Mod2(16) | Mod3(32) |
/// Mod5(128) = 178. Shift(1), Mod1(8) and Mod4(64) are never stripped.
pub(crate) const LOCK_STRIP: u32 = 2 | 16 | 32 | 128;

/// Every subset of the lock bits {2, 16, 32}: each binding is grabbed once
/// per variant so a press differing only in lock bits still matches an
/// exact-modifier grab (KBR-1, D1 — the X core protocol special-cases no
/// modifier, so Lock/Mod2/Mod3 must all be grabbed explicitly).
const LOCK_VARIANTS: [u16; 8] = [0, 2, 16, 32, 18, 34, 48, 50];

/// The X surface keyboard handling needs, abstracted so translation and
/// grabbing are scriptable headless (same seam shape as
/// [`X11Startup`](crate::display_server::X11Startup)).
pub(crate) trait KeyboardOps {
    /// `(min_keycode, max_keycode)` of the keyboard, from the server setup.
    fn keycode_range(&self) -> (u8, u8);
    /// Fetches the keysym table for `count` keycodes starting at
    /// `first_keycode` as `(keysyms_per_keycode, keysyms)`.
    fn keyboard_mapping(&self, first_keycode: u8, count: u8) -> Result<(u8, Vec<u32>), DErr>;
    /// Grabs `keycode` with `modifiers` on `window` (owner_events = false,
    /// async pointer/keyboard modes — the press is consumed by the grab).
    fn grab_key(&self, window: Window, modifiers: u16, keycode: u8) -> Result<(), DErr>;
    /// Fetches the server's raw `GetModifierMapping` reply as
    /// `(keycodes_per_modifier, flat keycodes)` — the modifier map the
    /// claim-time Mod4 diagnosis reads (SUP-1, D1). Same
    /// `(count, flat slice)` seam shape as [`Self::keyboard_mapping`].
    fn modifier_map(&self) -> Result<(u8, Vec<u8>), DErr>;
}

impl KeyboardOps for RustConnection {
    fn keycode_range(&self) -> (u8, u8) {
        (self.setup().min_keycode, self.setup().max_keycode)
    }
    fn keyboard_mapping(&self, first_keycode: u8, count: u8) -> Result<(u8, Vec<u32>), DErr> {
        let cookie = get_keyboard_mapping(self, first_keycode, count).map_err(map_conn_error)?;
        let reply = cookie.reply().map_err(map_reply_error)?;
        Ok((reply.keysyms_per_keycode, reply.keysyms))
    }
    fn grab_key(&self, window: Window, modifiers: u16, keycode: u8) -> Result<(), DErr> {
        let cookie = grab_key(
            self,
            false,
            window,
            ModMask::from(modifiers),
            keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .map_err(map_conn_error)?;
        cookie.check().map_err(map_reply_error)
    }
    fn modifier_map(&self) -> Result<(u8, Vec<u8>), DErr> {
        let cookie = get_modifier_mapping(self).map_err(map_conn_error)?;
        let reply = cookie.reply().map_err(map_reply_error)?;
        Ok((reply.keycodes_per_modifier(), reply.keycodes))
    }
}

/// The server's keycode → keysym table, loaded during `claim_wm` and cached
/// (v1 ignores `MappingNotify` — a keyboard map change needs a regrab, which
/// is out of scope).
pub(crate) struct Keymap {
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
}

impl Keymap {
    /// Builds a keymap from a raw (min_keycode, keysyms_per_keycode, keysyms)
    /// triple — the shape `get_keyboard_mapping` replies with.
    pub(crate) fn new(min_keycode: u8, keysyms_per_keycode: u8, keysyms: Vec<u32>) -> Self {
        Keymap {
            min_keycode,
            keysyms_per_keycode,
            keysyms,
        }
    }

    /// Fetches the mapping through `ops` over the full keycode range the
    /// server setup reports.
    pub(crate) fn load(ops: &impl KeyboardOps) -> Result<Keymap, DErr> {
        let (min, max) = ops.keycode_range();
        let count = max.saturating_sub(min).saturating_add(1);
        let (keysyms_per_keycode, keysyms) = ops.keyboard_mapping(min, count)?;
        Ok(Keymap::new(min, keysyms_per_keycode, keysyms))
    }

    /// The primary keysym for `keycode` (REQ-x11-008): `0` (NoSymbol) for a
    /// keycode below the minimum, beyond the table, or with an empty slot.
    pub(crate) fn keysym(&self, keycode: u8) -> u32 {
        if self.keysyms_per_keycode == 0 || keycode < self.min_keycode {
            return 0;
        }
        let index = usize::from(keycode - self.min_keycode) * usize::from(self.keysyms_per_keycode);
        self.keysyms.get(index).copied().unwrap_or(0)
    }

    /// Every keycode that currently maps to `keysym`, ascending — the reverse
    /// lookup `grab_keybindings` needs to turn config keysyms into grabs.
    pub(crate) fn keycodes_for_keysym(&self, keysym: u32) -> Vec<u8> {
        let max = self.min_keycode.saturating_add(
            (self.keysyms.len() / usize::from(self.keysyms_per_keycode.max(1))) as u8,
        );
        (self.min_keycode..=max)
            .filter(|&keycode| self.keysym(keycode) == keysym)
            .collect()
    }

    /// Every in-range keycode whose PRIMARY keysym is NoSymbol, ascending —
    /// the mapping holes `claim_wm` logs at claim (KBR-3, D6). Keycodes
    /// beyond the mapping table are never reported.
    pub(crate) fn nosymbol_keycodes(&self) -> Vec<u8> {
        let count = self.keysyms.len() / usize::from(self.keysyms_per_keycode.max(1));
        let max = self
            .min_keycode
            .saturating_add(count as u8)
            .saturating_sub(1);
        (self.min_keycode..=max)
            .filter(|&keycode| self.keysym(keycode) == 0)
            .collect()
    }
}

/// Resolves a raw `KeyPressed` (whose `key` is a keyCODE, as translated by
/// `crate::translate`) into one carrying the keysym the core's
/// `command_for_key` matches against (SC-x11-12). `None` for a key with no
/// keysym — unbound keys are not published.
///
/// The lock bits (2|16|32|128) are stripped from the published mods (KBR-2,
/// D2): a lock-variant grab delivers the press with locks in its state, and
/// the core must match on the meaningful modifiers exactly.
pub(crate) fn translate_key_press(keymap: &Keymap, raw: KeyCombo) -> Option<Event> {
    let keysym = keymap.keysym(raw.key as u8);
    if keysym == 0 {
        return None;
    }
    Some(Event::KeyPressed(KeyCombo {
        mods: raw.mods & !LOCK_STRIP,
        key: keysym,
    }))
}

/// X11 modifier keysym → name table for the claim-time Mod4 diagnosis
/// (SUP-1, D2): the keysyms a modifier keycode commonly carries, so the
/// claim line can print `133 (Super_L)` instead of `133 (0xffeb)`.
const KEYSYM_NAMES: &[(u32, &str)] = &[
    (0xffe1, "Shift_L"),
    (0xffe2, "Shift_R"),
    (0xffe3, "Control_L"),
    (0xffe4, "Control_R"),
    (0xffe7, "Meta_L"),
    (0xffe8, "Meta_R"),
    (0xffe9, "Alt_L"),
    (0xffea, "Alt_R"),
    (0xffeb, "Super_L"),
    (0xffec, "Super_R"),
    (0xffed, "Hyper_L"),
    (0xffee, "Hyper_R"),
];

/// The X11 name for a modifier `keysym` (`"Super_L"`), or `0x{keysym:x}`
/// when the keysym is not in [`KEYSYM_NAMES`] (D2 — never a panic, never
/// an empty string; the hex fallback keeps the claim line readable for any
/// layout).
pub(crate) fn keysym_name(keysym: u32) -> String {
    KEYSYM_NAMES
        .iter()
        .find(|(sym, _)| *sym == keysym)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| format!("0x{keysym:x}"))
}

/// The Mod4 row of a raw `GetModifierMapping` reply as
/// `(keycode, keysym name)` pairs, ascending, skipping `0` (unused) slots
/// (SUP-1, D2). The modifier map is `8 rows × keycodes_per_modifier` in
/// Shift, Lock, Control, Mod1..Mod5 order — Mod4 is row 6. Each row
/// keycode is resolved to its PRIMARY keysym's name through `keymap`, so
/// the claim line can tell the user WHICH keys carry Super.
pub(crate) fn mod4_keycodes(per: u8, map: &[u8], keymap: &Keymap) -> Vec<(u8, String)> {
    if per == 0 {
        return Vec::new();
    }
    let start = usize::from(per) * 6;
    let end = (start + usize::from(per)).min(map.len());
    if start >= map.len() {
        return Vec::new();
    }
    map[start..end]
        .iter()
        .filter(|&&keycode| keycode != 0)
        .map(|&keycode| (keycode, keysym_name(keymap.keysym(keycode))))
        .collect()
}

/// Result of a keybinding grab pass (D6, KBR-3): the CONFIGURED binding count
/// and the ACTUAL grab count — `grabs` drops below `bindings * 8` when a
/// binding's keysym resolves nowhere in the mapping (unresolved) or two
/// identical combos collapse onto one expanded (mods, keycode) pair. Each
/// unresolved binding is named in [`GrabStats::missing`] so the claim log
/// can say WHICH binding is dead, not just that the count dropped (KBR-3
/// modified, D4).
pub(crate) struct GrabStats {
    pub bindings: usize,
    pub grabs: usize,
    /// `(binding name, keysym)` for every binding whose keysym resolved to
    /// NO keycode in the mapping — empty on a healthy map, so the claim
    /// line only gains a `missing:` tail when there is something to name.
    pub missing: Vec<(String, u32)>,
}

/// Grabs every configured keybinding on `root` (REQ-x11-008): each binding's
/// keysym is resolved to its keycode(s) through `keymap` and grabbed with
/// every lock-variant mask (`base | variant` for each of the 8
/// `LOCK_VARIANTS`) so an exact-modifier grab fires under any lock
/// combination (KBR-1, D1). Bindings whose keysym exists nowhere in the
/// mapping are skipped and NAMED in `missing` (KBR-3, D4), and an expanded
/// `(mods, keycode)` pair is only grabbed once. Returns the configured
/// binding count plus how many grabs took effect (KBR-3). Grab failures
/// abort loudly — a silent drop would recreate the silent-binding-death
/// class this change exists to fix.
pub(crate) fn grab_keybindings(
    ops: &impl KeyboardOps,
    keymap: &Keymap,
    root: Window,
    cfg: &Config,
) -> Result<GrabStats, DErr> {
    // The name<->combo pairing comes from the single compile-enforced
    // registry (KBR-3, D6/D7) instead of a `fixed` array zipped against an
    // independent `names` list — that zip used to truncate silently on any
    // length mismatch, leaving a binding ungrabbed, unnamed, and still
    // counted as healthy.
    let bindings = cfg.keybindings.named_bindings();
    let mut missing = Vec::new();
    let mut grabbed: Vec<(u16, u8)> = Vec::new();
    for (name, combo) in &bindings {
        let keycodes = keymap.keycodes_for_keysym(combo.key);
        if keycodes.is_empty() {
            // KBR-3 (D4): the binding's keysym exists nowhere in the mapping
            // — nothing to grab, and the claim line names it as missing.
            missing.push((name.clone(), combo.key));
        }
        for keycode in keycodes {
            for variant in LOCK_VARIANTS {
                let pair = (combo.mods as u16 | variant, keycode);
                if !grabbed.contains(&pair) {
                    ops.grab_key(root, pair.0, pair.1)?;
                    grabbed.push(pair);
                }
            }
        }
    }
    Ok(GrabStats {
        // Derived from the very list that was iterated (D7) — it can no
        // longer disagree with what was grabbed and named, because there is
        // only one list to disagree with.
        bindings: bindings.len(),
        grabs: grabbed.len(),
        missing,
    })
}

#[cfg(test)]
mod tests {
    //! RED (T18): keysym lookup, raw-KeyPress translation (SC-x11-12) and
    //! per-binding grabs on a scripted seam.

    use std::cell::RefCell;

    use tessera_core::{Command, Config, DErr, Event, KeyCombo, command_for_key};
    use x11rb::protocol::xproto::Window;

    use super::*;

    const ROOT: Window = 0x0000_0010;
    /// Mod4 (Super) modifier mask, as in the config defaults. X11 Mod4 is
    /// bit 6 (`1 << 6` = 64) — `1 << 3` is Mod1/Alt and would never match.
    const MOD_SUPER: u16 = 1 << 6;
    /// XK_Return keysym (the default terminal binding).
    const KEY_RETURN: u32 = 0xff0d;

    /// One recorded keyboard call, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum KeyboardCall {
        Mapping {
            first_keycode: u8,
            count: u8,
        },
        Grab {
            window: Window,
            modifiers: u16,
            keycode: u8,
        },
        ModifierMap,
    }

    /// Scripted `KeyboardOps`: a fixed (range, keysym table) pair plus a
    /// recording grab log and a scriptable `GetModifierMapping` reply (D1).
    struct FakeKeyboardOps {
        calls: RefCell<Vec<KeyboardCall>>,
        range: (u8, u8),
        keysyms_per_keycode: u8,
        keysyms: Vec<u32>,
        /// The `(keycodes_per_modifier, flat keycodes)` reply
        /// `modifier_map` returns (D1: raw `GetModifierMapping` shape).
        modifier_map: RefCell<(u8, Vec<u8>)>,
    }

    impl FakeKeyboardOps {
        fn new(range: (u8, u8), keysyms_per_keycode: u8, keysyms: Vec<u32>) -> Self {
            FakeKeyboardOps {
                calls: RefCell::new(Vec::new()),
                range,
                keysyms_per_keycode,
                keysyms,
                modifier_map: RefCell::new((0, Vec::new())),
            }
        }

        fn calls(&self) -> Vec<KeyboardCall> {
            self.calls.borrow().clone()
        }

        /// Scripts the `GetModifierMapping` reply `modifier_map` returns.
        fn script_modifier_map(&self, keycodes_per_modifier: u8, keycodes: Vec<u8>) {
            *self.modifier_map.borrow_mut() = (keycodes_per_modifier, keycodes);
        }
    }

    impl KeyboardOps for FakeKeyboardOps {
        fn keycode_range(&self) -> (u8, u8) {
            self.range
        }
        fn keyboard_mapping(&self, first_keycode: u8, count: u8) -> Result<(u8, Vec<u32>), DErr> {
            self.calls.borrow_mut().push(KeyboardCall::Mapping {
                first_keycode,
                count,
            });
            Ok((self.keysyms_per_keycode, self.keysyms.clone()))
        }
        fn grab_key(&self, window: Window, modifiers: u16, keycode: u8) -> Result<(), DErr> {
            self.calls.borrow_mut().push(KeyboardCall::Grab {
                window,
                modifiers,
                keycode,
            });
            Ok(())
        }
        fn modifier_map(&self) -> Result<(u8, Vec<u8>), DErr> {
            self.calls.borrow_mut().push(KeyboardCall::ModifierMap);
            Ok(self.modifier_map.borrow().clone())
        }
    }

    /// A synthetic mapping (min_keycode 8, two keysyms per keycode) in which
    /// the given (keycode, keysym) pairs map to each other; everything else
    /// is NoSymbol.
    fn keymap_with(pairs: &[(u8, u32)]) -> Keymap {
        let mut keysyms = vec![0u32; 120];
        for &(keycode, keysym) in pairs {
            keysyms[usize::from(keycode - 8) * 2] = keysym;
        }
        Keymap::new(8, 2, keysyms)
    }

    /// The default-config mapping: one keycode per default binding.
    ///
    /// `move_to_workspace` reuses the SAME digit keysyms/keycodes as
    /// `workspace` (only the mods differ, Super+Shift vs Super), so it needs
    /// no new fixture entry. `workspace_prev`/`workspace_next` (Super+H/L, D13)
    /// need their own two keycodes: 43/46 (`h`/`l`, unused elsewhere in this
    /// fixture). WU2's directional focus bindings (Super+Shift+h/j/k/l,
    /// tessera-navigation-bindings) reuse the SAME four keycodes as
    /// `focus_next`/`focus_prev` (44/45) and `workspace_prev`/`workspace_next`
    /// (43/46) — only the mods differ again — so this fixture needs NO new
    /// entries for u2 either; it now resolves all 32 default bindings.
    fn default_keymap() -> Keymap {
        keymap_with(&[
            (36, 0xff0d), // terminal (Super+Enter)
            (44, 0x006a), // focus_next (Super+J)
            (45, 0x006b), // focus_prev (Super+K)
            (24, 0x0071), // close (Super+Q)
            (65, 0x0020), // toggle_layout (Super+Space)
            (10, 0x0031), // workspace 1..9, 10 (Super+1..9,0)
            (11, 0x0032),
            (12, 0x0033),
            (13, 0x0034),
            (14, 0x0035),
            (15, 0x0036),
            (16, 0x0037),
            (17, 0x0038),
            (18, 0x0039),
            (19, 0x0030),
            (43, 0x0068), // workspace_prev (Super+H)
            (46, 0x006c), // workspace_next (Super+L)
        ])
    }

    #[test]
    fn keymap_load_fetches_the_keyboard_mapping() {
        // The mapping is fetched over the full keycode range reported by the
        // server setup (max - min + 1), and the loaded keymap resolves a
        // keycode through it.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = Keymap::load(&fake).unwrap();
        assert_eq!(
            fake.calls(),
            vec![KeyboardCall::Mapping {
                first_keycode: 8,
                count: 60, // 67 - 8 + 1
            }]
        );
        assert_eq!(keymap.keysym(36), 0); // NoSymbol for an empty slot
    }

    #[test]
    fn keysym_maps_a_keycode_to_its_primary_keysym() {
        let keymap = keymap_with(&[(36, KEY_RETURN)]);
        assert_eq!(keymap.keysym(36), KEY_RETURN);
    }

    #[test]
    fn keysym_is_nosymbol_outside_the_mapping() {
        let keymap = keymap_with(&[(36, KEY_RETURN)]);
        // Below the minimum keycode, beyond the table, and empty slots all
        // resolve to NoSymbol (0) — never a panic, never a stale keysym.
        assert_eq!(keymap.keysym(7), 0);
        assert_eq!(keymap.keysym(200), 0);
        assert_eq!(keymap.keysym(37), 0);
    }

    #[test]
    fn translate_key_press_resolves_super_enter_to_the_terminal_command() {
        // SC-x11-12 seam: a raw Super+Enter KeyPress (keycode 36, Mod4)
        // becomes KeyPressed with the XK_Return keysym, which the core's
        // command_for_key maps to SpawnTerminal — the alacritty spawn is the
        // core loop's reaction (tested at the app layer in U3).
        let keymap = default_keymap();
        let raw = KeyCombo {
            mods: u32::from(MOD_SUPER),
            key: 36, // raw keyCODE
        };
        let translated = translate_key_press(&keymap, raw).unwrap();
        assert_eq!(
            translated,
            Event::KeyPressed(KeyCombo {
                mods: u32::from(MOD_SUPER),
                key: KEY_RETURN,
            })
        );
        let Event::KeyPressed(combo) = translated else {
            panic!("expected a KeyPressed event");
        };
        assert_eq!(
            command_for_key(&Config::default(), combo),
            Some(Command::SpawnTerminal)
        );
    }

    #[test]
    fn translate_key_press_skips_keys_without_a_keysym() {
        // A raw keycode that maps to NoSymbol (unmapped key) is not published
        // as a KeyPressed at all.
        let keymap = keymap_with(&[(36, KEY_RETURN)]);
        assert_eq!(
            translate_key_press(&keymap, KeyCombo { mods: 8, key: 37 }),
            None
        );
    }

    #[test]
    fn grab_keybindings_grabs_every_default_binding_as_lock_variants() {
        // KBR-1 (D1): each of the 32 default bindings (WU1
        // tessera-navigation-bindings adds workspace_prev/workspace_next + 10
        // move_to_workspace; WU2 adds the 4 directional focus bindings —
        // Super+Shift+{h,j,k,l} is a mod combo no other default uses, so all
        // 32 stay pairwise disjoint on (mods, keysym)) is expanded into the
        // 8 lock-variant masks (base | every subset of {2,16,32}), so
        // 32 x 8 = 256 grabs land on the root. Super bindings use the
        // {64,66,80,96,82,98,112,114} mask set; the launcher (Ctrl+Space)
        // uses its disjoint {4,6,20,36,22,38,52,54} set.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let stats = grab_keybindings(&fake, &keymap, ROOT, &Config::default()).unwrap();
        assert_eq!(stats.bindings, 32);
        assert_eq!(stats.grabs, 256);
        assert!(
            stats.missing.is_empty(),
            "a healthy mapping resolves every binding — nothing may be missing"
        );
        let calls = fake.calls();
        assert!(
            calls
                .iter()
                .all(|c| matches!(c, KeyboardCall::Grab { window: ROOT, .. })),
            "every grab must target the root window"
        );
        // The first binding (terminal, Super+Enter) grabs keycode 36 with
        // exactly the eight Super lock-variant masks, in config order.
        let terminal_grabs: Vec<u16> = calls[..8]
            .iter()
            .map(|c| match c {
                KeyboardCall::Grab { modifiers, .. } => *modifiers,
                _ => unreachable!("terminal grabs are the first 8 calls"),
            })
            .collect();
        assert_eq!(terminal_grabs, vec![64, 66, 80, 96, 82, 98, 112, 114]);
        // The launcher (Ctrl+Space) grabs keycode 65 (XK_space) with its
        // disjoint Control mask set — exactly one grab per variant, and no
        // other binding uses a Control mask.
        let launcher_grabs: Vec<(u16, u8)> = calls
            .iter()
            .filter_map(|c| match c {
                KeyboardCall::Grab {
                    modifiers, keycode, ..
                } if [4, 6, 20, 36, 22, 38, 52, 54].contains(modifiers) => {
                    Some((*modifiers, *keycode))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            launcher_grabs,
            vec![
                (4, 65),
                (6, 65),
                (20, 65),
                (36, 65),
                (22, 65),
                (38, 65),
                (52, 65),
                (54, 65),
            ]
        );
    }

    #[test]
    fn grab_keybindings_dedupes_identical_combos_to_one_variant_set() {
        // D1: dedup happens on the EXPANDED (mods, keycode) pair. When two
        // bindings are identical (same mods + keysym -> same keycode), their
        // variant sets coincide, so only 8 grabs land for that keycode
        // instead of 16 (248 grabs total: 31 distinct combos x 8 variants,
        // 32 bindings still configured — WU1+WU2 tessera-navigation-bindings).
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let mut cfg = Config::default();
        cfg.keybindings.launcher = cfg.keybindings.terminal; // Super+Enter twice
        let stats = grab_keybindings(&fake, &keymap, ROOT, &cfg).unwrap();
        assert_eq!(stats.bindings, 32);
        assert_eq!(stats.grabs, 248); // 31 distinct combos x 8 variants
        assert!(
            stats.missing.is_empty(),
            "identical-but-resolved combos are deduped, not missing"
        );
        let keycode_36_grabs = fake
            .calls()
            .iter()
            .filter(|c| matches!(c, KeyboardCall::Grab { keycode: 36, .. }))
            .count();
        assert_eq!(
            keycode_36_grabs, 8,
            "identical combos collapse to one 8-grab set"
        );
    }

    #[test]
    fn grab_keybindings_skips_bindings_without_a_keycode() {
        // A binding whose keysym exists nowhere in the mapping is skipped
        // (nothing to grab, no error); the other 31 bindings still expand to
        // their 8 lock variants (31 x 8 = 248). bindings still counts the
        // CONFIGURED 32 — the stats keep the config-derived count visible
        // when grabs < 256 (KBR-3, D6). This MUST keep breaking `terminal.key`
        // (unshared): breaking a digit keysym instead would make it shared
        // with `move-to-workspace-N`, dropping TWO bindings (30 x 8 = 240,
        // not 248).
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let mut cfg = Config::default();
        cfg.keybindings.terminal.key = 0x9999; // keysym not in the mapping
        let stats = grab_keybindings(&fake, &keymap, ROOT, &cfg).unwrap();
        assert_eq!(stats.bindings, 32);
        assert_eq!(stats.grabs, 248);
        assert_eq!(
            stats.missing,
            vec![("terminal".to_string(), 0x9999)],
            "the unresolved terminal keysym is named with its binding name (KBR-3)"
        );
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, KeyboardCall::Grab { keycode: 36, .. })),
            "the unbound terminal keysym must not produce a grab"
        );
    }

    #[test]
    fn modifier_map_is_fetched_through_the_ops_seam() {
        // D1: the Mod4 diagnosis reaches the server through the same
        // KeyboardOps seam as the keysym table — a scripted
        // `(keycodes_per_modifier, flat keycodes)` reply comes back verbatim
        // and the call is recorded, so the claim flow can be reasoned about
        // in call order.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        fake.script_modifier_map(4, vec![0u8; 32]);
        assert_eq!(fake.modifier_map().unwrap(), (4, vec![0u8; 32]));
        assert_eq!(fake.calls(), vec![KeyboardCall::ModifierMap]);
    }

    #[test]
    fn keysym_name_resolves_the_modifier_names_table() {
        // D2: known modifier keysyms resolve to their X11 names — Super_L is
        // the SUP-1 spec scenario; the L/R variants of the common modifiers
        // round out the table.
        assert_eq!(keysym_name(0xffeb), "Super_L");
        assert_eq!(keysym_name(0xffec), "Super_R");
        assert_eq!(keysym_name(0xffe9), "Alt_L");
        assert_eq!(keysym_name(0xffe3), "Control_L");
    }

    #[test]
    fn keysym_name_falls_back_to_hex_for_unknown_keysyms() {
        // D2: anything outside the table prints as `0x<keysym:hex>` — never
        // a panic, never an empty string, so the claim line stays readable
        // for ANY layout.
        assert_eq!(keysym_name(0x9999), "0x9999");
        assert_eq!(keysym_name(0xffffff), "0xffffff");
    }

    /// A modifier map with `per` keycodes per modifier: the Mod4 row (index
    /// 6 of 8 — Shift, Lock, Control, Mod1, Mod2, Mod3, Mod4, Mod5) carries
    /// `mod4`; every other slot is 0 (unused).
    fn modifier_map_with(per: u8, mod4: &[u8]) -> Vec<u8> {
        let mut map = vec![0u8; usize::from(per) * 8];
        map[usize::from(per) * 6..usize::from(per) * 7].copy_from_slice(mod4);
        map
    }

    #[test]
    fn mod4_keycodes_lists_the_named_keycodes_in_the_mod4_row() {
        // SUP-1 (D2): with 133 (Super_L) and 134 (Super_R) in Mod4, the
        // helper reports exactly those two keycodes with their keysym names,
        // skipping the unused 0 slots in the row.
        let mut keysyms = vec![0u32; 254]; // keycodes 8..=134 (both rows)
        keysyms[usize::from(133u8 - 8u8) * 2] = 0xffeb; // Super_L on 133
        keysyms[usize::from(134u8 - 8u8) * 2] = 0xffec; // Super_R on 134
        let keymap = Keymap::new(8, 2, keysyms);
        let map = modifier_map_with(4, &[133, 134, 0, 0]);
        let mod4 = mod4_keycodes(4, &map, &keymap);
        assert_eq!(
            mod4,
            vec![(133, "Super_L".to_string()), (134, "Super_R".to_string())]
        );
    }

    #[test]
    fn mod4_keycodes_uses_the_hex_fallback_for_an_unknown_keysym() {
        // Triangulation: a Mod4 keycode whose keysym is not in KEYSYM_NAMES
        // still shows up, named by its hex keysym — the row really comes
        // from the modifier map, not from a fixed answer.
        let mut keysyms = vec![0u32; 254]; // keycodes 8..=134
        keysyms[usize::from(133u8 - 8u8) * 2] = 0x9999;
        let keymap = Keymap::new(8, 2, keysyms);
        let map = modifier_map_with(4, &[133, 0, 0, 0]);
        assert_eq!(
            mod4_keycodes(4, &map, &keymap),
            vec![(133, "0x9999".to_string())]
        );
    }

    #[test]
    fn mod4_keycodes_is_empty_when_nothing_is_mapped_to_mod4() {
        // SUP-1 "Mod4 empty": an all-zero Mod4 row and a zero
        // keycodes_per_modifier both yield no entries — the claim logs the
        // WARNING line instead of a diagnosis list.
        let keymap = keymap_with(&[(36, KEY_RETURN)]);
        assert!(
            mod4_keycodes(4, &modifier_map_with(4, &[0, 0, 0, 0]), &keymap).is_empty(),
            "an all-zero Mod4 row must not produce keycode entries"
        );
        assert!(
            mod4_keycodes(0, &[], &keymap).is_empty(),
            "a zero keycodes_per_modifier reply has no rows at all"
        );
    }

    #[test]
    fn translate_key_press_strips_lock_bits_never_alt_or_super() {
        // KBR-2 (D2): lock bits 2|16|32|128 are stripped from the published
        // KeyPressed mods so the core matches on the meaningful modifiers
        // only. Shift(1), Mod1(8) and Mod4(64) are never stripped; Mod5
        // over-matches (accepted for v1 `us`).
        let keymap = default_keymap();
        // (raw mods, keycode, expected stripped mods) — each row is a press
        // one of the lock-variant grabs would deliver.
        let rows = [
            (u32::from(MOD_SUPER), 36, u32::from(MOD_SUPER)), // 64 -> 64
            (u32::from(MOD_SUPER) | 2, 36, u32::from(MOD_SUPER)), // +CapsLock
            (u32::from(MOD_SUPER) | 16, 36, u32::from(MOD_SUPER)), // +NumLock
            (u32::from(MOD_SUPER) | 2 | 16 | 32, 36, u32::from(MOD_SUPER)), // all locks
            (u32::from(MOD_SUPER) | 8, 36, u32::from(MOD_SUPER) | 8), // Mod1 kept
            (u32::from(MOD_SUPER) | 128, 36, u32::from(MOD_SUPER)), // Mod5 over-match
            (4, 65, 4),                                       // Ctrl+Space plain
            (4 | 16, 65, 4),                                  // Ctrl+Space + NumLock
            (4 | 128, 65, 4),                                 // Ctrl+Space + Mod5 over-match
        ];
        for (raw_mods, keycode, expected_mods) in rows {
            let translated = translate_key_press(
                &keymap,
                KeyCombo {
                    mods: raw_mods,
                    key: keycode,
                },
            )
            .expect("the bound keycode must translate");
            let Event::KeyPressed(combo) = translated else {
                panic!("expected a KeyPressed event");
            };
            assert_eq!(
                combo.mods, expected_mods,
                "mods {raw_mods} (keycode {keycode}) must strip to {expected_mods}"
            );
        }
        // The Mod1-preserved row (Super+Alt+Enter) must NOT match any
        // binding: the core sees 72, not the bound 64.
        let super_alt = translate_key_press(
            &keymap,
            KeyCombo {
                mods: u32::from(MOD_SUPER) | 8,
                key: 36,
            },
        )
        .unwrap();
        let Event::KeyPressed(alt_combo) = super_alt else {
            unreachable!()
        };
        assert_eq!(
            command_for_key(&Config::default(), alt_combo),
            None,
            "Super+Alt must not fire the terminal binding"
        );
        // And the Mod5 over-match: AltGr+Ctrl+Space strips to exactly the
        // configured launcher combo (4, XK_space) — the accepted over-match.
        let over = translate_key_press(
            &keymap,
            KeyCombo {
                mods: 4 | 128,
                key: 65,
            },
        )
        .unwrap();
        let Event::KeyPressed(over_combo) = over else {
            unreachable!()
        };
        assert_eq!(
            over_combo,
            Config::default().keybindings.launcher,
            "AltGr+Ctrl+Space must over-match the Ctrl+Space launcher binding"
        );
    }

    #[test]
    fn nosymbol_keycodes_reports_holes_in_the_mapping() {
        // KBR-3: keycodes whose PRIMARY keysym is NoSymbol — the holes a
        // claim log must surface. With one keycode mapped, every other
        // in-range keycode (8..=67) is a hole; keycodes beyond the table are
        // not reported.
        let keymap = keymap_with(&[(36, KEY_RETURN)]);
        let holes = keymap.nosymbol_keycodes();
        assert_eq!(holes.len(), 59); // 60 in-range keycodes minus the mapped one
        assert!(!holes.contains(&36), "the mapped keycode is not a hole");
        assert!(
            holes.contains(&37),
            "an empty slot right after the mapping is a hole"
        );
        assert!(holes.contains(&8), "the first in-range keycode is a hole");
        assert!(holes.contains(&67), "the last in-range keycode is a hole");
    }

    #[test]
    fn nosymbol_keycodes_reports_every_hole_when_the_mapping_is_sparse() {
        // Triangulation: a different hole layout yields a different
        // (non-empty) result — the hole set really comes from the mapping,
        // not from a fixed answer.
        let keymap = keymap_with(&[(36, KEY_RETURN), (65, 0x0020)]);
        let holes = keymap.nosymbol_keycodes();
        assert_eq!(holes.len(), 58);
        assert!(!holes.contains(&36));
        assert!(!holes.contains(&65));
        assert!(holes.contains(&64), "a hole between the mapped keycodes");
    }
}
