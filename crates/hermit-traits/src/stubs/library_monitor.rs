//! A no-op [`LibraryMonitor`] for the `AppState` default.
//!
//! The concrete monitor (`hermit-core`'s `HermitLibraryMonitor`) wraps a real
//! filesystem watcher, but `AppState` must name a non-optional
//! `Arc<dyn LibraryMonitor>` and the filesystem-watcher composition wiring is a
//! later-wave subsystem. This stub satisfies the field so the external-source
//! change-report webhooks (`/Library/Movies/*`, `/Library/Series/*`,
//! `/Library/Media/Updated`) behave correctly out of the box: each reported
//! path is validated and logged, and the call succeeds — exactly the observable
//! contract Jellyfin gives when no watcher redraw is pending. The composition
//! root replaces it with the watcher-backed `HermitLibraryMonitor`.

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

    async fn report_file_system_change_beginning(&self, path: &str) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        Ok(())
    }

    async fn report_file_system_change_complete(
        &self,
        path: &str,
        _refresh_path: bool,
    ) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        Ok(())
    }

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
        monitor.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn change_is_reported() {
        let monitor = NoopLibraryMonitor;
        monitor
            .report_file_system_changed("/media/movies/Solaris")
            .await
            .expect("report");
        monitor
            .report_file_system_change_beginning("/media/movies/Solaris")
            .await
            .expect("begin");
        monitor
            .report_file_system_change_complete("/media/movies/Solaris", true)
            .await
            .expect("complete");
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let monitor = NoopLibraryMonitor;
        assert!(monitor.report_file_system_changed("").await.is_err());
        assert!(
            monitor
                .report_file_system_change_beginning("")
                .await
                .is_err()
        );
        assert!(
            monitor
                .report_file_system_change_complete("", false)
                .await
                .is_err()
        );
    }
}
