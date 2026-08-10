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
//! `ReportFileSystemChanged` dispatches a real refresh: a non-suppressed change
//! queues a (coalescing) library scan through the injected [`LibraryScanTrigger`]
//! (the composition root passes the library manager). The C# debounce timer is
//! not ported — the scan's own in-flight guard folds a burst of reports into one
//! scan instead. Without a trigger attached (unit tests) a change is logged only.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryMonitor;

use crate::resolvers::FileSystemWatcher;

/// The narrow slice of the library manager the monitor needs: queue a rescan.
///
/// The monitor only ever asks "something changed — refresh"; depending on the
/// whole [`LibraryManager`](hermit_traits::library::LibraryManager) would drag in
/// ~50 unrelated methods. [`HermitLibraryManager`](crate::HermitLibraryManager)
/// implements this, so the composition root passes the same instance.
#[async_trait]
pub trait LibraryScanTrigger: Send + Sync {
    /// Queues a (coalescing) full library scan.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the scan cannot be queued.
    async fn queue_library_scan(&self) -> Result<(), ServiceError>;
}

/// Supplies the filesystem roots the monitor should watch.
///
/// Resolved fresh on every [`LibraryMonitor::start`], so a monitor restart
/// after a library-structure change (folder added/removed, realtime option
/// toggled) picks up the new set — the C# monitor re-reads
/// `GetVirtualFolders` the same way. The composition root implements this
/// over the virtual-folder manager, filtered to libraries whose
/// `enable_realtime_monitor` option is on; tests use a plain `Vec<String>`.
#[async_trait]
pub trait WatchRootsSource: Send + Sync {
    /// The root paths to watch right now.
    async fn watch_roots(&self) -> Vec<String>;
}

#[async_trait]
impl WatchRootsSource for Vec<String> {
    async fn watch_roots(&self) -> Vec<String> {
        self.clone()
    }
}

#[async_trait]
impl<T: WatchRootsSource + ?Sized> WatchRootsSource for Arc<T> {
    async fn watch_roots(&self) -> Vec<String> {
        (**self).watch_roots().await
    }
}

/// The concrete library monitor.
///
/// Owns the temporarily-ignored path set (self-suppression) and delegates the
/// watch lifecycle to the injected [`FileSystemWatcher`]. The roots to watch
/// come from the injected [`WatchRootsSource`], re-read on every `start` (the
/// composition root supplies the realtime-enabled library roots).
#[derive(Clone)]
pub struct HermitLibraryMonitor {
    watcher: Arc<dyn FileSystemWatcher>,
    roots: Arc<dyn WatchRootsSource>,
    /// Paths currently being written by the server; changes under them are
    /// suppressed. Guarded by a `std::sync::Mutex` because the guard never spans
    /// an `.await` (the set is touched synchronously inside each method).
    suppressed: Arc<Mutex<HashSet<String>>>,
    /// The scan trigger a real (non-suppressed) change dispatches — the port of
    /// C# `ReportFileSystemChanged` → `ProviderManager.QueueRefresh`. `None`
    /// (unit tests without a target) logs the change only.
    refresh_target: Option<Arc<dyn LibraryScanTrigger>>,
}

impl std::fmt::Debug for HermitLibraryMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLibraryMonitor")
            .finish_non_exhaustive()
    }
}

impl HermitLibraryMonitor {
    /// Creates a monitor over the given watcher and the source of library root
    /// paths to watch (a plain `Vec<String>` works for a fixed set).
    #[must_use]
    pub fn new(
        watcher: Arc<dyn FileSystemWatcher>,
        roots: impl WatchRootsSource + 'static,
    ) -> Self {
        Self {
            watcher,
            roots: Arc::new(roots),
            suppressed: Arc::new(Mutex::new(HashSet::new())),
            refresh_target: None,
        }
    }

    /// Attaches the library manager a real change should refresh. Called once by
    /// the composition root; without it a reported change is only logged.
    #[must_use]
    pub fn with_refresh_target(mut self, library: Arc<dyn LibraryScanTrigger>) -> Self {
        self.refresh_target = Some(library);
        self
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
        for root in self.roots.watch_roots().await {
            // Per-root failures (root unmounted, inotify limit) must not stop
            // the remaining roots from being watched — the C# monitor
            // try/catches each path the same way.
            if let Err(err) = self.watcher.watch(&root).await {
                tracing::warn!(root, %err, "failed to watch library root");
            }
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
        tracing::debug!(path, "library filesystem change reported");
        // Dispatch the refresh. Hermit's scanner is whole-library (not per-path),
        // so a reported change queues a coalescing `scan_all` — the scanner picks
        // up the new/changed file under `path`. Overlapping reports (a webhook
        // batch) fold into one scan via the library manager's in-flight guard.
        // ponytail: whole-library rescan, not Jellyfin's targeted per-item
        // refresh; upgrade to a path-scoped scan if full rescans get too costly.
        if let Some(library) = &self.refresh_target {
            library.queue_library_scan().await?;
        }
        Ok(())
    }
}

/// A [`FileSystemWatcher`] that watches nothing.
///
/// The composition root's fallback when the real
/// [`NotifyFileSystemWatcher`](crate::notify_watcher::NotifyFileSystemWatcher)
/// cannot initialize (e.g. inotify limits exhausted): the external-change
/// **webhooks** (`POST /Library/{Series,Movies,Media}/…`) still drive a real
/// refresh; only the passive OS-watch lifecycle (`start`/`stop`) is a no-op.
/// Unit tests use it wherever a monitor needs a watcher that never fires.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFileSystemWatcher;

#[async_trait]
impl FileSystemWatcher for NoopFileSystemWatcher {
    async fn watch(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn unwatch(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn unwatch_all(&self) -> Result<(), ServiceError> {
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

    /// A watcher that rejects one root and records the rest.
    struct FlakyWatcher {
        bad: &'static str,
        watched: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl FileSystemWatcher for FlakyWatcher {
        async fn watch(&self, path: &str) -> Result<(), ServiceError> {
            if path == self.bad {
                return Err(ServiceError::backend("gone"));
            }
            self.watched.lock().unwrap().push(path.to_owned());
            Ok(())
        }
        async fn unwatch(&self, _path: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn unwatch_all(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_continues_past_a_failing_root() {
        let watcher = Arc::new(FlakyWatcher {
            bad: "/media/unmounted",
            watched: StdMutex::default(),
        });
        let monitor = HermitLibraryMonitor::new(
            watcher.clone(),
            vec!["/media/unmounted".to_owned(), "/media/tv".to_owned()],
        );
        // One root failing to watch must not fail start or skip later roots.
        monitor.start().await.expect("start");
        assert_eq!(*watcher.watched.lock().unwrap(), vec!["/media/tv"]);
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

    /// A [`LibraryScanTrigger`] fake that counts `queue_library_scan` calls.
    #[derive(Default)]
    struct CountingLibrary {
        scans: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LibraryScanTrigger for CountingLibrary {
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn real_change_queues_a_scan() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = HermitLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone());
        monitor
            .report_file_system_changed("/media/movies/Solaris (1972)")
            .await
            .expect("report");
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn suppressed_change_does_not_scan() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = HermitLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone());
        let dir = "/media/movies/Solaris";
        monitor
            .report_file_system_change_beginning(dir)
            .await
            .expect("begin");
        // A change under a server-write-suppressed dir must not trigger a rescan.
        monitor
            .report_file_system_changed(&format!("{dir}/poster.jpg"))
            .await
            .expect("report");
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
