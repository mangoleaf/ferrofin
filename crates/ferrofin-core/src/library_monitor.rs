//! [`FerrofinLibraryMonitor`] — the concrete [`LibraryMonitor`] over a
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
//! queues a (coalescing) **path-scoped** scan through the injected
//! [`LibraryScanTrigger`] (the composition root passes the library manager), so
//! only the items touched by the changed paths are re-resolved. Like the C# timer, a
//! burst of changes **debounces**: each report (re)arms a settle window of
//! `LibraryMonitorDelay` seconds (read live from the injected configuration
//! manager, or fixed via [`FerrofinLibraryMonitor::with_debounce`]) and the scan
//! dispatches only once the window lapses with no further changes. A zero
//! delay dispatches immediately. Without a trigger attached (unit tests) a
//! change is logged only.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryMonitor;

use crate::resolvers::FileSystemWatcher;

/// The debounce accumulator: paths reported since the last dispatch, the
/// settle deadline (re-armed by every report), and whether a worker task is
/// currently waiting on that deadline.
struct PendingChanges {
    /// Changed paths accumulated in the current settle window.
    paths: HashSet<String>,
    /// When the current window lapses (last report + delay, clamped by
    /// [`Self::hard_deadline`]).
    deadline: Instant,
    /// The ceiling on [`Self::deadline`] for the current batch: the first
    /// report of the batch plus [`MAX_SETTLE_WINDOWS`] delays. Without it the
    /// window is re-armed by every report and a source that never goes quiet
    /// (a download directory, a recording being written) defers the scan
    /// forever while `paths` grows without bound.
    hard_deadline: Instant,
    /// Whether a debounce worker task is alive and will dispatch.
    worker_running: bool,
}

/// How many settle delays a batch may be deferred before it is dispatched
/// regardless of continuing reports.
///
/// This is the bound on the accumulator: `paths` can hold at most one entry per
/// distinct path reported within `LibraryMonitorDelay × MAX_SETTLE_WINDOWS`,
/// rather than one per path reported since the server started. It is also what
/// makes a continuously-written library root scan at all — with a purely
/// sliding window it never does.
///
/// FLAGGED as a candidate setting — plausible range 2…60, default 10 (10 min at
/// the default 60 s `LibraryMonitorDelay`).
const MAX_SETTLE_WINDOWS: u32 = 10;

/// Hard cap on accumulated paths: reaching it dispatches the batch immediately
/// instead of waiting the window out.
///
/// Paths are **dispatched**, never dropped — a dropped path is a change the
/// library never learns about. So this bounds how large one batch gets, not how
/// much work is done. Sized so the accumulator stays well under 10 MB even for
/// long paths, while sitting far above any plausible real import batch.
///
/// FLAGGED as a candidate setting — plausible range 1_000…200_000, default
/// 20_000.
const MAX_PENDING_PATHS: usize = 20_000;

/// The narrow slice of the library manager the monitor needs: queue a rescan.
///
/// The monitor only ever asks "something changed — refresh"; depending on the
/// whole [`LibraryManager`](ferrofin_traits::library::LibraryManager) would drag in
/// ~50 unrelated methods. [`FerrofinLibraryManager`](crate::FerrofinLibraryManager)
/// implements this, so the composition root passes the same instance.
#[async_trait]
pub trait LibraryScanTrigger: Send + Sync {
    /// Queues a (coalescing) full library scan.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the scan cannot be queued.
    async fn queue_library_scan(&self) -> Result<(), ServiceError>;

    /// Queues a (coalescing) scan covering only the items touched by the given
    /// changed filesystem paths. Defaults to a full scan so simple triggers
    /// need not implement path scoping;
    /// [`FerrofinLibraryManager`](crate::FerrofinLibraryManager) overrides it with
    /// the real path-scoped ingest.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the scan cannot be queued.
    async fn queue_scan_paths(&self, paths: Vec<String>) -> Result<(), ServiceError> {
        let _ = paths;
        self.queue_library_scan().await
    }
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
pub struct FerrofinLibraryMonitor {
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
    /// Debounce accumulator shared with the worker task. `std::sync::Mutex`:
    /// the guard never spans an `.await`.
    pending: Arc<Mutex<PendingChanges>>,
    /// Wakes the debounce worker out of its sleep when a report *lowers* the
    /// deadline (the size cap firing). Raising the deadline needs no wake — the
    /// worker re-reads it and sleeps again — but lowering one it is already
    /// sleeping past would otherwise not take effect until the old deadline.
    flush: Arc<tokio::sync::Notify>,
    /// A fixed settle delay, taking precedence over the configured one.
    fixed_debounce: Option<Duration>,
    /// Where the live `LibraryMonitorDelay` setting is read from. `None` (and
    /// no [`fixed_debounce`](Self::with_debounce)) means no debounce.
    config: Option<Arc<dyn ServerConfigurationManager>>,
}

impl std::fmt::Debug for FerrofinLibraryMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLibraryMonitor")
            .finish_non_exhaustive()
    }
}

impl FerrofinLibraryMonitor {
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
            pending: Arc::new(Mutex::new(PendingChanges {
                paths: HashSet::new(),
                deadline: Instant::now(),
                hard_deadline: Instant::now(),
                worker_running: false,
            })),
            flush: Arc::new(tokio::sync::Notify::new()),
            fixed_debounce: None,
            config: None,
        }
    }

    /// Attaches the library manager a real change should refresh. Called once by
    /// the composition root; without it a reported change is only logged.
    #[must_use]
    pub fn with_refresh_target(mut self, library: Arc<dyn LibraryScanTrigger>) -> Self {
        self.refresh_target = Some(library);
        self
    }

    /// Fixes the settle delay, overriding the configured `LibraryMonitorDelay`.
    #[must_use]
    pub fn with_debounce(mut self, delay: Duration) -> Self {
        self.fixed_debounce = Some(delay);
        self
    }

    /// Attaches the configuration manager the live `LibraryMonitorDelay`
    /// setting is read from (per report, so a dashboard change applies without
    /// a restart — matching the C# monitor).
    #[must_use]
    pub fn with_config(mut self, config: Arc<dyn ServerConfigurationManager>) -> Self {
        self.config = Some(config);
        self
    }

    /// The settle delay in effect right now: the fixed override, else the
    /// configured `LibraryMonitorDelay` (clamped at zero), else zero.
    async fn debounce_delay(&self) -> Duration {
        if let Some(fixed) = self.fixed_debounce {
            return fixed;
        }
        match &self.config {
            Some(config) => match config.configuration().await {
                Ok(c) => Duration::from_secs(u64::try_from(c.library_monitor_delay).unwrap_or(0)),
                Err(err) => {
                    tracing::warn!(%err, "failed to read LibraryMonitorDelay; not debouncing");
                    Duration::ZERO
                }
            },
            None => Duration::ZERO,
        }
    }

    /// Waits out the settle window (re-armed by each new report) and then
    /// dispatches one scan for the whole batch. Runs on its own task; exactly
    /// one worker is alive while changes are pending.
    async fn debounce_worker(self) {
        loop {
            let deadline = self
                .pending
                .lock()
                .expect("pending set not poisoned")
                .deadline;
            // Racing the sleep against `flush` is what lets the size cap take
            // effect while the worker is already asleep past that point. A
            // spurious wake simply re-reads the deadline and sleeps again.
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = self.flush.notified() => {}
            }
            let paths = {
                let mut pending = self.pending.lock().expect("pending set not poisoned");
                // A report during the sleep pushed the deadline out — keep waiting.
                if Instant::now() < pending.deadline {
                    continue;
                }
                pending.worker_running = false;
                std::mem::take(&mut pending.paths)
            };
            if !paths.is_empty() {
                tracing::info!(
                    changes = paths.len(),
                    "library changes settled; queueing scan"
                );
                if let Some(library) = &self.refresh_target
                    && let Err(err) = library.queue_scan_paths(paths.into_iter().collect()).await
                {
                    tracing::warn!(%err, "failed to queue debounced library scan");
                }
            }
            return;
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
impl LibraryMonitor for FerrofinLibraryMonitor {
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
        // Dispatch a path-scoped refresh: the scanner resolves and persists just
        // the items touched by the reported path (a deleted path prunes its
        // rows), so one new file does not re-walk the whole library. Overlapping
        // reports fold via the debounce window and the scan's in-flight guard.
        let delay = self.debounce_delay().await;
        if delay.is_zero() {
            if let Some(library) = &self.refresh_target {
                library.queue_scan_paths(vec![path.to_owned()]).await?;
            }
            return Ok(());
        }
        // Accumulate the path and (re)arm the settle window; the single worker
        // task dispatches once the window lapses with no further reports.
        //
        // The re-arm is clamped two ways, because a purely sliding window is
        // unbounded in both time and memory: a source that keeps reporting
        // holds the batch open forever, so the scan never runs and `paths`
        // grows for the life of the process (measured: 60,000 reported paths
        // held 27 MB resident, released only once the reports stopped). The
        // clamps dispatch the batch — they never drop a path.
        let (spawn_worker, wake_worker) = {
            let mut pending = self.pending.lock().expect("pending set not poisoned");
            let now = Instant::now();
            if pending.paths.is_empty() {
                pending.hard_deadline = now + delay * MAX_SETTLE_WINDOWS;
            }
            pending.paths.insert(path.to_owned());
            let previous = pending.deadline;
            pending.deadline = std::cmp::min(now + delay, pending.hard_deadline);
            if pending.paths.len() >= MAX_PENDING_PATHS {
                pending.deadline = now;
            }
            let spawn = !std::mem::replace(&mut pending.worker_running, true);
            // Only a *lowered* deadline needs a wake; a raised one is picked up
            // when the worker's current sleep ends.
            (spawn, !spawn && pending.deadline < previous)
        };
        if spawn_worker {
            tokio::spawn(self.clone().debounce_worker());
        } else if wake_worker {
            self.flush.notify_one();
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
        let monitor = FerrofinLibraryMonitor::new(
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
        let monitor = FerrofinLibraryMonitor::new(
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
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![]);
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
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![]);
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
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone());
        monitor
            .report_file_system_changed("/media/movies/Solaris (1972)")
            .await
            .expect("report");
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A [`LibraryScanTrigger`] fake recording each path-scoped dispatch.
    #[derive(Default)]
    struct PathsLibrary {
        batches: StdMutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl LibraryScanTrigger for PathsLibrary {
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            panic!("the monitor must dispatch path-scoped, not full, scans");
        }
        async fn queue_scan_paths(&self, paths: Vec<String>) -> Result<(), ServiceError> {
            self.batches.lock().unwrap().push(paths);
            Ok(())
        }
    }

    #[tokio::test]
    async fn immediate_dispatch_carries_the_changed_path() {
        let library = Arc::new(PathsLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone());
        monitor
            .report_file_system_changed("/media/movies/Solaris (1972).mkv")
            .await
            .expect("report");
        assert_eq!(
            *library.batches.lock().unwrap(),
            vec![vec!["/media/movies/Solaris (1972).mkv".to_owned()]]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn debounced_dispatch_carries_the_settled_path_batch() {
        let library = Arc::new(PathsLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        monitor
            .report_file_system_changed("/media/movies/a.mkv")
            .await
            .expect("report");
        monitor
            .report_file_system_changed("/media/movies/b.mkv")
            .await
            .expect("report");
        tokio::time::sleep(Duration::from_secs(61)).await;
        let batches = library.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "one settled batch");
        let mut paths = batches[0].clone();
        paths.sort();
        assert_eq!(paths, vec!["/media/movies/a.mkv", "/media/movies/b.mkv"]);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_of_changes_settles_into_one_scan() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        for i in 0..25 {
            monitor
                .report_file_system_changed(&format!("/media/movies/m{i}/m{i}.mkv"))
                .await
                .expect("report");
        }
        // Nothing dispatches inside the settle window…
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 0);
        // …and the whole burst folds into exactly one scan after it lapses.
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_mins(5)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn new_changes_extend_the_settle_window() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        monitor
            .report_file_system_changed("/media/movies/a.mkv")
            .await
            .expect("report");
        tokio::time::sleep(Duration::from_secs(30)).await;
        // A second change 30s in re-arms the window: the scan may not fire at
        // t=60 (30s after the last change)…
        monitor
            .report_file_system_changed("/media/movies/b.mkv")
            .await
            .expect("report");
        tokio::time::sleep(Duration::from_secs(45)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 0);
        // …but does fire once 60s pass with no further changes (t=90).
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A source that never goes quiet used to hold the batch open forever: the
    /// settle window was re-armed by every report with no ceiling, so the scan
    /// never ran and the accumulator grew for the life of the process
    /// (measured over HTTP: 60,000 reported paths held 27 MB resident, freed
    /// only once the reports stopped). The hard deadline dispatches the batch
    /// after `MAX_SETTLE_WINDOWS` delays regardless.
    #[tokio::test(start_paused = true)]
    async fn unending_reports_still_dispatch_at_the_hard_deadline() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        // A report every 30s — half the settle delay, so a purely sliding
        // window is re-armed before it ever lapses.
        for i in 0..(MAX_SETTLE_WINDOWS * 2) {
            monitor
                .report_file_system_changed(&format!("/media/movies/m{i}.mkv"))
                .await
                .expect("report");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        // The loop ran for MAX_SETTLE_WINDOWS * 2 delays without ever leaving a
        // full window quiet, so a purely sliding window would still be holding
        // the batch here. One extra tick lets the woken worker finish its
        // dispatch (the hard deadline and the loop's last sleep land together).
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            library.scans.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "a never-quiet source must still get scanned"
        );
    }

    /// The size cap must fire when the worker is ALREADY ASLEEP on the settle
    /// deadline — which is the production shape, and the one the sibling test
    /// below cannot reach.
    ///
    /// That test floods in a tight loop, so the worker never gets scheduled to
    /// begin its 60s sleep and simply reads the lowered deadline on its first
    /// pass; the wake-up is never exercised and deleting `flush.notify_one()`
    /// leaves it green. In production reports trickle in from inotify or one
    /// HTTP request at a time, the worker *is* parked on `now + 60s` when the
    /// capping path lands, and the notify is the only thing that dispatches it.
    /// Sleeping first is what forces the worker to park.
    #[tokio::test(start_paused = true)]
    async fn the_size_cap_wakes_a_worker_already_parked_on_the_settle_deadline() {
        let library = Arc::new(PathsLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));

        monitor
            .report_file_system_changed("/media/movies/first.mkv")
            .await
            .expect("report");
        // Let the worker actually reach its sleep on the 60s deadline.
        tokio::time::sleep(Duration::from_secs(1)).await;

        for i in 1..MAX_PENDING_PATHS {
            monitor
                .report_file_system_changed(&format!("/media/movies/m{i}.mkv"))
                .await
                .expect("report");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let batches = library.batches.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "the cap must wake the parked worker; without the notify the batch \
             waits out the full settle window and nothing is dispatched"
        );
        assert_eq!(batches[0].len(), MAX_PENDING_PATHS);
    }

    /// The hard deadline bounds the batch in time; the size cap bounds it in
    /// paths, for a burst that arrives faster than the window. Paths are
    /// dispatched, never dropped — so the batch must carry every reported path.
    #[tokio::test(start_paused = true)]
    async fn a_burst_past_the_size_cap_dispatches_without_dropping_paths() {
        let library = Arc::new(PathsLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        for i in 0..MAX_PENDING_PATHS {
            monitor
                .report_file_system_changed(&format!("/media/movies/m{i}.mkv"))
                .await
                .expect("report");
        }
        // Reaching the cap dispatches promptly. Without it nothing would have
        // been queued yet — the settle window still has 60s to run.
        tokio::time::sleep(Duration::from_millis(10)).await;
        {
            let batches = library.batches.lock().unwrap();
            assert_eq!(batches.len(), 1, "the cap must force a dispatch mid-burst");
            assert_eq!(batches[0].len(), MAX_PENDING_PATHS);
        }
        // The tail starts a fresh batch, dispatched by its own settle window.
        for i in MAX_PENDING_PATHS..(MAX_PENDING_PATHS + 5) {
            monitor
                .report_file_system_changed(&format!("/media/movies/m{i}.mkv"))
                .await
                .expect("report");
        }
        tokio::time::sleep(Duration::from_secs(61)).await;
        let batches = library.batches.lock().unwrap();
        let dispatched: usize = batches.iter().map(Vec::len).sum();
        assert_eq!(
            dispatched,
            MAX_PENDING_PATHS + 5,
            "the cap dispatches paths early, it never drops them"
        );
    }

    #[tokio::test]
    async fn a_second_batch_after_dispatch_scans_again() {
        tokio::time::pause();
        let library = Arc::new(CountingLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
            .with_refresh_target(library.clone())
            .with_debounce(Duration::from_mins(1));
        monitor
            .report_file_system_changed("/media/movies/a.mkv")
            .await
            .expect("report");
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The worker exited after dispatching; a fresh change starts a new one.
        monitor
            .report_file_system_changed("/media/movies/b.mkv")
            .await
            .expect("report");
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(library.scans.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn suppressed_change_does_not_scan() {
        let library = Arc::new(CountingLibrary::default());
        let monitor = FerrofinLibraryMonitor::new(Arc::new(FakeWatcher::default()), vec![])
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
