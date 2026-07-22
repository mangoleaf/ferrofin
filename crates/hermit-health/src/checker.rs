//! The `HealthChecker` trait and the `FnChecker` closure adapter.

use std::future::Future;
use std::pin::Pin;

/// A single readiness dependency probe.
///
/// Implementors report whether one dependency (a database, an object store, a
/// downstream service) is currently reachable. `/health/ready` runs every
/// registered checker; a single `Err` fails the probe. The check is `async` via
/// [`async_trait`](async_trait) so implementors can await I/O.
#[async_trait::async_trait]
pub trait HealthChecker: Send + Sync {
    /// Stable identifier for this dependency (e.g. `"database"`, `"storage"`).
    ///
    /// The name appears in the failing-checks list of a 503 readiness response,
    /// so it must be unique and human-legible.
    fn name(&self) -> &str;

    /// Probes the dependency, returning `Ok(())` when reachable or `Err(reason)`
    /// with a short human-readable failure description otherwise.
    async fn check(&self) -> Result<(), String>;
}

/// Boxed future returned by a [`FnChecker`] closure.
type CheckFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// A [`HealthChecker`] backed by a closure.
///
/// Wraps an `async` closure so a service-specific probe (an in-process store
/// ping, an SDK round-trip) can be registered without declaring a bespoke
/// checker type. The closure returns `Ok(())` when the dependency is reachable
/// and `Err(reason)` when it is not.
pub struct FnChecker {
    /// Dependency identifier reported by [`HealthChecker::name`].
    name: String,
    /// Closure producing the probe future on each [`HealthChecker::check`] call.
    f: Box<dyn Fn() -> CheckFuture + Send + Sync>,
}

impl FnChecker {
    /// Builds a checker named `name` that runs `f` on every probe.
    ///
    /// `f` is a closure returning a `Send` future that resolves to `Ok(())` when
    /// the dependency is healthy or `Err(reason)` when it is not.
    ///
    /// # Examples
    ///
    /// ```
    /// use hermit_health::FnChecker;
    ///
    /// let checker = FnChecker::new("database", || async { Ok(()) });
    /// ```
    pub fn new<F, Fut>(name: impl Into<String>, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            f: Box::new(move || Box::pin(f())),
        }
    }
}

#[async_trait::async_trait]
impl HealthChecker for FnChecker {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), String> {
        (self.f)().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fn_checker_reports_name() {
        let checker = FnChecker::new("database", || async { Ok(()) });
        assert_eq!(checker.name(), "database");
    }

    #[tokio::test]
    async fn fn_checker_passes_through_ok() {
        let checker = FnChecker::new("storage", || async { Ok(()) });
        assert!(checker.check().await.is_ok());
    }

    #[tokio::test]
    async fn fn_checker_passes_through_err() {
        let checker = FnChecker::new("storage", || async { Err("unreachable".to_string()) });
        assert_eq!(checker.check().await, Err("unreachable".to_string()));
    }
}
