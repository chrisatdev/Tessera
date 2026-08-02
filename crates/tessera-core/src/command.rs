//! User commands dispatched from keybindings.

use crate::geometry::WorkspaceId;

/// Commands the WM can execute from a `KeyPressed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SpawnTerminal,
    FocusNext,
    FocusPrev,
    CloseFocused,
    SwitchWorkspace(WorkspaceId),
    ToggleLayout,
}
