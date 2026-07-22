//! Eventing traits — the domain-event publish seam and client-event logger.
//!
//! Ports of `MediaBrowser.Controller.Events.IEventManager` and
//! `MediaBrowser.Controller.ClientEvent.IClientEventLogger`.
//!
//! Port rules applied:
//! - `IEventManager` is generic (`Publish<T>` / `PublishAsync<T>` where `T :
//!   EventArgs`). A generic method is **not** object-safe, so — mirroring the
//!   `SessionManager` message-broadcast collapse — it becomes a single
//!   non-generic method taking an event-type name plus a pre-serialized JSON
//!   payload (`&str`). Concrete consumers deserialize by name in `hermit-core`.
//!   The synchronous `Publish` and the awaitable `PublishAsync` fold into one
//!   `async fn`.
//! - `IClientEventLogger.WriteDocumentAsync` takes a `Stream`; that becomes
//!   owned bytes (`&[u8]`) so the value stays `Send`-safe across the object-safe
//!   boundary. It returns the created file name.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`.
//!
//! Both traits are object-safe and carry `_assert_object_safe_*` assertions.

use async_trait::async_trait;

use crate::error::ServiceError;

/// Publishes domain events to interested in-process subscribers.
///
/// Port of `IEventManager` with its generic `Publish<T>` collapsed to a
/// name-plus-JSON-payload form (see the module docs) to keep the trait
/// object-safe.
#[async_trait]
pub trait EventManager: Send + Sync {
    /// Publishes an event.
    ///
    /// `event_type` is the stable name of the event (the C# `EventArgs` type
    /// name); `payload` is its JSON-serialized body. Subscribers registered for
    /// `event_type` deserialize `payload` themselves.
    async fn publish(&self, event_type: &str, payload: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_event_manager(_: &dyn EventManager) {}

/// Persists opaque diagnostic documents uploaded by clients.
///
/// Port of `IClientEventLogger`.
#[async_trait]
pub trait ClientEventLogger: Send + Sync {
    /// Writes a client-uploaded document to the log directory, returning the
    /// created file name.
    ///
    /// The name/version are used to build a safe, unique file name; `contents`
    /// is the raw document body.
    async fn write_document(
        &self,
        client_name: &str,
        client_version: &str,
        contents: &[u8],
    ) -> Result<String, ServiceError>;
}

fn _assert_object_safe_client_event_logger(_: &dyn ClientEventLogger) {}
