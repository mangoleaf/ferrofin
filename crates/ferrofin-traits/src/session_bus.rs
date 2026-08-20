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
/// Invoking it must be cheap and non-blocking (it typically pushes onto a
/// **bounded** channel drained by the socket's write task). A failed enqueue
/// (socket already gone, or a client so far behind that its queue is full) is
/// swallowed by the sink; the socket unregisters on disconnect.
pub type MessageSink = Box<dyn Fn(String) + Send + Sync>;

/// Identifies one socket's registration on the bus, so a socket that closes
/// after a newer one opened for the same session cannot unregister the newer
/// one's sink (Jellyfin's `WebSocketController` removes only the socket that
/// actually closed).
pub type SinkToken = u64;

/// Routes server→client messages to connected session sockets by session id.
///
/// Port of the delivery half of `ISessionManager.SendMessageToSession`. The
/// registry is the missing piece that lets SyncPlay actually reach clients.
pub trait SessionMessageBus: Send + Sync {
    /// Registers `sink` as a delivery channel for `session_id` and returns the
    /// token identifying *this* registration.
    ///
    /// A session may have several open sockets at once (two browser tabs share
    /// one `Client`+`DeviceId`, hence one session id); delivery goes to the most
    /// recently registered one, matching Jellyfin's
    /// `WebSocketController.SendMessage`, which picks the most recently active
    /// open socket.
    fn register(&self, session_id: String, sink: MessageSink) -> SinkToken;

    /// Removes the registration `token` identifies (called when its socket
    /// closes) and reports whether the session still has a live socket
    /// afterwards. A token that is no longer registered is a no-op.
    ///
    /// The return value is what lets the caller end a session only when its
    /// **last** socket goes, as `WebSocketController.OnConnectionClosed` does:
    /// `false` means that was the last one.
    fn unregister(&self, session_id: &str, token: SinkToken) -> bool;

    /// Delivers `message` to the session if it is connected, returning whether a
    /// sink existed to receive it.
    fn send(&self, session_id: &str, message: String) -> bool;

    /// Whether a live sink is currently registered for `session_id`.
    fn is_connected(&self, session_id: &str) -> bool;
}

fn _assert_object_safe_session_message_bus(_: &dyn SessionMessageBus) {}
