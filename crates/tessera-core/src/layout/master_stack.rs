//! Master-stack layout (design D5): the focused window occupies a master area
//! on the left at a configurable ratio; the remaining windows tile vertically
//! in a stack on the right.

use crate::geometry::{Placement, Rect, WindowId};
use crate::layout::Layout;

/// Master-stack layout with a configurable master ratio and frame border.
pub struct MasterStack {
    ratio: f64,
    border: u16,
}

impl MasterStack {
    /// Creates a master-stack layout with the given master `ratio` (default
    /// 0.5) and `border` width baked into every placement (D5, REQ-lay-002).
    pub fn new(ratio: f64, border: u16) -> Self {
        todo!("arrange is not implemented yet")
    }
}

impl Layout for MasterStack {
    fn name(&self) -> &'static str {
        todo!("arrange is not implemented yet")
    }

    fn arrange(&self, windows: &[WindowId], area: Rect, focus: usize) -> Vec<Placement> {
        todo!("arrange is not implemented yet")
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use crate::geometry::{Placement, Rect, WindowId};
    use crate::layout::Layout;

    use super::*;

    const AREA: Rect = Rect { x: 0, y: 0, w: 800, h: 600 };

    fn ms() -> MasterStack {
        MasterStack::new(0.5, 2)
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
        let out = ms().arrange(&[1], AREA, 0);
        assert_eq!(out, vec![p(1, 2, 2, 396, 596)]);
    }

    #[test]
    fn golden_two_windows_master_and_stack() {
        // SC-lay-02: focused window left half, rest stack on the right.
        let out = ms().arrange(&[1, 2], AREA, 0);
        assert_eq!(
            out,
            vec![p(1, 2, 2, 396, 596), p(2, 402, 2, 396, 596)]
        );
    }

    #[test]
    fn golden_three_windows_tile_stack_vertically() {
        let out = ms().arrange(&[1, 2, 3], AREA, 0);
        assert_eq!(
            out,
            vec![
                p(1, 2, 2, 396, 596),
                p(2, 402, 2, 396, 296),
                p(3, 402, 302, 396, 296),
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
                p(3, 2, 2, 396, 596),
                p(1, 402, 2, 396, 296),
                p(2, 402, 302, 396, 296),
            ]
        );
    }

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        (a.x as i64) < (b.x as i64 + b.w as i64)
            && (b.x as i64) < (a.x as i64 + a.w as i64)
            && (a.y as i64) < (b.y as i64 + b.h as i64)
            && (b.y as i64) < (a.y as i64 + a.h as i64)
    }

    proptest! {
        /// SC-lay-04/05: over random areas, ratios, borders and window sets,
        /// placements stay inside the area, are pairwise disjoint, carry the
        /// same window set, and their border-expanded rects partition the area
        /// exactly (cover).
        #[test]
        fn arrange_places_inside_disjoint_and_covering(
            windows in vec(1u32..=8, 0..=4),
            x in 0i32..512,
            y in 0i32..512,
            w in 128u16..=1024,
            h in 128u16..=1024,
            focus in 0usize..=4,
            border in 0u16..=4,
            ratio in 0.1f64..=0.9,
        ) {
            let area = Rect { x, y, w, h };
            let out = MasterStack::new(ratio, border).arrange(&windows, area, focus);

            // One placement per input window, no additions.
            let mut ids: Vec<u32> = out.iter().map(|p| p.window).collect();
            ids.sort_unstable();
            let mut want: Vec<u32> = windows.clone();
            want.sort_unstable();
            prop_assert_eq!(ids, want);

            // Inside the area (border inset can only shrink placements).
            for p in &out {
                prop_assert!(p.rect.x >= area.x && p.rect.y >= area.y);
                prop_assert!(p.rect.w <= area.w && p.rect.h <= area.h);
                prop_assert!((p.rect.x as i64 + p.rect.w as i64)
                    <= (area.x as i64 + area.w as i64));
                prop_assert!((p.rect.y as i64 + p.rect.h as i64)
                    <= (area.y as i64 + area.h as i64));
            }

            // Disjoint: no two placements overlap.
            for i in 0..out.len() {
                for j in (i + 1)..out.len() {
                    prop_assert!(!rects_overlap(out[i].rect, out[j].rect));
                }
            }

            if windows.is_empty() {
                prop_assert!(out.is_empty());
            } else {
                // Cover: expanding each placement by its border reconstructs
                // the exact partition of the area (REQ-lay-004).
                let total: u64 = out
                    .iter()
                    .map(|p| {
                        (p.rect.w as u64 + 2 * p.border as u64)
                            * (p.rect.h as u64 + 2 * p.border as u64)
                    })
                    .sum();
                prop_assert_eq!(total, area.w as u64 * area.h as u64);
            }
        }
    }
}
