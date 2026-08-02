//! The event bus (design D4): bounded fan-out with drop-and-log overflow,
//! plus a std `watch` channel carrying the latest [`WmState`] snapshot.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::config::Config;
use crate::event::Event;
use crate::geometry::{LayoutKind, WindowId, WorkspaceId};
use crate::watch;

/// Bitmask over [`Event`] variants used by [`EventBus::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMask(u64);

impl EventMask {
    /// Matches every event variant.
    pub const ALL: EventMask = EventMask(u64::MAX);

    /// Mask matching exactly the event's variant.
    pub fn of(ev: &Event) -> Self {
        ev.mask()
    }

    /// Union of two masks.
    pub const fn union(self, other: Self) -> Self {
        EventMask(self.0 | other.0)
    }

    fn matches(self, ev: &Event) -> bool {
        self.0 & ev.mask().0 != 0
    }
}

impl Event {
    /// Bit identifying this event's variant (see [`EventMask`]).
    pub(crate) fn mask(&self) -> EventMask {
        use Event::*;
        let bit = match self {
            WindowMapRequested(_) => 0,
            WindowConfigureRequested(..) => 1,
            WindowUnmapNotify(_) => 2,
            WindowDestroyNotify(_) => 3,
            WindowManaged(_) => 4,
            WindowUnmapped(_) => 5,
            WindowFocusChanged(_) => 6,
            WindowTitleChanged(..) => 7,
            WorkspaceOpened(_) => 8,
            WorkspaceClosed(_) => 9,
            WorkspaceChanged(_) => 10,
            WorkspaceLayoutChanged(..) => 11,
            PlacementsChanged(..) => 12,
            KeyPressed(_) => 13,
            Command(_) => 14,
            ConfigReloaded(_) => 15,
            Shutdown => 16,
        };
        EventMask(1u64 << bit)
    }
}

/// Snapshot of one workspace inside [`WmState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub name: String,
    pub layout: LayoutKind,
    pub windows: Vec<WindowId>,
    pub focus: Option<WindowId>,
}

/// Complete window-manager state consumed by e.g. a status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WmState {
    pub current: WorkspaceId,
    pub focused: Option<WindowId>,
    pub workspaces: Vec<WorkspaceState>,
    pub config: Arc<Config>,
}

/// Per-subscriber bounded channels plus the WmState watch (design D4).
pub struct EventBus {
    subs: Mutex<Vec<(EventMask, Sender<Event>)>>,
    watch: watch::Sender<WmState>,
}

/// WmState watch receiver exposed by [`EventBus::state_rx`].
pub type StateReceiver = watch::Receiver<WmState>;

/// Capacity of each subscriber queue; overflow is dropped and logged.
const SUB_QUEUE_CAPACITY: usize = 16;

impl EventBus {
    /// Creates a bus with an initial empty [`WmState`] snapshot.
    pub fn new(config: Arc<Config>) -> Self {
        let initial = WmState {
            current: 1,
            focused: None,
            workspaces: Vec::new(),
            config,
        };
        let (watch_tx, _watch_rx) = watch::Sender::new(initial);
        EventBus {
            subs: Mutex::new(Vec::new()),
            watch: watch_tx,
        }
    }

    /// Subscribes to every event variant (SC-bus-02).
    pub fn subscribe_all(&self) -> Receiver<Event> {
        self.subscribe(EventMask::ALL)
    }

    /// Subscribes to events matching `mask` (SC-bus-02/05).
    pub fn subscribe(&self, mask: EventMask) -> Receiver<Event> {
        let (tx, rx) = bounded(SUB_QUEUE_CAPACITY);
        self.subs.lock().unwrap().push((mask, tx));
        rx
    }

    /// Publishes `ev` to every matching subscriber, in registration order.
    /// A full queue drops and logs the event; publishing never blocks.
    pub fn publish(&self, ev: Event) {
        todo!()
    }

    /// Watch receiver carrying the latest [`WmState`] (REQ-bus-004).
    pub fn state_rx(&self) -> StateReceiver {
        self.watch.subscribe()
    }

    /// Replaces the published [`WmState`] snapshot (REQ-bus-004).
    pub fn set_state(&self, s: WmState) {
        let _ = s;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_channel::RecvTimeoutError;

    use crate::config::Config;

    use super::*;

    fn bus() -> EventBus {
        EventBus::new(Arc::new(Config::default()))
    }

    #[test]
    fn subscribe_all_receives_every_event_in_order() {
        let bus = bus();
        let rx = bus.subscribe_all();
        bus.publish(Event::WindowManaged(1));
        bus.publish(Event::WindowUnmapped(1));
        bus.publish(Event::Shutdown);
        assert_eq!(rx.recv(), Ok(Event::WindowManaged(1)));
        assert_eq!(rx.recv(), Ok(Event::WindowUnmapped(1)));
        assert_eq!(rx.recv(), Ok(Event::Shutdown));
    }

    #[test]
    fn masked_subscribe_filters_unmatched_variants() {
        let bus = bus();
        let mask = EventMask::of(&Event::WorkspaceOpened(0))
            .union(EventMask::of(&Event::WorkspaceClosed(0)));
        let rx = bus.subscribe(mask);
        bus.publish(Event::WindowManaged(7));
        bus.publish(Event::WorkspaceOpened(1));
        bus.publish(Event::WindowUnmapped(7));
        bus.publish(Event::WorkspaceClosed(2));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceOpened(1)));
        assert_eq!(rx.recv(), Ok(Event::WorkspaceClosed(2)));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn publish_fans_out_to_every_subscriber() {
        let bus = bus();
        let rx1 = bus.subscribe_all();
        let rx2 = bus.subscribe_all();
        bus.publish(Event::WindowManaged(3));
        assert_eq!(rx1.recv(), Ok(Event::WindowManaged(3)));
        assert_eq!(rx2.recv(), Ok(Event::WindowManaged(3)));
    }
}
