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
use std::sync::Mutex;

use async_trait::async_trait;
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
    /// a `std::sync::Mutex` — safe here because the guard never spans an
    /// `.await` (both calls are synchronous).
    inner: Mutex<RecommendedWatcher>,
    /// Roots currently watched, for `unwatch_all`.
    watched: Mutex<HashSet<String>>,
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
    /// [`should_ignore_path`].
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] if the OS watcher cannot be initialized
    /// (e.g. inotify limits exhausted).
    pub fn new() -> Result<(Self, mpsc::UnboundedReceiver<String>), ServiceError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(event) => event,
                Err(err) => {
                    tracing::warn!(%err, "filesystem watch event error");
                    return;
                }
            };
            // Only content-affecting kinds: create, data/name modify, remove.
            // Access events (reads, close-no-write) are pure noise for a scan.
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for path in event.paths {
                let path = path.to_string_lossy().into_owned();
                if should_ignore_path(&path) {
                    continue;
                }
                // Send fails only when the receiver is gone (shutdown) — drop.
                let _ = tx.send(path);
            }
        })
        .map_err(|e| ServiceError::backend(format!("failed to create filesystem watcher: {e}")))?;
        Ok((
            Self {
                inner: Mutex::new(watcher),
                watched: Mutex::new(HashSet::new()),
            },
            rx,
        ))
    }
}

#[async_trait]
impl FileSystemWatcher for NotifyFileSystemWatcher {
    async fn watch(&self, path: &str) -> Result<(), ServiceError> {
        self.inner
            .lock()
            .expect("watcher lock not poisoned")
            .watch(Path::new(path), RecursiveMode::Recursive)
            .map_err(|e| ServiceError::backend(format!("failed to watch {path}: {e}")))?;
        self.watched
            .lock()
            .expect("watched set not poisoned")
            .insert(path.to_owned());
        tracing::info!(path, "watching library root for changes");
        Ok(())
    }

    async fn unwatch(&self, path: &str) -> Result<(), ServiceError> {
        // Unwatch of a root the OS never watched (e.g. it failed to start) is
        // fine to treat as done — the goal state "not watched" already holds.
        if let Err(e) = self
            .inner
            .lock()
            .expect("watcher lock not poisoned")
            .unwatch(Path::new(path))
        {
            tracing::debug!(path, error = %e, "unwatch skipped");
        }
        self.watched
            .lock()
            .expect("watched set not poisoned")
            .remove(path);
        Ok(())
    }

    async fn unwatch_all(&self) -> Result<(), ServiceError> {
        let roots: Vec<String> = self
            .watched
            .lock()
            .expect("watched set not poisoned")
            .iter()
            .cloned()
            .collect();
        for root in roots {
            self.unwatch(&root).await?;
        }
        Ok(())
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
