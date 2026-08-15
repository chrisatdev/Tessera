//! Layout engines (design D5): pure, X-free placement algorithms.
//!
//! Each layout maps a window list in focus-history order (most recent first)
//! onto an area; the `focus` index selects the master window.

use crate::geometry::{Placement, Rect, WindowId};

/// A pure window-layout algorithm with zero X dependencies (REQ-lay-001).
///
/// `arrange` receives `windows` in focus-history order (most recent first);
/// `focus` is the index of the focused window within that slice. Implementations
/// return one [`Placement`] per window.
pub trait Layout: Send + Sync {
    /// Human-readable layout name.
    fn name(&self) -> &'static str;

    /// Computes placements for `windows` inside `area` (REQ-lay-001..004).
    fn arrange(&self, windows: &[WindowId], area: Rect, focus: usize) -> Vec<Placement>;
}

mod master_stack;

pub use master_stack::{DEFAULT_MASTER_RATIO, MasterStack};
