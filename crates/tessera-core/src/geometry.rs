//! Core geometry and window/workspace identifiers.

use std::cmp::{max, min};

/// Identifier of an X11 window (client or frame).
pub type WindowId = u32;
/// Identifier of a workspace (1-based, `_NET_NUMBER_OF_DESKTOPS`-compatible).
pub type WorkspaceId = u32;

/// Axis-aligned rectangle in root-window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u16,
    pub h: u16,
}

/// Placement of one window inside a layout area, including its frame border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub window: WindowId,
    pub rect: Rect,
    pub border: u16,
}

/// Layout algorithms supported by the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    MasterStack,
}

/// Direction of a geometric focus move (DF-1). Screen axes: `Left` is -x,
/// `Up` is -y.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// `(left, right, top, bottom)` edges of `r`, widened to `i64` up front
/// (design D1): `x: i32` plus `w: u16` cannot overflow `i64`, so every
/// comparison below is panic-free — no cast, no indexing, no division.
fn edges(r: Rect) -> (i64, i64, i64, i64) {
    let l = i64::from(r.x);
    let t = i64::from(r.y);
    (l, l + i64::from(r.w), t, t + i64::from(r.h))
}

/// A placement is visible iff both dimensions are non-zero (D4).
/// `MasterStack::inset` and `configure_frame` both clamp to zero on a tiny
/// frame, so a degenerate rect is reachable, not hypothetical.
fn is_visible(p: &Placement) -> bool {
    p.rect.w > 0 && p.rect.h > 0
}

/// The window geometrically adjacent to `focused` in `dir` (DF-1), or `None`
/// when no candidate qualifies (DF-2 — the caller must then change nothing:
/// no wrap, unlike workspace stepping).
///
/// Pure and TOTAL over `&[Placement]`: no workspace state, no MRU, no
/// layout, no X. The result is INVARIANT UNDER PERMUTATION of `placements`
/// (D3, pinned by an exhaustive permutation test) — so a layout that emits
/// its placements in a different order can never silently change where focus
/// lands.
///
/// Admission (D2) is a same-direction edge test PLUS strictly positive
/// perpendicular-axis overlap; the edge test uses `>=`, not `>`, because a
/// zero-border config puts two adjacent windows' touching edges at the same
/// coordinate. Ranking (D3) is the lexicographic minimum of `(gap,
/// perpendicular_start, window_id)` — nearest edge, then topmost/leftmost
/// (reading order), then the id as a totality backstop (needed for the
/// permutation-invariance proof, unreachable while placements are disjoint).
///
/// Overlap MAGNITUDE is never a ranking key, only an admission predicate:
/// the last stack slice absorbs `MasterStack::arrange`'s division remainder
/// (`master_stack.rs`, stack height `area.h / stack_n`), so ranking by
/// overlap size would flip which window wins with the PARITY of the area
/// height — a 1081-tall area sends focus to the bottom window, a 1080-tall
/// one to the top. A parity-dependent focus target is unreviewable; nearest
/// edge is not.
pub fn resolve_direction(
    placements: &[Placement],
    focused: WindowId,
    dir: Direction,
) -> Option<WindowId> {
    let f = placements.iter().find(|p| p.window == focused)?;
    if !is_visible(f) {
        return None;
    }
    let (fl, fr, ft, fb) = edges(f.rect);
    placements
        .iter()
        .filter(|c| c.window != focused && is_visible(c))
        .filter_map(|c| {
            let (cl, cr, ct, cb) = edges(c.rect);
            let (admitted, gap, perp_start, perp_overlap) = match dir {
                Direction::Right => (cl >= fr, cl - fr, ct, min(fb, cb) - max(ft, ct)),
                Direction::Left => (cr <= fl, fl - cr, ct, min(fb, cb) - max(ft, ct)),
                Direction::Down => (ct >= fb, ct - fb, cl, min(fr, cr) - max(fl, cl)),
                Direction::Up => (cb <= ft, ft - cb, cl, min(fr, cr) - max(fl, cl)),
            };
            (admitted && perp_overlap > 0).then_some((gap, perp_start, c.window))
        })
        .min_by_key(|&(gap, perp_start, id)| (gap, perp_start, id))
        .map(|(_, _, window)| window)
}

#[cfg(test)]
mod tests {

    use crate::layout::{Layout, MasterStack};

    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 800,
        h: 600,
    };

    /// D5 case-table input: `MasterStack::new(0.5, 2).arrange(&[1, 2, 3], AREA, 0)`,
    /// never hand-typed rects — so the goldens cannot drift from the layout.
    fn goldens() -> Vec<Placement> {
        MasterStack::new(0.5, 2).arrange(&[1, 2, 3], AREA, 0)
    }

    #[test]
    fn resolve_direction_over_the_master_stack_goldens() {
        let p = goldens();
        // C1: master -> right is THE TIE (gap and overlap both equal between
        // the two stack windows); key 2 (topmost) separates them (D3).
        assert_eq!(resolve_direction(&p, 1, Direction::Right), Some(2));
        // C2: master -> left has no candidate at all.
        assert_eq!(resolve_direction(&p, 1, Direction::Left), None);
        // C3/C4: no candidate below/above a full-height master — the stack
        // sits to the RIGHT, not below (README-documented surprise, task 3.1).
        assert_eq!(resolve_direction(&p, 1, Direction::Down), None);
        assert_eq!(resolve_direction(&p, 1, Direction::Up), None);
        // C5: stack-top -> left reaches the master.
        assert_eq!(resolve_direction(&p, 2, Direction::Left), Some(1));
        // C6: stack-top -> down reaches stack-bottom (master excluded by admission).
        assert_eq!(resolve_direction(&p, 2, Direction::Down), Some(3));
        // C7: stack-top has no up or right candidate.
        assert_eq!(resolve_direction(&p, 2, Direction::Up), None);
        assert_eq!(resolve_direction(&p, 2, Direction::Right), None);
        // C8: stack-bottom -> left reaches the master.
        assert_eq!(resolve_direction(&p, 3, Direction::Left), Some(1));
        // C9: stack-bottom -> up reaches stack-top.
        assert_eq!(resolve_direction(&p, 3, Direction::Up), Some(2));
    }

    #[test]
    fn resolve_direction_breaks_the_stack_tie_by_topmost() {
        // C1 isolated (D3 key 2): both stack candidates admit at the SAME
        // gap (4) and the SAME perpendicular overlap (296) — largest-overlap
        // could not separate them even if it were a rank key (and D3
        // deliberately rejects overlap magnitude as one, see resolve_direction's
        // doc comment). Only `t(c)` (topmost) tells them apart: window 2 must
        // win, never window 3.
        let master = Placement {
            window: 1,
            rect: Rect {
                x: 2,
                y: 2,
                w: 396,
                h: 596,
            },
            border: 2,
        };
        let top = Placement {
            window: 2,
            rect: Rect {
                x: 402,
                y: 2,
                w: 396,
                h: 296,
            },
            border: 2,
        };
        let bottom = Placement {
            window: 3,
            rect: Rect {
                x: 402,
                y: 302,
                w: 396,
                h: 296,
            },
            border: 2,
        };
        let placements = vec![master, top, bottom];
        assert_eq!(resolve_direction(&placements, 1, Direction::Right), Some(2));
    }

    #[test]
    fn resolve_direction_admits_a_touching_edge_at_zero_border() {
        // C10: with border 0, the master's right edge and the stack's left
        // edge are the SAME coordinate (gap 0) — proves admission uses `>=`,
        // not `>`. A strict `>` would leave directional focus dead on any
        // zero-border config.
        let p = MasterStack::new(0.5, 0).arrange(&[1, 2, 3], AREA, 0);
        assert_eq!(resolve_direction(&p, 1, Direction::Right), Some(2));
    }

    #[test]
    fn resolve_direction_returns_none_without_a_candidate() {
        // C2-C4, isolated: no candidate in `dir` is a no-op, not a wrap — a
        // lone window (nothing exists in any direction) proves the "no
        // candidate" path never invents one, in every direction at once.
        let solo = MasterStack::new(0.5, 2).arrange(&[1], AREA, 0);
        for dir in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(resolve_direction(&solo, 1, dir), None);
        }
    }

    #[test]
    fn resolve_direction_ignores_zero_size_placements() {
        // C11 (D4): `configure_frame` clamps a client to zero size on a tiny
        // frame, so a zero-width/zero-height placement is reachable. It must
        // never be returned as a candidate, and a zero-size FOCUSED
        // placement must resolve to `None` rather than panicking or picking
        // nonsense.
        let visible = Placement {
            window: 1,
            rect: Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            border: 0,
        };
        let zero_w = Placement {
            window: 2,
            rect: Rect {
                x: 100,
                y: 0,
                w: 0,
                h: 100,
            },
            border: 0,
        };
        let zero_h = Placement {
            window: 3,
            rect: Rect {
                x: 0,
                y: 100,
                w: 100,
                h: 0,
            },
            border: 0,
        };
        assert_eq!(
            resolve_direction(&[visible, zero_w], 1, Direction::Right),
            None,
            "a zero-width candidate must never be returned"
        );
        assert_eq!(
            resolve_direction(&[visible, zero_h], 1, Direction::Down),
            None,
            "a zero-height candidate must never be returned"
        );
        assert_eq!(
            resolve_direction(&[zero_w, visible], 2, Direction::Right),
            None,
            "a zero-size FOCUSED placement must resolve to None, not panic"
        );
    }

    /// D3: the tie-break tuple `(gap, perpendicular_start, window_id)` is what
    /// makes the result INVARIANT under permutation of `placements` — a proof,
    /// not a doc comment, that the resolver never silently depends on slice
    /// order (the rejected MRU-via-slice-order rule this replaces would have
    /// failed this property the moment a layout reordered its output).
    ///
    /// Three placements have exactly six orderings, so the property is proven
    /// by ENUMERATING them rather than sampling: a plain loop is total here
    /// where a generator would only be probabilistic, and it costs no
    /// dependency.
    #[test]
    fn resolve_direction_is_invariant_under_input_permutation() {
        const PERMUTATIONS: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let placements = goldens();
        for perm in PERMUTATIONS {
            let shuffled: Vec<Placement> = perm.iter().map(|&i| placements[i]).collect();
            for dir in [
                Direction::Left,
                Direction::Right,
                Direction::Up,
                Direction::Down,
            ] {
                for focused in [1u32, 2, 3] {
                    assert_eq!(
                        resolve_direction(&placements, focused, dir),
                        resolve_direction(&shuffled, focused, dir),
                        "permutation {perm:?} changed the {dir:?} target from window {focused}"
                    );
                }
            }
        }
    }
}
