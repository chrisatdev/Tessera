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

use tessera_core::{Config, DErr, Event, KeyCombo};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{GrabMode, ModMask, Window, get_keyboard_mapping, grab_key};
use x11rb::rust_connection::RustConnection;

use crate::display_server::{map_conn_error, map_reply_error};

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
pub(crate) fn translate_key_press(keymap: &Keymap, raw: KeyCombo) -> Option<Event> {
    let keysym = keymap.keysym(raw.key as u8);
    if keysym == 0 {
        return None;
    }
    Some(Event::KeyPressed(KeyCombo {
        mods: raw.mods,
        key: keysym,
    }))
}

/// Grabs every configured keybinding on `root` (REQ-x11-008): each binding's
/// keysym is resolved to its keycode(s) through `keymap` and grabbed with the
/// binding's modifier mask. Bindings whose keysym exists nowhere in the
/// mapping are skipped, and a (mods, keycode) pair is only grabbed once.
/// Returns how many grabs took effect.
pub(crate) fn grab_keybindings(
    ops: &impl KeyboardOps,
    keymap: &Keymap,
    root: Window,
    cfg: &Config,
) -> Result<usize, DErr> {
    let k = &cfg.keybindings;
    let fixed = [
        k.terminal,
        k.focus_next,
        k.focus_prev,
        k.close,
        k.toggle_layout,
    ];
    let mut grabbed: Vec<(u16, u8)> = Vec::new();
    for combo in fixed.iter().chain(k.workspace.iter()) {
        for keycode in keymap.keycodes_for_keysym(combo.key) {
            let pair = (combo.mods as u16, keycode);
            if !grabbed.contains(&pair) {
                ops.grab_key(root, pair.0, pair.1)?;
                grabbed.push(pair);
            }
        }
    }
    Ok(grabbed.len())
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
    fn grab_keybindings_grabs_every_default_binding() {
        // REQ-x11-008: each configured binding is converted from keysym to
        // keycode(s) and grabbed on the root with its modifier mask. All 15
        // default bindings grab exactly once, in config order.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let grabbed = grab_keybindings(&fake, &keymap, ROOT, &Config::default()).unwrap();
        assert_eq!(grabbed, 15);
        assert_eq!(
            fake.calls(),
            vec![
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 36
                }, // terminal
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 44
                }, // focus_next
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 45
                }, // focus_prev
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 24
                }, // close
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 65
                }, // toggle_layout
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 10
                }, // ws 1
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 11
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 12
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 13
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 14
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 15
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 16
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 17
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 18
                },
                KeyboardCall::Grab {
                    window: ROOT,
                    modifiers: MOD_SUPER,
                    keycode: 19
                }, // ws 10
            ]
        );
    }

    #[test]
    fn grab_keybindings_skips_bindings_without_a_keycode() {
        // A binding whose keysym exists nowhere in the mapping is skipped
        // (nothing to grab, no error); the other bindings still grab.
        let fake = FakeKeyboardOps::new((8, 67), 2, vec![0u32; 120]);
        let keymap = default_keymap();
        let mut cfg = Config::default();
        cfg.keybindings.terminal.key = 0x9999; // keysym not in the mapping
        let grabbed = grab_keybindings(&fake, &keymap, ROOT, &cfg).unwrap();
        assert_eq!(grabbed, 14);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| matches!(c, KeyboardCall::Grab { keycode: 36, .. })),
            "the unbound terminal keysym must not produce a grab"
        );
    }
}
