//! [`FerrofinSessionMessageBus`] — the concrete session→socket message registry.
//!
//! Holds the [`MessageSink`]s of each connected session's open sockets. The
//! WebSocket handler registers a sink when a socket opens (a closure that pushes
//! onto that socket's write channel) and unregisters **its own** on close (by
//! the [`SinkToken`] registration returned); producers such as
//! [`FerrofinSyncPlayManager`](crate::FerrofinSyncPlayManager) call
//! [`send`](FerrofinSessionMessageBus::send) to deliver a message by session id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ferrofin_traits::session_bus::{MessageSink, SessionMessageBus, SinkToken};

/// In-memory registry mapping session id → its open sockets' message sinks.
///
/// One session id can hold **several** sinks at once: two browser tabs share a
/// `Client`+`DeviceId` and therefore a session id, and each opens its own
/// `/socket`. They are kept in registration order and delivery goes to the last
/// (most recent) one — Jellyfin's `WebSocketController` keeps the same list and
/// sends to the most recently active open socket. Keying by session id alone
/// (the previous shape) meant the *older* tab's close unregistered the *newer*
/// tab's sink, silently taking the live socket dark.
///
/// Cheap to share via `Arc`; the mutex only guards short map operations (no work
/// is done while held — sinks are cloned out or invoked after unlocking is not
/// needed since invoking a sink is itself non-blocking).
#[derive(Default)]
pub struct FerrofinSessionMessageBus {
    sinks: Mutex<HashMap<String, Vec<(SinkToken, MessageSink)>>>,
    /// Source of the per-registration tokens (never reused within a process).
    next_token: AtomicU64,
}

impl FerrofinSessionMessageBus {
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

impl SessionMessageBus for FerrofinSessionMessageBus {
    fn register(&self, session_id: String, sink: MessageSink) -> SinkToken {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.sinks
            .lock()
            .expect("session bus mutex poisoned")
            .entry(session_id)
            .or_default()
            .push((token, sink));
        token
    }

    fn unregister(&self, session_id: &str, token: SinkToken) -> bool {
        let mut guard = self.sinks.lock().expect("session bus mutex poisoned");
        let Some(sinks) = guard.get_mut(session_id) else {
            return false;
        };
        sinks.retain(|(t, _)| *t != token);
        if sinks.is_empty() {
            // Keep `is_connected` honest: an empty vector is not a connection.
            guard.remove(session_id);
            return false;
        }
        true
    }

    fn send(&self, session_id: &str, message: String) -> bool {
        let guard = self.sinks.lock().expect("session bus mutex poisoned");
        // Most recently registered socket wins (C# `MaxBy(LastActivityDate)`).
        match guard.get(session_id).and_then(|sinks| sinks.last()) {
            Some((_, sink)) => {
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

    /// A counting sink plus the counter it feeds.
    fn counting_sink() -> (Arc<AtomicUsize>, MessageSink) {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        (
            hits,
            Box::new(move |_msg| {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn send_reaches_registered_sink_only() {
        let bus = FerrofinSessionMessageBus::new();
        let (hits, sink) = counting_sink();
        let token = bus.register("s1".into(), sink);

        assert!(bus.is_connected("s1"));
        assert!(bus.send("s1", "hello".into()));
        assert!(!bus.send("s2", "nobody".into()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        bus.unregister("s1", token);
        assert!(!bus.is_connected("s1"));
        assert!(!bus.send("s1", "gone".into()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn send_goes_to_the_most_recently_registered_socket() {
        let bus = FerrofinSessionMessageBus::new();
        let (first, sink_a) = counting_sink();
        let (second, sink_b) = counting_sink();
        bus.register("s".into(), sink_a);
        bus.register("s".into(), sink_b);
        bus.send("s", "x".into());
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
        // One session, however many sockets it holds.
        assert_eq!(bus.connection_count(), 1);
    }

    /// Two tabs share a `Client`+`DeviceId`, so they share a session id. When
    /// the *older* tab closes it must not take the newer tab's delivery with
    /// it — the bug the [`SinkToken`] exists to prevent.
    #[test]
    fn closing_an_older_socket_leaves_the_newer_one_connected() {
        let bus = FerrofinSessionMessageBus::new();
        let (first, sink_a) = counting_sink();
        let (second, sink_b) = counting_sink();
        let old = bus.register("s".into(), sink_a);
        bus.register("s".into(), sink_b);

        bus.unregister("s", old);

        assert!(bus.is_connected("s"), "the newer socket is still connected");
        assert!(bus.send("s", "x".into()));
        assert_eq!(first.load(Ordering::SeqCst), 0, "closed socket never fires");
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    /// The `bool` is what lets a caller end the session only when the *last*
    /// socket goes. A dropped socket often notices its own death well after the
    /// client reconnected (TCP timeout >> reconnect), so the stale unregister
    /// must report "still connected" and leave the session alive.
    #[test]
    fn unregister_reports_whether_a_socket_remains() {
        let bus = FerrofinSessionMessageBus::new();
        let (live, sink_live) = counting_sink();
        let stale = bus.register("s".into(), Box::new(|_| {}));
        let current = bus.register("s".into(), sink_live);

        assert!(
            bus.unregister("s", stale),
            "the reconnected socket is still registered"
        );
        assert!(bus.send("s", "x".into()));
        assert_eq!(live.load(Ordering::SeqCst), 1);

        assert!(
            !bus.unregister("s", current),
            "that was the last socket — the session has no connection left"
        );
        assert!(!bus.is_connected("s"));
    }

    #[test]
    fn unregistering_an_unknown_token_is_a_no_op() {
        let bus = FerrofinSessionMessageBus::new();
        let (hits, sink) = counting_sink();
        bus.register("s".into(), sink);
        bus.unregister("s", u64::MAX);
        bus.unregister("other", 0);
        assert!(bus.is_connected("s"));
        assert!(bus.send("s", "x".into()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
