//! [`FerrofinEventManager`] — the in-process domain-event publish seam.
//!
//! Port of `Jellyfin.Server.Implementations.Events.EventManager`. The C# class
//! resolves every `IEventConsumer<T>` from the DI container for the event's
//! runtime type and awaits each in turn, swallowing (logging) any consumer
//! exception so one bad subscriber never breaks publication.
//!
//! The [`EventManager`](ferrofin_traits::events::EventManager) trait collapses the
//! generic `Publish<T>` / `PublishAsync<T>` to a single non-generic
//! `publish(event_type, payload)` (see the trait docs). This port mirrors the
//! C# fan-out with a name-keyed subscriber registry: consumers register a
//! [`EventConsumer`] callback under an `event_type` name, and [`publish`] awaits
//! every consumer registered for that name, logging and continuing past any
//! failure. The DI container becomes an explicit in-memory registry — the same
//! "container lookup becomes an explicit map" rule the rest of the port applies.
//!
//! [`publish`]: FerrofinEventManager::publish

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tracing::error;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::EventManager;

/// A single in-process event consumer.
///
/// Port of `IEventConsumer<T>.OnEvent`. The typed `T eventArgs` becomes the raw
/// JSON `payload` string the [`EventManager`] trait carries; the consumer
/// deserializes it by the `event_type` it registered under. Returning
/// [`ServiceError`] lets [`FerrofinEventManager::publish`] log-and-continue,
/// matching the C# per-consumer `try/catch`.
/// The future a consumer returns, awaited in registration order by
/// [`FerrofinEventManager::publish`].
pub type EventFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ServiceError>> + Send>>;

/// A consumer callback: it inspects the payload synchronously and returns the
/// work to await. A consumer whose effect must be *ordered* relative to later
/// publishes (the activity-log writers) puts the work in the future; one whose
/// effect is a fire-and-forget push (the WebSocket notifiers, which C# also
/// treats as best-effort sends) may spawn and return a ready future.
pub type EventConsumer = Arc<dyn Fn(&str) -> EventFuture + Send + Sync + 'static>;

/// A consumer future that is already finished — for a consumer that did all its
/// work synchronously or spawned it.
#[must_use]
pub fn consumer_done() -> EventFuture {
    Box::pin(std::future::ready(Ok(())))
}

/// The concrete in-process event manager.
///
/// Holds a name-keyed registry of [`EventConsumer`]s. Cloning shares the
/// registry (it is behind an [`Arc`]), so the manager can be handed to several
/// managers as an `Arc<dyn EventManager>` while subscribers registered through
/// any clone are visible to all.
#[derive(Clone, Default)]
pub struct FerrofinEventManager {
    consumers: Arc<RwLock<HashMap<String, Vec<EventConsumer>>>>,
}

impl std::fmt::Debug for FerrofinEventManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinEventManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinEventManager {
    /// Creates an event manager with no registered consumers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a consumer for the named event type.
    ///
    /// Mirrors the C# DI registration of an `IEventConsumer<T>`: every consumer
    /// registered for `event_type` is invoked (in registration order) by
    /// [`Self::publish`]. Multiple consumers may share a name.
    ///
    /// # Panics
    /// Panics if the internal consumers lock is poisoned (a subscriber panicked
    /// while holding it).
    pub fn subscribe(&self, event_type: impl Into<String>, consumer: EventConsumer) {
        self.consumers
            .write()
            .expect("event consumers lock poisoned")
            .entry(event_type.into())
            .or_default()
            .push(consumer);
    }

    /// The number of consumers registered for an event type (test/introspection
    /// aid). Returns `0` for an unknown name.
    ///
    /// # Panics
    /// Panics if the internal consumers lock is poisoned.
    #[must_use]
    pub fn consumer_count(&self, event_type: &str) -> usize {
        self.consumers
            .read()
            .expect("event consumers lock poisoned")
            .get(event_type)
            .map_or(0, Vec::len)
    }
}

#[async_trait]
impl EventManager for FerrofinEventManager {
    async fn publish(&self, event_type: &str, payload: &str) -> Result<(), ServiceError> {
        // Snapshot the matching consumers under the read lock, then release it
        // before invoking them so a consumer that re-enters `subscribe`/`publish`
        // cannot deadlock (the C# scope is likewise independent of publication).
        let consumers = {
            let guard = self
                .consumers
                .read()
                .expect("event consumers lock poisoned");
            guard.get(event_type).cloned().unwrap_or_default()
        };

        // Awaited in registration order, exactly as the C# `EventManager`
        // awaits each resolved `IEventConsumer<T>.OnEvent`. Order is
        // observable: `AuthenticateNewSessionInternal` raises `SessionStarted`
        // (awaited) before `AuthenticationSucceeded`, and the dashboard's
        // activity feed shows the pair in that order.
        for consumer in consumers {
            if let Err(err) = consumer(payload).await {
                // Mirror the C# per-consumer try/catch: log and continue so one
                // failing subscriber never aborts the fan-out.
                error!(
                    event_type,
                    %err,
                    "uncaught error in event consumer",
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn publish_invokes_all_matching_consumers() {
        let manager = FerrofinEventManager::new();
        let hits = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let hits = Arc::clone(&hits);
            manager.subscribe(
                "PlaybackStart",
                Arc::new(move |_payload: &str| {
                    hits.fetch_add(1, Ordering::SeqCst);
                    consumer_done()
                }),
            );
        }
        manager.subscribe(
            "SomethingElse",
            Arc::new(|_| -> EventFuture { panic!("wrong-name consumer must not fire") }),
        );

        manager.publish("PlaybackStart", "{}").await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert_eq!(manager.consumer_count("PlaybackStart"), 3);
    }

    #[tokio::test]
    async fn publish_receives_the_payload() {
        let manager = FerrofinEventManager::new();
        let seen = Arc::new(RwLock::new(String::new()));
        let seen_c = Arc::clone(&seen);
        manager.subscribe(
            "AuthResult",
            Arc::new(move |payload: &str| {
                *seen_c.write().unwrap() = payload.to_owned();
                consumer_done()
            }),
        );

        manager
            .publish("AuthResult", r#"{"ok":true}"#)
            .await
            .unwrap();
        assert_eq!(&*seen.read().unwrap(), r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn a_failing_consumer_does_not_abort_the_rest() {
        let manager = FerrofinEventManager::new();
        let after = Arc::new(AtomicUsize::new(0));

        manager.subscribe(
            "E",
            Arc::new(|_| Box::pin(async { Err(ServiceError::backend("boom")) })),
        );
        let after_c = Arc::clone(&after);
        manager.subscribe(
            "E",
            Arc::new(move |_| {
                after_c.fetch_add(1, Ordering::SeqCst);
                consumer_done()
            }),
        );

        // Publication still succeeds and the second consumer still ran.
        manager.publish("E", "{}").await.unwrap();
        assert_eq!(after.load(Ordering::SeqCst), 1);
    }

    /// C# `EventManager.PublishAsync` awaits each consumer in turn, so a
    /// consumer's effect has landed before `publish` returns and before the
    /// caller publishes its next event. `SessionManager` relies on this: it
    /// raises `SessionStarted` and only then `AuthenticationSucceeded`, and the
    /// dashboard activity feed shows the login pair in that order.
    #[tokio::test]
    async fn publish_awaits_each_consumer_in_registration_order() {
        let manager = FerrofinEventManager::new();
        let order = Arc::new(RwLock::new(Vec::<&'static str>::new()));

        for name in ["first", "second", "third"] {
            let order = Arc::clone(&order);
            manager.subscribe(
                "E",
                Arc::new(move |_| {
                    let order = Arc::clone(&order);
                    Box::pin(async move {
                        // Yield so a consumer that merely spawned its work
                        // would lose the race and land out of order.
                        tokio::task::yield_now().await;
                        order.write().unwrap().push(name);
                        Ok(())
                    })
                }),
            );
        }

        manager.publish("E", "{}").await.unwrap();
        // Already recorded when publish returned — not eventually.
        assert_eq!(&*order.read().unwrap(), &["first", "second", "third"]);
    }

    #[tokio::test]
    async fn publish_to_unknown_event_is_a_noop() {
        let manager = FerrofinEventManager::new();
        manager.publish("Nobody", "{}").await.unwrap();
        assert_eq!(manager.consumer_count("Nobody"), 0);
    }
}
