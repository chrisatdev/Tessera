//! Minimal snapshot channel (the `std::sync::watch` equivalent).
//!
//! This toolchain's `std` no longer ships `std::sync::watch`, so the design's
//! D4 watch semantics (latest snapshot, catch-up for new consumers, blocking
//! `changed()`) are implemented here on plain std primitives.

use std::sync::{Arc, Condvar, Mutex};

struct SharedState<T> {
    latest: T,
    version: u64,
}

/// Sending end of a snapshot channel.
pub struct Sender<T> {
    shared: Arc<(Mutex<SharedState<T>>, Condvar)>,
}

/// Receiving end of a snapshot channel; clone-free, created per consumer.
pub struct Receiver<T> {
    shared: Arc<(Mutex<SharedState<T>>, Condvar)>,
    seen: u64,
}

impl<T> Sender<T> {
    /// Creates a channel carrying `initial` as the current snapshot.
    pub fn new(initial: T) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new((
            Mutex::new(SharedState {
                latest: initial,
                version: 0,
            }),
            Condvar::new(),
        ));
        let rx = Receiver {
            shared: Arc::clone(&shared),
            seen: 0,
        };
        (Sender { shared }, rx)
    }

    /// Replaces the published snapshot and wakes every receiver.
    pub fn set(&self, value: T) {
        let mut state = self.shared.0.lock().unwrap();
        state.latest = value;
        state.version += 1;
        self.shared.1.notify_all();
    }

    /// Creates a receiver that sees the current snapshot immediately.
    pub fn subscribe(&self) -> Receiver<T> {
        let version = self.shared.0.lock().unwrap().version;
        Receiver {
            shared: Arc::clone(&self.shared),
            seen: version,
        }
    }
}

impl<T: Clone> Receiver<T> {
    /// Latest snapshot; a new consumer catches up to the full current state.
    pub fn borrow(&self) -> T {
        self.shared.0.lock().unwrap().latest.clone()
    }

    /// Blocks until a newer snapshot is published.
    pub fn changed(&mut self) {
        let mut state = self.shared.0.lock().unwrap();
        while state.version == self.seen {
            state = self.shared.1.wait(state).unwrap();
        }
        self.seen = state.version;
    }
}
