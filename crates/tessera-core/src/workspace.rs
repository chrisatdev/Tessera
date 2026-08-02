//! Dynamic workspaces (REQ-ws-001..005): lifecycle, attach/detach, switch.
//!
//! Windows inside a workspace are kept in focus-history order (most recent
//! first) so layouts can pick the master by index (REQ-lay-003).

use std::collections::HashMap;
use std::sync::Arc;

use crate::bus::EventBus;
use crate::event::Event;
use crate::geometry::{LayoutKind, WindowId, WorkspaceId};

/// One dynamic workspace: window list in focus-history order plus its layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    /// Auto-generated display name ("1", "2", ...), `_NET_DESKTOP_NAMES`-ready.
    pub name: String,
    pub layout: LayoutKind,
    /// Windows in focus-history order (most recent first).
    pub windows: Vec<WindowId>,
    /// Most recently focused window in this workspace.
    pub focus: Option<WindowId>,
}

/// Owns workspace lifecycle and publishes workspace events on the bus.
///
/// `current` is 0 while no workspace exists (the first `open` makes a
/// workspace current). The clamp "at least one workspace" is enforced in the
/// single choke point [`WorkspaceManager::close`].
pub struct WorkspaceManager {
    workspaces: HashMap<WorkspaceId, Workspace>,
    current: WorkspaceId,
    next_id: u32,
    bus: Arc<EventBus>,
}

impl WorkspaceManager {
    /// Creates an empty manager that publishes on `bus`.
    pub fn new(bus: Arc<EventBus>) -> Self {
        WorkspaceManager {
            workspaces: HashMap::new(),
            current: 0,
            next_id: 1,
            bus,
        }
    }

    /// Creates a workspace with the next auto name and publishes
    /// `WorkspaceOpened`; the first one becomes current (REQ-ws-001).
    pub fn open(&mut self) -> WorkspaceId {
        let id = self.next_id;
        self.next_id += 1;
        let ws = Workspace {
            id,
            name: id.to_string(),
            layout: LayoutKind::MasterStack,
            windows: Vec::new(),
            focus: None,
        };
        self.workspaces.insert(id, ws);
        if self.current == 0 {
            self.current = id; // first workspace becomes current
        }
        self.bus.publish(Event::WorkspaceOpened(id));
        id
    }

    /// Attaches `w` to the focused workspace, auto-opening one when none
    /// exists (SC-ws-01); the attached window becomes the workspace focus
    /// (SC-ws-07).
    pub fn attach(&mut self, w: WindowId) {
        if self.workspaces.is_empty() {
            self.open();
        }
        let ws = self
            .workspaces
            .get_mut(&self.current)
            .expect("a current workspace exists after auto-open");
        if !ws.windows.contains(&w) {
            ws.windows.insert(0, w); // most recent first
        }
        ws.focus = Some(w);
    }

    /// Switches the current workspace to `id`, publishing `WorkspaceChanged`.
    /// Returns false (no-op) when `id` is unknown or already current.
    ///
    /// Note: full switch semantics (visibility projection, MRU focus
    /// establishment) land in the switch work unit; this minimal form only
    /// moves `current` so attach-to-focused is testable (SC-ws-07).
    pub fn switch(&mut self, id: WorkspaceId) -> bool {
        if !self.workspaces.contains_key(&id) || id == self.current {
            return false;
        }
        self.current = id;
        self.bus.publish(Event::WorkspaceChanged(id));
        true
    }

    /// Id of the current workspace, or 0 when none exists.
    pub fn current_id(&self) -> WorkspaceId {
        self.current
    }

    /// Number of workspaces.
    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    /// True when no workspace exists.
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// The workspace `id`, if it exists.
    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::Receiver;

    use crate::config::Config;
    use crate::event::Event;

    use super::*;

    fn setup() -> (Arc<EventBus>, Receiver<Event>, WorkspaceManager) {
        let bus = Arc::new(EventBus::new(Arc::new(Config::default())));
        let rx = bus.subscribe_all();
        let wm = WorkspaceManager::new(Arc::clone(&bus));
        (bus, rx, wm)
    }

    #[test]
    fn first_attach_auto_opens_workspace() {
        // SC-ws-01: no workspaces -> first attach creates one, publishes
        // WorkspaceOpened, and the window lands in it as focus.
        let (_, rx, mut wm) = setup();
        wm.attach(1);
        assert_eq!(wm.len(), 1);
        assert_eq!(wm.current_id(), 1);
        let ws = wm.workspace(1).expect("workspace 1 exists");
        assert_eq!(ws.name, "1");
        assert_eq!(ws.windows, vec![1]);
        assert_eq!(ws.focus, Some(1));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
    }

    #[test]
    fn attach_goes_to_focused_workspace() {
        // SC-ws-07: a window mapping while workspace B is focused attaches to
        // B, never to the workspace it visually appears on.
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        assert_eq!((a, b), (1, 2));
        assert_eq!(wm.current_id(), a); // first open becomes current
        assert!(wm.switch(b)); // B is now focused
        wm.attach(7);
        assert_eq!(wm.workspace(b).unwrap().windows, vec![7]);
        assert_eq!(wm.workspace(b).unwrap().focus, Some(7));
        assert_eq!(wm.workspace(a).unwrap().windows, Vec::<WindowId>::new());
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(2)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(2)));
    }
}
