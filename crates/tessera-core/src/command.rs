//! User commands dispatched from keybindings.

use crate::geometry::{Direction, WorkspaceId};

/// Commands the WM can execute from a `KeyPressed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SpawnTerminal,
    /// Spawn the configured `[general] launcher` argv (ALA-2, D4).
    SpawnLauncher,
    FocusNext,
    FocusPrev,
    CloseFocused,
    SwitchWorkspace(WorkspaceId),
    ToggleLayout,
    /// Step to the neighbouring workspace in ascending id order, wrapping
    /// (WS-1). `-1` = previous, `1` = next.
    CycleWorkspace(i8),
    /// Send the focused window to `WorkspaceId` WITHOUT following it (MV-1/2).
    MoveToWorkspace(WorkspaceId),
    /// Move focus to the geometrically adjacent window in `Direction` (DF-1).
    /// Resolved in `App::on_command`, never in `WindowManager::apply_command`
    /// — placements exist only transiently inside `App::recompute`, and
    /// `WindowManager` owns neither `layout` nor `area` (D8).
    FocusDirection(Direction),
}
