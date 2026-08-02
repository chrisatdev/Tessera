//! Tessera core: pure, X-free types, event bus, state and configuration.
//!
//! This crate has zero X11 dependencies so the window-manager core stays
//! headless-testable. `tessera-x11` implements the display seam.

pub mod bus;
pub mod command;
pub mod config;
pub mod event;
pub mod geometry;
pub mod layout;
pub mod watch;
pub mod window;
pub mod workspace;

pub use bus::{EventBus, EventMask, WmState, WorkspaceState};
pub use command::Command;
pub use config::{Config, GeneralConfig, Keybindings};
pub use event::{Event, KeyCombo};
pub use geometry::{LayoutKind, Placement, Rect, WindowId, WorkspaceId};
pub use layout::{Layout, MasterStack};
pub use window::{CommandEffect, WindowManager, WindowState};
pub use workspace::{Workspace, WorkspaceManager};
