//! Core event loop (design data flow, T12): the pure WM core driven by a
//! [`DisplayServer`].
//!
//! Loop per design: X event (`DisplayServer::next_event`) → publish to the
//! [`EventBus`] → recompute placements → apply them (`configure`/`focus`/
//! `map`/`unmap`) → publish `PlacementsChanged` and a fresh `WmState` watch
//! snapshot. [`WindowManager`] (U3-A) owns the pure lifecycle; the display
//! seam owns every side effect (REQ-x11-003/005/006, SC-x11-05/07/08/09).
//!
//! # WmState sentinel convention (reconciled in T12)
//!
//! A current workspace id of `0` means "no workspace yet" — the same
//! convention as [`WorkspaceManager::current_id`]. The watch's initial
//! snapshot published by [`EventBus::new`] agrees (U1 shipped `current: 1`
//! as a placeholder; T12 aligned it with the manager's real sentinel). Once
//! the first window auto-opens a workspace, both report the real id (>= 1).

use std::collections::HashMap;
use std::sync::Arc;

use crate::bus::{EventBus, WmState};
use crate::command::Command;
use crate::config::Config;
use crate::display::{DErr, DisplayServer, FrameId};
use crate::event::{Event, KeyCombo};
use crate::geometry::{Rect, WindowId, WorkspaceId};
use crate::layout::{Layout, MasterStack};
use crate::window::{CommandEffect, WindowManager, WindowState};

/// The core window manager: one [`DisplayServer`] plus the pure state
/// machines, driven by [`App::run`].
pub struct App {
    bus: Arc<EventBus>,
    wm: WindowManager,
    display: Box<dyn DisplayServer>,
    config: Arc<Config>,
    area: Rect,
    layout: MasterStack,
    /// Frame id created per managed client (for `destroy_frame`).
    frames: HashMap<WindowId, FrameId>,
    /// Clients whose frame is currently mapped on screen.
    mapped: Vec<WindowId>,
}

impl App {
    /// Creates an app driving `display` over `area`, publishing on a fresh
    /// bus seeded with `config`.
    pub fn new(display: Box<dyn DisplayServer>, config: Arc<Config>, area: Rect) -> Self {
        let bus = Arc::new(EventBus::new(Arc::clone(&config)));
        let wm = WindowManager::new(Arc::clone(&bus));
        App {
            bus,
            wm,
            display,
            config,
            area,
            layout: MasterStack::default(),
            frames: HashMap::new(),
            mapped: Vec::new(),
        }
    }

    /// The event bus; subscribe before [`App::run`] to observe the stream.
    pub fn bus(&self) -> Arc<EventBus> {
        Arc::clone(&self.bus)
    }

    /// The window/workspace core (wiring and tests).
    pub fn wm(&mut self) -> &mut WindowManager {
        &mut self.wm
    }

    /// Runs the loop until the display reports no more events, a `Shutdown`
    /// event arrives, or the connection dies (logged and stopped). Display
    /// failures are logged; the loop keeps running (T13).
    pub fn run(&mut self) {
        loop {
            match self.display.next_event() {
                Ok(Some(ev)) => {
                    self.bus.publish(ev.clone());
                    if matches!(ev, Event::Shutdown) {
                        break; // graceful stop (U5 binary / tests)
                    }
                    if let Err(err) = self.handle(ev) {
                        eprintln!("tessera: {err}");
                    }
                }
                Ok(None) => break, // connection closed / script exhausted
                Err(err) => {
                    // The connection died: log and stop instead of spinning.
                    eprintln!("tessera: {err}");
                    break;
                }
            }
        }
    }

    /// Applies one translated event in place. [`App::run`] publishes each
    /// event on the bus and then calls this.
    pub fn handle(&mut self, ev: Event) -> Result<(), DErr> {
        match ev {
            Event::WindowMapRequested(w) => self.on_map_request(w),
            Event::WindowConfigureRequested(..) => self.recompute(),
            Event::WindowUnmapNotify(w) => {
                // Iconify: the client hid itself. The window stays attached
                // and its frame is kept until destroy (design v1 decision).
                self.wm.unmap_notify(w);
                Ok(())
            }
            Event::WindowDestroyNotify(w) => {
                self.wm.destroy_notify(w);
                self.destroy_frame_for(w);
                self.recompute()
            }
            Event::KeyPressed(combo) => match command_for_key(&self.config, combo) {
                Some(cmd) => self.on_command(cmd),
                None => Ok(()),
            },
            Event::Command(cmd) => self.on_command(cmd),
            Event::ConfigReloaded(cfg) => {
                self.config = cfg;
                self.publish_state();
                Ok(())
            }
            _ => Ok(()), // events the loop itself produced are already applied
        }
    }

    /// Fresh MapRequest: attach the window, create and map its frame, confirm
    /// it managed, then re-tile. An iconified (`UnmanagePending`) MapRequest
    /// only returns the window to `Managed` — its frame was never unmapped.
    fn on_map_request(&mut self, w: WindowId) -> Result<(), DErr> {
        let pending = self.wm.state_of(w) == Some(WindowState::UnmanagePending);
        self.wm.map_request(w);
        if !pending {
            let frame = self.display.manage(w)?;
            self.frames.insert(w, frame);
            self.display.map_window(w)?;
            self.mapped.push(w);
            self.wm.managed(w);
        }
        self.recompute()
    }

    /// Applies a user [`Command`]: core state changes are re-applied to the
    /// display; display-layer effects (U3-A's [`CommandEffect`]) are routed to
    /// the seam (Spawn -> `spawn`, Close -> `destroy_frame`, ...).
    fn on_command(&mut self, cmd: Command) -> Result<(), DErr> {
        match self.wm.apply_command(cmd) {
            CommandEffect::Applied => self.recompute(),
            CommandEffect::Ignored | CommandEffect::Unsupported => Ok(()),
            CommandEffect::SpawnTerminal => self.display.spawn(&self.config.general.terminal),
            CommandEffect::CloseFocused => {
                if let Some(focused) = self.wm.focused_window() {
                    self.destroy_frame_for(focused);
                }
                Ok(())
            }
        }
    }

    /// Destroys the frame of client `w` when one exists. Idempotent: the
    /// frame mapping is removed first, so a second call for the same client
    /// no-ops (the display-layer reaction to `WindowUnmapped`).
    fn destroy_frame_for(&mut self, w: WindowId) {
        if let Some(frame) = self.frames.remove(&w) {
            self.mapped.retain(|&m| m != w);
            if let Err(err) = self.display.destroy_frame(frame) {
                eprintln!("tessera: {err}");
            }
        }
    }

    /// Recomputes placements for the visible windows and applies them (design
    /// data flow step 3+4): unmap frames that left the visible set, map frames
    /// that joined it, configure each placement, focus the focused window,
    /// then publish `PlacementsChanged` and a fresh `WmState` snapshot.
    fn recompute(&mut self) -> Result<(), DErr> {
        let windows = self.wm.visible_windows();
        let focus = self.wm.focused_window();
        let focus_idx = focus
            .and_then(|f| windows.iter().position(|&w| w == f))
            .unwrap_or(0);
        let placements = self.layout.arrange(&windows, self.area, focus_idx);

        // Unmap the hidden first (SC-ws-06 "old unmap, new map"), then map the
        // newly visible. `mapped` mirrors the display's map state.
        for &w in self.mapped.clone().iter() {
            if !windows.contains(&w) {
                self.display.unmap_window(w)?;
                self.mapped.retain(|&m| m != w);
            }
        }
        for &w in &windows {
            if !self.mapped.contains(&w) {
                self.display.map_window(w)?;
                self.mapped.push(w);
            }
        }
        for p in &placements {
            self.display.configure(p.window, p.rect)?;
        }
        if let Some(f) = focus {
            self.display.focus_window(f)?;
        }
        self.bus
            .publish(Event::PlacementsChanged(self.wm.current_id(), placements));
        self.publish_state();
        Ok(())
    }

    /// Publishes a fresh [`WmState`] snapshot to the watch (REQ-bus-004).
    fn publish_state(&self) {
        let state = WmState {
            current: self.wm.current_id(),
            focused: self.wm.focused_window(),
            workspaces: self.wm.state_snapshots(),
            config: Arc::clone(&self.config),
        };
        self.bus.set_state(state);
    }
}

/// Pure keybinding lookup (REQ-x11-008): the [`Command`] bound to `combo`
/// under `cfg`, if any. `workspace[i]` maps to workspace `i + 1` (Super+0,
/// index 9, maps to workspace 10).
pub fn command_for_key(cfg: &Config, combo: KeyCombo) -> Option<Command> {
    let k = &cfg.keybindings;
    if combo == k.terminal {
        return Some(Command::SpawnTerminal);
    }
    if combo == k.focus_next {
        return Some(Command::FocusNext);
    }
    if combo == k.focus_prev {
        return Some(Command::FocusPrev);
    }
    if combo == k.close {
        return Some(Command::CloseFocused);
    }
    if combo == k.toggle_layout {
        return Some(Command::ToggleLayout);
    }
    for (i, bound) in k.workspace.iter().enumerate() {
        if *bound == combo {
            return Some(Command::SwitchWorkspace(i as WorkspaceId + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel::Receiver;

    use crate::bus::WorkspaceState;
    use crate::config::Config;
    use crate::display::test_double::{DisplayCall, MockDisplay};
    use crate::geometry::{LayoutKind, Placement, Rect};

    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 800,
        h: 600,
    };
    /// Frame border baked into every placement (config default).
    const B: u16 = 2;
    /// Default terminal (config default).
    const TERM: &str = "alacritty";

    /// Solo-window placement: the full area inset by the border.
    const SOLO: Rect = Rect {
        x: 2,
        y: 2,
        w: 796,
        h: 596,
    };
    /// Master placement for two windows at ratio 0.5.
    const MASTER: Rect = Rect {
        x: 2,
        y: 2,
        w: 396,
        h: 596,
    };
    /// Stack placement for two windows at ratio 0.5.
    const STACK: Rect = Rect {
        x: 402,
        y: 2,
        w: 396,
        h: 596,
    };

    fn app_with(script: Vec<Event>, config: Config) -> (App, Arc<Mutex<Vec<DisplayCall>>>) {
        let (mock, log) = MockDisplay::new(script);
        (App::new(Box::new(mock), Arc::new(config), AREA), log)
    }

    fn calls(log: &Arc<Mutex<Vec<DisplayCall>>>) -> Vec<DisplayCall> {
        log.lock().unwrap().clone()
    }

    fn drain(rx: &Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn map_request_manages_maps_and_places_focus() {
        // SC-x11-07 seam: MapRequest -> manage -> map -> configure (layout
        // placement, border inset) -> focus, in that order (REQ-x11-005).
        let (mut app, log) = app_with(vec![Event::WindowMapRequested(1)], Config::default());
        app.run();
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Manage(1),
                DisplayCall::Map(1),
                DisplayCall::Configure(1, SOLO),
                DisplayCall::Focus(1),
            ]
        );
    }

    #[test]
    fn publishes_full_event_stream_in_order() {
        // REQ-bus-001/002/005 at the loop level: the translated input event
        // is published, then the lifecycle and placement events the core
        // produces, all in arrival/processing order.
        let (mut app, _log) = app_with(vec![Event::WindowMapRequested(1)], Config::default());
        let rx = app.bus().subscribe_all();
        app.run();
        assert_eq!(
            drain(&rx),
            vec![
                Event::WindowMapRequested(1),
                Event::WorkspaceOpened(1),
                Event::WindowManaged(1),
                Event::PlacementsChanged(
                    1,
                    vec![Placement {
                        window: 1,
                        rect: SOLO,
                        border: B,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn watch_starts_at_no_workspace_and_snapshots_lifecycle() {
        // REQ-bus-004 at the loop level, plus the reconciled sentinel: the
        // initial snapshot reports current 0 ("no workspace yet", matching
        // WorkspaceManager), and after the first window a complete snapshot
        // is published.
        let (mut app, _log) = app_with(vec![Event::WindowMapRequested(1)], Config::default());
        let state_rx = app.bus().state_rx();
        assert_eq!(state_rx.borrow().current, 0); // sentinel, reconciled in T12
        app.run();
        let s = state_rx.borrow();
        assert_eq!(s.current, 1);
        assert_eq!(s.focused, Some(1));
        assert_eq!(
            s.workspaces,
            vec![WorkspaceState {
                id: 1,
                name: "1".to_string(),
                layout: LayoutKind::MasterStack,
                windows: vec![1],
                focus: Some(1),
            }]
        );
    }

    #[test]
    fn configure_request_retiles_to_layout_placement() {
        // SC-x11-09 / REQ-x11-006: a ConfigureRequest for a managed client is
        // answered by re-tiling to the layout placement — the requested size
        // is ignored.
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowConfigureRequested(
                    1,
                    Rect {
                        x: 0,
                        y: 0,
                        w: 9999,
                        h: 9999,
                    },
                ),
            ],
            Config::default(),
        );
        app.run();
        let calls = calls(&log);
        // The initial tiling, then the re-tile answering the ConfigureRequest:
        // the requested size (9999) is ignored, the layout placement applied.
        assert_eq!(calls.len(), 6);
        assert_eq!(calls[4], DisplayCall::Configure(1, SOLO));
        assert!(!calls.contains(&DisplayCall::Configure(
            1,
            Rect {
                x: 0,
                y: 0,
                w: 9999,
                h: 9999
            }
        )));
    }

    #[test]
    fn map_attaches_to_currently_focused_workspace() {
        // SC-x11-08 seam: a client mapping while workspace B is focused
        // attaches to B, not the workspace it visually appears on.
        let (mut app, log) = app_with(vec![Event::WindowMapRequested(9)], Config::default());
        app.wm().open(); // workspace 1
        let b = app.wm().open(); // workspace 2
        assert!(app.wm().switch(b));
        app.run();
        assert_eq!(app.wm().workspace(b).unwrap().windows, vec![9]);
        assert_eq!(
            app.wm().workspace(1).unwrap().windows,
            Vec::<WindowId>::new()
        );
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Manage(9),
                DisplayCall::Map(9),
                DisplayCall::Configure(9, SOLO),
                DisplayCall::Focus(9),
            ]
        );
    }

    #[test]
    fn switching_workspaces_unmaps_and_maps_frames() {
        // SC-ws-06 at the display seam: switching unmaps the old workspace's
        // frames, maps the new one's, re-tiles (master = MRU window) and
        // focuses the MRU window.
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowMapRequested(2),
                Event::Command(Command::SwitchWorkspace(1)),
                Event::WindowMapRequested(3),
                Event::Command(Command::SwitchWorkspace(2)),
            ],
            Config::default(),
        );
        app.wm().open();
        let b = app.wm().open();
        app.wm().switch(b);
        app.run();
        assert_eq!(
            calls(&log),
            vec![
                // MapRequest(1) on workspace 2
                DisplayCall::Manage(1),
                DisplayCall::Map(1),
                DisplayCall::Configure(1, SOLO),
                DisplayCall::Focus(1),
                // MapRequest(2): master/stack split
                DisplayCall::Manage(2),
                DisplayCall::Map(2),
                DisplayCall::Configure(2, MASTER),
                DisplayCall::Configure(1, STACK),
                DisplayCall::Focus(2),
                // Switch to workspace 1: workspace 2's frames unmapped
                DisplayCall::Unmap(1),
                DisplayCall::Unmap(2),
                // MapRequest(3) on workspace 1
                DisplayCall::Manage(3),
                DisplayCall::Map(3),
                DisplayCall::Configure(3, SOLO),
                DisplayCall::Focus(3),
                // Switch back to workspace 2: map 2,1; tile; focus MRU (2)
                DisplayCall::Unmap(3),
                DisplayCall::Map(2),
                DisplayCall::Map(1),
                DisplayCall::Configure(2, MASTER),
                DisplayCall::Configure(1, STACK),
                DisplayCall::Focus(2),
            ]
        );
    }

    #[test]
    fn destroy_destroys_frame_exactly_once() {
        // SC-x11-10/11 at the loop level: UnmapNotify then DestroyNotify in
        // one drain removes the window and destroys its frame exactly once.
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowUnmapNotify(1),
                Event::WindowDestroyNotify(1),
            ],
            Config::default(),
        );
        app.run();
        let calls = calls(&log);
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, DisplayCall::DestroyFrame(_)))
                .count(),
            1
        );
        assert_eq!(calls.last(), Some(&DisplayCall::DestroyFrame(FrameId(1))));
    }

    #[test]
    fn destroy_stream_publishes_unmapped_exactly_once() {
        // SC-x11-10/11: the full bus stream proves the window is unmanaged
        // (and the workspace emptied) without any duplicate WindowUnmapped.
        let (mut app, _log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowUnmapNotify(1),
                Event::WindowDestroyNotify(1),
            ],
            Config::default(),
        );
        let rx = app.bus().subscribe_all();
        app.run();
        assert_eq!(
            drain(&rx),
            vec![
                Event::WindowMapRequested(1),
                Event::WorkspaceOpened(1),
                Event::WindowManaged(1),
                Event::PlacementsChanged(
                    1,
                    vec![Placement {
                        window: 1,
                        rect: SOLO,
                        border: B,
                    }],
                ),
                Event::WindowUnmapNotify(1),
                Event::WindowDestroyNotify(1),
                Event::WindowUnmapped(1),
                Event::PlacementsChanged(1, vec![]),
            ]
        );
    }

    #[test]
    fn close_focused_destroys_the_focused_frame() {
        // CommandEffect::CloseFocused -> destroy_frame of the focused client's
        // frame; the pure core state is untouched until the client actually
        // dies (DestroyNotify).
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::Command(Command::CloseFocused),
            ],
            Config::default(),
        );
        app.run();
        assert_eq!(app.wm().focused_window(), Some(1));
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Manage(1),
                DisplayCall::Map(1),
                DisplayCall::Configure(1, SOLO),
                DisplayCall::Focus(1),
                DisplayCall::DestroyFrame(FrameId(1)),
            ]
        );
    }

    #[test]
    fn keypress_spawns_the_configured_terminal() {
        // SC-x11-12 seam: Super+Enter (default binding) reaches the display
        // layer as a spawn of the configured terminal.
        let (mut app, log) = app_with(
            vec![Event::KeyPressed(KeyCombo {
                mods: 1 << 6,
                key: 0xff0d,
            })],
            Config::default(),
        );
        app.run();
        assert_eq!(calls(&log), vec![DisplayCall::Spawn(TERM.to_string())]);
    }

    #[test]
    fn command_for_key_maps_the_default_bindings() {
        let cfg = Config::default();
        let super_ = 1 << 6;
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0xff0d
                }
            ),
            Some(Command::SpawnTerminal)
        );
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x006a
                }
            ),
            Some(Command::FocusNext)
        );
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x006b
                }
            ),
            Some(Command::FocusPrev)
        );
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x0071
                }
            ),
            Some(Command::CloseFocused)
        );
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x0020
                }
            ),
            Some(Command::ToggleLayout)
        );
        // Super+1..9 -> workspaces 1..9; Super+0 -> workspace 10.
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x0031
                }
            ),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(
            command_for_key(
                &cfg,
                KeyCombo {
                    mods: super_,
                    key: 0x0030
                }
            ),
            Some(Command::SwitchWorkspace(10))
        );
        assert_eq!(command_for_key(&cfg, KeyCombo { mods: 0, key: 0 }), None);
    }

    #[test]
    fn shutdown_stops_the_loop() {
        // Shutdown ends run(): events after it are never read.
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::Shutdown,
                Event::WindowMapRequested(2),
            ],
            Config::default(),
        );
        app.run();
        assert_eq!(app.wm().state_of(2), None);
        assert!(!calls(&log).contains(&DisplayCall::Manage(2)));
    }

    #[test]
    fn config_reload_updates_the_shared_snapshot() {
        // ConfigReloaded swaps the shared config and republishes the WmState
        // snapshot (D6): a watch consumer sees the new config immediately.
        let (mut app, _log) = app_with(Vec::new(), Config::default());
        let new_cfg = Arc::new(Config::default());
        app.handle(Event::ConfigReloaded(Arc::clone(&new_cfg)))
            .unwrap();
        let state_rx = app.bus().state_rx();
        assert!(Arc::ptr_eq(&state_rx.borrow().config, &new_cfg));
    }

    #[test]
    fn spawn_failure_is_logged_and_the_loop_survives() {
        // T13: a bogus terminal program must not take the WM down. Both
        // spawn attempts are made, each failure is logged, and the loop runs
        // to completion.
        let mut cfg = Config::default();
        cfg.general.terminal = "tessera-no-such-program-xyz".to_string();
        let (mut app, log) = app_with(
            vec![
                Event::Command(Command::SpawnTerminal),
                Event::Command(Command::SpawnTerminal),
            ],
            cfg,
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Spawn("tessera-no-such-program-xyz".to_string()),
                DisplayCall::Spawn("tessera-no-such-program-xyz".to_string()),
            ]
        );
    }

    #[test]
    fn spawn_success_does_not_stop_the_loop() {
        // A successful spawn is a side effect, not a state change: the loop
        // keeps processing the events that follow it.
        let mut cfg = Config::default();
        cfg.general.terminal = "/bin/true".to_string();
        let (mut app, log) = app_with(
            vec![
                Event::Command(Command::SpawnTerminal),
                Event::WindowMapRequested(1),
            ],
            cfg,
        );
        app.run();
        let calls = calls(&log);
        assert_eq!(
            calls.first(),
            Some(&DisplayCall::Spawn("/bin/true".to_string()))
        );
        assert!(calls.contains(&DisplayCall::Manage(1)));
    }
}
