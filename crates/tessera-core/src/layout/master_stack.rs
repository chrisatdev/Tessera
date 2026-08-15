//! Master-stack layout (design D5): the focused window occupies a master area
//! on the left at a configurable ratio; the remaining windows tile vertically
//! in a stack on the right.

use crate::geometry::{Placement, Rect, WindowId};
use crate::layout::Layout;

/// Master ratio every layout is built with today (design "default 0.5"): the
/// config has no `ratio` key yet, so [`App::new`](crate::App) and
/// [`MasterStack::default`] share this one constant instead of two literals
/// that could drift apart.
pub const DEFAULT_MASTER_RATIO: f64 = 0.5;

/// Master-stack layout with a configurable master ratio, frame border and gap.
pub struct MasterStack {
    ratio: f64,
    border: u16,
    gaps: u16,
}

impl MasterStack {
    /// Creates a master-stack layout with the given master `ratio` (default
    /// 0.5), `border` width baked into every placement (D5, REQ-lay-002) and
    /// `gaps` shrinking every cell (REQ-lay-004).
    pub fn new(ratio: f64, border: u16, gaps: u16) -> Self {
        MasterStack {
            ratio,
            border,
            gaps,
        }
    }

    /// Shrinks a layout CELL into the placement footprint by applying `gaps`
    /// on every side, clamped to zero size.
    ///
    /// The border is NOT subtracted here: `Placement::rect` is the OUTER box
    /// the window occupies, border included (see [`Placement`]), and X draws
    /// a frame's border outside its `w x h`, so subtracting it twice is
    /// exactly the off-screen-border bug this helper replaced. The single
    /// border subtraction lives at the X boundary (`configure_frame`).
    ///
    /// Adjacent cells SHARE an edge, so the visible distance between two
    /// neighbouring windows is `2 * gaps` while a window against the screen
    /// edge keeps a single `gaps` margin: `gaps = 3` means 6px between
    /// windows and 3px at the edges.
    ///
    /// A gap wider than the cell clamps the size to zero AND caps the origin
    /// shift at the cell's own width/height, so a degenerate placement can
    /// never poke out of the area it was cut from (the containment invariant
    /// the proptest pins).
    fn gapped(&self, cell: Rect) -> Rect {
        let g = i32::from(self.gaps);
        let dx = g.min(i32::from(cell.w));
        let dy = g.min(i32::from(cell.h));
        Rect {
            x: cell.x.saturating_add(dx),
            y: cell.y.saturating_add(dy),
            w: (i32::from(cell.w) - 2 * g).max(0) as u16,
            h: (i32::from(cell.h) - 2 * g).max(0) as u16,
        }
    }
}

impl Default for MasterStack {
    /// Default master ratio 0.5, border 2 and gaps 3 — the config defaults
    /// (`general.border_width`, `general.gaps`), so a layout built for tests
    /// tiles exactly like the one [`App::new`](crate::App) builds from a
    /// default config.
    fn default() -> Self {
        MasterStack::new(DEFAULT_MASTER_RATIO, 2, 3)
    }
}

impl Layout for MasterStack {
    fn name(&self) -> &'static str {
        "master-stack"
    }

    /// Partitions `area` into cells that tile it EXACTLY and contiguously
    /// (REQ-lay-004) — master on the left, stack slices on the right — and
    /// emits each cell shrunk by `gaps` as the window's OUTER footprint.
    /// No border arithmetic happens here (see [`MasterStack::gapped`]).
    fn arrange(&self, windows: &[WindowId], area: Rect, focus: usize) -> Vec<Placement> {
        if windows.is_empty() {
            return Vec::new();
        }
        // The focus index selects the master (REQ-lay-003); an out-of-range
        // index (stale focus after a window was removed) clamps to 0.
        let focus = if focus < windows.len() { focus } else { 0 };
        // A lone window spans the full area (cover-fully, REQ-lay-004); the
        // ratio split only shapes the master when a stack exists.
        let master_w = if windows.len() > 1 {
            ((area.w as f64 * self.ratio).round() as i32).clamp(0, area.w as i32) as u16
        } else {
            area.w
        };

        let mut out = Vec::with_capacity(windows.len());
        let master_cell = Rect {
            x: area.x,
            y: area.y,
            w: master_w,
            h: area.h,
        };
        out.push(Placement {
            window: windows[focus],
            rect: self.gapped(master_cell),
            border: self.border,
        });

        // Stack: the remaining windows tile vertically in the right slice.
        let stack_n = windows.len() - 1;
        if stack_n > 0 {
            let stack_w = area.w - master_w;
            let stack_x = area.x + master_w as i32;
            let slice_h = area.h / stack_n as u16;
            let mut idx = 0usize;
            for (i, w) in windows.iter().enumerate() {
                if i == focus {
                    continue;
                }
                // The last stack window absorbs the remainder so the stack
                // covers the slice fully (REQ-lay-004).
                let h = if idx == stack_n - 1 {
                    area.h - slice_h * (stack_n as u16 - 1)
                } else {
                    slice_h
                };
                let cell = Rect {
                    x: stack_x,
                    y: area.y + (idx as i32 * slice_h as i32),
                    w: stack_w,
                    h,
                };
                out.push(Placement {
                    window: *w,
                    rect: self.gapped(cell),
                    border: self.border,
                });
                idx += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use crate::geometry::{Placement, Rect, WindowId};
    use crate::layout::Layout;

    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 800,
        h: 600,
    };

    /// Gapless layout: the placements ARE the cells, so the goldens below
    /// read as a plain partition of `AREA`.
    fn ms() -> MasterStack {
        MasterStack::new(0.5, 2, 0)
    }

    fn p(window: WindowId, x: i32, y: i32, w: u16, h: u16) -> Placement {
        Placement {
            window,
            rect: Rect { x, y, w, h },
            border: 2,
        }
    }

    #[test]
    fn name_identifies_the_layout() {
        assert_eq!(ms().name(), "master-stack");
    }

    #[test]
    fn golden_zero_windows() {
        // No windows -> no placements (the area is intentionally uncovered).
        assert_eq!(ms().arrange(&[], AREA, 0), Vec::<Placement>::new());
    }

    #[test]
    fn golden_one_window_fills_master() {
        // A single window covers the whole area (REQ-lay-004 cover-fully);
        // the master/stack split only applies with 2+ windows (SC-lay-02).
        // The placement is the OUTER footprint: at gaps 0 it equals `AREA`
        // itself, borders included — it is NOT inset by the border, because
        // X draws the frame's border outside its `w x h` and an inset rect
        // pushed that ring 2px off the right/bottom screen edges.
        let out = ms().arrange(&[1], AREA, 0);
        assert_eq!(out, vec![p(1, 0, 0, 800, 600)]);
    }

    #[test]
    fn golden_two_windows_master_and_stack() {
        // SC-lay-02: focused window left half, rest stack on the right. The
        // two footprints SHARE the x=400 edge — adjacent placements touch.
        let out = ms().arrange(&[1, 2], AREA, 0);
        assert_eq!(out, vec![p(1, 0, 0, 400, 600), p(2, 400, 0, 400, 600)]);
    }

    #[test]
    fn golden_three_windows_tile_stack_vertically() {
        let out = ms().arrange(&[1, 2, 3], AREA, 0);
        assert_eq!(
            out,
            vec![
                p(1, 0, 0, 400, 600),
                p(2, 400, 0, 400, 300),
                p(3, 400, 300, 400, 300),
            ]
        );
    }

    #[test]
    fn focus_index_selects_the_master_window() {
        // SC-lay-03: windows in focus-history order, focus index selects master.
        let out = ms().arrange(&[1, 2, 3], AREA, 2);
        assert_eq!(
            out,
            vec![
                p(3, 0, 0, 400, 600),
                p(1, 400, 0, 400, 300),
                p(2, 400, 300, 400, 300),
            ]
        );
    }

    #[test]
    fn gaps_shrink_every_cell_by_the_gap_on_all_four_sides() {
        // REQ-lay-004 gaps: each cell is shrunk by `gaps` on all four sides.
        // Cells touch, so the visible distance between two neighbours is
        // TWICE the gap while a window against the area edge keeps a single
        // gap — with `gaps = 3` that is 6px between windows and 3px at the
        // edges, the ratio the default config ships.
        let out = MasterStack::new(0.5, 2, 3).arrange(&[1, 2], AREA, 0);
        let (master, stack) = (out[0].rect, out[1].rect);
        assert_eq!(
            out,
            vec![p(1, 3, 3, 394, 594), p(2, 403, 3, 394, 594)],
            "gaps 3 must inset both cells by 3 on every side"
        );
        assert_eq!(
            stack.x - (master.x + i32::from(master.w)),
            6,
            "two adjacent windows are 2 * gaps apart"
        );
        assert_eq!(master.x - AREA.x, 3, "the area edge keeps a single gap");
        assert_eq!(
            (AREA.x + i32::from(AREA.w)) - (stack.x + i32::from(stack.w)),
            3,
            "the area edge keeps a single gap"
        );
    }

    #[test]
    fn gaps_clamp_to_zero_size_inside_the_area_when_wider_than_the_cell() {
        // A gap wider than the cell must clamp to a zero-size placement (the
        // `is_visible` case `resolve_direction` already guards) and must NOT
        // shift the origin past the cell it was cut from — a degenerate
        // placement outside the area would break the containment invariant
        // just as the old border inset did.
        let area = Rect {
            x: 10,
            y: 20,
            w: 8,
            h: 6,
        };
        let out = MasterStack::new(0.5, 2, 40).arrange(&[1], area, 0);
        assert_eq!(out, vec![p(1, 18, 26, 0, 0)]);
        assert_eq!(out[0].rect.x, area.x + i32::from(area.w));
        assert_eq!(out[0].rect.y, area.y + i32::from(area.h));
    }

    #[test]
    fn default_layout_matches_the_config_defaults() {
        // The layout `App::new` builds from a default config (border 2,
        // gaps 3) and `MasterStack::default()` must tile identically — a
        // drift here is exactly the class of bug where the X layer draws a
        // 4px border while the layout reserved 2.
        assert_eq!(
            MasterStack::default().arrange(&[1, 2], AREA, 0),
            MasterStack::new(DEFAULT_MASTER_RATIO, 2, 3).arrange(&[1, 2], AREA, 0)
        );
    }

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        (a.x as i64) < (b.x as i64 + b.w as i64)
            && (b.x as i64) < (a.x as i64 + a.w as i64)
            && (a.y as i64) < (b.y as i64 + b.h as i64)
            && (b.y as i64) < (a.y as i64 + a.h as i64)
    }

    proptest! {
        /// SC-lay-04/05: over random areas, ratios, borders, gaps and window
        /// sets, every placement's OUTER footprint stays inside the area and
        /// no two overlap; with `gaps = 0` the footprints tile the area
        /// exactly.
        ///
        /// The containment half is the invariant whose violation WAS the
        /// off-screen-border bug: the old layout inset each rect by the
        /// border and this property still passed, because it was checking
        /// the inset rect while X drew the border ring OUTSIDE it, 2px past
        /// the right and bottom screen edges. Now `rect` IS the footprint
        /// borders included, so the same four comparisons finally mean what
        /// they say.
        #[test]
        fn arrange_places_inside_disjoint_and_covering(
            windows in vec(1u32..=8, 0..=4),
            x in 0i32..512,
            y in 0i32..512,
            w in 128u16..=1024,
            h in 128u16..=1024,
            focus in 0usize..=4,
            border in 0u16..=4,
            gaps in 0u16..=16,
            ratio in 0.1f64..=0.9,
        ) {
            let area = Rect { x, y, w, h };
            let out = MasterStack::new(ratio, border, gaps).arrange(&windows, area, focus);

            // One placement per input window, no additions.
            let mut ids: Vec<u32> = out.iter().map(|p| p.window).collect();
            ids.sort_unstable();
            let mut want: Vec<u32> = windows.clone();
            want.sort_unstable();
            prop_assert_eq!(ids, want);

            // Inside the area, border included: nothing may be drawn off the
            // edge of the region the layout was handed.
            for p in &out {
                prop_assert!(p.rect.x >= area.x && p.rect.y >= area.y);
                prop_assert!(p.rect.w <= area.w && p.rect.h <= area.h);
                prop_assert!((p.rect.x as i64 + p.rect.w as i64)
                    <= (area.x as i64 + area.w as i64));
                prop_assert!((p.rect.y as i64 + p.rect.h as i64)
                    <= (area.y as i64 + area.h as i64));
            }

            // Disjoint: no two placements overlap (gaps only shrink cells,
            // so this holds at every gap width).
            for i in 0..out.len() {
                for j in (i + 1)..out.len() {
                    prop_assert!(!rects_overlap(out[i].rect, out[j].rect));
                }
            }

            if windows.is_empty() {
                prop_assert!(out.is_empty());
            } else if gaps == 0 {
                // Cover (REQ-lay-004): the footprints are pairwise disjoint
                // and all inside `area` (both asserted above), so summing to
                // the area's own surface leaves nothing uncovered and
                // nothing counted twice — the union IS `area`.
                let total: u64 = out
                    .iter()
                    .map(|p| p.rect.w as u64 * p.rect.h as u64)
                    .sum();
                prop_assert_eq!(total, area.w as u64 * area.h as u64);
            }
        }
    }

    #[test]
    fn gapless_placements_tile_the_area_with_nothing_left_over() {
        // The exact-tiling invariant spelled out per pixel rather than by
        // area arithmetic: with `gaps = 0` every point of `area` belongs to
        // exactly one placement. A layout that inset by the border (the old
        // model) leaves a border-wide moat uncovered here.
        let area = Rect {
            x: 5,
            y: 7,
            w: 61,
            h: 43,
        };
        for n in 1..=4usize {
            let windows: Vec<WindowId> = (1..=n as u32).collect();
            let out = MasterStack::new(0.5, 2, 0).arrange(&windows, area, 0);
            for px in area.x..(area.x + i32::from(area.w)) {
                for py in area.y..(area.y + i32::from(area.h)) {
                    let hits = out
                        .iter()
                        .filter(|p| {
                            px >= p.rect.x
                                && px < p.rect.x + i32::from(p.rect.w)
                                && py >= p.rect.y
                                && py < p.rect.y + i32::from(p.rect.h)
                        })
                        .count();
                    assert_eq!(
                        hits, 1,
                        "({px},{py}) is covered {hits} times with {n} windows"
                    );
                }
            }
        }
    }
}
