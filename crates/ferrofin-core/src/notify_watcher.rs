//! [`NotifyFileSystemWatcher`] — the real OS filesystem watcher behind the
//! [`FileSystemWatcher`] seam, over the `notify` crate (inotify on Linux).
//!
//! Port of the `FileSystemWatcher` half of
//! `Emby.Server.Implementations.IO.LibraryMonitor`: one recursive watch per
//! library root, create/modify/remove/rename events forwarded as changed
//! paths. The C# watcher raises events straight into the monitor; here the
//! `notify` backend delivers them on its own thread, so the watcher pushes
//! each path onto an unbounded channel and the composition root pumps the
//! receiving end into
//! [`LibraryMonitor::report_file_system_changed`](ferrofin_traits::library::LibraryMonitor::report_file_system_changed)
//! — which keeps the monitor→watcher ownership one-way.
//!
//! Noise filtering mirrors the C# `ShouldIgnoreChange`: access-only events are
//! dropped at the event-kind level and always-ignored paths (artwork, sample
//! files, trash dirs) via [`should_ignore_path`] — the same predicate the
//! scanner's planner uses, so the watcher never reports a path the scan would
//! skip anyway.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use ferrofin_traits::error::ServiceError;

use crate::resolvers::{FileSystemWatcher, should_ignore_path};

/// A [`FileSystemWatcher`] over a [`notify`] recursive watcher.
///
/// Constructed once at the composition root; the paired receiver yields every
/// non-ignored changed path under the watched roots.
pub struct NotifyFileSystemWatcher {
    /// The OS watcher. `notify`'s watch/unwatch take `&mut`, so it lives behind
    /// a `std::sync::Mutex`, taken only inside the blocking-pool jobs that make
    /// those calls (it never spans an `.await`).
    inner: Arc<Mutex<RecommendedWatcher>>,
    /// Roots currently watched (without a trailing slash), for
    /// `unwatch_all` — and for the event callback, which drops a
    /// metadata-only change of a root itself.
    watched: Arc<Mutex<HashSet<String>>>,
    /// Every library location (without a trailing slash), from
    /// [`FileSystemWatcher::set_library_roots`]: the event callback drops a
    /// metadata-only change of these too, including locations nested in a
    /// watched root, which are never watched on their own.
    library_roots: Arc<Mutex<HashSet<String>>>,
}

/// `path` without its trailing slashes (`/` stays `/`).
fn root_key(path: &str) -> String {
    match path.trim_end_matches('/') {
        "" if path.starts_with('/') => "/".to_owned(),
        path => path.to_owned(),
    }
}

impl std::fmt::Debug for NotifyFileSystemWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyFileSystemWatcher")
            .finish_non_exhaustive()
    }
}

impl NotifyFileSystemWatcher {
    /// Creates the watcher and the channel its change events arrive on.
    ///
    /// The receiver yields the absolute path of each created/modified/removed
    /// file or directory under the watched roots, already filtered through
    /// [`should_ignore_path`], and every watched root when the kernel's event
    /// queue overflowed and changes were lost.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the OS watcher cannot be initialized
    /// (e.g. inotify limits exhausted).
    pub fn new() -> Result<(Self, mpsc::UnboundedReceiver<String>), ServiceError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let root_set: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let library_root_set: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let watched_roots = Arc::clone(&root_set);
        let library_roots = Arc::clone(&library_root_set);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            forward_event(res, &watched_roots, &library_roots, &tx);
        })
        .map_err(|e| ServiceError::backend(format!("failed to create filesystem watcher: {e}")))?;
        Ok((
            Self {
                inner: Arc::new(Mutex::new(watcher)),
                watched: root_set,
                library_roots: library_root_set,
            },
            rx,
        ))
    }
}

/// The watcher's event callback: forwards each changed path of `res` that a
/// scan would not skip, and every watched root when the event says changes
/// were lost.
fn forward_event(
    res: notify::Result<notify::Event>,
    watched_roots: &Mutex<HashSet<String>>,
    library_roots: &Mutex<HashSet<String>>,
    tx: &mpsc::UnboundedSender<String>,
) {
    let event = match res {
        Ok(event) => event,
        Err(err) => {
            tracing::warn!(%err, "filesystem watch event error");
            return;
        }
    };
    // The kernel's event queue overflowed (`IN_Q_OVERFLOW`, past
    // `fs.inotify.max_queued_events`): every change in that window is lost,
    // and notify reports only that a rescan is needed. Upstream's
    // `OnWatcherError` logs it and stops watching; Ferrofin reports every
    // watched root instead, so the monitor's settle window turns the storm
    // into one scan per root and nothing stays missed until the next
    // scheduled scan (owner decision 2026-10-25).
    if event.need_rescan() {
        let mut roots: Vec<String> = watched_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        roots.sort();
        tracing::warn!(
            roots = roots.len(),
            "filesystem watch queue overflowed, so changes were missed; rescanning the \
             watched library roots (raise the fs.inotify.max_queued_events sysctl)"
        );
        for root in roots {
            // Send fails only when the receiver is gone (shutdown) — drop.
            let _ = tx.send(root);
        }
        return;
    }
    // Only content-affecting kinds: create, data/name modify, remove.
    // Access events (reads, close-no-write) are pure noise for a scan.
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return;
    }
    // A directory's own `Modify` is never about its entries (those
    // arrive as their own create/remove events): an attribute change
    // (`IN_ATTRIB`), or its mtime set alone (`IN_MODIFY`).
    let metadata_only = matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Data(_))
    );
    // A poisoned set still holds whole entries (each update is one
    // insert, remove or replacement).
    let is_library_root = |path: &str| {
        let key = root_key(path);
        [watched_roots, library_roots].iter().any(|set| {
            set.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key)
        })
    };
    for path in event.paths {
        let path = path.to_string_lossy().into_owned();
        if should_ignore_path(&path) {
            continue;
        }
        // A library root's own attributes or times changing
        // (`touch`, `chmod`, `rsync -a` of the root) says nothing
        // about its contents, but the root is the whole library:
        // reporting it would rescan the library for a metadata-only
        // change. A file or folder created or removed in it arrives
        // with its own path.
        if metadata_only && is_library_root(&path) {
            tracing::trace!(path, "metadata-only change of a library root ignored");
            continue;
        }
        // Send fails only when the receiver is gone (shutdown) — drop.
        let _ = tx.send(path);
    }
}

#[async_trait]
impl FileSystemWatcher for NotifyFileSystemWatcher {
    async fn watch(&self, path: &str) -> Result<(), ServiceError> {
        // A recursive watch walks the whole tree to add one inotify watch per
        // directory, and `watch` blocks until notify's thread has done it:
        // seconds on a large or network-mounted library. So it runs on the
        // blocking pool, never on a runtime worker (upstream starts each
        // watch on a thread-pool task, `LibraryMonitor.StartWatchingPath`).
        //
        // The root joins the watched set in the same job, under the watcher's
        // lock, once the watch exists: a job runs on even when this future is
        // dropped, and the set must still name every watch `unwatch_all` has
        // to remove.
        let inner = Arc::clone(&self.inner);
        let watched = Arc::clone(&self.watched);
        let root = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut os_watcher = inner.lock().expect("watcher lock not poisoned");
            os_watcher.watch(Path::new(&root), RecursiveMode::Recursive)?;
            watched
                .lock()
                .expect("watched set not poisoned")
                .insert(root_key(&root));
            Ok::<_, notify::Error>(())
        })
        .await
        .map_err(|e| ServiceError::backend(format!("watch task for {path} did not finish: {e}")))?
        .map_err(|e| ServiceError::backend(format!("failed to watch {path}: {e}")))?;
        tracing::info!(path, "watching library root for changes");
        Ok(())
    }

    async fn unwatch(&self, path: &str) -> Result<(), ServiceError> {
        // Unwatch of a root the OS never watched (e.g. it failed to start) is
        // fine to treat as done — the goal state "not watched" already holds.
        // Removing a recursive watch drops one inotify watch per directory,
        // so it runs on the blocking pool as `watch` does.
        // The root leaves the watched set in the same job, as in `watch`.
        let inner = Arc::clone(&self.inner);
        let watched = Arc::clone(&self.watched);
        let root = path.to_owned();
        let unwatched = tokio::task::spawn_blocking(move || {
            let mut os_watcher = inner.lock().expect("watcher lock not poisoned");
            let unwatched = os_watcher.unwatch(Path::new(&root));
            watched
                .lock()
                .expect("watched set not poisoned")
                .remove(&root_key(&root));
            unwatched
        })
        .await
        .map_err(|e| {
            ServiceError::backend(format!("unwatch task for {path} did not finish: {e}"))
        })?;
        if let Err(e) = unwatched {
            tracing::debug!(path, error = %e, "unwatch skipped");
        }
        Ok(())
    }

    fn set_library_roots(&self, roots: &[String]) {
        *self
            .library_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            roots.iter().map(|root| root_key(root)).collect();
    }

    async fn unwatch_all(&self) -> Result<(), ServiceError> {
        // One job under the watcher's lock, which a recursive walk holds
        // until its root has joined the watched set: a walk already running
        // when this is called (its `start` cancelled mid-walk, its job
        // running on) finishes first, and its root is removed with the rest.
        // A watch job that has not yet taken the lock can still land after.
        let inner = Arc::clone(&self.inner);
        let watched = Arc::clone(&self.watched);
        tokio::task::spawn_blocking(move || {
            let mut os_watcher = inner.lock().expect("watcher lock not poisoned");
            let roots: Vec<String> = watched
                .lock()
                .expect("watched set not poisoned")
                .drain()
                .collect();
            for root in roots {
                if let Err(e) = os_watcher.unwatch(Path::new(&root)) {
                    tracing::debug!(path = root, error = %e, "unwatch skipped");
                }
            }
        })
        .await
        .map_err(|e| ServiceError::backend(format!("unwatch task did not finish: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Receives paths until `pred` matches or the timeout lapses; returns every
    /// path seen along the way.
    async fn recv_until(
        rx: &mut mpsc::UnboundedReceiver<String>,
        pred: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(path)) => {
                    let done = pred(&path);
                    seen.push(path);
                    if done {
                        return seen;
                    }
                }
                _ => return seen,
            }
        }
    }

    /// A kernel queue overflow arrives as a path-less rescan event. It must
    /// not be dropped as a non-content kind: every watched root is reported,
    /// so the monitor scans what the lost events would have reported.
    #[test]
    fn a_queue_overflow_reports_every_watched_root() {
        let watched = Mutex::new(HashSet::from([
            "/media/tv".to_owned(),
            "/media/movies".to_owned(),
        ]));
        let library_roots = Mutex::new(HashSet::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let overflow = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);

        forward_event(Ok(overflow), &watched, &library_roots, &tx);

        let mut reported = Vec::new();
        while let Ok(path) = rx.try_recv() {
            reported.push(path);
        }
        assert_eq!(reported, ["/media/movies", "/media/tv"]);
    }

    /// Any other non-content event (an access, an `Other` without the rescan
    /// flag) still reports nothing.
    #[test]
    fn a_non_content_event_without_a_rescan_reports_nothing() {
        let watched = Mutex::new(HashSet::from(["/media/tv".to_owned()]));
        let library_roots = Mutex::new(HashSet::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let other = notify::Event::new(EventKind::Other).add_path("/media/tv/Show".into());

        forward_event(Ok(other), &watched, &library_roots, &tx);

        assert!(rx.try_recv().is_err());
    }

    /// Registering a recursive watch (notify walks the whole tree, one
    /// inotify watch per directory, and blocks until it is done) runs on the
    /// blocking pool, never inline on a runtime worker: with the pool's only
    /// thread held busy, the watch cannot complete.
    #[test]
    fn watching_a_root_goes_through_the_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(dir.path().join("Show/Season 1")).expect("tree");
            let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
            let watcher = Arc::new(watcher);

            let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                busy_tx.send(()).ok();
                release_rx.recv().ok();
            });
            busy_rx.await.unwrap();

            let root = dir.path().to_string_lossy().into_owned();
            let mut watch = tokio::spawn({
                let watcher = Arc::clone(&watcher);
                async move { watcher.watch(&root).await }
            });
            let starved = tokio::time::timeout(Duration::from_millis(250), &mut watch).await;
            assert!(
                starved.is_err(),
                "the watch registered with the blocking pool starved, so inline on the runtime"
            );

            release_tx.send(()).unwrap();
            hog.await.unwrap();
            watch.await.unwrap().expect("watch");
            // The watch is live once registered.
            let file = dir.path().join("Show/Season 1/e1.mkv");
            std::fs::write(&file, b"x").expect("write");
            let target = file.to_string_lossy().into_owned();
            let seen = recv_until(&mut rx, |p| p == target).await;
            assert!(seen.contains(&target), "{seen:?}");
        });
    }

    #[tokio::test]
    async fn create_under_watched_root_reports_the_file_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        watcher
            .watch(&dir.path().to_string_lossy())
            .await
            .expect("watch");

        std::fs::write(dir.path().join("Solaris (1972).mkv"), b"x").expect("write");

        let seen = recv_until(&mut rx, |p| p.ends_with("Solaris (1972).mkv")).await;
        assert!(
            seen.iter().any(|p| p.ends_with("Solaris (1972).mkv")),
            "expected a change event for the new file, saw: {seen:?}"
        );
    }

    #[tokio::test]
    async fn ignored_paths_are_filtered_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        watcher
            .watch(&dir.path().to_string_lossy())
            .await
            .expect("watch");

        // An always-ignored artwork file first, then a real one: by the time the
        // real file's event arrives (inotify delivers in order), the ignored
        // path must not have been reported.
        std::fs::write(dir.path().join("AlbumArt.jpg"), b"x").expect("write");
        std::fs::write(dir.path().join("movie.mkv"), b"x").expect("write");

        let seen = recv_until(&mut rx, |p| p.ends_with("movie.mkv")).await;
        assert!(
            seen.iter().any(|p| p.ends_with("movie.mkv")),
            "expected the real file, saw: {seen:?}"
        );
        assert!(
            !seen.iter().any(|p| p.ends_with("AlbumArt.jpg")),
            "ignored artwork must be filtered, saw: {seen:?}"
        );
    }

    /// A library root's own metadata changing (`touch` of the root) is not
    /// reported — it would rescan the whole library for nothing — while a
    /// file created in the root, and a metadata change of a folder inside
    /// it, still are.
    #[tokio::test]
    async fn a_metadata_only_change_of_a_root_is_not_reported() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        // Not the tempdir itself: its hidden `.tmp…` name is ignored anyway.
        let library = dir.path().join("tv");
        let sub = library.join("Show");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let root = library.to_string_lossy().into_owned();
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        watcher.watch(&format!("{root}/")).await.expect("watch");

        // Its mtime set alone (`IN_MODIFY`), then its mode (`IN_ATTRIB`).
        let later = std::time::SystemTime::now() + Duration::from_secs(3_600);
        std::fs::File::open(&library)
            .expect("root")
            .set_modified(later)
            .expect("touch root");
        std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o755))
            .expect("chmod root");
        std::fs::File::open(&sub)
            .expect("sub")
            .set_modified(later)
            .expect("touch sub");
        std::fs::write(library.join("new.mkv"), b"x").expect("write");

        let seen = recv_until(&mut rx, |p| p.ends_with("new.mkv")).await;
        assert!(
            seen.iter().any(|p| p.ends_with("new.mkv")),
            "the new file is reported: {seen:?}"
        );
        assert!(
            seen.iter().any(|p| Path::new(p) == sub),
            "a folder inside the root is reported: {seen:?}"
        );
        assert!(
            !seen.iter().any(|p| root_key(p) == root_key(&root)),
            "the root's own metadata change is not: {seen:?}"
        );
    }

    /// Libraries where only the outer one has real-time monitoring.
    struct OnlyOuterMonitored {
        outer: String,
        inner: String,
    }

    #[async_trait]
    impl crate::library_monitor::WatchRootsSource for OnlyOuterMonitored {
        async fn watch_roots(&self) -> Vec<String> {
            vec![self.outer.clone()]
        }

        async fn library_locations(&self) -> Vec<String> {
            vec![self.outer.clone(), self.inner.clone()]
        }
    }

    /// A library without real-time monitoring inside a monitored one is not
    /// watched at all, but a change under it is still matched to it; its own
    /// metadata change must not rescan it either.
    #[tokio::test]
    async fn a_metadata_only_change_of_an_unmonitored_library_inside_a_watched_one_is_not_reported()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("media");
        let movies = media.join("movies");
        std::fs::create_dir_all(&movies).expect("mkdir");
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        let monitor = crate::library_monitor::FerrofinLibraryMonitor::new(
            Arc::new(watcher),
            OnlyOuterMonitored {
                outer: media.to_string_lossy().into_owned(),
                inner: movies.to_string_lossy().into_owned(),
            },
        );
        ferrofin_traits::library::LibraryMonitor::start(&monitor)
            .await
            .expect("start");

        let later = std::time::SystemTime::now() + Duration::from_secs(3_600);
        std::fs::File::open(&movies)
            .expect("movies")
            .set_modified(later)
            .expect("touch movies");
        std::fs::write(movies.join("new.mkv"), b"x").expect("write");

        let seen = recv_until(&mut rx, |p| p.ends_with("new.mkv")).await;
        assert!(
            seen.iter().any(|p| p.ends_with("new.mkv")),
            "the new file is reported: {seen:?}"
        );
        assert!(
            !seen.iter().any(|p| Path::new(p) == movies),
            "the inner library's own metadata change is not: {seen:?}"
        );
    }

    /// `set_library_roots` replaces the set: a location dropped from it is an
    /// ordinary folder of its parent library again.
    #[tokio::test]
    async fn a_location_dropped_from_the_library_roots_is_reported_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("media");
        let movies = media.join("movies");
        std::fs::create_dir_all(&movies).expect("mkdir");
        let (media_root, movies_root) = (
            media.to_string_lossy().into_owned(),
            movies.to_string_lossy().into_owned(),
        );
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        watcher.watch(&media_root).await.expect("watch");
        watcher.set_library_roots(&[media_root.clone(), movies_root]);
        watcher.set_library_roots(&[media_root]);

        let later = std::time::SystemTime::now() + Duration::from_secs(3_600);
        std::fs::File::open(&movies)
            .expect("movies")
            .set_modified(later)
            .expect("touch movies");

        let seen = recv_until(&mut rx, |p| Path::new(p) == movies).await;
        assert!(
            seen.iter().any(|p| Path::new(p) == movies),
            "the folder's metadata change is reported: {seen:?}"
        );
    }

    /// A library location nested in another is covered by its parent's
    /// recursive watch, so the monitor never watches it on its own; it is a
    /// library root all the same, and its own metadata change must not
    /// rescan its library.
    #[tokio::test]
    async fn a_metadata_only_change_of_a_nested_library_root_is_not_reported() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        // Library A at `media`, library B at `media/movies` inside it.
        let media = dir.path().join("media");
        let movies = media.join("movies");
        std::fs::create_dir_all(&movies).expect("mkdir");
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        let monitor = crate::library_monitor::FerrofinLibraryMonitor::new(
            Arc::new(watcher),
            // The nested location as the dashboard often spells it, with a
            // trailing slash the event path does not have.
            vec![
                media.to_string_lossy().into_owned(),
                format!("{}/", movies.to_string_lossy()),
            ],
        );
        ferrofin_traits::library::LibraryMonitor::start(&monitor)
            .await
            .expect("start");

        // `touch` and `chmod` of the nested location, then a new file in it.
        let later = std::time::SystemTime::now() + Duration::from_secs(3_600);
        std::fs::File::open(&movies)
            .expect("movies")
            .set_modified(later)
            .expect("touch movies");
        std::fs::set_permissions(&movies, std::fs::Permissions::from_mode(0o755))
            .expect("chmod movies");
        std::fs::write(movies.join("new.mkv"), b"x").expect("write");

        let seen = recv_until(&mut rx, |p| p.ends_with("new.mkv")).await;
        assert!(
            seen.iter().any(|p| p.ends_with("new.mkv")),
            "the new file is reported: {seen:?}"
        );
        assert!(
            !seen.iter().any(|p| Path::new(p) == movies),
            "the nested root's own metadata change is not: {seen:?}"
        );
    }

    #[tokio::test]
    async fn unwatch_all_stops_reporting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (watcher, mut rx) = NotifyFileSystemWatcher::new().expect("watcher");
        watcher
            .watch(&dir.path().to_string_lossy())
            .await
            .expect("watch");
        watcher.unwatch_all().await.expect("unwatch_all");

        std::fs::write(dir.path().join("after.mkv"), b"x").expect("write");

        // The inotify watch is removed synchronously, so nothing may arrive.
        let got = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            got.is_err(),
            "no events expected after unwatch_all: {got:?}"
        );
    }

    /// `unwatch_all` called while a recursive walk is still running (its
    /// `start` cancelled, the blocking job running on) waits for the walk and
    /// removes the root it records, instead of missing it.
    #[tokio::test]
    async fn unwatch_all_removes_a_root_whose_walk_was_still_running() {
        let (watcher, _rx) = NotifyFileSystemWatcher::new().expect("watcher");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        // Stands in for `watch`'s job: it holds the watcher for the walk, then
        // records the root.
        let walk = std::thread::spawn({
            let inner = Arc::clone(&watcher.inner);
            let roots = Arc::clone(&watcher.watched);
            move || {
                let _walking = inner.lock().unwrap();
                locked_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                roots.lock().unwrap().insert("/media/movies".to_owned());
            }
        });
        locked_rx.recv().unwrap();

        watcher.unwatch_all().await.expect("unwatch_all");

        walk.join().unwrap();
        assert!(watcher.watched.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn watching_a_missing_root_errors() {
        let (watcher, _rx) = NotifyFileSystemWatcher::new().expect("watcher");
        assert!(
            watcher
                .watch("/nonexistent/ferrofin-test-root")
                .await
                .is_err()
        );
    }
}
