//! Status-bar placeholder (T19, REQ-bus-004 / SC-bus-04).
//!
//! The real bar (X drawing) is a later change; this placeholder owns the
//! snapshot-consumer seam — it subscribes to the WmState watch and renders
//! the complete current snapshot as plain text. It has zero X dependencies:
//! it consumes [`WmState`] only, so it can be promoted to its own crate later
//! (design: a `src/bar.rs` module in the binary crate for now).

use tessera_core::WmState;
use tessera_core::bus::StateReceiver;

/// Snapshot consumer for the status bar (REQ-bus-004 / SC-bus-04).
///
/// Holds the latest [`WmState`] published on the watch and renders it as
/// plain text. The actual X drawing is a later change; this placeholder owns
/// the seam — it must NOT depend on X specifics, only consume [`WmState`].
pub struct Bar {
    /// The watch receiver; [`Bar::refresh`] pulls the newest snapshot from it.
    state_rx: StateReceiver,
    /// The complete current snapshot (SC-bus-04).
    latest: WmState,
}

impl Bar {
    /// Subscribes to the WmState watch and immediately captures the complete
    /// current snapshot (SC-bus-04: a new consumer catches up to the full
    /// current state, not the event history).
    pub fn new(state_rx: StateReceiver) -> Self {
        let latest = state_rx.borrow();
        Bar { state_rx, latest }
    }

    /// The complete current snapshot the bar holds. Test-facing today: the
    /// placeholder's output path is [`Bar::render`]; the real bar's read path
    /// will promote this accessor.
    #[cfg(test)]
    pub(crate) fn latest(&self) -> &WmState {
        &self.latest
    }

    /// Pulls the newest snapshot from the watch. The core republishes state
    /// after every placement change (`recompute` -> `publish_state`); in the
    /// single-threaded binary this is called when the app re-reads state. A
    /// blocking `changed()` consumer belongs to the real bar (later change).
    pub fn refresh(&mut self) {
        self.latest = self.state_rx.borrow();
    }

    /// A plain-text, render-ready line for the snapshot, e.g. `*1[2]:2,3 2:4`
    /// (current workspace 1 focused on window 2 with windows 2,3, then
    /// workspace 2 focused on window 4). The X drawing is a later change.
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for ws in &self.latest.workspaces {
            let mut part = String::new();
            if ws.id == self.latest.current {
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tessera_core::bus::EventBus;
    use tessera_core::{Config, LayoutKind, WmState, WorkspaceState};

    use super::*;

    fn state(current: u32, focused: Option<u32>, workspaces: Vec<WorkspaceState>) -> WmState {
        WmState {
            current,
            focused,
            workspaces,
            config: Arc::new(Config::default()),
        }
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
        let bus = EventBus::new(Arc::new(Config::default()));
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
        let bus = EventBus::new(Arc::new(Config::default()));
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
        let bus = EventBus::new(Arc::new(Config::default()));
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
        let bus = EventBus::new(Arc::new(Config::default()));
        let bar = Bar::new(bus.state_rx());
        assert_eq!(bar.render(), "no workspaces");
    }
}
