//! Window lifecycle state machine (REQ-x11-007) and command dispatch
//! (REQ-x11-008).
//!
//! [`WindowManager`] owns the per-window 4-state machine (design D3) layered
//! over the [`WorkspaceManager`] state, and translates user [`Command`]s into
//! workspace-state changes. DestroyNotify is the sole authority for removal;
//! the `UnmanagePending` state absorbs an UnmapNotify that precedes it so a
//! window is unmanaged at most once (SC-x11-10/11).

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::bus::EventBus;
use crate::command::Command;
use crate::event::Event;
use crate::geometry::WindowId;
use crate::workspace::WorkspaceManager;

/// Lifecycle state of one managed window (design D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    /// MapRequest received; frame creation in progress (display seam, T12).
    Managing,
    /// Fully managed and mapped.
    Managed,
    /// UnmapNotify seen (iconify): the window is hidden but still managed,
    /// and a later DestroyNotify still removes it.
    UnmanagePending,
    /// Removed; every further event for the window is ignored.
    Unmanaged,
}

/// What the display layer must do after [`WindowManager::apply_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEffect {
    /// Core state changed (focus moved or workspace switched).
    Applied,
    /// No state changed (no target to cycle, unknown workspace, ...).
    Ignored,
    /// The display layer must spawn the configured terminal (T13).
    SpawnTerminal,
    /// The display layer must request close of the focused client (T12+).
    CloseFocused,
    /// Command not wired to any behavior yet.
    Unsupported,
}

/// Owns window lifecycle and user-command dispatch.
///
/// Windows live in the [`WorkspaceManager`] (reachable through `Deref`); this
/// type tracks their lifecycle state and drives removal + focus repair.
pub struct WindowManager {
    ws: WorkspaceManager,
    states: HashMap<WindowId, WindowState>,
    bus: Arc<EventBus>,
}

impl WindowManager {
    /// Creates an empty window manager publishing on `bus`.
    pub fn new(bus: Arc<EventBus>) -> Self {
        WindowManager {
            ws: WorkspaceManager::new(Arc::clone(&bus)),
            states: HashMap::new(),
            bus,
        }
    }

    /// Handles a MapRequest for `w`.
    pub fn map_request(&mut self, w: WindowId) {
        todo!("window lifecycle state machine")
    }

    /// Marks `w` fully managed, publishing `WindowManaged` exactly once.
    pub fn managed(&mut self, w: WindowId) {
        todo!("window lifecycle state machine")
    }

    /// Handles an UnmapNotify for `w` (iconify path).
    pub fn unmap_notify(&mut self, w: WindowId) {
        todo!("window lifecycle state machine")
    }

    /// Handles a DestroyNotify for `w` (the authority for removal).
    pub fn destroy_notify(&mut self, w: WindowId) {
        todo!("window lifecycle state machine")
    }

    /// Current lifecycle state of `w`, if it is known.
    pub fn state_of(&self, w: WindowId) -> Option<WindowState> {
        todo!("window lifecycle state machine")
    }
}

impl Deref for WindowManager {
    type Target = WorkspaceManager;

    fn deref(&self) -> &Self::Target {
        &self.ws
    }
}

impl DerefMut for WindowManager {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ws
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_channel::{Receiver, RecvTimeoutError};

    use crate::config::Config;
    use crate::event::Event;

    use super::*;

    fn setup() -> (Arc<EventBus>, Receiver<Event>, WindowManager) {
        let bus = Arc::new(EventBus::new(Arc::new(Config::default())));
        let rx = bus.subscribe_all();
        let wm = WindowManager::new(Arc::clone(&bus));
        (bus, rx, wm)
    }

    /// Maps `w` through the full manage path: MapRequest + frame confirmation.
    fn manage(wm: &mut WindowManager, w: WindowId) {
        wm.map_request(w);
        wm.managed(w);
    }

    #[test]
    fn unmap_then_destroy_in_one_drain_removes_exactly_once() {
        // SC-x11-11: UnmapNotify followed by DestroyNotify for the same window
        // in one drain removes it once; no duplicate unmanage is published.
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        wm.unmap_notify(1); // Managed -> UnmanagePending (iconify)
        wm.destroy_notify(1); // UnmanagePending -> remove()
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn destroy_notify_alone_removes_exactly_once() {
        // SC-x11-10: DestroyNotify is the authority for removal; a managed
        // window is unmanaged and WindowUnmapped published exactly once.
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        wm.destroy_notify(1);
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn second_destroy_notify_is_ignored() {
        // At-most-once: after removal the window is Unmanaged and any further
        // DestroyNotify for it is ignored (no second WindowUnmapped).
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        wm.destroy_notify(1);
        wm.destroy_notify(1); // Unmanaged -> ignored
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn destroy_notify_for_unknown_window_is_ignored() {
        // A window that was never managed is not unmanaged (nothing published).
        let (_, rx, mut wm) = setup();
        wm.destroy_notify(99);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }
}
