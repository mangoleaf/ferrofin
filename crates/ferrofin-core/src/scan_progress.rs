//! Live item counts and lifecycle for library refresh indicators.
//!
//! Each scan owns a guard. Dropping it removes only that scan, including on
//! cancellation or unwinding. An enclosing scan keeps ownership of a library's
//! indicator while nested refreshes run; their counts are never added twice.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use ferrofin_traits::events::EventManager;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use uuid::Uuid;

/// The work currently being performed by a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    /// Waiting for the scan worker.
    Queued,
    /// Discovering items; the denominator is not known yet.
    Planning,
    /// Processing the planned items.
    Items,
    /// All items processed; pruning and closing passes are still running.
    Finalizing,
}

/// A consistent view of the work for one library in one scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LibraryScanProgress {
    /// Owning scan generation.
    pub scan_id: u64,
    /// The library's collection-folder id.
    pub library_id: Uuid,
    /// Number of items already handled, including unchanged items.
    pub completed: usize,
    /// Number of planned items; unknown while planning.
    pub total: Option<usize>,
    /// Current work phase.
    pub phase: ScanPhase,
}

impl LibraryScanProgress {
    /// Item completion percentage. An empty or unknown plan starts at zero.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn percent(self) -> f64 {
        match self.total {
            Some(total) if total > 0 => 100.0 * self.completed as f64 / total as f64,
            _ if self.phase == ScanPhase::Finalizing => 100.0,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    runs: BTreeMap<u64, BTreeMap<Uuid, LibraryScanProgress>>,
}

/// Shared refresh state, injected into the scanner and virtual-folder reader.
#[derive(Clone, Default)]
pub struct ScanProgressTracker {
    state: Arc<Mutex<State>>,
    reports: Arc<OnceLock<mpsc::UnboundedSender<Report>>>,
    queued: Arc<OnceLock<QueueReader>>,
}

type QueueReader = Arc<dyn Fn(Uuid, &[String]) -> bool + Send + Sync>;

impl std::fmt::Debug for ScanProgressTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanProgressTracker")
            .field("active", &self.libraries())
            .finish_non_exhaustive()
    }
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ScanProgressTracker {
    /// Connects the worker queue without owning it or creating a reference cycle.
    pub(crate) fn set_queue_reader(&self, reader: QueueReader) {
        let _ = self.queued.set(reader);
    }

    /// Whether work for this library is waiting in the worker queue.
    #[must_use]
    pub fn is_queued(&self, library: Uuid, locations: &[String]) -> bool {
        self.queued
            .get()
            .is_some_and(|reader| reader(library, locations))
    }

    /// Starts ordered event delivery. The timer is parked while no scans are active.
    /// Clones share one reporter. The host calls `shutdown` after stopping scans;
    /// dropping the last tracker also closes delivery when no consumer owns it.
    pub fn set_events(&self, events: Arc<dyn EventManager>) {
        self.reports.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(
                report_loop(Arc::downgrade(&self.state), rx, events)
                    .instrument(tracing::info_span!(parent: None, "scan_progress_reporter")),
            );
            tx
        });
    }

    /// Flushes captured transitions and stops the reporter after scan workers stop.
    /// Explicit shutdown also handles event consumers that own a tracker clone.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(Report {
            updates: Vec::new(),
            delivered: Some(tx),
            stop: true,
        });
        let _ = rx.await;
    }

    fn send(&self, report: Report) {
        if let Some(tx) = self.reports.get() {
            let _ = tx.send(report);
        }
    }

    /// Registers a scan before its filesystem walk starts.
    #[must_use]
    pub fn begin(&self, libraries: impl IntoIterator<Item = Uuid>) -> ScanProgressRun {
        let run = self.queued(libraries);
        run.activate();
        run
    }

    /// Registers work waiting for the scan worker.
    #[must_use]
    pub fn queued(&self, libraries: impl IntoIterator<Item = Uuid>) -> ScanProgressRun {
        let mut state = lock(&self.state);
        state.next_id += 1;
        let id = state.next_id;
        let libraries = libraries
            .into_iter()
            .map(|library_id| {
                (
                    library_id,
                    LibraryScanProgress {
                        scan_id: id,
                        library_id,
                        completed: 0,
                        total: None,
                        phase: ScanPhase::Queued,
                    },
                )
            })
            .collect();
        state.runs.insert(id, libraries);
        ScanProgressRun {
            tracker: self.clone(),
            id,
            finished: false,
        }
    }

    /// Reads the oldest active scan for a library, so nested work cannot reset it.
    #[must_use]
    pub fn library(&self, library_id: Uuid) -> Option<LibraryScanProgress> {
        visible(&lock(&self.state)).get(&library_id).copied()
    }

    /// Reads the visible progress for every active library in one snapshot.
    #[must_use]
    pub fn libraries(&self) -> Vec<LibraryScanProgress> {
        visible(&lock(&self.state)).into_values().collect()
    }
}

/// Owns one scan's progress. Removal is guaranteed on every exit path.
#[derive(Debug)]
pub struct ScanProgressRun {
    tracker: ScanProgressTracker,
    id: u64,
    finished: bool,
}

impl ScanProgressRun {
    /// Marks queued work active before planning begins.
    pub fn activate(&self) {
        let mut state = lock(&self.tracker.state);
        if let Some(run) = state.runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.phase = ScanPhase::Planning;
            }
        }
        self.tracker.send(Report::active(&state));
    }

    /// Waits for the start event to be published before processing any items.
    pub async fn started(&self) {
        let (tx, rx) = oneshot::channel();
        self.tracker.send(Report {
            updates: Vec::new(),
            delivered: Some(tx),
            stop: false,
        });
        let _ = rx.await;
    }

    /// Publishes terminal state and waits for its delivery to the event seam.
    pub async fn finish(mut self, completed: bool) {
        let (tx, rx) = oneshot::channel();
        self.remove(completed, Some(tx));
        let _ = rx.await;
    }

    fn remove(&mut self, completed: bool, delivered: Option<oneshot::Sender<()>>) {
        self.finished = true;
        let mut state = lock(&self.tracker.state);
        let before = visible(&state);
        let removed = state.runs.remove(&self.id).unwrap_or_default();
        let after = visible(&state);
        let updates = removed
            .iter()
            .filter_map(|(id, progress)| {
                // A nested scan never clears the enclosing scan's indicator.
                if before.get(id).is_none_or(|p| p.scan_id != self.id) {
                    return None;
                }
                Some(after.get(id).copied().map_or_else(
                    || Update {
                        progress: *progress,
                        percent: if completed { 100.0 } else { 0.0 },
                        status: "Idle",
                    },
                    Update::active,
                ))
            })
            .collect();
        self.tracker.send(Report {
            updates,
            delivered,
            stop: false,
        });
    }

    /// Sets the denominator once planning finishes; libraries with no items remain.
    pub fn planned(&self, totals: impl IntoIterator<Item = (Uuid, usize)>) {
        let mut state = lock(&self.tracker.state);
        if let Some(run) = state.runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.total = Some(0);
                progress.phase = ScanPhase::Items;
            }
            for (id, total) in totals {
                if let Some(progress) = run.get_mut(&id) {
                    progress.total = Some(total);
                }
            }
        }
    }

    /// Records an item only after the scanner has handled it.
    pub fn advance(&self, library: Uuid) {
        let mut state = lock(&self.tracker.state);
        if let Some(progress) = state
            .runs
            .get_mut(&self.id)
            .and_then(|run| run.get_mut(&library))
        {
            progress.completed = progress
                .completed
                .saturating_add(1)
                .min(progress.total.unwrap_or(0));
        }
    }

    /// Keeps the library active while the closing passes run.
    pub fn finalizing(&self) {
        if let Some(run) = lock(&self.tracker.state).runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.phase = ScanPhase::Finalizing;
            }
        }
    }
}

impl Drop for ScanProgressRun {
    fn drop(&mut self) {
        if !self.finished {
            self.remove(false, None);
        }
    }
}

fn visible(state: &State) -> BTreeMap<Uuid, LibraryScanProgress> {
    let mut libraries = BTreeMap::new();
    for run in state.runs.values() {
        for (&id, progress) in run {
            let current = libraries.entry(id).or_insert(*progress);
            if current.phase == ScanPhase::Queued && progress.phase != ScanPhase::Queued {
                *current = *progress;
            }
        }
    }
    libraries
}

#[derive(Debug)]
struct Update {
    progress: LibraryScanProgress,
    percent: f64,
    status: &'static str,
}

impl Update {
    fn active(progress: LibraryScanProgress) -> Self {
        Self {
            progress,
            percent: progress.percent(),
            status: if progress.phase == ScanPhase::Queued {
                "Queued"
            } else {
                "Active"
            },
        }
    }
}

#[derive(Debug)]
struct Report {
    stop: bool,
    updates: Vec<Update>,
    delivered: Option<oneshot::Sender<()>>,
}

impl Report {
    fn active(state: &State) -> Self {
        Self {
            updates: visible(state).into_values().map(Update::active).collect(),
            delivered: None,
            stop: false,
        }
    }
}

/// Lifecycle reports and periodic samples share one ordered delivery loop.
/// Drain lifecycle changes under the same lock used to capture a tick, so a
/// sample cannot overtake its start event or follow a newer terminal event.
async fn report_loop(
    state: std::sync::Weak<Mutex<State>>,
    mut rx: mpsc::UnboundedReceiver<Report>,
    events: Arc<dyn EventManager>,
) {
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer.tick().await;
    let mut active = false;
    loop {
        let mut batch = Vec::new();
        let tick = tokio::select! {
            biased;
            report = rx.recv() => {
                let Some(report) = report else { break };
                batch.push(report);
                false
            }
            _ = timer.tick(), if active => true,
        };
        if let Some(state) = state.upgrade() {
            let state = lock(&state);
            while let Ok(report) = rx.try_recv() {
                batch.push(report);
            }
            let was_active = active;
            active = !state.runs.is_empty();
            if !was_active && active {
                timer.reset();
            }
            if tick && active {
                batch.push(Report::active(&state));
            }
        } else {
            // Guard cleanup can enqueue terminal state just before the final
            // tracker drops. Those captured reports need no live counters.
            active = false;
        }
        for report in batch {
            for update in report.updates {
                let payload = serde_json::json!({
                    "ItemId": update.progress.library_id.simple().to_string(),
                    "Progress": format!("{:.2}", update.percent),
                    "RefreshStatus": update.status,
                })
                .to_string();
                // Keep every timer sample, including stalled counts, available
                // for debugging without adding noise to normal scan logs.
                tracing::debug!(
                    library = %update.progress.library_id,
                    scan_id = update.progress.scan_id,
                    completed = update.progress.completed,
                    total = ?update.progress.total,
                    phase = ?update.progress.phase,
                    progress = update.percent,
                    refresh_status = update.status,
                    "publishing library scan progress"
                );
                if let Err(error) = events.publish("RefreshProgress", &payload).await {
                    tracing::warn!(library = %update.progress.library_id, %error, "scan progress publication failed");
                }
            }
            if let Some(delivered) = report.delivered {
                let _ = delivered.send(());
            }
            if report.stop {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn capture() -> (ScanProgressTracker, Arc<Mutex<Vec<serde_json::Value>>>) {
        let events = Arc::new(crate::event_manager::FerrofinEventManager::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        events.subscribe(
            "RefreshProgress",
            Arc::new(move |payload| {
                lock(&sink).push(serde_json::from_str(payload).unwrap());
                crate::event_manager::consumer_done()
            }),
        );
        let tracker = ScanProgressTracker::default();
        tracker.set_events(events);
        (tracker, seen)
    }

    #[tokio::test(start_paused = true)]
    async fn abort_cleans_state_and_a_new_scan_cannot_receive_stale_ticks() {
        let (tracker, seen) = capture();
        let library = Uuid::new_v4();
        let run = tracker.begin([library]);
        run.started().await;
        let worker = tokio::spawn(async move {
            let _run = run;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(tracker.libraries().is_empty());
        let next = tracker.begin([library]);
        next.started().await;
        let states: Vec<_> = lock(&seen)
            .iter()
            .map(|v| v["RefreshStatus"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(states, ["Active", "Idle", "Active"]);
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            lock(&seen).len(),
            4,
            "missed deadlines do not cause catch-up bursts"
        );
        next.finish(true).await;
        assert_eq!(lock(&seen).last().unwrap()["Progress"], "100.00");
        let count = lock(&seen).len();
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(lock(&seen).len(), count);
    }

    #[tokio::test(start_paused = true)]
    async fn nested_finish_keeps_parent_active_and_empty_scan_finishes_without_a_tick() {
        let (tracker, seen) = capture();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let parent = tracker.begin([a, b]);
        parent.started().await;
        parent.planned([(a, 3)]);
        parent.advance(a);
        let nested = tracker.begin([a]);
        nested.started().await;
        nested.planned([(a, 1)]);
        nested.advance(a);
        nested.finish(true).await;
        assert!(lock(&seen).iter().all(|v| v["RefreshStatus"] == "Active"));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        {
            let values = lock(&seen);
            let a_id = a.simple().to_string();
            assert_eq!(
                values.iter().rev().find(|v| v["ItemId"] == a_id).unwrap()["Progress"],
                "33.33"
            );
        }
        parent.finalizing();
        parent.finish(true).await;
        assert!(tracker.libraries().is_empty());
        let empty = tracker.begin([b]);
        empty.planned([]);
        empty.finalizing();
        empty.finish(true).await;
        let values = lock(&seen);
        assert_eq!(values[values.len() - 2]["Progress"], "0.00");
        assert_eq!(values.last().unwrap()["Progress"], "100.00");
        assert_eq!(values.last().unwrap()["RefreshStatus"], "Idle");
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_shutdown_releases_reporter_with_a_consumer_owning_the_tracker() {
        let events = Arc::new(crate::event_manager::FerrofinEventManager::new());
        let weak = Arc::downgrade(&events);
        let tracker = ScanProgressTracker::default();
        let consumer_tracker = tracker.clone();
        events.subscribe(
            "RefreshProgress",
            Arc::new(move |_| {
                let _ = consumer_tracker.libraries();
                crate::event_manager::consumer_done()
            }),
        );
        tracker.set_events(events);
        let run = tracker.begin([Uuid::new_v4()]);
        run.started().await;
        drop(run);
        tracker.shutdown().await;
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_none());
        // Shutdown is idempotent even when another holder calls it again.
        tracker.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_last_tracker_releases_the_reporter() {
        let events = Arc::new(crate::event_manager::FerrofinEventManager::new());
        let weak = Arc::downgrade(&events);
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let sink = seen.clone();
        events.subscribe(
            "RefreshProgress",
            Arc::new(move |payload| {
                lock(&sink).push(serde_json::from_str(payload).unwrap());
                crate::event_manager::consumer_done()
            }),
        );
        let tracker = ScanProgressTracker::default();
        tracker.set_events(events);
        let run = tracker.begin([Uuid::new_v4()]);
        run.started().await;
        drop(run);
        drop(tracker);
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(lock(&seen).last().unwrap()["RefreshStatus"], "Idle");
        assert!(
            weak.upgrade().is_none(),
            "reporter does not retain the event manager on shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ticks_publish_counts_while_work_is_stalled_and_finish_is_immediate() {
        let events = Arc::new(crate::event_manager::FerrofinEventManager::new());
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let sink = Arc::clone(&seen);
        events.subscribe(
            "RefreshProgress",
            Arc::new(move |payload| {
                lock(&sink).push(serde_json::from_str(payload).unwrap());
                crate::event_manager::consumer_done()
            }),
        );
        let tracker = ScanProgressTracker::default();
        tracker.set_events(events);
        let library = Uuid::new_v4();
        let run = tracker.begin([library]);
        run.started().await;
        assert_eq!(lock(&seen)[0]["Progress"], "0.00");
        assert_eq!(lock(&seen)[0]["RefreshStatus"], "Active");
        run.planned([(library, 300)]);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(lock(&seen).last().unwrap()["Progress"], "0.00");
        for _ in 0..100 {
            run.advance(library);
        }
        let count = lock(&seen).len();
        tokio::task::yield_now().await;
        assert_eq!(lock(&seen).len(), count, "item 100 does not publish");
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(lock(&seen).last().unwrap()["Progress"], "33.33");
        run.finish(false).await;
        assert_eq!(lock(&seen).last().unwrap()["RefreshStatus"], "Idle");
        assert_eq!(lock(&seen).last().unwrap()["Progress"], "0.00");
        let count = lock(&seen).len();
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(lock(&seen).len(), count, "no ticks after stopping");
    }

    #[test]
    fn counts_and_cleanup_follow_the_owning_scan() {
        let tracker = ScanProgressTracker::default();
        let library = Uuid::new_v4();
        let run = tracker.begin([library]);
        assert_eq!(tracker.library(library).unwrap().percent(), 0.0);
        run.planned([(library, 3)]);
        run.advance(library);
        assert!((tracker.library(library).unwrap().percent() - 100.0 / 3.0).abs() < f64::EPSILON);
        let nested = tracker.begin([library]);
        nested.planned([(library, 1)]);
        nested.advance(library);
        assert_eq!(tracker.library(library).unwrap().completed, 1);
        drop(nested);
        assert_eq!(tracker.libraries().len(), 1);
        run.advance(library);
        run.advance(library);
        run.finalizing();
        assert_eq!(
            tracker.library(library).unwrap().phase,
            ScanPhase::Finalizing
        );
        drop(run);
        assert!(tracker.libraries().is_empty());
        let next = tracker.begin([library]);
        assert_eq!(tracker.library(library).unwrap().completed, 0);
        drop(next);
    }

    #[test]
    fn empty_libraries_and_overlapping_scans_have_independent_lifetimes() {
        let tracker = ScanProgressTracker::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let first = tracker.begin([a]);
        first.planned([]);
        first.finalizing();
        assert_eq!(tracker.library(a).unwrap().percent(), 100.0);
        let second = tracker.begin([a, b]);
        drop(first);
        assert_eq!(tracker.library(a).unwrap().phase, ScanPhase::Planning);
        assert_eq!(tracker.libraries().len(), 2);
        drop(second);
        assert!(tracker.libraries().is_empty());
    }
}
