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
use x11rb::protocol::xproto::{GrabMode, ModMask, Window, get_keyboard_mapping, grab_key};
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

/// Result of a keybinding grab pass (D6, KBR-3): the CONFIGURED binding count
/// and the ACTUAL grab count — `grabs` drops below `bindings * 8` when a
/// binding's keysym resolves nowhere in the mapping (unresolved) or two
/// identical combos collapse onto one expanded (mods, keycode) pair.
pub(crate) struct GrabStats {
    pub bindings: usize,
    pub grabs: usize,
}

/// Grabs every configured keybinding on `root` (REQ-x11-008): each binding's
/// keysym is resolved to its keycode(s) through `keymap` and grabbed with
/// every lock-variant mask (`base | variant` for each of the 8
/// `LOCK_VARIANTS`) so an exact-modifier grab fires under any lock
/// combination (KBR-1, D1). Bindings whose keysym exists nowhere in the
/// mapping are skipped, and an expanded `(mods, keycode)` pair is only
/// grabbed once. Returns the configured binding count plus how many grabs
/// took effect (KBR-3). Grab failures abort loudly — a silent drop would
/// recreate the silent-binding-death class this change exists to fix.
pub(crate) fn grab_keybindings(
    ops: &impl KeyboardOps,
    keymap: &Keymap,
    root: Window,
    cfg: &Config,
) -> Result<GrabStats, DErr> {
    let k = &cfg.keybindings;
    let fixed = [
        k.terminal,
        k.focus_next,
        k.focus_prev,
        k.close,
        k.toggle_layout,
        k.launcher,
    ];
    let mut grabbed: Vec<(u16, u8)> = Vec::new();
    for combo in fixed.iter().chain(k.workspace.iter()) {
        for keycode in keymap.keycodes_for_keysym(combo.key) {
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
        bindings: fixed.len() + k.workspace.len(),
        grabs: grabbed.len(),
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
    }

    /// Scripted `KeyboardOps`: a fixed (range, keysym table) pair plus a
    /// recording grab log.
    struct FakeKeyboardOps {
        calls: RefCell<Vec<KeyboardCall>>,
        range: (u8, u8),
        keysyms_per_keycode: u8,
        keysyms: Vec<u32>,
    }

    impl FakeKeyboardOps {
        fn new(range: (u8, u8), keysyms_per_keycode: u8, keysyms: Vec<u32>) -> Self {
            FakeKeyboardOps {
                calls: RefCell::new(Vec::new()),
                range,
                keysyms_per_keycode,
                keysyms,
            }
        }

        fn calls(&self) -> Vec<KeyboardCall> {
            self.calls.borrow().clone()
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
        // KBR-1 (D1): each of the 16 default bindings is expanded into the 8
        // lock-variant masks (base | every subset of {2,16,32}), so 16 x 8 =
        // 128 grabs land on the root. Super bindings use the
        // {64,66,80,96,82,98,112,114} mask set; the launcher (Ctrl+Space)
        // uses its disjoint {4,6,20,36,22,38,52,54} set.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let stats = grab_keybindings(&fake, &keymap, ROOT, &Config::default()).unwrap();
        assert_eq!(stats.bindings, 16);
        assert_eq!(stats.grabs, 128);
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
        // The last binding (launcher, Ctrl+Space) grabs keycode 65
        // (XK_space) with its disjoint Control mask set.
        let launcher_grabs: Vec<u16> = calls[calls.len() - 8..]
            .iter()
            .map(|c| match c {
                KeyboardCall::Grab { modifiers, .. } => *modifiers,
                _ => unreachable!("launcher grabs are the last 8 calls"),
            })
            .collect();
        assert_eq!(launcher_grabs, vec![4, 6, 20, 36, 22, 38, 52, 54]);
    }

    #[test]
    fn grab_keybindings_dedupes_identical_combos_to_one_variant_set() {
        // D1: dedup happens on the EXPANDED (mods, keycode) pair. When two
        // bindings are identical (same mods + keysym -> same keycode), their
        // variant sets coincide, so only 8 grabs land for that keycode
        // instead of 16 (120 grabs total, 16 bindings still configured).
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let mut cfg = Config::default();
        cfg.keybindings.launcher = cfg.keybindings.terminal; // Super+Enter twice
        let stats = grab_keybindings(&fake, &keymap, ROOT, &cfg).unwrap();
        assert_eq!(stats.bindings, 16);
        assert_eq!(stats.grabs, 120); // 15 distinct combos x 8 variants
        let keycode_36_grabs = fake
            .calls()
            .iter()
            .filter(|c| matches!(c, KeyboardCall::Grab { keycode: 36, .. }))
            .count();
        assert_eq!(keycode_36_grabs, 8, "identical combos collapse to one 8-grab set");
    }

    #[test]
    fn grab_keybindings_skips_bindings_without_a_keycode() {
        // A binding whose keysym exists nowhere in the mapping is skipped
        // (nothing to grab, no error); the other 15 bindings still expand to
        // their 8 lock variants (15 x 8 = 120). bindings still counts the
        // CONFIGURED 16 — the stats keep the config-derived count visible
        // when grabs < 128 (KBR-3, D6).
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let mut cfg = Config::default();
        cfg.keybindings.terminal.key = 0x9999; // keysym not in the mapping
        let stats = grab_keybindings(&fake, &keymap, ROOT, &cfg).unwrap();
        assert_eq!(stats.bindings, 16);
        assert_eq!(stats.grabs, 120);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, KeyboardCall::Grab { keycode: 36, .. })),
            "the unbound terminal keysym must not produce a grab"
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
            (u32::from(MOD_SUPER), 36, u32::from(MOD_SUPER)),          // 64 -> 64
            (u32::from(MOD_SUPER) | 2, 36, u32::from(MOD_SUPER)),      // +CapsLock
            (u32::from(MOD_SUPER) | 16, 36, u32::from(MOD_SUPER)),     // +NumLock
            (u32::from(MOD_SUPER) | 2 | 16 | 32, 36, u32::from(MOD_SUPER)), // all locks
            (u32::from(MOD_SUPER) | 8, 36, u32::from(MOD_SUPER) | 8),  // Mod1 kept
            (u32::from(MOD_SUPER) | 128, 36, u32::from(MOD_SUPER)),    // Mod5 over-match
            (4, 65, 4),     // Ctrl+Space plain
            (4 | 16, 65, 4), // Ctrl+Space + NumLock
            (4 | 128, 65, 4), // Ctrl+Space + Mod5 over-match
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
        let over = translate_key_press(&keymap, KeyCombo { mods: 4 | 128, key: 65 }).unwrap();
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
        assert!(holes.contains(&37), "an empty slot right after the mapping is a hole");
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
