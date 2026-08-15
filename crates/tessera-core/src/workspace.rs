//! Dynamic workspaces (REQ-ws-001..005): lifecycle, attach/detach, switch.
//!
//! Windows inside a workspace are kept in focus-history order (most recent
//! first) so layouts can pick the master by index (REQ-lay-003).

use std::collections::HashMap;
use std::sync::Arc;

use crate::bus::{EventBus, WorkspaceState};
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

    /// Live workspace ids in ascending NUMERIC order (WS-2, D11). `self.order`
    /// is CREATION order and deliberately not used here: the user navigates
    /// the numbers printed on the bar, not the order they happened to be
    /// created.
    fn sorted_ids(&self) -> Vec<WorkspaceId> {
        let mut ids: Vec<WorkspaceId> = self.workspaces.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The workspace `step` places from the current one in ascending id
    /// order, wrapping at both ends (WS-1, D11). `None` when fewer than two
    /// live workspaces exist (single-workspace no-op) or when `current` is
    /// not itself live.
    pub fn relative_id(&self, step: i8) -> Option<WorkspaceId> {
        let ids = self.sorted_ids();
        if ids.len() < 2 {
            return None;
        }
        let pos = ids.iter().position(|&id| id == self.current)?;
        let n = ids.len() as i64;
        let next = (pos as i64 + i64::from(step)).rem_euclid(n) as usize;
        ids.get(next).copied()
    }

    /// Creates a workspace with the next auto name and publishes
    /// `WorkspaceOpened`; the first one becomes current (REQ-ws-001).
    pub fn open(&mut self) -> WorkspaceId {
        self.open_with_id(self.next_id)
    }

    /// Creates workspace `id` (no-op when it already exists) and publishes
    /// `WorkspaceOpened`; the first one becomes current. Advances `next_id`
    /// past `id` so a later sequential [`WorkspaceManager::open`] never
    /// collides with an id created out of band (switch auto-create).
    fn open_with_id(&mut self, id: WorkspaceId) -> WorkspaceId {
        if let Some(&existing) = self.workspaces.keys().find(|&&k| k == id) {
            return existing;
        }
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
        if id >= self.next_id {
            self.next_id = id + 1;
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
    /// Returns false (no-op) only when `id` is already current; an unknown
    /// `id` is auto-created empty first (dynamic workspaces, README "auto-create
    /// on demand"), so switching to any workspace tag always works.
    ///
    /// Full switch semantics (REQ-ws-004): the old workspace's windows are no
    /// longer visible, the new one's become visible, and the new workspace's
    /// MRU window regains focus — repairing any stale focus left by a detach.
    pub fn switch(&mut self, id: WorkspaceId) -> bool {
        if id == self.current {
            return false;
        }
        self.open_with_id(id);
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

    /// Snapshot of every workspace in creation order, for the `WmState` watch
    /// (T12: watch consumers such as the bar need the full workspace set).
    pub fn state_snapshots(&self) -> Vec<WorkspaceState> {
        self.order
            .iter()
            .filter_map(|id| self.workspaces.get(id))
            .map(|ws| WorkspaceState {
                id: ws.id,
                name: ws.name.clone(),
                layout: ws.layout,
                windows: ws.windows.clone(),
                focus: ws.focus,
            })
            .collect()
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

    /// Removes `w` from its workspace and clears a focus that pointed at it.
    /// Returns the workspace it left, or `None` when `w` is unmanaged.
    ///
    /// D9: this is the removal step ONLY. The emptied-workspace policy
    /// (SC-ws-02 destroy / SC-ws-04 auto-switch) deliberately lives in
    /// [`Self::detach`], NOT here, because [`Self::move_window`] must NOT
    /// trigger it (MV-2) — a later refactor cannot "simplify" by routing the
    /// move path through `detach`, because doing so would resurrect that
    /// policy on a path that must never run it.
    fn unlink(&mut self, w: WindowId) -> Option<WorkspaceId> {
        let id = self.workspace_of(w)?;
        let ws = self.workspaces.get_mut(&id)?;
        ws.windows.retain(|&x| x != w);
        if ws.focus == Some(w) {
            ws.focus = None;
        }
        Some(id)
    }

    /// Removes `w` from its workspace (SC-ws-02..04). An empty unfocused
    /// workspace is destroyed; an emptied focused workspace moves focus to the
    /// nearest remaining workspace (or stays with no focus when it is the
    /// sole one). EWMH `set_desktops` sync is deferred to the EWMH work unit.
    ///
    /// DO NOT call [`Self::unlink`] from [`Self::move_window`] and expect this
    /// policy to follow: the auto-switch below is the destroy path's behavior
    /// and MV-2 forbids it on the move path (D9).
    pub fn detach(&mut self, w: WindowId) {
        let Some(id) = self.unlink(w) else {
            return;
        };
        let ws = self.workspaces.get(&id).expect("workspace exists");
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

    /// Moves `w` to workspace `to` WITHOUT following it (MV-1/2). `to` is
    /// auto-created when absent, mirroring `switch`'s auto-create. Returns
    /// false when `w` is unmanaged or already on `to`.
    pub fn move_window(&mut self, w: WindowId, to: WorkspaceId) -> bool {
        let Some(from) = self.workspace_of(w) else {
            return false;
        };
        if from == to {
            return false;
        }
        self.unlink(w);
        // Source focus repair: `unlink` cleared `focus` when `w` held it, and
        // `WindowManager::repair_focus` is private to window.rs and
        // unreachable here. Re-express it the way `switch` already does:
        // the MRU window is `windows[0]`.
        if let Some(src) = self.workspaces.get_mut(&from)
            && src.focus.is_none()
        {
            src.focus = src.windows.first().copied();
        }
        // MV-2: no auto-switch. `detach`'s SC-ws-04 is deliberately not on
        // this path (D9) — an emptied source stays current and empty.
        self.open_with_id(to);
        let Some(dst) = self.workspaces.get_mut(&to) else {
            return false;
        };
        if !dst.windows.contains(&w) {
            dst.windows.insert(0, w); // most recent first, as `attach`
        }
        dst.focus = Some(w);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossbeam_channel::{Receiver, RecvTimeoutError};

    use crate::config::Config;
    use crate::event::Event;
    use crate::theme::Theme;

    use super::*;

    fn setup() -> (Arc<EventBus>, Receiver<Event>, WorkspaceManager) {
        let bus = Arc::new(EventBus::new(
            Arc::new(Config::default()),
            Arc::new(Theme::default()),
        ));
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
    fn switch_auto_creates_unknown_workspace() {
        // Dynamic workspaces: switching to a tag that does not exist yet
        // creates it empty (publishing WorkspaceOpened), switches to it, and
        // advances the sequential id so a later open() never collides.
        let (_, rx, mut wm) = setup();
        let a = wm.open(); // 1, current
        assert!(wm.switch(5)); // unknown -> auto-created, switched
        assert_eq!(wm.current_id(), 5);
        assert_eq!(wm.len(), 2); // 1 and 5, no 2..4
        assert!(wm.workspace(5).expect("5 exists").windows.is_empty());
        let c = wm.open(); // sequential id must skip past 5
        assert_eq!(c, 6);
        assert!(wm.switch(a)); // back to the original workspace
        assert_eq!(wm.current_id(), a);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(5)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(5)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(6)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(1)));
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
    fn relative_id_steps_and_wraps_over_sorted_ids() {
        // WS-1/D11: relative_id walks live ids in ASCENDING NUMERIC order and
        // wraps at both ends, regardless of creation order.
        let (_, _, mut wm) = setup();
        let a = wm.open(); // 1
        let b = wm.open(); // 2
        let c = wm.open(); // 3
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(wm.current_id(), a);
        assert_eq!(wm.relative_id(1), Some(2)); // step forward
        assert!(wm.switch(3));
        assert_eq!(wm.relative_id(1), Some(1)); // wrap forward at the top
        assert!(wm.switch(1));
        assert_eq!(wm.relative_id(-1), Some(3)); // wrap backward at the bottom
    }

    #[test]
    fn relative_id_is_none_with_a_single_workspace() {
        // WS-1 no-op scenario: fewer than two live workspaces -> None, so the
        // caller never publishes WorkspaceChanged for a self-switch.
        let (_, _, mut wm) = setup();
        wm.open(); // sole workspace
        assert_eq!(wm.relative_id(1), None);
        assert_eq!(wm.relative_id(-1), None);
    }

    #[test]
    fn relative_id_orders_by_numeric_id_not_creation_order() {
        // WS-2: ids are sorted NUMERICALLY, not by `order` (creation) or `mru`
        // (touch-recency). Create 5 first, then 2 — creation order is [5, 2],
        // but the ring must still walk 2 -> 5 (numeric ascending).
        let (_, _, mut wm) = setup();
        wm.open(); // 1, current
        assert!(wm.switch(5)); // auto-created out of sequence; mru = [5, 1]
        assert!(wm.switch(2)); // auto-created; mru = [2, 5, 1]; order = [1, 5, 2]
        assert_eq!(wm.current_id(), 2);
        // Numeric order is [1, 2, 5]; from 2, next is 5 (not 1, which is what
        // `order`-based or `mru`-based traversal would produce).
        assert_eq!(wm.relative_id(1), Some(5));
        assert_eq!(wm.relative_id(-1), Some(1));
    }

    #[test]
    fn move_window_attaches_to_the_target_and_repairs_source_focus() {
        // MV-1: the focused window moves to `to` WITHOUT following (current
        // stays put). D9/D10: the source's remaining window regains focus,
        // re-expressing the repair `switch` already does (windows[0] = MRU).
        let (_, _, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(a);
        wm.attach(1);
        wm.attach(2); // a: windows [2, 1], focus 2
        assert!(wm.move_window(2, b));
        assert_eq!(wm.current_id(), a); // MV-1: view does not follow
        assert_eq!(wm.workspace(a).unwrap().windows, vec![1]);
        assert_eq!(wm.workspace(a).unwrap().focus, Some(1)); // repaired
        assert_eq!(wm.workspace(b).unwrap().windows, vec![2]);
        assert_eq!(wm.workspace(b).unwrap().focus, Some(2));
    }

    #[test]
    fn move_window_of_the_last_window_leaves_the_user_on_the_empty_source() {
        // D9 guard, half A (MATCHED with the destroy-path test below): moving
        // the LAST window off the current workspace must NOT trigger the
        // SC-ws-04 auto-switch. The bus assertion is load-bearing — any
        // refactor that routes move_window through detach (or copies its
        // policy) publishes WorkspaceChanged here and this test catches it,
        // even if the id assertion alone would not.
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(a);
        wm.attach(1); // a's sole window
        rx.try_recv().ok(); // drain WorkspaceOpened(1)
        rx.try_recv().ok(); // drain WorkspaceOpened(2)
        rx.try_recv().ok(); // drain the attach's implicit nothing (no-op safe)
        assert!(wm.move_window(1, b));
        assert_eq!(wm.current_id(), a); // stays on the now-empty source
        assert!(wm.workspace(a).unwrap().windows.is_empty());
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout),
            "MV-2: no WorkspaceChanged may publish on the move path"
        );
    }

    #[test]
    fn destroy_path_still_auto_switches_off_an_emptied_workspace() {
        // D9 guard, half B (MATCHED with the move-path test above): the
        // move-path suppression above applies ONLY to move_window. Destroying
        // (detach) the last window on the current workspace must still
        // auto-switch to the nearest remaining workspace, exactly as before
        // this change (SC-ws-04, unchanged).
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        wm.switch(a);
        wm.attach(1); // a's sole window
        wm.detach(1); // destroy path, not move
        assert_eq!(wm.current_id(), b); // auto-switched
        let events: Vec<_> = (0..3).map(|_| rx.recv()).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            events,
            vec![
                Event::WorkspaceOpened(1),
                Event::WorkspaceOpened(2),
                Event::WorkspaceChanged(2), // SC-ws-04 auto-switch still fires
            ]
        );
    }

    #[test]
    fn move_window_auto_creates_a_missing_target() {
        // Mirrors `switch`'s auto-create: MoveToWorkspace to an id that does
        // not exist yet creates it and attaches the window.
        let (_, _, mut wm) = setup();
        wm.attach(1); // sole workspace 1, focused
        assert_eq!(wm.len(), 1);
        assert!(wm.move_window(1, 5));
        assert_eq!(wm.len(), 2);
        assert_eq!(wm.workspace(5).unwrap().windows, vec![1]);
        assert_eq!(wm.workspace(5).unwrap().focus, Some(1));
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
