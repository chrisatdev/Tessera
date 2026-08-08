//! Status-bar binary wrapper (T19/Phase-2, REQ-bus-004 / SC-bus-04).
//!
//! Owns the snapshot-consumer seam: [`Bar`] subscribes to the WmState watch
//! and keeps the complete current snapshot as plain text ([`Bar::render`]).
//! Phase 2 replaces the placeholder output with a real X-drawn bar: a
//! [`BarRenderer`] runs on its own dedicated thread (task 2.7) over the
//! shared connection, and [`Bar::draw`] feeds it exactly one snapshot per
//! recompute (design D4 — never on idle event polling).
//!
//! The snapshot seam is X-free: [`Bar::new`] builds a bar that only renders
//! text, which the 4 existing bar unit tests depend on. [`Bar::spawn`] is the
//! real constructor (X connection + renderer thread).

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use tessera_core::bus::StateReceiver;
use tessera_core::{BarConfig, DErr, Rect, WmState};
use tessera_x11::{BarRenderer, RustConnection, Visualid, Window};

/// Snapshot consumer for the status bar (REQ-bus-004 / SC-bus-04).
///
/// Holds the latest [`WmState`] published on the watch and renders it as
/// plain text; when spawned with an X connection it also forwards each
/// recompute snapshot to a dedicated bar-renderer thread (task 2.7, D4).
pub struct Bar {
    /// The watch receiver; [`Bar::refresh`] pulls the newest snapshot.
    state_rx: StateReceiver,
    /// The complete current snapshot (SC-bus-04).
    latest: WmState,
    /// The dedicated renderer thread, present when spawned with X.
    worker: Option<BarWorker>,
}

/// The dedicated bar-renderer thread plus the channel that feeds it one
/// snapshot per recompute (task 2.7). Draining the channel (closing the
/// sender) makes the thread finish its current draw and exit.
struct BarWorker {
    tx: mpsc::Sender<WmState>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Bar {
    /// Subscribes to the WmState watch and immediately captures the complete
    /// current snapshot (SC-bus-04). X-free: `render`/`latest` stay usable
    /// without a display (the snapshot-seam contract the tests rely on).
    #[cfg(test)]
    pub(crate) fn new(state_rx: StateReceiver) -> Self {
        let latest = state_rx.borrow();
        Bar {
            state_rx,
            latest,
            worker: None,
        }
    }

    /// Real constructor (task 2.7): spawns the dedicated renderer thread
    /// owning a [`BarRenderer`] over the shared connection. [`Bar::draw`]
    /// then feeds it one snapshot per recompute. Never aborts on a renderer
    /// problem beyond its own error.
    pub fn spawn(
        conn: Arc<RustConnection>,
        root: Window,
        depth: u8,
        visual: Visualid,
        monitor: Rect,
        bar: &BarConfig,
        state_rx: StateReceiver,
    ) -> Result<Self, DErr> {
        let latest = state_rx.borrow();
        let renderer = BarRenderer::new(Arc::clone(&conn), root, depth, visual, monitor, bar)?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("tessera-bar".to_string())
            .spawn(move || bar_thread(rx, renderer))
            .map_err(|err| DErr::X(format!("cannot spawn the bar thread: {err}")))?;
        Ok(Bar {
            state_rx,
            latest,
            worker: Some(BarWorker {
                tx,
                handle: Some(handle),
            }),
        })
    }

    /// Forwards one recompute snapshot to the renderer thread (D4: the
    /// binary hook fires once per recompute, so the bar draws once per
    /// recompute — never on idle event polling). Also keeps the snapshot
    /// seam current. Without a spawned thread this is a safe no-op (the
    /// X-free snapshot seam).
    pub fn draw(&mut self, state: &WmState) -> Result<(), DErr> {
        self.latest = state.clone();
        if let Some(worker) = &self.worker {
            worker
                .tx
                .send(state.clone())
                .map_err(|_| DErr::X("the bar thread has stopped".to_string()))?;
        }
        Ok(())
    }

    /// Pulls the newest snapshot from the watch (the core republishes state
    /// after every placement change — `recompute` -> `publish_state`).
    pub fn refresh(&mut self) {
        self.latest = self.state_rx.borrow();
    }

    /// The complete current snapshot the bar holds. Test-facing today: the
    /// placeholder's output path is [`Bar::render`]; the real bar's read path
    /// promotes this accessor.
    #[cfg(test)]
    pub(crate) fn latest(&self) -> &WmState {
        &self.latest
    }

    /// A plain-text, render-ready line for the snapshot, e.g. `*1[2]:2,3 2:4`
    /// (current workspace 1 focused on window 2 with windows 2,3, then
    /// workspace 2 focused on window 4).
    pub fn render(&self) -> String {
        render_text(&self.latest)
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        // Signal the renderer thread to finish its last draw and exit, then
        // wait so a final in-flight flush is not torn down mid-draw.
        if let Some(worker) = self.worker.take() {
            drop(worker.tx);
            if let Some(handle) = worker.handle {
                let _ = handle.join();
            }
        }
    }
}

/// The renderer thread loop: draw exactly once per received snapshot, then
/// wait for the next one. A closed channel (the [`Bar`] was dropped) exits.
fn bar_thread(rx: mpsc::Receiver<WmState>, renderer: BarRenderer<Arc<RustConnection>>) {
    while let Ok(state) = rx.recv() {
        if let Err(err) = renderer.draw(&state) {
            eprintln!("tessera: {err}");
        }
    }
}

/// Plain-text rendering of a snapshot (the placeholder's output, kept as the
/// snapshot-seam contract for the bar tests).
fn render_text(state: &WmState) -> String {
    let mut parts: Vec<String> = Vec::new();
    for ws in &state.workspaces {
        let mut part = String::new();
        if ws.id == state.current {
            part.push('*');
        }
        part.push_str(&ws.name);
        if let Some(focus) = ws.focus {
            part.push('[');
            part.push_str(&focus.to_string());
            part.push(']');
        }
        if !ws.windows.is_empty() {
            part.push(':');
            part.push_str(
                &ws.windows
                    .iter()
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        parts.push(part);
    }
    if parts.is_empty() {
        "no workspaces".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tessera_core::bus::EventBus;
    use tessera_core::{Config, LayoutKind, Theme, WmState, WorkspaceState};

    use super::*;

    fn state(current: u32, focused: Option<u32>, workspaces: Vec<WorkspaceState>) -> WmState {
        WmState {
            current,
            focused,
            workspaces,
            config: Arc::new(Config::default()),
            theme: Arc::new(Theme::default()),
        }
    }

    fn bus() -> EventBus {
        EventBus::new(Arc::new(Config::default()), Arc::new(Theme::default()))
    }

    fn ws(id: u32, name: &str, windows: Vec<u32>, focus: Option<u32>) -> WorkspaceState {
        WorkspaceState {
            id,
            name: name.to_string(),
            layout: LayoutKind::MasterStack,
            windows,
            focus,
        }
    }

    #[test]
    fn new_subscriber_catches_up_to_the_complete_current_snapshot() {
        // SC-bus-04: the bar subscribes AFTER many events and still receives
        // the complete current WmState — not the event history.
        let bus = bus();
        bus.set_state(state(1, Some(5), vec![ws(1, "1", vec![5], Some(5))]));
        let latest = state(
            2,
            Some(9),
            vec![ws(1, "1", vec![5], Some(5)), ws(2, "2", vec![9], Some(9))],
        );
        bus.set_state(latest.clone());
        let bar = Bar::new(bus.state_rx());
        assert_eq!(*bar.latest(), latest);
    }

    #[test]
    fn refresh_follows_the_snapshot_published_after_placement_changes() {
        // The core republishes state after every placement change (recompute
        // -> publish_state); refresh() pulls the newest snapshot into the bar.
        let bus = bus();
        let mut bar = Bar::new(bus.state_rx());
        assert_eq!(bar.latest().current, 0); // sentinel: no workspace yet
        bus.set_state(state(1, Some(7), vec![ws(1, "1", vec![7], Some(7))]));
        bar.refresh();
        assert_eq!(bar.latest().current, 1);
        assert_eq!(bar.latest().focused, Some(7));
        assert_eq!(bar.latest().workspaces[0].windows, vec![7]);
    }

    #[test]
    fn render_produces_plain_text_for_the_current_snapshot() {
        let bus = bus();
        bus.set_state(state(
            1,
            Some(2),
            vec![
                ws(1, "1", vec![2, 3], Some(2)),
                ws(2, "2", vec![4], Some(4)),
            ],
        ));
        let bar = Bar::new(bus.state_rx());
        assert_eq!(bar.render(), "*1[2]:2,3 2[4]:4");
    }

    #[test]
    fn render_reports_when_there_are_no_workspaces_yet() {
        // The initial snapshot (current 0, no workspaces): the sentinel case.
        let bus = bus();
        let bar = Bar::new(bus.state_rx());
        assert_eq!(bar.render(), "no workspaces");
    }

    #[test]
    fn draw_is_a_safe_noop_without_a_spawned_renderer_thread() {
        // The X-free snapshot seam: a bar built with `new` has no renderer
        // thread, so draw must accept (and drop) snapshots without error.
        let bus = bus();
        let mut bar = Bar::new(bus.state_rx());
        bar.draw(&state(1, Some(7), vec![ws(1, "1", vec![7], Some(7))]))
            .unwrap();
        assert_eq!(bar.render(), "*1[7]:7");
    }
}
