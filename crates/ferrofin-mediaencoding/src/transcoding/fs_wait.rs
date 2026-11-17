//! Event-driven "wait for ffmpeg to write a file".
//!
//! The transcode runtime used to `sleep(100ms)` between existence checks, so
//! every play-start paid up to a full tick *after* ffmpeg had already produced
//! the file (measured: the stream-copy time-to-first-segment sat on one tick).
//! [`FsWaiter`] parks on an inotify watch of the file's directory instead and
//! wakes the moment anything in it changes — create, write, or the
//! `temp_file` rename that publishes a finished segment.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Notify;

use super::manager::SEGMENT_READY_POLL_INTERVAL_MS;

#[cfg(test)]
thread_local! {
    /// How many watches [`FsWaiter::new`] was asked to arm from this thread (it
    /// arms them on the blocking pool), so a test can tell a wait that never
    /// parked from one that did.
    pub(crate) static ARMED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A current-thread runtime whose blocking pool has one thread, for the tests
/// that hold that thread busy.
#[cfg(test)]
pub(crate) fn one_blocking_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
}

/// Polls `work` with the blocking pool's only thread held busy and asserts it
/// cannot finish, then frees the thread and returns its output. Filesystem
/// work done inline on the runtime worker would have let it finish (gl-1804).
#[cfg(test)]
pub(crate) async fn needs_the_blocking_pool<F: std::future::Future>(work: F) -> F::Output {
    let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let hog = tokio::task::spawn_blocking(move || {
        busy_tx.send(()).ok();
        release_rx.recv().ok();
    });
    busy_rx.await.unwrap();
    tokio::pin!(work);
    let starved = tokio::time::timeout(Duration::from_millis(250), &mut work).await;
    assert!(
        starved.is_err(),
        "finished with the blocking pool starved, so its filesystem work ran inline"
    );
    release_tx.send(()).unwrap();
    hog.await.unwrap();
    work.await
}

/// A wake-up source for "has ffmpeg written `path` yet?" loops.
///
/// Each one costs a thread, an inotify instance, an epoll and an eventfd, so a
/// caller may check once before creating it and skip the wait. Once created,
/// check again **before** the first wait: a change that lands between that
/// check and the wait is then held as a permit, never lost. Callers keep their
/// own `exists()` / `has_exited()` predicate and call [`wait`](Self::wait)
/// between checks.
pub struct FsWaiter {
    _watcher: Option<RecommendedWatcher>,
    notify: Arc<Notify>,
}

impl FsWaiter {
    /// Watches the directory containing `path` (non-recursively). If the watch
    /// cannot be established (inotify limit, missing directory) the waiter
    /// silently degrades to the fallback tick, so the loop still terminates.
    ///
    /// The watch is armed on the blocking pool: arming spawns the watcher's
    /// thread and blocks until that thread has added the watch, which must not
    /// hold a runtime worker (gl-1804).
    #[must_use]
    pub async fn new(path: &Path) -> Self {
        #[cfg(test)]
        ARMED.with(|armed| armed.set(armed.get() + 1));
        let notify = Arc::new(Notify::new());
        let wake = Arc::clone(&notify);
        let dir = path.parent().map(Path::to_path_buf);
        let watcher = tokio::task::spawn_blocking(move || {
            let dir = dir?;
            let mut w = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
                wake.notify_one();
            })
            .ok()?;
            w.watch(&dir, RecursiveMode::NonRecursive).ok()?;
            Some(w)
        })
        .await
        .ok()
        .flatten();
        Self {
            _watcher: watcher,
            notify,
        }
    }

    /// Parks until the directory changes or one fallback tick
    /// ([`SEGMENT_READY_POLL_INTERVAL_MS`]) elapses — the tick is what still
    /// observes a process exit that produced no filesystem event.
    pub async fn wait(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS),
            self.notify.notified(),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn wakes_on_file_creation_well_before_the_fallback_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("out0.ts");
        let waiter = FsWaiter::new(&target).await;
        let t0 = Instant::now();
        let write_to = target.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(write_to, b"seg").unwrap();
        });
        while !target.exists() {
            waiter.wait().await;
        }
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS),
            "event wake took {took:?}, i.e. the fallback tick, not inotify"
        );
    }

    #[tokio::test]
    async fn change_before_wait_is_not_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("out0.ts");
        let waiter = FsWaiter::new(&target).await;
        std::fs::write(&target, b"seg").unwrap();
        // Give the watcher thread a moment to deliver, then the permit must
        // already be stored: wait() returns without burning a tick.
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        waiter.wait().await;
        assert!(t0.elapsed() < Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS));
    }

    /// Arming the watch (a thread spawn, an inotify instance and a cross-thread
    /// reply) runs on the blocking pool, never on the runtime worker that
    /// awaits it (gl-1804).
    #[test]
    fn arming_the_watch_runs_on_the_blocking_pool() {
        one_blocking_thread().block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let target = tmp.path().join("out0.ts");
            needs_the_blocking_pool(FsWaiter::new(&target)).await;
        });
    }

    #[tokio::test]
    async fn missing_directory_degrades_to_the_tick() {
        let waiter = FsWaiter::new(Path::new("/nonexistent-ferrofin-dir/out0.ts")).await;
        let t0 = Instant::now();
        waiter.wait().await;
        assert!(t0.elapsed() >= Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS));
    }
}
