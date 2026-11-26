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
//!   payload (`&str`). Concrete consumers deserialize by name in `ferrofin-core`.
//!   The synchronous `Publish` and the awaitable `PublishAsync` fold into one
//!   `async fn`.
//! - `IClientEventLogger.WriteDocumentAsync` takes a `Stream`; that becomes
//!   owned bytes (`&[u8]`) so the value stays `Send`-safe across the object-safe
//!   boundary. It returns the created file name.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`.
//!
//! Every trait is object-safe and carries an `_assert_object_safe_*` assertion.

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

/// The in-process event every debounced library change set is published as,
/// its payload the unfiltered `LibraryUpdateInfo`; never forwarded to clients
/// (`LibraryChanged` is, filtered per user).
pub const ITEMS_CHANGED: &str = "ItemsChanged";

/// The in-process event a plugin configuration save publishes, with
/// `{"Id": "<plugin id>"}`; never forwarded to clients.
pub const PLUGIN_CONFIGURATION_CHANGED: &str = "PluginConfigurationChanged";

/// The future a consumer returns, awaited in registration order by the
/// event manager's `publish`.
pub type EventFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ServiceError>> + Send>>;

/// A single in-process event consumer: port of `IEventConsumer<T>.OnEvent`,
/// its typed `T eventArgs` the raw JSON payload [`EventManager::publish`]
/// carries, which the consumer deserializes by the event type it registered
/// under. An error is logged and the fan-out continues, matching the C#
/// per-consumer `try/catch`.
///
/// The callback inspects the payload synchronously and returns the work to
/// await. A consumer whose effect must be *ordered* relative to later
/// publishes (the activity-log writers) puts the work in the future; one whose
/// effect is a fire-and-forget push (the WebSocket notifiers, which C# also
/// treats as best-effort sends) may spawn and return a ready future.
pub type EventConsumer = std::sync::Arc<dyn Fn(&str) -> EventFuture + Send + Sync + 'static>;

/// A consumer future that is already finished — for a consumer that did all its
/// work synchronously or spawned it.
pub fn consumer_done() -> EventFuture {
    Box::pin(std::future::ready(Ok(())))
}

/// Registers in-process consumers of published events: the subscribe half of
/// the event manager, the C# DI registration of an `IEventConsumer<T>`.
pub trait EventSubscriber: Send + Sync {
    /// Registers `consumer` for the named event type. Every consumer
    /// registered for `event_type` runs, in registration order, on each
    /// [`EventManager::publish`] of it; several may share a name.
    fn subscribe(&self, event_type: &str, consumer: EventConsumer);
}

fn _assert_object_safe_event_subscriber(_: &dyn EventSubscriber) {}

/// The per-user audience for a `LibraryChanged` push.
///
/// `LibraryChangedNotifier.SendChangeNotifications` does not broadcast one
/// payload: it walks the distinct user ids that have a session, builds a
/// `LibraryUpdateInfo` filtered to what THAT user can see
/// (`TranslatePhysicalItemToUserLibrary`), skips the user entirely when the
/// filtered result is empty, and sends the rest with
/// `SendMessageToUserSessions`. Broadcasting instead would tell every signed-in
/// client the item ids of libraries it has no access to.
///
/// This is the notifier's `ISessionManager` + `ILibraryManager` collaborators
/// reduced to the three questions it actually asks, which keeps it object-safe
/// and keeps `ferrofin-core`'s library manager from having to own a session
/// manager that owns it back.
#[async_trait]
pub trait LibraryChangeAudience: Send + Sync {
    /// The distinct user ids that currently have a session.
    async fn active_user_ids(&self) -> Result<Vec<uuid::Uuid>, ServiceError>;

    /// The collection-folder (library) ids `user_id` is allowed to see.
    ///
    /// `GetUserRootFolder().GetChildren(user, true).OfType<Folder>()`.
    async fn visible_library_ids(
        &self,
        user_id: uuid::Uuid,
    ) -> Result<Vec<uuid::Uuid>, ServiceError>;

    /// The `TopParentId`s the items of `libraries` (collection-folder ids)
    /// are stored under: each library itself and the folders of its
    /// locations (its `PhysicalFolderIds`), which is what an item's
    /// `TopParentId` names. The default is the libraries alone.
    async fn library_top_parents(
        &self,
        libraries: &[uuid::Uuid],
    ) -> Result<Vec<uuid::Uuid>, ServiceError> {
        Ok(libraries.to_vec())
    }

    /// Which of `ids` `user_id` may see on their own:
    /// `TranslatePhysicalItemToUserLibrary`'s `item.IsVisibleStandalone(user)`
    /// in `LibraryChangedNotifier`, which besides the library applies the
    /// item's parents, the user's parental rating and tag rules, and a
    /// playlist's owner and shares. An id with no row is not visible.
    ///
    /// The default admits every id; only test doubles rely on it, and the
    /// server's audience overrides it.
    async fn visible_item_ids(
        &self,
        user_id: uuid::Uuid,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<uuid::Uuid>, ServiceError> {
        let _ = user_id;
        Ok(ids.to_vec())
    }

    /// Pushes one user's already-filtered payload to that user's sessions.
    async fn deliver(&self, user_id: uuid::Uuid, payload: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_library_change_audience(_: &dyn LibraryChangeAudience) {}

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
