//! Window classification and its manage policy (design D1/D4).
//!
//! `WindowKind` is the EWMH-flavored kind of a window (`_NET_WM_WINDOW_TYPE`),
//! resolved by the display layer at map time. `ManagePolicy` is the core's
//! only reaction to it: tile it like every window today, or map it raw and
//! leave workspace state untouched. This module is pure — no X types, no
//! `DisplayServer` — so it stays testable without a display at all.

/// What the core does with a classified window (design D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagePolicy {
    /// Framed, tiled, and focusable — today's exact behavior.
    Tile,
    /// Mapped and raised, never framed, tiled, or focused (spec
    /// "Ignore-But-Map Policy").
    MapOnly,
}

/// The EWMH-flavored kind of a window, resolved once at map time (spec
/// "Map-Time Classification, Once").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKind {
    Normal,
    Dialog,
    Utility,
    Toolbar,
    Notification,
    Tooltip,
    DropdownMenu,
    PopupMenu,
    Menu,
    Dock,
    Splash,
}

/// THE single choke point (D4): moving a kind between groups, or adding a
/// new one, is a one-line edit here (plus its entry in [`WindowKind::ALL`]).
/// A `const` table instead of an exhaustive `match` because the settled
/// decision requires the grouping to be DATA; the compile-time
/// exhaustiveness a `match` would give for free is recovered instead by
/// [`tests::every_kind_maps_to_exactly_one_policy`].
const POLICIES: &[(WindowKind, ManagePolicy)] = &[
    (WindowKind::Normal, ManagePolicy::Tile),
    (WindowKind::Dialog, ManagePolicy::Tile),
    (WindowKind::Utility, ManagePolicy::Tile),
    (WindowKind::Toolbar, ManagePolicy::Tile),
    (WindowKind::Notification, ManagePolicy::MapOnly),
    (WindowKind::Tooltip, ManagePolicy::MapOnly),
    (WindowKind::DropdownMenu, ManagePolicy::MapOnly),
    (WindowKind::PopupMenu, ManagePolicy::MapOnly),
    (WindowKind::Menu, ManagePolicy::MapOnly),
    (WindowKind::Dock, ManagePolicy::MapOnly),
    (WindowKind::Splash, ManagePolicy::MapOnly),
];

impl WindowKind {
    /// Every kind, for exhaustiveness tests and the X-side atom round trip
    /// (D5, Unit 2). Order matches [`POLICIES`]; not otherwise significant.
    pub const ALL: &'static [WindowKind] = &[
        WindowKind::Normal,
        WindowKind::Dialog,
        WindowKind::Utility,
        WindowKind::Toolbar,
        WindowKind::Notification,
        WindowKind::Tooltip,
        WindowKind::DropdownMenu,
        WindowKind::PopupMenu,
        WindowKind::Menu,
        WindowKind::Dock,
        WindowKind::Splash,
    ];

    /// The policy this kind resolves to. Falls back to [`ManagePolicy::Tile`]
    /// (the fail-safe direction, D4) instead of panicking when a kind is
    /// somehow missing from [`POLICIES`] — a table hole is instead caught by
    /// [`tests::every_kind_maps_to_exactly_one_policy`].
    pub fn policy(self) -> ManagePolicy {
        POLICIES
            .iter()
            .find(|&&(kind, _)| kind == self)
            .map_or(ManagePolicy::Tile, |&(_, policy)| policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_maps_to_exactly_one_policy() {
        // D4: `POLICIES` is DATA, not an exhaustive `match`, so nothing at
        // compile time stops a kind from being added to `WindowKind` without
        // a matching row (or added twice). This test recovers the guarantee
        // an exhaustive match would have given for free: every kind in
        // `ALL` must resolve through exactly one row of `POLICIES`.
        assert_eq!(WindowKind::ALL.len(), POLICIES.len());
        for &kind in WindowKind::ALL {
            let matches = POLICIES.iter().filter(|&&(k, _)| k == kind).count();
            assert_eq!(matches, 1, "{kind:?} must appear in POLICIES exactly once");
        }
    }

    #[test]
    fn ignore_group_is_map_only_and_tiled_group_is_tile() {
        // Spec "Ignore-But-Map Policy" / "Tiled Policy Is Unchanged": the two
        // named groups resolve to their expected policy, not a default that
        // happens to agree with both.
        let tiled = [
            WindowKind::Normal,
            WindowKind::Dialog,
            WindowKind::Utility,
            WindowKind::Toolbar,
        ];
        let ignored = [
            WindowKind::Notification,
            WindowKind::Tooltip,
            WindowKind::DropdownMenu,
            WindowKind::PopupMenu,
            WindowKind::Menu,
            WindowKind::Dock,
            WindowKind::Splash,
        ];
        for kind in tiled {
            assert_eq!(kind.policy(), ManagePolicy::Tile, "{kind:?} must tile");
        }
        for kind in ignored {
            assert_eq!(
                kind.policy(),
                ManagePolicy::MapOnly,
                "{kind:?} must be map-only"
            );
        }
    }
}
