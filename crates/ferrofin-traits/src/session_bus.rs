//! The session message bus — server→client push over the session WebSocket.
//!
//! Jellyfin's `IWebSocketConnection` / `ISessionManager.SendMessageToSession`
//! path lets the server push messages (now-playing, remote-control, and
//! **SyncPlay** commands) down a client's live socket. Ferrofin models that as a
//! small runtime-agnostic seam: each open session socket registers a **sink**
//! (a `Fn(String)` that enqueues one serialized message toward that socket), and
//! producers ([`SyncPlayManager`](crate::stubs::SyncPlayManager), the session
//! manager, …) call [`SessionMessageBus::send`] to deliver a message by session
//! id.
//!
//! The sink is a plain closure rather than a channel type so this trait carries
//! no async-runtime dependency: the WebSocket handler owns its channel and
//! registers a sender-closure; the bus just holds and invokes it.

/// A one-way sink that enqueues a serialized message toward one session's socket.
///
/// Invoking it must be cheap and non-blocking (it typically pushes onto an
/// unbounded channel drained by the socket's write task). A failed enqueue
/// (socket already gone) is swallowed by the sink; the socket unregisters on
/// disconnect.
pub type MessageSink = Box<dyn Fn(String) + Send + Sync>;

/// Routes server→client messages to connected session sockets by session id.
///
/// Port of the delivery half of `ISessionManager.SendMessageToSession`. The
/// registry is the missing piece that lets SyncPlay actually reach clients.
pub trait SessionMessageBus: Send + Sync {
    /// Registers `sink` as the delivery channel for `session_id`, replacing any
    /// existing registration for that session (a reconnect supersedes the old
    /// socket).
    fn register(&self, session_id: String, sink: MessageSink);

    /// Removes the session's sink (called when its socket closes).
    fn unregister(&self, session_id: &str);

    /// Delivers `message` to the session if it is connected, returning whether a
    /// sink existed to receive it.
    fn send(&self, session_id: &str, message: String) -> bool;

    /// Whether a live sink is currently registered for `session_id`.
    fn is_connected(&self, session_id: &str) -> bool;
}

fn _assert_object_safe_session_message_bus(_: &dyn SessionMessageBus) {}
