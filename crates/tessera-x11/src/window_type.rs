//! `_NET_WM_WINDOW_TYPE` classification (design D5): resolves a window's
//! [`WindowKind`] from its EWMH type property, so the core can decide its
//! [`tessera_core::ManagePolicy`] before the window is ever framed or tiled
//! (spec "Map-Time Classification, Once"). Every X side effect goes through
//! the [`WindowTypeOps`] seam so the classification is scriptable headless;
//! [`RustConnection`] implements it directly.
//!
//! A NEW seam rather than extending [`crate::ewmh::EwmhOps`] (design D5): the
//! EWMH seam is a root-window WRITE surface (desktop property sync) and this
//! is a client-window READ surface — handing one the other's capability
//! would blur two different responsibilities, and `FakeEwmhOps` panics on
//! any interned name outside its own four, so reusing it would force edits
//! to a fake whose exact-sequence tests must stay green.

use tessera_core::{DErr, WindowKind};
use x11rb::protocol::xproto::{Atom, AtomEnum, Window, get_property, intern_atom};
use x11rb::rust_connection::RustConnection;

use crate::display_server::{map_conn_error, map_reply_error};

/// Upper bound on ATOM values read from `_NET_WM_WINDOW_TYPE`, a
/// client-controlled property: a hostile client cannot force a large
/// allocation through it (32 atoms = 128 bytes).
const MAX_TYPE_ATOMS: u32 = 32;

/// X atom NAME -> core kind. Atom vocabulary is X's job (this table); the
/// POLICY grouping is the core's (`tessera_core::window_kind::POLICIES`) —
/// two different facts, one direction each. One entry per
/// [`WindowKind::ALL`] member.
const TYPE_ATOMS: &[(&str, WindowKind)] = &[
    ("_NET_WM_WINDOW_TYPE_NOTIFICATION", WindowKind::Notification),
    ("_NET_WM_WINDOW_TYPE_TOOLTIP", WindowKind::Tooltip),
    (
        "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
        WindowKind::DropdownMenu,
    ),
    ("_NET_WM_WINDOW_TYPE_POPUP_MENU", WindowKind::PopupMenu),
    ("_NET_WM_WINDOW_TYPE_MENU", WindowKind::Menu),
    ("_NET_WM_WINDOW_TYPE_DOCK", WindowKind::Dock),
    ("_NET_WM_WINDOW_TYPE_SPLASH", WindowKind::Splash),
    ("_NET_WM_WINDOW_TYPE_DIALOG", WindowKind::Dialog),
    ("_NET_WM_WINDOW_TYPE_UTILITY", WindowKind::Utility),
    ("_NET_WM_WINDOW_TYPE_TOOLBAR", WindowKind::Toolbar),
    ("_NET_WM_WINDOW_TYPE_NORMAL", WindowKind::Normal),
];

/// The X surface classification needs, abstracted so it is scriptable
/// headless (same seam shape as [`crate::ewmh::EwmhOps`] / [`crate::frames::FrameOps`]).
pub(crate) trait WindowTypeOps {
    /// Interns a named atom and returns its id.
    fn intern(&self, name: &str) -> Result<Atom, DErr>;
    /// Reads a format-32 ATOM list property on `window`. An absent property,
    /// or one of the wrong type/format, resolves to an empty list rather
    /// than an error — "no usable value" is not a protocol failure.
    fn atom_property(&self, window: Window, property: Atom) -> Result<Vec<Atom>, DErr>;
}

impl WindowTypeOps for RustConnection {
    fn intern(&self, name: &str) -> Result<Atom, DErr> {
        let cookie = intern_atom(self, false, name.as_bytes()).map_err(map_conn_error)?;
        cookie
            .reply()
            .map(|reply| reply.atom)
            .map_err(map_reply_error)
    }
    fn atom_property(&self, window: Window, property: Atom) -> Result<Vec<Atom>, DErr> {
        let cookie = get_property(
            self,
            false,
            window,
            property,
            AtomEnum::ATOM,
            0,
            MAX_TYPE_ATOMS,
        )
        .map_err(map_conn_error)?;
        let reply = cookie.reply().map_err(map_reply_error)?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }
}

/// Resolves `window`'s [`WindowKind`] from `_NET_WM_WINDOW_TYPE` (spec
/// "Map-Time Classification, Once"). EWMH orders the property's value list
/// by client preference: the FIRST recognized atom wins, unrecognized atoms
/// are skipped. An absent property, an empty list, or a list holding only
/// unrecognized atoms all resolve to [`WindowKind::Normal`] — the fail-safe
/// default (spec "Fail-Safe Default to Normal"), not an error.
pub(crate) fn window_kind(ops: &impl WindowTypeOps, window: Window) -> Result<WindowKind, DErr> {
    let net_wm_window_type = ops.intern("_NET_WM_WINDOW_TYPE")?;
    let value = ops.atom_property(window, net_wm_window_type)?;
    if value.is_empty() {
        return Ok(WindowKind::Normal);
    }
    // Interned lazily, only once the property actually carries a value: up
    // to 1 + 11 interns per classified MapRequest (Known Limitation, no
    // caching — matches `ewmh::set_desktops`' existing convention).
    let mut recognized = Vec::with_capacity(TYPE_ATOMS.len());
    for &(name, kind) in TYPE_ATOMS {
        recognized.push((ops.intern(name)?, kind));
    }
    for atom in value {
        if let Some(&(_, kind)) = recognized.iter().find(|&&(id, _)| id == atom) {
            return Ok(kind);
        }
    }
    Ok(WindowKind::Normal)
}

#[cfg(test)]
mod tests {
    //! RED (D5): `window_kind` must resolve the FIRST recognized atom in the
    //! client's preference order, default to `Normal` when nothing is
    //! recognized (or the property is absent), and surface a property-read
    //! failure as `DErr` instead of silently defaulting.

    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use super::*;

    const WINDOW: Window = 0x0000_1000;

    /// Scripted `WindowTypeOps`: assigns a fresh deterministic atom id to
    /// each newly-seen name (mirrors a real server's monotonic allocation —
    /// no test needs to know exact numeric values) and records a scripted
    /// `_NET_WM_WINDOW_TYPE` value list per window.
    struct FakeWindowTypeOps {
        atoms: RefCell<HashMap<String, Atom>>,
        next_atom: Cell<Atom>,
        properties: HashMap<Window, Vec<Atom>>,
        fail_property: Vec<Window>,
    }

    impl FakeWindowTypeOps {
        fn new() -> Self {
            FakeWindowTypeOps {
                atoms: RefCell::new(HashMap::new()),
                next_atom: Cell::new(1),
                properties: HashMap::new(),
                fail_property: Vec::new(),
            }
        }

        /// The atom id `intern` would assign to `name`, precomputed so a
        /// test can script a property value that resolves through the SAME
        /// name -> id mapping the code under test will see.
        fn atom_for(&self, name: &str) -> Atom {
            if let Some(&id) = self.atoms.borrow().get(name) {
                return id;
            }
            let id = self.next_atom.get();
            self.next_atom.set(id + 1);
            self.atoms.borrow_mut().insert(name.to_string(), id);
            id
        }

        /// Scripts `window`'s `_NET_WM_WINDOW_TYPE` value list, in the given
        /// client-preference order.
        fn set_property(&mut self, window: Window, type_names: &[&str]) {
            let ids = type_names.iter().map(|name| self.atom_for(name)).collect();
            self.properties.insert(window, ids);
        }

        /// Scripts the property read for `window` to fail.
        fn fail_property_read(&mut self, window: Window) {
            self.fail_property.push(window);
        }
    }

    impl WindowTypeOps for FakeWindowTypeOps {
        fn intern(&self, name: &str) -> Result<Atom, DErr> {
            Ok(self.atom_for(name))
        }
        fn atom_property(&self, window: Window, _property: Atom) -> Result<Vec<Atom>, DErr> {
            if self.fail_property.contains(&window) {
                return Err(DErr::X(format!("scripted failure: atom_property {window}")));
            }
            Ok(self.properties.get(&window).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn first_recognized_atom_in_preference_order_wins() {
        // The client lists an unrecognized atom, then TOOLTIP, then
        // NOTIFICATION — TOOLTIP must win because it appears FIRST in the
        // client's own preference order, not because of TYPE_ATOMS' order
        // (which lists NOTIFICATION before TOOLTIP).
        let mut fake = FakeWindowTypeOps::new();
        let unknown = fake.atom_for("_SOME_VENDOR_EXTENSION");
        fake.properties.insert(
            WINDOW,
            vec![
                unknown,
                fake.atom_for("_NET_WM_WINDOW_TYPE_TOOLTIP"),
                fake.atom_for("_NET_WM_WINDOW_TYPE_NOTIFICATION"),
            ],
        );
        assert_eq!(window_kind(&fake, WINDOW).unwrap(), WindowKind::Tooltip);
    }

    #[test]
    fn absent_or_unknown_window_type_is_normal() {
        // Triangulation: an absent property (no scripted value at all) and a
        // property holding only unrecognized atoms both resolve to Normal —
        // the same fail-safe outcome from two different real-world causes.
        let mut fake = FakeWindowTypeOps::new();
        assert_eq!(window_kind(&fake, WINDOW).unwrap(), WindowKind::Normal);

        fake.set_property(WINDOW, &["_SOME_VENDOR_EXTENSION", "_ANOTHER_UNKNOWN_ONE"]);
        assert_eq!(window_kind(&fake, WINDOW).unwrap(), WindowKind::Normal);
    }

    #[test]
    fn property_read_failure_is_reported_as_derr() {
        let mut fake = FakeWindowTypeOps::new();
        fake.fail_property_read(WINDOW);
        assert_eq!(
            window_kind(&fake, WINDOW).unwrap_err(),
            DErr::X(format!("scripted failure: atom_property {WINDOW}"))
        );
    }
}
