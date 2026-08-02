//! Typed events published on the [`EventBus`](crate::bus::EventBus).

use std::sync::Arc;

use crate::command::Command;
use crate::config::Config;
use crate::geometry::{LayoutKind, Placement, Rect, WindowId, WorkspaceId};

/// A bound key combination. `mods` is the X11 modifier mask and `key` the
/// keysym; both are raw integers so the core stays X-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct KeyCombo {
    pub mods: u32,
    pub key: u32,
}

/// Every event flowing through the bus (design D4, all 17 variants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    WindowMapRequested(WindowId),
    WindowConfigureRequested(WindowId, Rect),
    WindowUnmapNotify(WindowId),
    WindowDestroyNotify(WindowId),
    WindowManaged(WindowId),
    WindowUnmapped(WindowId),
    WindowFocusChanged(Option<WindowId>),
    WindowTitleChanged(WindowId, String),
    WorkspaceOpened(WorkspaceId),
    WorkspaceClosed(WorkspaceId),
    WorkspaceChanged(WorkspaceId),
    WorkspaceLayoutChanged(WorkspaceId, LayoutKind),
    PlacementsChanged(WorkspaceId, Vec<Placement>),
    KeyPressed(KeyCombo),
    Command(Command),
    ConfigReloaded(Arc<Config>),
    Shutdown,
}
