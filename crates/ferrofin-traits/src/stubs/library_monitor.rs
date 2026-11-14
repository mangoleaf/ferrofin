//! A no-op [`LibraryMonitor`] for the `AppState` default.
//!
//! The concrete monitor (`ferrofin-core`'s `FerrofinLibraryMonitor`) wraps a real
//! filesystem watcher, but `AppState` must name a non-optional
//! `Arc<dyn LibraryMonitor>`, and an `AppState` built without a watcher (the
//! test constructors) still needs one. This stub satisfies the field so the
//! external-source change-report webhooks (`/Library/Movies/*`, `/Library/Series/*`,
//! `/Library/Media/Updated`) behave correctly out of the box: each reported
//! path is validated and logged, and the call succeeds — exactly the observable
//! contract Jellyfin gives when no watcher redraw is pending. The composition
//! root replaces it with the watcher-backed `FerrofinLibraryMonitor`.

use async_trait::async_trait;

use crate::error::ServiceError;
use crate::library::LibraryMonitor;

/// A no-op [`LibraryMonitor`]: change reports are validated and logged, the
/// watch lifecycle is a benign no-op.
///
/// Used as the `AppState` default so the change-report webhooks are wired
/// end-to-end before the real filesystem watcher is installed. Never touches the
/// filesystem. An empty path is still rejected, matching the concrete monitor.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopLibraryMonitor;

#[async_trait]
impl LibraryMonitor for NoopLibraryMonitor {
    async fn start(&self) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn stop(&self) -> Result<(), ServiceError> {
        Ok(())
    }

    fn restart(&self) {}

    async fn report_file_system_changed(&self, path: &str) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::NoopLibraryMonitor;
    use crate::library::LibraryMonitor;

    #[tokio::test]
    async fn lifecycle_is_a_noop() {
        let monitor = NoopLibraryMonitor;
        monitor.start().await.expect("start");
        monitor.restart();
        monitor.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn change_is_reported() {
        let monitor = NoopLibraryMonitor;
        monitor
            .report_file_system_changed("/media/movies/Solaris")
            .await
            .expect("report");
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let monitor = NoopLibraryMonitor;
        assert!(monitor.report_file_system_changed("").await.is_err());
    }
}
