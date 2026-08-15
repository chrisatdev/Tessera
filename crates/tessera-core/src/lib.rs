//! Tessera core: pure, X-free types, event bus, state and configuration.
//!
//! This crate has zero X11 dependencies so the window-manager core stays
//! headless-testable. `tessera-x11` implements the display seam.

pub mod app;
pub mod bus;
pub mod command;
pub mod config;
pub mod display;
pub mod event;
pub mod geometry;
pub mod layout;
pub mod theme;
pub mod watch;
pub mod window;
pub mod window_kind;
pub mod workspace;

pub use app::{App, command_for_key};
pub use bus::{EventBus, EventMask, WmState, WorkspaceState};
pub use command::Command;
pub use config::{BarConfig, BarPosition, Config, GeneralConfig, Keybindings};
pub use display::{DErr, DisplayServer, FrameId, spawn_program, spawn_program_args};
pub use event::{Event, KeyCombo};
pub use geometry::{Direction, LayoutKind, Placement, Rect, WindowId, WorkspaceId};
pub use layout::{Layout, MasterStack};
pub use theme::{Color, Theme};
pub use window::{CommandEffect, WindowManager, WindowState};
pub use window_kind::{ManagePolicy, WindowKind};
pub use workspace::{Workspace, WorkspaceManager};
