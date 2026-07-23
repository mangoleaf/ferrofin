//! [`HermitLibraryMonitor`] — the concrete [`LibraryMonitor`] over a
//! [`FileSystemWatcher`].
//!
//! Port of `Emby.Server.Implementations.IO.LibraryMonitor`. The C# monitor wraps
//! a set of `FileSystemWatcher`s over each library root and, on a debounce timer,
//! feeds changed paths back into the library refresh pipeline. Two behaviors of
//! that class carry over to this seam and the rest is deferred:
//! - **self-suppression:** while the server itself is writing under a path
//!   (metadata save, image download) it registers the path as "temporarily
//!   ignored" so the resulting change events do not trigger a redundant refresh.
//!   That set is the in-memory state this type owns.
//! - **watch lifecycle:** `Start`/`Stop` begin/stop watching, delegated to the
//!   injected [`FileSystemWatcher`] so the real inotify wrapper is supplied at
//!   the composition root and tests use a fake.
//!
//! The debounce timer and the actual refresh dispatch (`ReportFileSystemChanged`
//! → `ProviderManager.QueueRefresh`) are deferred to the scan wave; here the
//! change reports update suppression state and log, returning success so API
//! callers get the expected semantics.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryMonitor;

use crate::resolvers::FileSystemWatcher;

/// The concrete library monitor.
///
/// Owns the temporarily-ignored path set (self-suppression) and delegates the
/// watch lifecycle to the injected [`FileSystemWatcher`]. The roots to watch are
/// supplied at construction (the composition root reads them from the library
/// configuration).
#[derive(Clone)]
pub struct HermitLibraryMonitor {
    watcher: Arc<dyn FileSystemWatcher>,
    roots: Arc<Vec<String>>,
    /// Paths currently being written by the server; changes under them are
    /// suppressed. Guarded by a `std::sync::Mutex` because the guard never spans
    /// an `.await` (the set is touched synchronously inside each method).
    suppressed: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for HermitLibraryMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLibraryMonitor")
            .field("roots", &self.roots.len())
            .finish_non_exhaustive()
    }
}

impl HermitLibraryMonitor {
    /// Creates a monitor over the given watcher and the library root paths to
    /// watch.
    #[must_use]
    pub fn new(watcher: Arc<dyn FileSystemWatcher>, roots: Vec<String>) -> Self {
        Self {
            watcher,
            roots: Arc::new(roots),
            suppressed: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether `path` (or an ancestor of it) is currently suppressed. Mirrors C#
    /// `IsPathLocked` — a change under a directory the server is writing is
    /// ignored.
    ///
    /// # Panics
    ///
    /// Panics only if the internal suppression lock was poisoned by a thread that
    /// panicked while holding it — which cannot happen here, since the guard is
    /// never held across code that can panic.
    #[must_use]
    pub fn is_path_suppressed(&self, path: &str) -> bool {
        let guard = self.suppressed.lock().expect("suppressed set not poisoned");
        guard
            .iter()
            .any(|p| path == p || path.starts_with(&format!("{p}/")))
    }
}

#[async_trait]
impl LibraryMonitor for HermitLibraryMonitor {
    async fn start(&self) -> Result<(), ServiceError> {
        for root in self.roots.iter() {
            self.watcher.watch(root).await?;
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), ServiceError> {
        self.watcher.unwatch_all().await
    }

    async fn report_file_system_change_beginning(&self, path: &str) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        self.suppressed
            .lock()
            .expect("suppressed set not poisoned")
            .insert(path.to_owned());
        Ok(())
    }

    async fn report_file_system_change_complete(
        &self,
        path: &str,
        refresh_path: bool,
    ) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        self.suppressed
            .lock()
            .expect("suppressed set not poisoned")
            .remove(path);
        if refresh_path {
            self.report_file_system_changed(path).await?;
        }
        Ok(())
    }

    async fn report_file_system_changed(&self, path: &str) -> Result<(), ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path can't be empty"));
        }
        if self.is_path_suppressed(path) {
            tracing::trace!(path, "change suppressed (server-initiated write)");
            return Ok(());
        }
        // The debounced refresh dispatch is deferred to the scan wave; a real
        // change is logged so the intent is observable end-to-end.
        tracing::debug!(path, "library filesystem change reported");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// An in-memory watcher that records the watch/unwatch calls it receives.
    #[derive(Default)]
    struct FakeWatcher {
        watched: StdMutex<Vec<String>>,
        unwatched_all: StdMutex<bool>,
    }

    #[async_trait]
    impl FileSystemWatcher for FakeWatcher {
        async fn watch(&self, path: &str) -> Result<(), ServiceError> {
            self.watched.lock().unwrap().push(path.to_owned());
            Ok(())
        }
        async fn unwatch(&self, _path: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn unwatch_all(&self) -> Result<(), ServiceError> {
            *self.unwatched_all.lock().unwrap() = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_watches_every_root() {
        let watcher = Arc::new(FakeWatcher::default());
        let monitor = HermitLibraryMonitor::new(
            watcher.clone(),
            vec!["/media/movies".to_owned(), "/media/tv".to_owned()],
        );
        monitor.start().await.expect("start");
        assert_eq!(watcher.watched.lock().unwrap().len(), 2);

        monitor.stop().await.expect("stop");
        assert!(*watcher.unwatched_all.lock().unwrap());
    }

    #[tokio::test]
    async fn suppression_spans_a_change_window() {
        let monitor = HermitLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![]);
        let dir = "/media/movies/Solaris";

        monitor
            .report_file_system_change_beginning(dir)
            .await
            .expect("begin");
        assert!(monitor.is_path_suppressed(dir));
        // A change under the suppressed directory is ignored.
        assert!(monitor.is_path_suppressed(&format!("{dir}/poster.jpg")));

        monitor
            .report_file_system_change_complete(dir, false)
            .await
            .expect("complete");
        assert!(!monitor.is_path_suppressed(dir));
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let monitor = HermitLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![]);
        assert!(monitor.report_file_system_changed("").await.is_err());
    }
}
