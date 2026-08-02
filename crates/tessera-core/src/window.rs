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
    ///
    /// A fresh window is attached to the focused workspace and enters
    /// `Managing`; the display layer then creates and maps the frame and calls
    /// [`Self::managed`] to complete the transition. An iconified
    /// (`UnmanagePending`) window is mapped again — back to `Managed`, its
    /// frame re-mapped, without touching its attachment.
    pub fn map_request(&mut self, w: WindowId) {
        match self
            .states
            .get(&w)
            .copied()
            .unwrap_or(WindowState::Unmanaged)
        {
            WindowState::UnmanagePending => {
                // Re-map: the iconified window is shown again. It was never
                // detached, so nothing else changes.
                self.states.insert(w, WindowState::Managed);
            }
            WindowState::Unmanaged => {
                // Fresh MapRequest: attach (auto-opens a workspace) and enter
                // Managing until the frame is up.
                self.ws.attach(w);
                self.states.insert(w, WindowState::Managing);
            }
            _ => {} // duplicate MapRequest while Managing/Managed: ignore
        }
    }

    /// Marks `w` fully managed — the display layer mapped its frame — and
    /// publishes `WindowManaged` exactly once (SC-x11-07).
    pub fn managed(&mut self, w: WindowId) {
        if self.states.get(&w) == Some(&WindowState::Managing) {
            self.states.insert(w, WindowState::Managed);
            self.bus.publish(Event::WindowManaged(w));
        }
    }

    /// Handles an UnmapNotify for `w` (iconify path, SC-x11-11).
    ///
    /// A managed window becomes `UnmanagePending`: hidden but NOT removed. A
    /// later DestroyNotify still removes it; a MapRequest brings it back.
    pub fn unmap_notify(&mut self, w: WindowId) {
        if self.states.get(&w) == Some(&WindowState::Managed) {
            self.states.insert(w, WindowState::UnmanagePending);
        }
    }

    /// Handles a DestroyNotify for `w` — the authority for removal
    /// (SC-x11-10). Only `Managed` or `UnmanagePending` windows are removed;
    /// anything else (already `Unmanaged`, or unknown) is ignored, which is
    /// what makes unmanaging at-most-once.
    pub fn destroy_notify(&mut self, w: WindowId) {
        if matches!(
            self.states.get(&w),
            Some(WindowState::Managed) | Some(WindowState::UnmanagePending)
        ) {
            self.remove(w);
        }
    }

    /// Removes `w` exactly once: detach from its workspace, publish
    /// `WindowUnmapped`, and repair focus when the focused window died.
    ///
    /// Frame destruction is the display layer's reaction to `WindowUnmapped`
    /// (display seam, T12); the core owns the pure part of the removal.
    fn remove(&mut self, w: WindowId) {
        self.states.insert(w, WindowState::Unmanaged);
        let removed_focused = self.ws.focused_window() == Some(w);
        self.ws.detach(w);
        self.bus.publish(Event::WindowUnmapped(w));
        if removed_focused {
            self.repair_focus();
        }
    }

    /// After the focused window died, re-establish focus on the current
    /// workspace's MRU window when it still has windows (the detach may have
    /// switched workspaces when the focused workspace emptied).
    fn repair_focus(&mut self) {
        let Some(ws) = self.ws.workspace(self.ws.current_id()) else {
            return;
        };
        if ws.focus.is_none() && !ws.windows.is_empty() {
            // attach() sets focus to the window and, because it is already
            // present, does not re-insert it — pure focus repair.
            let mru = ws.windows[0];
            self.ws.attach(mru);
        }
    }

    /// Current lifecycle state of `w`, if it is known.
    pub fn state_of(&self, w: WindowId) -> Option<WindowState> {
        self.states.get(&w).copied()
    }

    /// FocusNext: cycle focus to the next window of the focused workspace in
    /// focus-history (MRU-first) order, wrapping at the end.
    pub fn focus_next(&mut self) -> bool {
        self.cycle(1)
    }

    /// FocusPrev: mirror of [`Self::focus_next`], wrapping at the start.
    pub fn focus_prev(&mut self) -> bool {
        self.cycle(-1)
    }

    /// Applies a user [`Command`] to the workspace state and reports what the
    /// display layer must do next (REQ-x11-008).
    ///
    /// Focus and workspace commands are applied here; spawn/close/layout
    /// commands have no pure-core effect and are handed back to the display
    /// layer through [`CommandEffect`].
    pub fn apply_command(&mut self, cmd: Command) -> CommandEffect {
        match cmd {
            Command::FocusNext => {
                if self.cycle(1) {
                    CommandEffect::Applied
                } else {
                    CommandEffect::Ignored
                }
            }
            Command::FocusPrev => {
                if self.cycle(-1) {
                    CommandEffect::Applied
                } else {
                    CommandEffect::Ignored
                }
            }
            Command::SwitchWorkspace(id) => {
                if self.ws.switch(id) {
                    CommandEffect::Applied
                } else {
                    CommandEffect::Ignored
                }
            }
            Command::SpawnTerminal => CommandEffect::SpawnTerminal,
            Command::CloseFocused => CommandEffect::CloseFocused,
            Command::ToggleLayout => CommandEffect::Unsupported,
        }
    }

    /// Cycles the focused workspace's focus by `dir` (1 = next, -1 = prev).
    ///
    /// Closed decision: FocusNext/Prev walk the workspace's focus-history
    /// (MRU-first) window list in order and wrap at the ends. The list stays
    /// fixed — the ring defines the cycle — so the focus moves through it via
    /// [`WorkspaceManager::focus_window`] without reordering history.
    fn cycle(&mut self, dir: i8) -> bool {
        let Some(ws) = self.ws.workspace(self.ws.current_id()) else {
            return false;
        };
        if ws.windows.len() < 2 {
            return false; // nothing to cycle between
        }
        let Some(target) = next_cycle_focus(&ws.windows, ws.focus, dir) else {
            return false;
        };
        self.ws.focus_window(target)
    }
}

/// Pure cycle step: the window that gains focus after moving `dir` positions
/// (1 = next, -1 = prev) through the MRU-first `windows` list, wrapping at the
/// ends. With no focused window, `next` starts at the front and `prev` at the
/// back; with fewer than two windows the focus is unchanged.
fn next_cycle_focus(windows: &[WindowId], focus: Option<WindowId>, dir: i8) -> Option<WindowId> {
    if windows.len() < 2 {
        return focus;
    }
    let n = windows.len() as i64;
    let idx = focus
        .and_then(|f| windows.iter().position(|&w| w == f))
        .map(|i| i as i64);
    let start = idx.unwrap_or(if dir > 0 { -1 } else { n });
    let next = (start + i64::from(dir)).rem_euclid(n);
    Some(windows[next as usize])
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

    use crate::command::Command;
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
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1))); // auto-open (SC-ws-01)
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
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1))); // auto-open (SC-ws-01)
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
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1))); // auto-open (SC-ws-01)
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

    #[test]
    fn map_request_starts_window_in_managing_state() {
        // A fresh MapRequest attaches the window but does not publish
        // WindowManaged until the frame is confirmed via `managed()`.
        let (_, rx, mut wm) = setup();
        wm.map_request(1);
        assert_eq!(wm.state_of(1), Some(WindowState::Managing));
        assert_eq!(wm.visible_windows(), vec![1]); // attached (auto-open)
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        wm.managed(1);
        assert_eq!(wm.state_of(1), Some(WindowState::Managed));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
    }

    #[test]
    fn unmap_only_iconifies_without_removing() {
        // UnmapNotify alone leaves the window managed but pending: it stays
        // attached; only a later DestroyNotify removes it (SC-x11-11).
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        wm.unmap_notify(1);
        assert_eq!(wm.state_of(1), Some(WindowState::UnmanagePending));
        assert_eq!(wm.visible_windows(), vec![1]); // still attached, just hidden
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout) // not removed yet
        );
        wm.destroy_notify(1);
        assert_eq!(wm.state_of(1), Some(WindowState::Unmanaged));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
    }

    #[test]
    fn map_request_on_pending_window_remaps_without_reattaching() {
        // MapRequest while UnmanagePending -> Managed (frame re-mapped); the
        // window was never detached so it is not re-attached and no second
        // WindowManaged is published.
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        wm.unmap_notify(1);
        wm.map_request(1);
        assert_eq!(wm.state_of(1), Some(WindowState::Managed));
        assert_eq!(wm.visible_windows(), vec![1]);
        wm.destroy_notify(1);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
    }

    #[test]
    fn removing_focused_window_repairs_focus_to_next_mru() {
        // remove(): detach + publish WindowUnmapped once + repair focus to the
        // workspace's new MRU window when the removed window was focused.
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2); // windows [2, 1], focus 2
        assert_eq!(wm.focused_window(), Some(2));
        wm.destroy_notify(2);
        assert_eq!(wm.focused_window(), Some(1)); // repaired, not dangling
        assert_eq!(wm.visible_windows(), vec![1]);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(2)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(2)));
    }

    #[test]
    fn removing_unfocused_window_keeps_focus() {
        // Destroying a non-focused window leaves the current focus untouched.
        let (_, rx, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2); // [2, 1], focus 2
        wm.destroy_notify(1); // 1 is not the focused window
        assert_eq!(wm.focused_window(), Some(2));
        assert_eq!(wm.visible_windows(), vec![2]);
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(2)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
    }

    #[test]
    fn destroying_window_on_unfocused_workspace_closes_it() {
        // SC-ws-02: when the removed window empties a non-focused workspace it
        // is destroyed; WorkspaceClosed precedes WindowUnmapped (design:
        // detach first, then publish the removal).
        let (_, rx, mut wm) = setup();
        let a = wm.open();
        let b = wm.open();
        assert!(wm.switch(b));
        manage(&mut wm, 1); // lands on the focused workspace b
        assert!(wm.switch(a));
        wm.destroy_notify(1); // window on the unfocused workspace b
        assert_eq!(wm.workspace(b), None); // empty + unfocused -> destroyed
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(2)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(2)));
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceChanged(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceClosed(2)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
    }

    #[test]
    fn focus_next_cycles_in_mru_order_and_wraps() {
        // Closed decision: FocusNext walks the focused workspace's MRU-first
        // list (3 is most recent), wrapping at the end back to the front.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2);
        manage(&mut wm, 3); // windows [3, 2, 1], focus 3
        assert!(wm.focus_next());
        assert_eq!(wm.focused_window(), Some(2));
        assert!(wm.focus_next());
        assert_eq!(wm.focused_window(), Some(1));
        assert!(wm.focus_next()); // wrap to the front
        assert_eq!(wm.focused_window(), Some(3));
    }

    #[test]
    fn focus_prev_cycles_backwards_and_wraps() {
        // FocusPrev is the mirror cycle, wrapping at the start back to the end.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2);
        manage(&mut wm, 3); // [3, 2, 1], focus 3
        assert!(wm.focus_prev());
        assert_eq!(wm.focused_window(), Some(1));
        assert!(wm.focus_prev());
        assert_eq!(wm.focused_window(), Some(2));
        assert!(wm.focus_prev()); // wrap to the end
        assert_eq!(wm.focused_window(), Some(3));
    }

    #[test]
    fn dispatch_applies_focus_and_switch_commands() {
        // The dispatch wires Command::FocusNext/FocusPrev to the cycle and
        // Command::SwitchWorkspace to the workspace switch (REQ-x11-008).
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2); // [2, 1], focus 2
        assert_eq!(wm.apply_command(Command::FocusNext), CommandEffect::Applied);
        assert_eq!(wm.focused_window(), Some(1));
        let b = wm.open(); // workspace 2; current stays 1
        assert_eq!(
            wm.apply_command(Command::SwitchWorkspace(b)),
            CommandEffect::Applied
        );
        assert_eq!(wm.current_id(), b);
    }

    #[test]
    fn dispatch_ignores_noop_commands() {
        // Commands with no target are ignored, never fatal: no windows to
        // cycle, a sole window, or an unknown workspace.
        let (_, _, mut wm) = setup();
        assert_eq!(wm.apply_command(Command::FocusNext), CommandEffect::Ignored);
        manage(&mut wm, 1);
        assert_eq!(wm.apply_command(Command::FocusPrev), CommandEffect::Ignored);
        assert_eq!(
            wm.apply_command(Command::SwitchWorkspace(9)),
            CommandEffect::Ignored
        );
        assert_eq!(wm.focused_window(), Some(1)); // state untouched
    }

    #[test]
    fn focus_cycle_keeps_list_order_stable() {
        // The closed decision walks the fixed MRU ring: cycling must not
        // reorder the focus-history list, only move the focus pointer.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2);
        manage(&mut wm, 3); // [3, 2, 1]
        wm.focus_next();
        assert_eq!(wm.visible_windows(), vec![3, 2, 1]); // order unchanged
        assert_eq!(wm.focused_window(), Some(2));
        wm.focus_next();
        assert_eq!(wm.visible_windows(), vec![3, 2, 1]);
        assert_eq!(wm.focused_window(), Some(1));
    }

    #[test]
    fn focus_next_from_no_focus_starts_at_mru() {
        // With no focused window (stale state after a detach), Next starts at
        // the most recently focused window.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2);
        manage(&mut wm, 3); // [3, 2, 1]
        wm.detach(3); // clears focus (U2 detach semantics), list [2, 1]
        assert_eq!(wm.focused_window(), None);
        assert!(wm.focus_next());
        assert_eq!(wm.focused_window(), Some(2)); // MRU end
        assert_eq!(wm.visible_windows(), vec![2, 1]);
    }

    #[test]
    fn focus_prev_from_no_focus_starts_at_lru() {
        // With no focused window, Prev starts at the least recent end.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2);
        manage(&mut wm, 3);
        wm.detach(3); // focus None, list [2, 1]
        assert!(wm.focus_prev());
        assert_eq!(wm.focused_window(), Some(1)); // LRU end
        assert_eq!(wm.visible_windows(), vec![2, 1]);
    }

    #[test]
    fn focus_cycle_needs_at_least_two_windows() {
        // Cycling with fewer than two windows is a no-op, never fatal.
        let (_, _, mut wm) = setup();
        assert!(!wm.focus_next()); // no workspace at all
        assert!(!wm.focus_prev());
        manage(&mut wm, 1);
        assert!(!wm.focus_next()); // sole window: nothing to cycle
        assert!(!wm.focus_prev());
        assert_eq!(wm.focused_window(), Some(1));
    }

    #[test]
    fn focus_cycle_stays_within_current_workspace() {
        // Cycling only touches the focused workspace: windows on other
        // workspaces are never brought into the cycle.
        let (_, _, mut wm) = setup();
        manage(&mut wm, 1);
        manage(&mut wm, 2); // [2, 1] on workspace 1
        let b = wm.open();
        assert!(wm.switch(b));
        manage(&mut wm, 9); // [9] on workspace 2
        wm.switch(1);
        wm.focus_next(); // cycle on workspace 1: 2 -> 1
        assert_eq!(wm.focused_window(), Some(1));
        assert_eq!(wm.workspace(b).unwrap().focus, Some(9)); // untouched
    }

    #[test]
    fn dispatch_reports_display_layer_commands() {
        // Commands with no pure-core effect are reported for the display
        // layer (T12/T13), never executed or silently swallowed.
        let (_, _, mut wm) = setup();
        assert_eq!(
            wm.apply_command(Command::SpawnTerminal),
            CommandEffect::SpawnTerminal
        );
        assert_eq!(
            wm.apply_command(Command::CloseFocused),
            CommandEffect::CloseFocused
        );
        assert_eq!(
            wm.apply_command(Command::ToggleLayout),
            CommandEffect::Unsupported
        );
    }
}
