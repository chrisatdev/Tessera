//! Core geometry and window/workspace identifiers.

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
