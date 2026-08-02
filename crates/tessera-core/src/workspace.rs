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
    /// Workspaces in creation order (drives EWMH desktop naming in the EWMH
    /// work unit).
    order: Vec<WorkspaceId>,
    /// Workspaces in touch-recency order, front = most recently touched; the
    /// focus-repair fallback (SC-ws-04) picks the nearest remaining one.
    mru: Vec<WorkspaceId>,
    current: WorkspaceId,
    next_id: u32,
    bus: Arc<EventBus>,
}

impl WorkspaceManager {
    /// Creates an empty manager that publishes on `bus`.
    pub fn new(bus: Arc<EventBus>) -> Self {
        WorkspaceManager {
            workspaces: HashMap::new(),
            order: Vec::new(),
            mru: Vec::new(),
            current: 0,
            next_id: 1,
            bus,
        }
    }

    /// Records `id` as the most recently touched workspace.
    fn bump_mru(&mut self, id: WorkspaceId) {
        self.mru.retain(|&m| m != id);
        self.mru.insert(0, id);
    }

    /// The workspace currently holding window `w`, if any.
    fn workspace_of(&self, w: WindowId) -> Option<WorkspaceId> {
        self.workspaces
            .iter()
            .find(|(_, ws)| ws.windows.contains(&w))
            .map(|(&id, _)| id)
    }

    /// The most recently used workspace other than `id`.
    fn nearest_remaining(&self, id: WorkspaceId) -> Option<WorkspaceId> {
        self.mru
            .iter()
            .copied()
            .find(|&m| m != id && self.workspaces.contains_key(&m))
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
        self.order.push(id);
        self.bump_mru(id);
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
        self.bump_mru(self.current);
    }

    /// Switches the current workspace to `id`, publishing `WorkspaceChanged`.
    /// Returns false (no-op) when `id` is unknown or already current.
    ///
    /// Full switch semantics (REQ-ws-004): the old workspace's windows are no
    /// longer visible, the new one's become visible, and the new workspace's
    /// MRU window regains focus — repairing any stale focus left by a detach.
    pub fn switch(&mut self, id: WorkspaceId) -> bool {
        if !self.workspaces.contains_key(&id) || id == self.current {
            return false;
        }
        self.current = id;
        self.bump_mru(id);
        // focus new.mru[0]: windows are in focus-history order, so the MRU
        // window is windows[0] (None when the workspace is empty).
        let ws = self.workspaces.get_mut(&id).expect("workspace exists");
        ws.focus = ws.windows.first().copied();
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

    /// Windows that should currently be mapped: the focused workspace's window
    /// list in focus-history order. Switching changes this set, which is the
    /// pure-core "unmap old, map new" contract (SC-ws-06).
    pub fn visible_windows(&self) -> Vec<WindowId> {
        self.workspaces
            .get(&self.current)
            .map(|ws| ws.windows.clone())
            .unwrap_or_default()
    }

    /// The currently focused window: the focused workspace's MRU window.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.workspaces.get(&self.current).and_then(|ws| ws.focus)
    }

    /// Sets `w` as the focused window of its workspace WITHOUT reordering the
    /// focus-history list, and without publishing (used by focus cycling,
    /// REQ-x11-008: the cycle walks the fixed MRU ring). Returns false when
    /// `w` is not managed in any workspace.
    pub fn focus_window(&mut self, w: WindowId) -> bool {
        let Some(id) = self.workspace_of(w) else {
            return false;
        };
        self.workspaces
            .get_mut(&id)
            .expect("workspace exists")
            .focus = Some(w);
        true
    }

    /// Closes `id` only when it is empty, unfocused, and not the sole
    /// workspace (clamp >= 1, single choke point, REQ-ws-002). On success
    /// publishes `WorkspaceClosed`.
    pub fn close(&mut self, id: WorkspaceId) -> bool {
        let closable = self.workspaces.len() > 1
            && id != self.current
            && self
                .workspaces
                .get(&id)
                .is_some_and(|ws| ws.windows.is_empty());
        if !closable {
            return false; // clamp >= 1 (and not closeable) choke point
        }
        self.workspaces.remove(&id);
        self.order.retain(|&w| w != id);
        self.mru.retain(|&w| w != id);
        self.bus.publish(Event::WorkspaceClosed(id));
        true
    }

    /// Removes `w` from its workspace (SC-ws-02..04). An empty unfocused
    /// workspace is destroyed; an emptied focused workspace moves focus to the
    /// nearest remaining workspace (or stays with no focus when it is the
    /// sole one). EWMH `set_desktops` sync is deferred to the EWMH work unit.
    pub fn detach(&mut self, w: WindowId) {
        let Some(id) = self.workspace_of(w) else {
            return;
        };
        let ws = self.workspaces.get_mut(&id).expect("workspace exists");
        ws.windows.retain(|&x| x != w);
        if ws.focus == Some(w) {
            ws.focus = None;
        }
        if !ws.windows.is_empty() {
            return;
        }
        if id == self.current {
            // SC-ws-04: the focused workspace emptied. Prefer the nearest
            // remaining workspace; when this is the sole one it simply keeps
            // no focused window.
            if let Some(next) = self.nearest_remaining(id) {
                self.switch(next);
            }
        } else {
            // SC-ws-02: empty + unfocused -> destroy (clamp enforced inside).
            self.close(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossbeam_channel::{Receiver, RecvTimeoutError};

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

    #[test]
    fn switch_remaps_visible_windows_and_focuses_mru() {
        // SC-ws-06: switching unmaps the old workspace's windows, maps the
        // new one's, and focuses its MRU window.
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(b);
        wm.attach(5);
        wm.attach(8);
        wm.switch(a);
        wm.attach(6);
        // Before switching, only a's windows are visible.
        assert_eq!(wm.visible_windows(), vec![6]);
        assert_eq!(wm.focused_window(), Some(6));
        assert!(wm.switch(b));
        // Old windows unmapped, new ones mapped, MRU window (8) focused.
        assert_eq!(wm.visible_windows(), vec![8, 5]);
        assert_eq!(wm.focused_window(), Some(8));
        assert_eq!(wm.current_id(), b);
        // Switching back restores a's visibility and focus.
        assert!(wm.switch(a));
        assert_eq!(wm.visible_windows(), vec![6]);
        assert_eq!(wm.focused_window(), Some(6));
        let events: Vec<_> = (0..5).map(|_| rx.recv()).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            events,
            vec![
                Event::WorkspaceOpened(1),
                Event::WorkspaceOpened(2),
                Event::WorkspaceChanged(2),
                Event::WorkspaceChanged(1),
                Event::WorkspaceChanged(2),
            ]
        );
    }

    #[test]
    fn switch_reestablishes_stale_focus_to_mru_window() {
        // A detach clears the workspace focus while windows remain; switching
        // back must restore focus to the workspace's MRU window ("focus
        // new.mru[0]", REQ-ws-004) instead of leaving it stale.
        let (_, _, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(b);
        wm.attach(5);
        wm.attach(8);
        wm.detach(8); // b keeps window 5 but its focus becomes None
        wm.switch(a);
        assert_eq!(wm.focused_window(), None); // a is still empty
        wm.attach(6);
        assert!(wm.switch(b));
        assert_eq!(wm.focused_window(), Some(5)); // repaired to b's MRU window
        assert_eq!(wm.visible_windows(), vec![5]);
    }

    #[test]
    fn detaching_last_window_destroys_empty_unfocused_workspace() {
        // SC-ws-02: a non-focused workspace that becomes empty is destroyed
        // and WorkspaceClosed is published.
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(b);
        wm.attach(5);
        wm.switch(a);
        wm.attach(6);
        wm.detach(5); // last window of the non-focused workspace b
        assert_eq!(wm.len(), 1);
        assert_eq!(wm.workspace(b), None);
        assert_eq!(wm.workspace(a).unwrap().windows, vec![6]);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(2)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(2)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceClosed(2)));
    }

    #[test]
    fn sole_workspace_survives_last_window_close() {
        // SC-ws-03: the only workspace is never destroyed; it stays
        // empty-but-present with no focused window.
        let (_, rx, mut wm) = setup();
        wm.attach(1); // sole workspace 1, focused
        wm.detach(1); // its last window closes
        assert_eq!(wm.len(), 1);
        assert_eq!(wm.current_id(), 1);
        let ws = wm.workspace(1).unwrap();
        assert_eq!(ws.windows, Vec::<WindowId>::new());
        assert_eq!(ws.focus, None);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        // clamp: no WorkspaceClosed is published for the sole workspace.
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn close_rejects_sole_and_focused_workspaces() {
        // The clamp >= 1 lives in close(), the single choke point (REQ-ws-002).
        let (_, rx, mut wm) = setup();
        wm.attach(1); // sole + focused workspace
        assert!(!wm.close(1)); // sole -> rejected by the clamp
        assert_eq!(wm.len(), 1);
        assert!(wm.workspace(1).is_some());
        wm.open(); // now two workspaces, current is still 1 (focused)
        assert!(!wm.close(1)); // focused -> rejected
        assert_eq!(wm.len(), 2);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(2)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn emptying_focused_workspace_moves_to_nearest_remaining() {
        // SC-ws-04: when the focused workspace empties, focus moves to the
        // most recently used remaining workspace.
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(b);
        wm.attach(5);
        wm.switch(a);
        wm.attach(6);
        wm.attach(7);
        wm.detach(7); // focus drops, but a still has a window
        wm.detach(6); // a is empty and focused -> move to nearest (b)
        assert_eq!(wm.current_id(), b);
        assert_eq!(wm.workspace(b).unwrap().windows, vec![5]);
        assert_eq!(wm.workspace(a).unwrap().windows, Vec::<WindowId>::new());
        let events: Vec<_> = (0..5).map(|_| rx.recv()).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            events,
            vec![
                Event::WorkspaceOpened(1),
                Event::WorkspaceOpened(2),
                Event::WorkspaceChanged(2),
                Event::WorkspaceChanged(1),
                Event::WorkspaceChanged(2),
            ]
        );
    }
}
