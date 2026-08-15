//! Core event loop (design data flow, T12): the pure WM core driven by a
//! [`DisplayServer`].
//!
//! Loop per design: X event (`DisplayServer::next_event`) → publish to the
//! [`EventBus`] → recompute placements → apply them (`configure`/`focus`/
//! `map`/`unmap`) → publish `PlacementsChanged` and a fresh `WmState` watch
//! snapshot. [`WindowManager`] (U3-A) owns the pure lifecycle; the display
//! seam owns every side effect (REQ-x11-003/005/006, SC-x11-05/07/08/09).
//!
//! # WmState sentinel convention (reconciled in T12, narrowed here)
//!
//! A current workspace id of `0` means "no workspace yet" — the same
//! convention as [`WorkspaceManager::current_id`]. The watch's initial
//! snapshot seeded by [`EventBus::new`] still carries that sentinel (U1
//! shipped `current: 1` as a placeholder; T12 aligned it with the manager's
//! real sentinel), but [`App::new`] now replaces it immediately: it opens
//! workspace 1 during construction and publishes the resulting snapshot, so
//! every consumer of an `App`-driven watch sees `current == 1` and one
//! workspace from the start.
//!
//! The sentinel is therefore only ever observable on a bare `EventBus` that
//! no `App` has published to. It exists so a state consumer that DOES draw
//! the workspace strip (the bar) has something real to paint before the
//! first client attaches, instead of an empty bar until the first window
//! maps.

use std::collections::HashMap;
use std::sync::Arc;

use crate::bus::{EventBus, WmState};
use crate::command::Command;
use crate::config::Config;
use crate::display::{DErr, DisplayServer, FrameId};
use crate::event::{Event, KeyCombo};
use crate::geometry::{Direction, Rect, WindowId, WorkspaceId, resolve_direction};
use crate::layout::{DEFAULT_MASTER_RATIO, Layout, MasterStack};
use crate::theme::Theme;
use crate::window::{CommandEffect, WindowManager, WindowState};
use crate::window_kind::ManagePolicy;

/// The core window manager: one [`DisplayServer`] plus the pure state
/// machines, driven by [`App::run`].
pub struct App {
    bus: Arc<EventBus>,
    wm: WindowManager,
    display: Box<dyn DisplayServer>,
    config: Arc<Config>,
    /// Resolved theme (D4): published to the watch so state consumers read
    /// the palette; never mutated after construction.
    theme: Arc<Theme>,
    area: Rect,
    layout: MasterStack,
    /// Frame id created per managed client (for `destroy_frame`).
    frames: HashMap<WindowId, FrameId>,
    /// Clients whose frame is currently mapped on screen.
    mapped: Vec<WindowId>,
    /// D4 bar hook: called after every recompute publishes a fresh
    /// [`WmState`] snapshot. The binary uses it to redraw the bar exactly
    /// once per recompute — never on idle event polling.
    on_recompute: Option<Box<dyn FnMut()>>,
}

impl App {
    /// Creates an app driving `display` over `area`, publishing on a fresh
    /// bus seeded with `config` and the resolved `theme` (D4 seam), with
    /// workspace 1 already open.
    ///
    /// Opening workspace 1 here (rather than waiting for the first client to
    /// auto-open one through `WorkspaceManager::attach`) is what makes the
    /// workspace strip visible at startup: a state consumer that draws the
    /// snapshot has one real workspace to paint instead of the empty list
    /// that `EventBus::new` seeds. `publish_state` runs right after, so the
    /// watch reflects it before any event is handled — a consumer reading
    /// `state_rx().borrow()` immediately after construction already sees
    /// `current == 1`.
    pub fn new(
        display: Box<dyn DisplayServer>,
        config: Arc<Config>,
        theme: Arc<Theme>,
        area: Rect,
    ) -> Self {
        let bus = Arc::new(EventBus::new(Arc::clone(&config), Arc::clone(&theme)));
        let wm = WindowManager::new(Arc::clone(&bus));
        let layout = layout_for(&config);
        let mut app = App {
            bus,
            wm,
            display,
            config,
            theme,
            area,
            layout,
            frames: HashMap::new(),
            mapped: Vec::new(),
            on_recompute: None,
        };
        // The first workspace becomes current (REQ-ws-001), so this replaces
        // the bus's `0` sentinel with the real id 1. `WorkspaceOpened(1)` is
        // published here, BEFORE any caller can subscribe.
        app.wm.open();
        app.publish_state();
        app
    }

    /// The event bus; subscribe before [`App::run`] to observe the stream.
    pub fn bus(&self) -> Arc<EventBus> {
        Arc::clone(&self.bus)
    }

    /// Registers a callback invoked after every recompute publishes a fresh
    /// [`WmState`] snapshot (design D4). The binary hooks the bar here so it
    /// redraws exactly once per recompute and never on idle event polling.
    /// The core stays bar-free: it only knows "something wants to hear when
    /// state changed".
    pub fn set_on_recompute(&mut self, cb: Box<dyn FnMut()>) {
        self.on_recompute = Some(cb);
    }

    /// The window/workspace core (wiring and tests).
    pub fn wm(&mut self) -> &mut WindowManager {
        &mut self.wm
    }

    /// Runs the loop until the display reports no more events, a `Shutdown`
    /// event arrives, or the connection dies (logged and stopped). Display
    /// failures are logged; the loop keeps running (T13).
    ///
    /// The `on_recompute` hook fires ONCE before the first event is read, so
    /// a consumer that draws on the hook paints the startup snapshot
    /// (workspace 1, opened by [`App::new`]) instead of waiting for the
    /// first window to map. Inside the loop the D4 contract is unchanged:
    /// the hook fires once per recompute and never on idle event polling.
    pub fn run(&mut self) {
        if let Some(cb) = self.on_recompute.as_mut() {
            cb();
        }
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
                // The layout is rebuilt with the config it was derived from
                // (REQ-lay-002/004): `X11Display::configure` reads
                // `general.border_width` out of the SHARED config on every
                // pass, so a layout left on the old border/gaps after a
                // SIGHUP reload would recreate the exact mismatch this
                // change fixes — frames drawn at one width, space reserved
                // for another.
                self.layout = layout_for(&cfg);
                self.config = cfg;
                self.publish_state();
                Ok(())
            }
            _ => Ok(()), // events the loop itself produced are already applied
        }
    }

    /// Fresh MapRequest: classify the window (design D7), and for an
    /// ignore-but-map kind, map it raw and return BEFORE the function's only
    /// workspace mutation — so it never attaches, is never framed, never
    /// enters `mapped`, and `recompute` never runs for it (spec "Ignore-But-
    /// Map Policy", "No Geometry Requests for an Ignored Window", "Focus
    /// Survives an Ignored Window's Whole Life", "No Workspace Opens for an
    /// Ignored Window" — all four fall out of this one early return, not
    /// separate skip logic).
    ///
    /// Otherwise: attach the window, create and map its frame, confirm it
    /// managed, then re-tile — byte-identical to before this change. An
    /// iconified (`UnmanagePending`) MapRequest is never (re)classified (spec
    /// "A later type change is ignored"): `!pending` gates the classification
    /// check, so it only returns the window to `Managed` — its frame was
    /// never unmapped.
    fn on_map_request(&mut self, w: WindowId) -> Result<(), DErr> {
        let pending = self.wm.state_of(w) == Some(WindowState::UnmanagePending);
        if !pending && self.policy_for(w) == ManagePolicy::MapOnly {
            return self.display.map_unmanaged(w);
        }
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

    /// Fail-safe policy lookup (design D8): a classification failure logs
    /// through the loop's existing `tessera: <err>` pattern (see
    /// [`App::run`]) and resolves to [`ManagePolicy::Tile`] — the direction
    /// that keeps a misclassified window visible, framed, and closable
    /// instead of silently dropping it.
    fn policy_for(&mut self, w: WindowId) -> ManagePolicy {
        match self.display.window_kind(w) {
            Ok(kind) => kind.policy(),
            Err(err) => {
                eprintln!("tessera: {err}");
                ManagePolicy::Tile
            }
        }
    }

    /// Applies a user [`Command`]: core state changes are re-applied to the
    /// display; display-layer effects (U3-A's [`CommandEffect`]) are routed to
    /// the seam (Spawn -> `spawn`, Close -> `destroy_frame`, ...).
    fn on_command(&mut self, cmd: Command) -> Result<(), DErr> {
        // D8: `FocusDirection` is the only command needing geometry.
        // `Placement`s exist transiently inside `recompute`, and
        // `WindowManager` has neither `layout` nor `area` — threading them
        // through `apply_command` would pollute the pure core's signature
        // for one command, so it is intercepted here instead.
        if let Command::FocusDirection(dir) = cmd {
            return self.focus_direction(dir);
        }
        match self.wm.apply_command(cmd) {
            CommandEffect::Applied => self.recompute(),
            CommandEffect::Ignored | CommandEffect::Unsupported => Ok(()),
            CommandEffect::SpawnTerminal => self.display.spawn(&self.config.general.terminal),
            CommandEffect::SpawnLauncher => {
                self.display.spawn_with_args(&self.config.general.launcher)
            }
            CommandEffect::CloseFocused => {
                if let Some(focused) = self.wm.focused_window() {
                    self.destroy_frame_for(focused);
                }
                Ok(())
            }
        }
    }

    /// Resolves and applies a directional focus move (DF-1, D8): computes
    /// placements the SAME way `recompute` would (same `arrange` call, same
    /// focus index), resolves the pure geometry through
    /// [`resolve_direction`], then moves focus WITHOUT reordering the MRU
    /// ring (`WorkspaceManager::focus_window`) and re-tiles. With no
    /// focused window, or no candidate in `dir` (DF-2), this is a silent
    /// no-op — focus, placements and the workspace stay exactly as they
    /// were; NO wrap, unlike workspace stepping (a plane has no defined
    /// "next" window the way a ring has a defined successor).
    fn focus_direction(&mut self, dir: Direction) -> Result<(), DErr> {
        let Some(focused) = self.wm.focused_window() else {
            return Ok(());
        };
        let windows = self.wm.visible_windows();
        let focus_idx = windows.iter().position(|&w| w == focused).unwrap_or(0);
        let placements = self.layout.arrange(&windows, self.area, focus_idx);
        match resolve_direction(&placements, focused, dir) {
            Some(target) if self.wm.focus_window(target) => self.recompute(),
            _ => Ok(()), // DF-2: no candidate -> focus/placements/workspace unchanged
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
    ///
    /// D4: one window's display failure never aborts the pass. Each of the
    /// four steps below logs (`tessera: <err>`) and continues past a failed
    /// call instead of returning early; `self.mapped` is mutated only after
    /// the matching call succeeds, so a failed unmap KEEPS its entry
    /// (retried next pass — dropping it would strand a visible window
    /// floating over the layout) and a failed map is NOT pushed (also
    /// retried next pass). The publish + snapshot + bar hook tail always
    /// runs, and the pass always returns `Ok` once it has processed
    /// everything it could — `App::run` must not log the same per-window
    /// failure a second time.
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
                match self.display.unmap_window(w) {
                    Ok(()) => self.mapped.retain(|&m| m != w),
                    Err(err) => eprintln!("tessera: {err}"),
                }
            }
        }
        for &w in &windows {
            if !self.mapped.contains(&w) {
                match self.display.map_window(w) {
                    Ok(()) => self.mapped.push(w),
                    Err(err) => eprintln!("tessera: {err}"),
                }
            }
        }
        for p in &placements {
            if let Err(err) = self.display.configure(p.window, p.rect) {
                eprintln!("tessera: {err}");
            }
        }
        if let Some(f) = focus
            && let Err(err) = self.display.focus_window(f)
        {
            eprintln!("tessera: {err}");
        }
        self.bus
            .publish(Event::PlacementsChanged(self.wm.current_id(), placements));
        self.publish_state();
        // D4: the bar hook fires here — after the fresh snapshot is on the
        // watch — once per recompute, and only here (never on idle
        // polling). It always fires, even when one or more of the calls
        // above failed: one dying client must never silence the rest of the
        // pass.
        if let Some(cb) = self.on_recompute.as_mut() {
            cb();
        }
        Ok(())
    }

    /// Publishes a fresh [`WmState`] snapshot to the watch (REQ-bus-004).
    fn publish_state(&self) {
        let state = WmState {
            current: self.wm.current_id(),
            focused: self.wm.focused_window(),
            workspaces: self.wm.state_snapshots(),
            config: Arc::clone(&self.config),
            theme: Arc::clone(&self.theme),
        };
        self.bus.set_state(state);
    }
}

/// Builds the tiling layout FROM `config` (REQ-lay-002/004) — never
/// `MasterStack::default()`, whose values are only the config defaults.
///
/// The X layer sizes every frame with `general.border_width`
/// (`X11Display::configure`, saturating through the same `u16::try_from`),
/// so a layout hardcoding border 2 reserves the wrong footprint for any
/// other configured width, and one ignoring `general.gaps` renders the key
/// inert. `ratio` has no config key yet and stays at the design default.
fn layout_for(config: &Config) -> MasterStack {
    MasterStack::new(
        DEFAULT_MASTER_RATIO,
        u16::try_from(config.general.border_width).unwrap_or(u16::MAX),
        u16::try_from(config.general.gaps).unwrap_or(u16::MAX),
    )
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
    if combo == k.launcher {
        return Some(Command::SpawnLauncher);
    }
    if combo == k.workspace_next {
        return Some(Command::CycleWorkspace(1));
    }
    if combo == k.workspace_prev {
        return Some(Command::CycleWorkspace(-1));
    }
    if combo == k.focus_left {
        return Some(Command::FocusDirection(Direction::Left));
    }
    if combo == k.focus_down {
        return Some(Command::FocusDirection(Direction::Down));
    }
    if combo == k.focus_up {
        return Some(Command::FocusDirection(Direction::Up));
    }
    if combo == k.focus_right {
        return Some(Command::FocusDirection(Direction::Right));
    }
    for (i, bound) in k.workspace.iter().enumerate() {
        if *bound == combo {
            return Some(Command::SwitchWorkspace(i as WorkspaceId + 1));
        }
    }
    for (i, bound) in k.move_to_workspace.iter().enumerate() {
        if *bound == combo {
            return Some(Command::MoveToWorkspace(i as WorkspaceId + 1));
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
    use crate::display::test_double::{DisplayCall, FailAt, MockDisplay};
    use crate::geometry::{LayoutKind, Placement, Rect};
    use crate::theme::Theme;
    use crate::window_kind::WindowKind;

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

    /// Every constant below is the OUTER footprint the default config
    /// produces over `AREA` (border 2, gaps 3): the area cut into cells that
    /// tile it exactly, each shrunk by 3 on all four sides. Borders are NOT
    /// subtracted — `configure_frame` does that once, at the X boundary.
    ///
    /// Solo-window placement: the whole area, minus the edge gap.
    const SOLO: Rect = Rect {
        x: 3,
        y: 3,
        w: 794,
        h: 594,
    };
    /// Master placement for two windows at ratio 0.5.
    const MASTER: Rect = Rect {
        x: 3,
        y: 3,
        w: 394,
        h: 594,
    };
    /// Stack placement for two windows at ratio 0.5.
    const STACK: Rect = Rect {
        x: 403,
        y: 3,
        w: 394,
        h: 594,
    };
    /// Upper stack slot for three windows at ratio 0.5 (D4 resilience tests).
    const STACK_TOP: Rect = Rect {
        x: 403,
        y: 3,
        w: 394,
        h: 294,
    };
    /// Lower stack slot for three windows at ratio 0.5 (D4 resilience tests).
    const STACK_BOTTOM: Rect = Rect {
        x: 403,
        y: 303,
        w: 394,
        h: 294,
    };

    fn app_with(script: Vec<Event>, config: Config) -> (App, Arc<Mutex<Vec<DisplayCall>>>) {
        let (mock, log) = MockDisplay::new(script);
        (
            App::new(
                Box::new(mock),
                Arc::new(config),
                Arc::new(Theme::default()),
                AREA,
            ),
            log,
        )
    }

    fn app_with_theme(
        script: Vec<Event>,
        config: Config,
        theme: Arc<Theme>,
    ) -> (App, Arc<Mutex<Vec<DisplayCall>>>) {
        let (mock, log) = MockDisplay::new(script);
        (App::new(Box::new(mock), Arc::new(config), theme, AREA), log)
    }

    /// Sibling of [`app_with`] that scripts `failures` on the underlying
    /// [`MockDisplay`] before the app ever sees it (D4 resilience tests).
    /// Kept separate so `app_with`'s signature — and every test already
    /// calling it — stays untouched.
    fn app_with_failures(
        script: Vec<Event>,
        config: Config,
        failures: Vec<FailAt>,
    ) -> (App, Arc<Mutex<Vec<DisplayCall>>>) {
        let (mut mock, log) = MockDisplay::new(script);
        for at in failures {
            mock.fail_at(at);
        }
        (
            App::new(
                Box::new(mock),
                Arc::new(config),
                Arc::new(Theme::default()),
                AREA,
            ),
            log,
        )
    }

    /// Sibling of [`app_with`] that scripts `kinds` on the underlying
    /// [`MockDisplay`] before the app ever sees it (D9 window-type-awareness
    /// tests). Kept separate so `app_with`'s signature — and every test
    /// already calling it — stays untouched, matching [`app_with_failures`].
    fn app_with_kinds(
        script: Vec<Event>,
        config: Config,
        kinds: Vec<(WindowId, WindowKind)>,
    ) -> (App, Arc<Mutex<Vec<DisplayCall>>>) {
        let (mut mock, log) = MockDisplay::new(script);
        for (w, kind) in kinds {
            mock.set_kind(w, kind);
        }
        (
            App::new(
                Box::new(mock),
                Arc::new(config),
                Arc::new(Theme::default()),
                AREA,
            ),
            log,
        )
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
    fn layout_is_built_from_the_configured_border_and_gaps() {
        // REQ-lay-002/004: the layout must be built FROM the config, not from
        // `MasterStack::default()`. Before this change `App::new` hardcoded
        // border 2 and ignored `gaps` entirely, so a user configuring
        // `border_width = 4, gaps = 10` got frames the X layer drew with a
        // 4px border inside a layout that had reserved 2 — and no gap at
        // all. The single-window placement is the sharpest probe: its
        // footprint is the whole area minus one gap per side, and its
        // published border must be the CONFIGURED width.
        let mut cfg = Config::default();
        cfg.general.border_width = 4;
        cfg.general.gaps = 10;
        let (mut app, log) = app_with(vec![Event::WindowMapRequested(1)], cfg);
        let rx = app.bus().subscribe_all();
        app.run();
        let wide = Rect {
            x: 10,
            y: 10,
            w: 780,
            h: 580,
        };
        assert!(calls(&log).contains(&DisplayCall::Configure(1, wide)));
        assert!(
            drain(&rx).contains(&Event::PlacementsChanged(
                1,
                vec![Placement {
                    window: 1,
                    rect: wide,
                    border: 4,
                }],
            )),
            "the published placement must carry the configured border width"
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
                // `WorkspaceOpened(1)` is NOT here: `App::new` opens
                // workspace 1 during construction, before `subscribe_all`.
                Event::WindowMapRequested(1),
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
    fn recompute_hook_fires_once_per_recompute_not_on_idle_events() {
        // D4 hook (bar drawing, design D4 / task 2.6): the binary redraws the
        // bar exactly once per recompute — never on idle event polling. A
        // MapRequest triggers one recompute; a following UnmapNotify does NOT
        // (its handler never re-tiles). `run` also fires the hook once up
        // front so the startup snapshot is painted, so the total is two: the
        // initial fire plus the one recompute, and the idle UnmapNotify adds
        // nothing.
        let (mut app, _log) = app_with(
            vec![Event::WindowMapRequested(1), Event::WindowUnmapNotify(1)],
            Config::default(),
        );
        let fires = Arc::new(Mutex::new(0));
        let hook = Arc::clone(&fires);
        app.set_on_recompute(Box::new(move || *hook.lock().unwrap() += 1));
        app.run();
        assert_eq!(
            *fires.lock().unwrap(),
            2,
            "one startup fire plus one per recompute, never on idle events"
        );
    }

    #[test]
    fn recompute_hook_fires_at_startup_before_any_event() {
        // Startup visibility: a consumer that draws on the hook must paint
        // the initial state instead of waiting for the first window. The
        // script is EMPTY, so the only possible fire is the one `run` issues
        // before entering the loop — and the snapshot it can read already
        // carries workspace 1.
        let (mut app, _log) = app_with(Vec::new(), Config::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::clone(&seen);
        let rx = app.bus().state_rx();
        app.set_on_recompute(Box::new(move || {
            hook.lock().unwrap().push(rx.borrow().current);
        }));
        app.run();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![1],
            "the hook fires exactly once at startup, on a snapshot already \
             carrying workspace 1"
        );
    }

    #[test]
    fn new_opens_workspace_one_and_publishes_it_before_any_event() {
        // Startup visibility at the source: the bar's early return on an
        // empty workspace list is correct, so the core must not hand it an
        // empty snapshot. `App::new` opens workspace 1 and publishes, so
        // both the manager and the watch report it before `run` is ever
        // called.
        let (mut app, _log) = app_with(Vec::new(), Config::default());
        let state_rx = app.bus().state_rx();
        assert_eq!(app.wm().current_id(), 1);
        let s = state_rx.borrow();
        assert_eq!(s.current, 1);
        assert_eq!(
            s.workspaces,
            vec![WorkspaceState {
                id: 1,
                name: "1".to_string(),
                layout: LayoutKind::MasterStack,
                windows: Vec::new(),
                focus: None,
            }],
            "exactly one workspace, empty, before any window maps"
        );
    }

    #[test]
    fn app_publishes_resolved_theme_in_state_snapshot() {
        // D4: the App owns the resolved theme and exposes it through the
        // WmState watch, so the bar (Change-2) reads the palette headless.
        let theme = Arc::new(Theme::default());
        let (mut app, _log) = app_with_theme(
            vec![Event::WindowMapRequested(1)],
            Config::default(),
            Arc::clone(&theme),
        );
        let state_rx = app.bus().state_rx();
        app.run();
        assert!(Arc::ptr_eq(&state_rx.borrow().theme, &theme));
    }

    #[test]
    fn bus_seeds_the_no_workspace_sentinel_before_an_app_publishes() {
        // The `0` = "no workspace yet" sentinel still exists exactly where
        // T12 put it — on the bare bus. It is just no longer observable
        // through an `App`, which replaces it during construction (see
        // `watch_starts_at_workspace_one_and_snapshots_lifecycle`).
        let bus = EventBus::new(Arc::new(Config::default()), Arc::new(Theme::default()));
        assert_eq!(bus.state_rx().borrow().current, 0);
        assert!(bus.state_rx().borrow().workspaces.is_empty());
    }

    #[test]
    fn watch_starts_at_workspace_one_and_snapshots_lifecycle() {
        // REQ-bus-004 at the loop level: the initial snapshot already
        // reports workspace 1 (opened by `App::new`, replacing the bus's `0`
        // sentinel), and after the first window a complete snapshot is
        // published.
        let (mut app, _log) = app_with(vec![Event::WindowMapRequested(1)], Config::default());
        let state_rx = app.bus().state_rx();
        assert_eq!(state_rx.borrow().current, 1);
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
        // `App::new` already opened workspace 1; this adds workspace 2.
        let b = app.wm().open();
        assert_eq!(b, 2);
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
        // `App::new` already opened workspace 1; this adds workspace 2.
        let b = app.wm().open();
        assert_eq!(b, 2);
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
                // `WorkspaceOpened(1)` fired in `App::new`, before subscribe.
                Event::WindowMapRequested(1),
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
        assert_eq!(
            calls(&log),
            vec![DisplayCall::Spawn(vec![TERM.to_string()])]
        );
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
    fn command_for_key_maps_the_workspace_step_and_move_bindings() {
        // D7: workspace_next/workspace_prev resolve to CycleWorkspace(±1);
        // move_to_workspace[i] resolves to MoveToWorkspace(i + 1), mirroring
        // the existing `workspace` loop exactly.
        let cfg = Config::default();
        let k = &cfg.keybindings;
        assert_eq!(
            command_for_key(&cfg, k.workspace_next),
            Some(Command::CycleWorkspace(1))
        );
        assert_eq!(
            command_for_key(&cfg, k.workspace_prev),
            Some(Command::CycleWorkspace(-1))
        );
        assert_eq!(
            command_for_key(&cfg, k.move_to_workspace[0]),
            Some(Command::MoveToWorkspace(1))
        );
        assert_eq!(
            command_for_key(&cfg, k.move_to_workspace[9]),
            Some(Command::MoveToWorkspace(10))
        );
    }

    // === Directional focus — WU2 (tessera-navigation-bindings) ===

    #[test]
    fn focus_direction_moves_focus_and_recomputes() {
        // D8: FocusDirection resolves through the SAME layout arrange() call
        // recompute uses, moves focus without reordering the MRU ring, and
        // triggers exactly one recompute pass. Window 2 is mapped first,
        // then window 1 (attach-order MRU-first), so window 1 ends up
        // master — matching the spec's "w1 x[2,398]..., w2 x[402,798]..."
        // 2-window golden. FocusDirection(Right) from the master reaches the
        // stack window (spec scenario "Right, single unambiguous candidate").
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(2),
                Event::WindowMapRequested(1),
                Event::Command(Command::FocusDirection(Direction::Right)),
            ],
            Config::default(),
        );
        app.run();
        assert_eq!(app.wm().focused_window(), Some(2));
        assert_eq!(calls(&log).last(), Some(&DisplayCall::Focus(2)));
    }

    #[test]
    fn focus_direction_without_a_candidate_changes_nothing() {
        // DF-2: no candidate in `dir` is a no-op — focus, placements and the
        // workspace stay exactly as they were, and the command must NOT
        // trigger a second recompute (no extra Configure/Focus calls beyond
        // the initial map).
        let (mut app, log) = app_with(
            vec![
                Event::WindowMapRequested(1),
                Event::Command(Command::FocusDirection(Direction::Right)),
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
            ],
            "a directional focus with no candidate must not trigger a second recompute"
        );
    }

    #[test]
    fn command_for_key_maps_the_directional_focus_bindings() {
        // D7: Super+Shift+{h,j,k,l} resolve to FocusDirection(Left/Down/Up/Right).
        let cfg = Config::default();
        let k = &cfg.keybindings;
        assert_eq!(
            command_for_key(&cfg, k.focus_left),
            Some(Command::FocusDirection(Direction::Left))
        );
        assert_eq!(
            command_for_key(&cfg, k.focus_down),
            Some(Command::FocusDirection(Direction::Down))
        );
        assert_eq!(
            command_for_key(&cfg, k.focus_up),
            Some(Command::FocusDirection(Direction::Up))
        );
        assert_eq!(
            command_for_key(&cfg, k.focus_right),
            Some(Command::FocusDirection(Direction::Right))
        );
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
    fn config_reload_rebuilds_the_layout_from_the_new_config() {
        // A SIGHUP reload is a supported way to apply `border_width`/`gaps`
        // (README "restart (or SIGHUP)"), and `X11Display::configure` reads
        // the border out of the shared config on the very next pass — so a
        // layout kept at the OLD values would leave the WM drawing 4px
        // borders inside space reserved for 2px, the mismatch this change
        // exists to remove. The re-tile after the reload must use the new
        // geometry.
        let (mut app, log) = app_with(vec![Event::WindowMapRequested(1)], Config::default());
        app.run();
        assert!(calls(&log).contains(&DisplayCall::Configure(1, SOLO)));

        let mut reloaded = Config::default();
        reloaded.general.border_width = 4;
        reloaded.general.gaps = 10;
        app.handle(Event::ConfigReloaded(Arc::new(reloaded)))
            .unwrap();
        app.handle(Event::WindowConfigureRequested(1, AREA))
            .unwrap();

        assert_eq!(
            calls(&log).last(),
            Some(&DisplayCall::Focus(1)),
            "the reload must still end in a normal re-tile pass"
        );
        assert!(
            calls(&log).contains(&DisplayCall::Configure(
                1,
                Rect {
                    x: 10,
                    y: 10,
                    w: 780,
                    h: 580,
                }
            )),
            "the placement after a reload must use the reloaded border and gaps"
        );
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
                DisplayCall::Spawn(vec!["tessera-no-such-program-xyz".to_string()]),
                DisplayCall::Spawn(vec!["tessera-no-such-program-xyz".to_string()]),
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
            Some(&DisplayCall::Spawn(vec!["/bin/true".to_string()]))
        );
        assert!(calls.contains(&DisplayCall::Manage(1)));
    }

    // === Launcher routing — PR3 / WU3 (tessera-keybinds-launcher) ===

    #[test]
    fn keypress_ctrl_space_spawns_the_configured_launcher() {
        // ALA-2 scenario "Default Ctrl+Space opens rofi": Ctrl+Space
        // (mods=4, key=0x0020) is looked up and dispatched to a spawn of the
        // configured launcher array, recorded as Spawn(vec![...]).
        let (mut app, log) = app_with(
            vec![Event::KeyPressed(KeyCombo {
                mods: 4,
                key: 0x0020,
            })],
            Config::default(),
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![DisplayCall::Spawn(vec![
                "rofi".to_string(),
                "-show".to_string(),
                "drun".to_string(),
            ])]
        );
    }

    #[test]
    fn keypress_ctrl_space_uses_the_configured_launcher_override() {
        // ALA-2 scenario "Launcher is configurable": the [general] launcher
        // override replaces the rofi default at the loop level.
        let mut cfg = Config::default();
        cfg.general.launcher = vec!["dmenu_run".to_string()];
        let (mut app, log) = app_with(
            vec![Event::KeyPressed(KeyCombo {
                mods: 4,
                key: 0x0020,
            })],
            cfg,
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![DisplayCall::Spawn(vec!["dmenu_run".to_string()])]
        );
    }

    #[test]
    fn launcher_failure_is_logged_and_the_loop_survives() {
        // ALA-3 "rofi missing": a launcher absent from PATH (DErr::Spawn) is
        // logged by run(), the loop keeps running, and the binding stays
        // inert — no WM state changes, nothing crashes.
        let mut cfg = Config::default();
        cfg.general.launcher = vec!["tessera-no-such-program-xyz".to_string()];
        let (mut app, log) = app_with(
            vec![
                Event::KeyPressed(KeyCombo {
                    mods: 4,
                    key: 0x0020,
                }),
                Event::KeyPressed(KeyCombo {
                    mods: 4,
                    key: 0x0020,
                }),
            ],
            cfg,
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Spawn(vec!["tessera-no-such-program-xyz".to_string()]),
                DisplayCall::Spawn(vec!["tessera-no-such-program-xyz".to_string()]),
            ]
        );
        // Binding inert: the startup workspace is still the only one and it
        // is still empty — the failed launcher changed no WM state.
        assert_eq!(app.wm().current_id(), 1);
        assert_eq!(app.wm().state_snapshots().len(), 1);
        assert_eq!(
            app.wm().workspace(1).unwrap().windows,
            Vec::<WindowId>::new()
        );
    }

    #[test]
    fn defaults_preserved_super_space_toggles_and_super_enter_spawns() {
        // ALA-2 scenario "Existing defaults preserved": Super+Space still maps
        // to ToggleLayout (no spawn at all) and Super+Enter still spawns the
        // terminal — the launcher change must not disturb the Super set.
        let (mut app, log) = app_with(
            vec![
                Event::KeyPressed(KeyCombo {
                    mods: 1 << 6,
                    key: 0x0020,
                }),
                Event::KeyPressed(KeyCombo {
                    mods: 1 << 6,
                    key: 0xff0d,
                }),
            ],
            Config::default(),
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![DisplayCall::Spawn(vec![TERM.to_string()])]
        );
    }

    // === recompute log-and-continue — PR3 / WU4 (core-recompute-resilience, D4) ===

    #[test]
    fn recompute_configure_failure_for_one_window_still_configures_the_rest_and_reaches_focus() {
        // D4 / task list "cover at minimum": a failing configure for one
        // window must not abort the pass — the remaining placements are
        // still applied, and focus is still attempted afterward.
        let (mut app, log) = app_with_failures(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowMapRequested(2),
                Event::WindowMapRequested(3),
            ],
            Config::default(),
            vec![FailAt::Configure(2)],
        );
        app.run();
        let calls = calls(&log);
        // Windows attach most-recent-first, so after mapping 1, 2, 3 the
        // final pass's placements are: 3 (master), 2 (upper stack), 1
        // (lower stack) — window 2's configure was attempted and recorded
        // despite failing...
        assert!(calls.contains(&DisplayCall::Configure(2, STACK_TOP)));
        // ...and the pass continued: window 1's configure (ordered AFTER
        // the failing one) still ran...
        assert!(calls.contains(&DisplayCall::Configure(1, STACK_BOTTOM)));
        // ...and focus of the master window (3) was still reached last.
        assert_eq!(calls.last(), Some(&DisplayCall::Focus(3)));
    }

    #[test]
    fn recompute_publishes_state_and_fires_the_bar_hook_after_a_failed_focus() {
        // D4 task 3.1: a failing focus call must not skip the state publish
        // or the bar hook — only the individual display call is allowed to
        // fail.
        let (mut app, log) = app_with_failures(
            vec![Event::WindowMapRequested(1)],
            Config::default(),
            vec![FailAt::Focus(1)],
        );
        let rx = app.bus().subscribe_all();
        let state_rx = app.bus().state_rx();
        let fires = Arc::new(Mutex::new(0));
        let hook = Arc::clone(&fires);
        app.set_on_recompute(Box::new(move || *hook.lock().unwrap() += 1));
        app.run();

        // The focus attempt was still made and recorded despite the
        // scripted failure.
        assert!(calls(&log).contains(&DisplayCall::Focus(1)));
        // The bar hook still fires for the recompute — a focus failure does
        // not skip it (the second fire; the first is `run`'s startup one).
        assert_eq!(*fires.lock().unwrap(), 2);
        // A fresh WmState snapshot was still published.
        assert_eq!(state_rx.borrow().focused, Some(1));
        // PlacementsChanged was still published on the bus.
        assert!(
            drain(&rx)
                .iter()
                .any(|ev| matches!(ev, Event::PlacementsChanged(..)))
        );
    }

    #[test]
    fn recompute_keeps_the_mapped_mirror_when_unmap_fails() {
        // D4 task 3.3: a failed unmap must not drop the window from
        // `mapped` (it is retried on a later pass instead of stranding a
        // visible frame with no display-side liveness tracking), AND it
        // must not block the unmap of the OTHER windows leaving visibility
        // in the same pass. Two windows both go hidden in one switch;
        // window 1's unmap is scripted to fail and is ordered FIRST in
        // `mapped`, so the current `?`-abort bug would also swallow
        // window 2's unmap entirely.
        let (mut app, log) = app_with_failures(
            vec![
                Event::WindowMapRequested(1),
                Event::WindowMapRequested(2),
                Event::Command(Command::SwitchWorkspace(2)),
            ],
            Config::default(),
            vec![FailAt::Unmap(1)],
        );
        app.run();
        let calls = calls(&log);
        // Window 1's unmap was attempted and recorded despite failing...
        assert!(calls.contains(&DisplayCall::Unmap(1)));
        // ...and the pass continued: window 2's unmap (ordered AFTER the
        // failing one) still ran.
        assert!(calls.contains(&DisplayCall::Unmap(2)));
        // `mapped` keeps window 1 (failed, retried next pass) but drops
        // window 2 (succeeded).
        assert!(app.mapped.contains(&1));
        assert!(!app.mapped.contains(&2));
    }

    #[test]
    fn recompute_retries_the_map_next_pass_when_map_fails() {
        // D4 task 3.2: window 1 already has a live frame and is attached +
        // Managed, but is not yet reflected in `mapped` — exactly the state
        // recompute's own map loop must handle (distinct from
        // `on_map_request`'s unrelated first-map path, out of D4's scope).
        // A failing map here must not abort the pass and must not push the
        // window into `mapped` anyway.
        let (mut app, log) = app_with_failures(Vec::new(), Config::default(), vec![FailAt::Map(1)]);
        app.wm().map_request(1);
        app.wm().managed(1);
        app.frames.insert(1, FrameId(1));
        let _ = app.handle(Event::WindowConfigureRequested(1, AREA));

        // The map attempt was made and recorded despite the scripted
        // failure...
        assert!(calls(&log).contains(&DisplayCall::Map(1)));
        // ...and recompute still continued past it: the window was
        // configured and focus was still attempted, instead of the pass
        // aborting silently.
        assert!(calls(&log).contains(&DisplayCall::Configure(1, SOLO)));
        assert!(calls(&log).contains(&DisplayCall::Focus(1)));
        // A failed map must not be mirrored into `mapped` — pushing it
        // anyway would desync the mirror from the display's real state and
        // skip the retry on the next pass permanently.
        assert!(!app.mapped.contains(&1));
    }

    #[test]
    fn recompute_log_and_continue_survives_the_second_and_third_sequential_close() {
        // core-recompute-resilience scenario "second and third close in a
        // row keep working": D4's log-and-continue must not be a one-shot
        // fix that only covers the FIRST failing pass. The original bug's
        // `?` early-return aborted on the very first failure, so a test
        // that closes once cannot tell "fixed" from "still broken from the
        // second close onward". Window 1 stays managed and visible for the
        // whole test and its configure is scripted to fail on EVERY
        // invocation (not just once), so the assertions below prove the
        // failure is logged-and-continued identically on the first,
        // second, AND third sequential close.
        let (mut app, log) =
            app_with_failures(Vec::new(), Config::default(), vec![FailAt::Configure(1)]);
        let fires = Arc::new(Mutex::new(0));
        let hook = Arc::clone(&fires);
        app.set_on_recompute(Box::new(move || *hook.lock().unwrap() += 1));

        for w in [1, 2, 3, 4] {
            app.handle(Event::WindowMapRequested(w)).unwrap();
        }

        // Close windows 4, 3, and 2 one after another; window 1 is never
        // closed, so it remains visible (and its configure keeps failing)
        // through all three passes.
        for (which, closed) in [("first", 4u32), ("second", 3), ("third", 2)] {
            log.lock().unwrap().clear();
            *fires.lock().unwrap() = 0;

            app.handle(Event::WindowDestroyNotify(closed)).unwrap();

            let calls_now = calls(&log);
            assert!(
                calls_now
                    .iter()
                    .any(|c| matches!(c, DisplayCall::Configure(1, _))),
                "{which} close: window 1's configure was attempted despite its \
                 persistent scripted failure"
            );
            assert!(
                calls_now.iter().any(|c| matches!(c, DisplayCall::Focus(_))),
                "{which} close: focus was still attempted after the configure \
                 failure instead of the pass aborting early"
            );
            assert_eq!(
                *fires.lock().unwrap(),
                1,
                "{which} close: the bar hook still fires exactly once despite \
                 the persistent configure failure"
            );
        }

        // Window 1 itself was never closed; it stays managed the whole time.
        assert!(app.frames.contains_key(&1));
    }

    // === Window-type awareness — PR1 / WU1 (tessera-window-type-awareness) ===
    //
    // NOTE (framing, tasks Work Unit 1): `MockDisplay::window_kind` defaults
    // to `WindowKind::Normal` (D2), and these tests script the classification
    // headless through `set_kind`/`app_with_kinds`. `X11Display` has no
    // override yet (Unit 2), so in production every real window still
    // classifies `Normal` and this branch is UNREACHABLE live — proven here,
    // not yet wired to a real X server.

    #[test]
    fn notification_map_request_maps_raw_and_never_enters_workspace_state() {
        // Spec "Notification is visible but untracked" / "No Geometry
        // Requests for an Ignored Window": D7's early return at statement 3
        // fires BEFORE the function's only workspace mutation, so the
        // ignored window's MapRequest issues exactly one raw-map call
        // (`MapUnmanaged`) and nothing else — no `Configure`, and the
        // workspace's window list never gains it.
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1), Event::WindowMapRequested(2)],
            Config::default(),
            vec![(2, WindowKind::Notification)],
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![
                DisplayCall::Manage(1),
                DisplayCall::Map(1),
                DisplayCall::Configure(1, SOLO),
                DisplayCall::Focus(1),
                DisplayCall::MapUnmanaged(2),
            ]
        );
        assert_eq!(app.wm().workspace(1).unwrap().windows, vec![1]);
    }

    #[test]
    fn normal_window_map_request_still_issues_configure_call() {
        // Spec "A NORMAL window is still configured to its layout
        // placement": the regression guard for D7 — "no Configure" must
        // stay scoped to the ignore-but-map path and never leak into the
        // tiled path. First direct `on_map_request` coverage (exploration
        // #86 found none existed before this change).
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1), Event::WindowMapRequested(2)],
            Config::default(),
            vec![(1, WindowKind::Normal), (2, WindowKind::Normal)],
        );
        app.run();
        let calls = calls(&log);
        assert!(calls.contains(&DisplayCall::Configure(2, MASTER)));
        assert!(calls.contains(&DisplayCall::Configure(1, STACK)));
    }

    #[test]
    fn notification_map_request_does_not_steal_focus() {
        // Spec "Focus Survives an Ignored Window's Whole Life": because the
        // `MapOnly` path returns before `recompute` ever runs, no `Focus`
        // call is issued for the ignored window, and the previously focused
        // NORMAL window stays focused.
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1), Event::WindowMapRequested(2)],
            Config::default(),
            vec![(2, WindowKind::Notification)],
        );
        app.run();
        assert!(!calls(&log).contains(&DisplayCall::Focus(2)));
        assert_eq!(app.wm().focused_window(), Some(1));
    }

    #[test]
    fn notification_as_first_window_opens_no_workspace() {
        // Spec "First-ever window is a notification": mapping an
        // ignore-but-map window as the very first window must not create a
        // workspace as a side effect — `recompute` (the only path that would
        // touch workspace state indirectly) never runs for it. `App::new`
        // opens workspace 1 unconditionally, so the probe is "still exactly
        // that one, still empty" rather than the old `0` sentinel.
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1)],
            Config::default(),
            vec![(1, WindowKind::Notification)],
        );
        app.run();
        assert_eq!(calls(&log), vec![DisplayCall::MapUnmanaged(1)]);
        assert_eq!(app.wm().current_id(), 1);
        assert_eq!(app.wm().state_snapshots().len(), 1);
        assert_eq!(
            app.wm().workspace(1).unwrap().windows,
            Vec::<WindowId>::new()
        );
    }

    #[test]
    fn unreadable_window_type_falls_back_to_tiling() {
        // Spec "Unreadable type property defaults to tiled" / design D8: a
        // classification failure must not drop the window — it is logged in
        // place and resolves to `Tile`, so the window is framed, tiled, and
        // focused exactly like a NORMAL window.
        let (mut app, log) = app_with_failures(
            vec![Event::WindowMapRequested(1)],
            Config::default(),
            vec![FailAt::WindowKind(1)],
        );
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
    fn map_unmanaged_failure_is_reported_and_mutates_nothing() {
        // Design D8: `map_unmanaged`'s `Err` is the terminal statement of
        // the ignore-but-map path — propagated as `Err` from
        // `on_map_request`, with zero residue (no workspace opened, no
        // frame, no `mapped` entry) since nothing before it mutated state.
        let (mut mock, log) = MockDisplay::new(Vec::new());
        mock.set_kind(1, WindowKind::Notification);
        mock.fail_at(FailAt::MapUnmanaged(1));
        let mut app = App::new(
            Box::new(mock),
            Arc::new(Config::default()),
            Arc::new(Theme::default()),
            AREA,
        );

        let result = app.handle(Event::WindowMapRequested(1));

        assert!(matches!(result, Err(DErr::X(_))));
        assert_eq!(calls(&log), vec![DisplayCall::MapUnmanaged(1)]);
        // Zero residue: no workspace beyond the one `App::new` opened, and
        // that one never gained the window.
        assert_eq!(app.wm().current_id(), 1);
        assert_eq!(app.wm().state_snapshots().len(), 1);
        assert_eq!(
            app.wm().workspace(1).unwrap().windows,
            Vec::<WindowId>::new()
        );
        assert!(app.frames.is_empty());
        assert!(app.mapped.is_empty());
    }

    #[test]
    fn dialog_window_map_request_is_framed_tiled_and_focused() {
        // Spec "A DIALOG window is still tiled" / design's "Tiled Policy Is
        // Unchanged": verify-report obs #97 (WARNING) found this scenario
        // proven only compositionally — via the generic policy-table test
        // (`ignore_group_is_map_only_and_tiled_group_is_tile`, which proves
        // Dialog -> Tile as DATA) plus a Normal-kind Tile-path test (which
        // proves the Tile branch's *behavior* but only ever instantiated
        // with `WindowKind::Normal`). Neither drives `on_map_request` with
        // `WindowKind::Dialog` itself. This test closes that gap directly,
        // with the same exact-sequence shape as
        // `map_request_manages_maps_and_places_focus`: a DIALOG-classified
        // window is framed, tiled, and focused exactly like NORMAL — pinning
        // today's deliberate interim decision (no floating layout yet) so a
        // future floating-layout change has a concrete scenario to
        // supersede instead of an inferred one.
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1)],
            Config::default(),
            vec![(1, WindowKind::Dialog)],
        );
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
    fn two_distinct_ignored_windows_never_produce_a_configure_call() {
        // Headless companion to the E2E
        // `distinct_notifications_keep_their_own_distinct_sizes` (spec
        // "Different notifications keep their own distinct sizes" / "No
        // Geometry Requests for an Ignored Window", verify-report obs #97
        // finding CRITICAL): every pre-existing test here mapped exactly ONE
        // MapOnly window, which cannot distinguish "the WM never configures
        // an ignored window" from "the WM configures it but both happen to
        // match". Two MapOnly windows in one run prove there is no shared
        // sizing/configure path: the display-call log contains zero
        // `Configure` calls for EITHER of them (the real on-screen geometry
        // claim itself is proven at the E2E layer, which can observe actual
        // pixel sizes; this test proves the WM issues no geometry request at
        // all, for any ignored window, not just the first).
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowMapRequested(1), Event::WindowMapRequested(2)],
            Config::default(),
            vec![(1, WindowKind::Notification), (2, WindowKind::Notification)],
        );
        app.run();
        assert_eq!(
            calls(&log),
            vec![DisplayCall::MapUnmanaged(1), DisplayCall::MapUnmanaged(2)]
        );
        assert!(
            !calls(&log)
                .iter()
                .any(|c| matches!(c, DisplayCall::Configure(..))),
            "no ignore-but-map window may ever receive a Configure call"
        );
    }

    #[test]
    fn iconified_remap_is_not_reclassified() {
        // Spec "A later type change is ignored" / design D7's `!pending`
        // gate: a window already managed and merely iconified (not a fresh
        // MapRequest) must never be reclassified on remap, even if its
        // scripted kind would now resolve to `MapOnly`. The window is put
        // directly into the `Managed` + framed + mapped state a real
        // MapRequest would have produced (bypassing `on_map_request` itself,
        // matching `recompute_retries_the_map_next_pass_when_map_fails`
        // above), so the only event under test is the iconify + remap pair.
        let (mut app, log) = app_with_kinds(
            vec![Event::WindowUnmapNotify(1), Event::WindowMapRequested(1)],
            Config::default(),
            vec![(1, WindowKind::Notification)],
        );
        app.wm().map_request(1);
        app.wm().managed(1);
        app.frames.insert(1, FrameId(1));
        app.mapped.push(1);

        app.run();

        let calls = calls(&log);
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, DisplayCall::MapUnmanaged(_))),
            "a pending (iconified) remap must never be reclassified"
        );
        assert!(calls.contains(&DisplayCall::Configure(1, SOLO)));
        assert_eq!(app.wm().state_of(1), Some(WindowState::Managed));
    }
}
