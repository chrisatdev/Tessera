//! User commands dispatched from keybindings.

use crate::geometry::WorkspaceId;

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
}
