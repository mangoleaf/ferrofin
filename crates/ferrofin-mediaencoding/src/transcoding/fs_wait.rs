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

/// A wake-up source for "has ffmpeg written `path` yet?" loops.
///
/// Create it **before** the first existence check: a change that lands between
/// the check and the wait is then held as a permit, never lost. Callers keep
/// their own `exists()` / `has_exited()` predicate and call [`wait`](Self::wait)
/// between checks.
pub struct FsWaiter {
    _watcher: Option<RecommendedWatcher>,
    notify: Arc<Notify>,
}

impl FsWaiter {
    /// Watches the directory containing `path` (non-recursively). If the watch
    /// cannot be established (inotify limit, missing directory) the waiter
    /// silently degrades to the fallback tick, so the loop still terminates.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        let notify = Arc::new(Notify::new());
        let wake = Arc::clone(&notify);
        let watcher = path.parent().and_then(|dir| {
            let mut w = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
                wake.notify_one();
            })
            .ok()?;
            w.watch(dir, RecursiveMode::NonRecursive).ok()?;
            Some(w)
        });
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
        let waiter = FsWaiter::new(&target);
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
        let waiter = FsWaiter::new(&target);
        std::fs::write(&target, b"seg").unwrap();
        // Give the watcher thread a moment to deliver, then the permit must
        // already be stored: wait() returns without burning a tick.
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        waiter.wait().await;
        assert!(t0.elapsed() < Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS));
    }

    #[tokio::test]
    async fn missing_directory_degrades_to_the_tick() {
        let waiter = FsWaiter::new(Path::new("/nonexistent-ferrofin-dir/out0.ts"));
        let t0 = Instant::now();
        waiter.wait().await;
        assert!(t0.elapsed() >= Duration::from_millis(SEGMENT_READY_POLL_INTERVAL_MS));
    }
}
