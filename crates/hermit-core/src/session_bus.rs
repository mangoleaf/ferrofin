//! [`HermitSessionMessageBus`] — the concrete session→socket message registry.
//!
//! Holds one [`MessageSink`] per connected session id. The WebSocket handler
//! registers a sink when a socket opens (a closure that pushes onto that
//! socket's write channel) and unregisters it on close; producers such as
//! [`HermitSyncPlayManager`](crate::HermitSyncPlayManager) call
//! [`send`](HermitSessionMessageBus::send) to deliver a message by session id.

use std::collections::HashMap;
use std::sync::Mutex;

use hermit_traits::session_bus::{MessageSink, SessionMessageBus};

/// In-memory registry mapping session id → its socket message sink.
///
/// Cheap to share via `Arc`; the mutex only guards short map operations (no work
/// is done while held — sinks are cloned out or invoked after unlocking is not
/// needed since invoking a sink is itself non-blocking).
#[derive(Default)]
pub struct HermitSessionMessageBus {
    sinks: Mutex<HashMap<String, MessageSink>>,
}

impl HermitSessionMessageBus {
    /// Creates an empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently connected sessions (test/introspection helper).
    ///
    /// # Panics
    /// Panics only if the internal mutex has been poisoned by a prior panic
    /// while a lock was held (not reachable in normal operation).
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.sinks.lock().expect("session bus mutex poisoned").len()
    }
}

impl SessionMessageBus for HermitSessionMessageBus {
    fn register(&self, session_id: String, sink: MessageSink) {
        self.sinks
            .lock()
            .expect("session bus mutex poisoned")
            .insert(session_id, sink);
    }

    fn unregister(&self, session_id: &str) {
        self.sinks
            .lock()
            .expect("session bus mutex poisoned")
            .remove(session_id);
    }

    fn send(&self, session_id: &str, message: String) -> bool {
        let guard = self.sinks.lock().expect("session bus mutex poisoned");
        match guard.get(session_id) {
            Some(sink) => {
                sink(message);
                true
            }
            None => false,
        }
    }

    fn is_connected(&self, session_id: &str) -> bool {
        self.sinks
            .lock()
            .expect("session bus mutex poisoned")
            .contains_key(session_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn send_reaches_registered_sink_only() {
        let bus = HermitSessionMessageBus::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        bus.register(
            "s1".into(),
            Box::new(move |_msg| {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        );

        assert!(bus.is_connected("s1"));
        assert!(bus.send("s1", "hello".into()));
        assert!(!bus.send("s2", "nobody".into()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        bus.unregister("s1");
        assert!(!bus.is_connected("s1"));
        assert!(!bus.send("s1", "gone".into()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn register_replaces_previous_sink() {
        let bus = HermitSessionMessageBus::new();
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let (f, s) = (Arc::clone(&first), Arc::clone(&second));
        bus.register(
            "s".into(),
            Box::new(move |_| {
                f.fetch_add(1, Ordering::SeqCst);
            }),
        );
        bus.register(
            "s".into(),
            Box::new(move |_| {
                s.fetch_add(1, Ordering::SeqCst);
            }),
        );
        bus.send("s", "x".into());
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
        assert_eq!(bus.connection_count(), 1);
    }
}
