//! [`FerrofinLibraryManager`] — the concrete [`LibraryManager`] orchestrator.
//!
//! Port of the object-safe, domain-tree-free subset of
//! `Emby.Server.Implementations.Library.LibraryManager`. The C# manager owns the
//! whole `BaseItem` OOP tree (resolvers, path/sort/named-view logic); those parts
//! live as free functions in [`crate::resolvers`] and [`crate::kinds`]. What
//! remains here is pure orchestration over the persistence seam: every query,
//! count, people, genre/studio/artist, and mutate call delegates to an injected
//! repository trait ([`ItemRepository`], [`ItemCountService`],
//! [`ItemPersistenceService`], [`PeopleRepository`]) rather than touching the
//! pool directly, so the manager stays composition-root agnostic.
//!
//! Port simplifications, all faithful to the trait surface:
//! - `create_items`/`update_items` collapse to `save_items` on the persistence
//!   service (the row upsert is idempotent); the `parent_id` argument is accepted
//!   for API parity but the parent linkage is already carried on each row's
//!   `ParentId` column.
//! - `delete_item` deletes the item's row and its children's through the
//!   persistence service. TODO(parity, open work item): upstream deletes the
//!   media files (`DeleteFileLocation = true`); not ported yet, it waits on
//!   scanner parity. See `brain/plans/PLAN_ITEM_FILE_DELETION.md`.
//! - The `IProgress` scan plumbing is dropped. A scan runs on the scan
//!   queue's own worker task and is cancelled cooperatively, between two
//!   items, through a [`ScanCancel`] (the `CancellationToken`'s role).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tracing::Instrument as _;

use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::{ImageType, MediaStreamType, MetadataField};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    LibraryManager, ScanTarget, ScanTrigger, image_type_allows_multiple,
};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::persistence::{
    ItemCountService, ItemPersistenceService, ItemRepository, ItemWithCounts, PeopleRepository,
};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

use crate::library_scan::{LaneRefresh, PriorityLane, ScanCancel, ScanPasses, ScanRun};
use ferrofin_traits::library::ScanProgressSink;

/// The placeholder item row seeded by the initial migration, which every real
/// item query excludes (the `Uuid` form of
/// [`PLACEHOLDER_ID`](crate::translate_query::PLACEHOLDER_ID),
/// `00000000-0000-0000-0000-000000000001`).
const PLACEHOLDER_ITEM_ID: Uuid = Uuid::from_u128(1);

/// The concrete library manager.
///
/// Holds cheaply-cloneable `Arc<dyn _>` handles to the four persistence traits it
/// orchestrates. All are injected at the composition root so the same concrete
/// repositories back both this manager and any other consumer.
#[derive(Clone)]
pub struct FerrofinLibraryManager {
    items: Arc<dyn ItemRepository>,
    visibility: Option<Arc<crate::item_visibility::ItemVisibility>>,
    virtual_folders: Option<Arc<dyn ferrofin_traits::library::VirtualFolderManager>>,
    counts: Arc<dyn ItemCountService>,
    persistence: Arc<dyn ItemPersistenceService>,
    people: Arc<dyn PeopleRepository>,
    /// The filesystem scanner, set by the composition root. When present,
    /// `queue_library_scan` runs it; `None` (unit tests) keeps it a no-op.
    scanner: Option<Arc<dyn ScanRunner>>,
    /// The single-scan claim and the scans queued behind it. One scan runs
    /// at a time; a request arriving meanwhile queues (coalesced by
    /// [`ScanQueue::enqueue`]) instead of starting a second one — the
    /// library monitor fans a webhook batch into one report per path, and
    /// `/Library/Refresh` can be double-clicked.
    scan_queue: Arc<Mutex<ScanQueue>>,
    /// Bumped whenever a queued scan finishes or the claim is released, so
    /// the callers waiting on a queued scan re-check it.
    scan_progress: Arc<tokio::sync::watch::Sender<u64>>,
    scan_tracker: Option<crate::scan_progress::ScanProgressTracker>,
    /// Chapter rows, for serving chapter thumbnails. Set by the composition
    /// root; `None` (unit tests) means an item has no chapter images. The
    /// repository (not the `ChapterManager`) is held because the manager is
    /// built on top of this manager — taking it here would be a cycle.
    chapters: Option<Arc<dyn ferrofin_traits::persistence::ChapterRepository>>,
    /// The `UserRootFolder` provisioner (`GetUserRootFolder()`), set by the
    /// composition root. `None` (unit tests) falls back to resolving an
    /// already-persisted root row.
    user_root: Option<crate::user_root_folder::UserRootFolderStore>,
    /// The `Year` by-name item provisioner (`GetYear`), set by the composition
    /// root. `None` (unit tests) resolves only persisted `Year` rows.
    years: Option<crate::years::YearStore>,
    /// The `Genre`/`MusicGenre`/`Studio`/`MusicArtist` by-name provisioner
    /// (`CreateItemByName<T>`), set by the composition root. `None` (unit
    /// tests) resolves only persisted rows.
    by_name: Option<crate::by_name_store::ByNameStore>,
    /// The debounced `LibraryChanged` push (`LibraryChangedNotifier`), set by
    /// the composition root. `None` (unit tests) means item writes announce
    /// nothing, which is what every test that does not assert on the push wants.
    changed: Option<Arc<crate::library_changed_notifier::LibraryChangedNotifier>>,
}

/// One scan request: what it covers, the options its items refresh with,
/// and why it was made.
#[derive(Debug, Clone, PartialEq)]
struct ScanRequest {
    /// The libraries, library or paths it covers.
    scope: ScanTarget,
    /// The refresh options every item in scope refreshes with.
    options: MetadataRefreshOptions,
    /// The options for the ancestors a path-scoped scan carries along for
    /// context: the defaults for the library monitor's refresh, `None`/`None`
    /// for a folder refresh (upstream validates a folder's subtree and never
    /// refreshes the folders above it).
    ancestors: MetadataRefreshOptions,
    /// Why it runs: its `library_scan` span's `trigger`, and the label of the
    /// scan metrics it records.
    trigger: ScanTrigger,
    /// Its closing passes: the whole library's for library validation, only
    /// the touched items' for a refresh over the API.
    passes: ScanPasses,
    /// It waits in the priority lane: an item refresh a caller is waiting
    /// for, which runs ahead of every queued scan and inside a running one.
    priority: bool,
}

impl ScanRequest {
    /// A request refreshing with the `MetadataRefreshOptions` constructor
    /// defaults — every trigger but a refresh over the API.
    fn defaults(trigger: ScanTrigger, scope: ScanTarget) -> Self {
        Self {
            scope,
            options: MetadataRefreshOptions::default(),
            ancestors: MetadataRefreshOptions::default(),
            trigger,
            passes: ScanPasses::Library,
            priority: false,
        }
    }

    /// A folder's `POST /Items/{itemId}/Refresh`.
    fn folder_refresh(scope: ScanTarget, options: MetadataRefreshOptions) -> Self {
        Self {
            scope,
            options,
            ancestors: MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::None,
                image_refresh_mode: MetadataRefreshMode::None,
                ..MetadataRefreshOptions::default()
            },
            // A request over the API (`LOGGING.md`'s trigger vocabulary).
            trigger: ScanTrigger::Api,
            // `RefreshItem` runs no post-scan task.
            passes: ScanPasses::Touched,
            priority: false,
        }
    }

    /// An item refresh a caller waits on or expects promptly — a file
    /// item's `POST /Items/{itemId}/Refresh`, an Identify's Apply, the
    /// refresh after a subtitle or lyric change: upstream's provider refresh
    /// queue, which runs alongside library validation, not behind it.
    fn item_refresh(scope: ScanTarget, options: MetadataRefreshOptions) -> Self {
        Self {
            priority: true,
            ..Self::folder_refresh(scope, options)
        }
    }

    /// The library monitor's settled change batch — the disk watcher's or
    /// the *arr webhooks' (`trigger`): `FileRefresher`'s `ChangedExternally`
    /// refresh (`FileRefresher.cs:135-208`, `BaseItem.cs:2275-2278`), the
    /// default options for the item each changed path refreshes, nothing for
    /// the folders above it (a refresh never touches them), and no post-scan
    /// task (`ProviderManager.RefreshItem` runs none).
    fn changed(paths: Vec<String>, trigger: ScanTrigger) -> Self {
        Self {
            scope: ScanTarget::Changed(paths),
            options: MetadataRefreshOptions::default(),
            ancestors: MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::None,
                image_refresh_mode: MetadataRefreshMode::None,
                ..MetadataRefreshOptions::default()
            },
            trigger,
            passes: ScanPasses::Touched,
            priority: false,
        }
    }

    /// Whether this queued request does everything `other` asks for, so
    /// `other` may join it: the same options (a replace never widens, a
    /// default never swallows one) in the same lane, over a scope that
    /// covers `other`'s — a full scan any, a library or artist scan an
    /// identical one — with closing passes that include `other`'s (the
    /// library's include the touched items'). A scan narrower than a
    /// library must also treat the folders it carries for context alike; a
    /// library's validation refreshes every item with its options, so a
    /// full scan covers a path-scoped request's context folders too.
    fn covers(&self, other: &Self) -> bool {
        let scope = match (&self.scope, &other.scope) {
            (ScanTarget::All, _) => true,
            (ScanTarget::Library(queued), ScanTarget::Library(new)) => queued == new,
            (queued @ ScanTarget::Artist { .. }, new @ ScanTarget::Artist { .. }) => queued == new,
            _ => false,
        };
        let whole_libraries = matches!(self.scope, ScanTarget::All | ScanTarget::Library(_));
        scope
            && self.options == other.options
            && self.priority == other.priority
            && (self.passes == other.passes || self.passes == ScanPasses::Library)
            && (whole_libraries || self.ancestors == other.ancestors)
    }

    /// Whether two requests refresh alike, so that one may cover the other.
    fn refreshes_like(&self, other: &Self) -> bool {
        self.options == other.options
            && self.ancestors == other.ancestors
            && self.passes == other.passes
            && self.priority == other.priority
    }
}

/// Who waits for a scan, queued or running.
#[derive(Debug, Default)]
struct Waiters {
    /// The tickets of the callers waiting for it.
    tickets: Vec<u64>,
    /// The requests nobody waits for (the fire-and-forget entry points: the
    /// API's refreshes, the watcher) — never withdrawn.
    detached: usize,
}

impl Waiters {
    /// Adds a request: its caller's ticket, or a detached one.
    fn add(&mut self, ticket: Option<u64>) {
        match ticket {
            Some(ticket) => self.tickets.push(ticket),
            None => self.detached += 1,
        }
    }

    /// Takes over another scan's waiters (the scan covering it runs for
    /// them).
    fn absorb(&mut self, other: Self) {
        self.tickets.extend(other.tickets);
        self.detached += other.detached;
    }

    /// Nobody wants the scan any more.
    fn is_empty(&self) -> bool {
        self.tickets.is_empty() && self.detached == 0
    }
}

/// A scan waiting its turn.
#[derive(Debug)]
struct PendingScan {
    /// What it will run.
    request: ScanRequest,
    /// Who waits for it.
    waiters: Waiters,
}

/// A priority-lane refresh a running scan is serving.
#[derive(Debug)]
struct ServingRefresh {
    /// What it runs (back to the lane if the worker dies under it).
    request: ScanRequest,
    /// Who waits for it.
    waiters: Waiters,
    /// Stops it.
    cancel: ScanCancel,
}

/// The scan being run.
#[derive(Debug)]
struct RunningScan {
    /// Who waits for it.
    waiters: Waiters,
    /// Stops it between two items once nobody wants it any more.
    cancel: ScanCancel,
}

/// How a scan a caller waited for ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanResult {
    /// It ran to the end.
    Completed,
    /// It was cancelled and stopped early (or never ran: the server is
    /// shutting down).
    Stopped,
    /// It failed, or panicked; the message says why.
    Failed(String),
}

impl ScanResult {
    /// The `result` label of the scan metrics.
    fn metric_end(&self) -> crate::scan_metrics::ScanEnd {
        match self {
            Self::Completed => crate::scan_metrics::ScanEnd::Completed,
            Self::Stopped => crate::scan_metrics::ScanEnd::Stopped,
            Self::Failed(_) => crate::scan_metrics::ScanEnd::Failed,
        }
    }
}

/// The scan queue: at most one scan runs at a time, on the queue's own
/// worker task, and the requests that arrive meanwhile wait their turn.
#[derive(Default)]
struct ScanQueue {
    /// A worker task is draining the queue.
    worker: bool,
    /// The host is shutting down: no scan is queued any more.
    closed: bool,
    /// The scan the worker is running.
    running: Option<RunningScan>,
    /// The requests waiting their turn, in the order they run.
    pending: VecDeque<PendingScan>,
    /// The priority lane: item refreshes that run ahead of `pending` and,
    /// while a scan runs, inside it ([`PriorityLane`]).
    lane: VecDeque<PendingScan>,
    /// The lane refreshes a running scan is serving, by key.
    serving: HashMap<u64, ServingRefresh>,
    /// The key of the next lane refresh served.
    next_lane_key: u64,
    /// The next ticket to hand a waiting caller.
    next_ticket: u64,
    /// How the scans of the tickets not yet collected by their callers
    /// ended.
    finished: HashMap<u64, ScanResult>,
    /// Where a waiting caller wants its scan's progress reported.
    sinks: HashMap<u64, ScanProgressSink>,
    /// The worker's task, for a test to simulate it dying.
    #[cfg(test)]
    worker_task: Option<tokio::task::AbortHandle>,
}

impl std::fmt::Debug for ScanQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanQueue")
            .field("worker", &self.worker)
            .field("closed", &self.closed)
            .field("running", &self.running)
            .field("pending", &self.pending)
            .field("lane", &self.lane)
            .field("serving", &self.serving)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ScanQueue {
    /// Queues `request` and returns the ticket its caller waits on (`None`
    /// for a request nobody waits for). A request coalesces only with a
    /// queued one that does everything it asks ([`ScanRequest::covers`]),
    /// or that refreshes alike ([`ScanRequest::refreshes_like`]) when their
    /// paths union, so no request ever runs with another's options — a
    /// "Replace all metadata" on one library never widens to every library,
    /// and a default scan never swallows it:
    ///
    /// - a full scan covers a queued library, path, changed-path or artist
    ///   scan: it takes the place of the earliest one and the waiters of all
    ///   of them;
    /// - a library, path, changed-path or artist scan joins a queued full
    ///   scan (which will see its files), and a library or artist scan an
    ///   identical queued one;
    /// - folder path scans union their paths, and so do the library
    ///   monitor's changed-path scans — never with each other. A watcher
    ///   batch and a webhook batch join like any two (the trigger is a label
    ///   only); the joined scan keeps the queued request's trigger;
    /// - a library scan never becomes a full one, and path scans never join
    ///   a library scan: a watcher or webhook report queued behind a
    ///   library scan runs after it, scoped (by then its item is current,
    ///   so it is nearly free).
    ///
    /// Everything else queues after what is already there. The running scan
    /// is never joined: it may already be past the files a new request is
    /// about, so a report that arrives while a scan runs queues a scoped
    /// rerun behind it.
    fn enqueue(&mut self, request: ScanRequest, waiting: bool) -> Option<u64> {
        let ticket = waiting.then(|| {
            let ticket = self.next_ticket;
            self.next_ticket += 1;
            ticket
        });
        // The priority lane keeps its own order; an identical refresh
        // already waiting there runs once for both.
        if request.priority {
            if let Some(pending) = self.lane.iter_mut().find(|p| p.request == request) {
                pending.waiters.add(ticket);
            } else {
                let mut waiters = Waiters::default();
                waiters.add(ticket);
                self.lane.push_back(PendingScan { request, waiters });
            }
            return ticket;
        }
        // A queued scan that already covers this request: a full scan covers
        // anything, a library or artist scan an identical one.
        if let Some(pending) = self.pending.iter_mut().find(|p| p.request.covers(&request)) {
            pending.waiters.add(ticket);
            return ticket;
        }
        let mut waiters = Waiters::default();
        waiters.add(ticket);
        match request.scope {
            ScanTarget::All => {
                // Covers every queued scan that refreshes alike: it takes the
                // earliest one's place and all their waiters.
                let mut at = None;
                let mut kept = VecDeque::with_capacity(self.pending.len() + 1);
                for pending in self.pending.drain(..) {
                    if request.covers(&pending.request) {
                        at.get_or_insert(kept.len());
                        waiters.absorb(pending.waiters);
                    } else {
                        kept.push_back(pending);
                    }
                }
                let at = at.unwrap_or(kept.len());
                kept.insert(at, PendingScan { request, waiters });
                self.pending = kept;
            }
            ScanTarget::Paths(ref paths) | ScanTarget::Changed(ref paths) => {
                let same_kind = |queued: &ScanTarget| {
                    std::mem::discriminant(queued) == std::mem::discriminant(&request.scope)
                };
                if let Some(pending) = self
                    .pending
                    .iter_mut()
                    .find(|p| p.request.refreshes_like(&request) && same_kind(&p.request.scope))
                {
                    if let ScanTarget::Paths(queued) | ScanTarget::Changed(queued) =
                        &mut pending.request.scope
                    {
                        let mut known: std::collections::HashSet<String> =
                            queued.iter().cloned().collect();
                        for path in paths {
                            if known.insert(path.clone()) {
                                queued.push(path.clone());
                            }
                        }
                    }
                    pending.waiters.absorb(waiters);
                } else {
                    self.pending.push_back(PendingScan { request, waiters });
                }
            }
            ScanTarget::Library(_) | ScanTarget::Artist { .. } | ScanTarget::Items(_) => {
                self.pending.push_back(PendingScan { request, waiters });
            }
        }
        ticket
    }

    /// Withdraws a caller that stopped waiting (it returned, or its task was
    /// cancelled). A queued scan nobody wants any more is dropped — a
    /// cancelled request does not run — and a running one is told to stop
    /// (`CancelIfRunningAndQueue` cancelling the running validation's
    /// token). A scan another caller still waits for, or that a
    /// fire-and-forget request queued, runs on.
    fn withdraw(&mut self, ticket: u64) {
        self.finished.remove(&ticket);
        self.sinks.remove(&ticket);
        for pending in self.pending.iter_mut().chain(self.lane.iter_mut()) {
            pending.waiters.tickets.retain(|t| *t != ticket);
        }
        self.pending.retain(|pending| !pending.waiters.is_empty());
        self.lane.retain(|pending| !pending.waiters.is_empty());
        for serving in self.serving.values_mut() {
            if serving.waiters.tickets.contains(&ticket) {
                serving.waiters.tickets.retain(|t| *t != ticket);
                if serving.waiters.is_empty() {
                    serving.cancel.cancel();
                }
            }
        }
        if let Some(running) = &mut self.running
            && running.waiters.tickets.contains(&ticket)
        {
            running.waiters.tickets.retain(|t| *t != ticket);
            if running.waiters.is_empty() {
                running.cancel.cancel();
            }
        }
    }

    /// Records how the running scan ended, for its waiters.
    fn finish_running(&mut self, result: &ScanResult) {
        if let Some(done) = self.running.take() {
            for ticket in done.waiters.tickets {
                self.sinks.remove(&ticket);
                self.finished.insert(ticket, result.clone());
            }
        }
    }

    /// Settles as failed every lane refresh still marked as being served —
    /// the scan serving it ended without handing it back (it panicked or was
    /// aborted), and nobody else will.
    fn fail_serving(&mut self) {
        let lost: Vec<ServingRefresh> = self.serving.drain().map(|(_, s)| s).collect();
        for refresh in lost {
            refresh.cancel.cancel();
            for ticket in refresh.waiters.tickets {
                self.sinks.remove(&ticket);
                self.finished.insert(
                    ticket,
                    ScanResult::Failed(
                        "the scan serving this item refresh ended before it finished".to_owned(),
                    ),
                );
            }
        }
    }

    /// Whether `ticket`'s request is still waiting its turn.
    fn is_queued(&self, ticket: u64) -> bool {
        self.pending
            .iter()
            .chain(self.lane.iter())
            .any(|p| p.waiters.tickets.contains(&ticket))
    }

    /// Reports `percent` to the running scan's waiting callers.
    fn report(&self, percent: f64) {
        let Some(running) = &self.running else {
            return;
        };
        for ticket in &running.waiters.tickets {
            if let Some(sink) = self.sinks.get(ticket) {
                sink(percent);
            }
        }
    }
}

/// Locks the scan queue, recovering it from a panic elsewhere: every update
/// under the lock is a few field writes, so a poisoned guard still holds a
/// consistent queue.
fn lock_queue(queue: &Mutex<ScanQueue>) -> std::sync::MutexGuard<'_, ScanQueue> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the manager runs a scan on: the [`LibraryScanner`] in production,
/// a controllable stand-in in tests.
///
/// [`LibraryScanner`]: crate::library_scan::LibraryScanner
#[async_trait]
pub(crate) trait ScanRunner: Send + Sync {
    /// Scans `scope` as `run` says.
    async fn run(
        &self,
        scope: &ScanTarget,
        run: ScanRun<'_>,
    ) -> Result<crate::library_scan::ScanOutcome, ServiceError>;
}

#[async_trait]
impl ScanRunner for crate::library_scan::LibraryScanner {
    async fn run(
        &self,
        scope: &ScanTarget,
        run: ScanRun<'_>,
    ) -> Result<crate::library_scan::ScanOutcome, ServiceError> {
        // Boxed: the scan future is ~16 KB — a whole pass over the planner
        // lives in it (`clippy::large_futures`).
        Box::pin(self.scan_target(scope, run)).await
    }
}

/// The scope's kind, for the scan log lines.
fn scope_label(scope: &ScanTarget) -> &'static str {
    match scope {
        ScanTarget::All => "all",
        ScanTarget::Library(_) => "library",
        ScanTarget::Paths(_) => "paths",
        ScanTarget::Changed(_) => "changed",
        ScanTarget::Items(_) => "items",
        ScanTarget::Artist { .. } => "artist",
    }
}

/// The queue's worker: it runs the queued scans one at a time, each on a
/// task of its own, until the queue is empty.
///
/// The scan is never run on — and so never dropped with — a caller's
/// future. Cancelling a caller only withdraws its request
/// ([`ScanQueue::withdraw`]); a running scan nobody wants any more stops
/// cooperatively (between two items, or in a wait before an item writes
/// anything), and only then does the next request run. A scan that fails
/// or panics is contained in its own task: the worker records it for the
/// scan's waiters and goes on with the queue.
struct ScanWorker {
    runner: Arc<dyn ScanRunner>,
    queue: Arc<Mutex<ScanQueue>>,
    progress: Arc<tokio::sync::watch::Sender<u64>>,
}

impl ScanWorker {
    /// Drains the queue, then stands down. Runs inside the `library_scan`
    /// span of the request that started it.
    async fn drain(self) {
        let mut release = WorkerRelease {
            queue: Arc::clone(&self.queue),
            progress: Arc::clone(&self.progress),
            request: None,
            scan_task: None,
            done: false,
        };
        let started = std::time::Instant::now();
        // `ferrofin_library_scan_in_progress` while the queue drains.
        let _in_progress = crate::scan_metrics::ScanInProgress::enter();
        tracing::info!("library scan started");
        let mut total = crate::library_scan::ScanOutcome::default();
        loop {
            let next = {
                let mut queue = lock_queue(&self.queue);
                // The priority lane runs ahead of every queued scan.
                let next = match queue.lane.pop_front() {
                    Some(pending) => Some(pending),
                    None => queue.pending.pop_front(),
                };
                if let Some(pending) = next {
                    let cancel = ScanCancel::new();
                    let waited = !pending.waiters.tickets.is_empty();
                    queue.running = Some(RunningScan {
                        waiters: pending.waiters,
                        cancel: cancel.clone(),
                    });
                    Some((pending.request, cancel, waited))
                } else {
                    // Standing down under the same lock that a new request
                    // checks, so it is never left queued with no worker.
                    queue.worker = false;
                    queue.running = None;
                    None
                }
            };
            let Some((request, cancel, waited)) = next else {
                release.done = true;
                self.progress.send_modify(|n| *n = n.wrapping_add(1));
                break;
            };
            let span = tracing::info_span!(
                "library_scan_pass",
                trigger = request.trigger.as_str(),
                scope = scope_label(&request.scope)
            );
            let begun = std::time::Instant::now();
            let task = self.spawn_scan(&request, &cancel, &span);
            release.request = Some(request.clone());
            release.scan_task = Some(task.abort_handle());
            let (result, pass) = Self::finish(task, &request, span, waited).await;
            crate::scan_metrics::scan_finished(
                request.trigger,
                result.metric_end(),
                pass.as_ref(),
                begun.elapsed(),
            );
            release.request = None;
            release.scan_task = None;
            if let Some(pass) = pass {
                total += pass;
            }
            {
                let mut queue = lock_queue(&self.queue);
                queue.finish_running(&result);
                // A scan that ends normally has handed every refresh it
                // served back; one that panicked has not.
                queue.fail_serving();
            }
            self.progress.send_modify(|n| *n = n.wrapping_add(1));
        }
        let elapsed_ms = started.elapsed().as_millis();
        let (created, updated, unchanged, removed) =
            (total.created, total.updated, total.unchanged, total.removed);
        // A drain that included a stopped pass did not complete.
        if total.stopped {
            tracing::info!(
                created,
                updated,
                unchanged,
                removed,
                elapsed_ms,
                "library scan stopped"
            );
        } else {
            tracing::info!(
                created,
                updated,
                unchanged,
                removed,
                elapsed_ms,
                "library scan complete"
            );
        }
    }

    /// Starts one scan on a task of its own, under `span`, reporting its
    /// progress to the waiting callers.
    fn spawn_scan(
        &self,
        request: &ScanRequest,
        cancel: &ScanCancel,
        span: &tracing::Span,
    ) -> tokio::task::JoinHandle<Result<crate::library_scan::ScanOutcome, ServiceError>> {
        let runner = Arc::clone(&self.runner);
        let queue = Arc::clone(&self.queue);
        let lane = QueueLane {
            queue: Arc::clone(&self.queue),
            progress: Arc::clone(&self.progress),
        };
        let request = request.clone();
        let cancel = cancel.clone();
        tokio::spawn(
            async move {
                let report = move |percent: f64| lock_queue(&queue).report(percent);
                runner
                    .run(
                        &request.scope,
                        ScanRun::new(&request.options, &request.ancestors, &cancel)
                            .with_progress(&report)
                            .with_passes(request.passes)
                            .with_lane(&lane),
                    )
                    .await
            }
            .instrument(span.clone()),
        )
    }

    /// Waits for one scan's task and logs how it ended, inside `span` (only
    /// while polled: the span never stays entered on a parked thread). A
    /// failure a caller waits for (`waited`) is that caller's to report.
    async fn finish(
        task: tokio::task::JoinHandle<Result<crate::library_scan::ScanOutcome, ServiceError>>,
        request: &ScanRequest,
        span: tracing::Span,
        waited: bool,
    ) -> (ScanResult, Option<crate::library_scan::ScanOutcome>) {
        let ended = task.instrument(span.clone()).await;
        let _entered = span.enter();
        match ended {
            Ok(Ok(pass)) if pass.stopped => {
                tracing::info!(
                    created = pass.created,
                    updated = pass.updated,
                    unchanged = pass.unchanged,
                    removed = pass.removed,
                    "library scan stopped before it finished"
                );
                (ScanResult::Stopped, Some(pass))
            }
            Ok(Ok(pass)) => {
                tracing::info!(
                    metadata_mode = ?request.options.metadata_refresh_mode,
                    image_mode = ?request.options.image_refresh_mode,
                    replace_all_metadata = request.options.replace_all_metadata,
                    replace_all_images = request.options.replace_all_images,
                    created = pass.created,
                    updated = pass.updated,
                    unchanged = pass.unchanged,
                    removed = pass.removed,
                    "library scan pass complete"
                );
                (ScanResult::Completed, Some(pass))
            }
            // Logged exactly once, at the outermost layer: here, unless a
            // caller waits for the scan and reports its failure itself (an
            // Identify refresh that ran as the worker's own pass).
            Ok(Err(err)) => {
                if waited {
                    tracing::debug!(%err, "library scan failed; its caller reports it");
                } else {
                    tracing::error!(%err, "library scan failed");
                }
                (ScanResult::Failed(err.to_string()), None)
            }
            // The panic itself is reported by the panic hook.
            Err(err) if err.is_panic() => {
                tracing::error!(%err, "library scan panicked");
                (
                    ScanResult::Failed(format!("library scan panicked: {err}")),
                    None,
                )
            }
            Err(_) => (ScanResult::Stopped, None),
        }
    }
}

/// The scan queue's priority lane, as the running scan serves it.
struct QueueLane {
    queue: Arc<Mutex<ScanQueue>>,
    progress: Arc<tokio::sync::watch::Sender<u64>>,
}

impl PriorityLane for QueueLane {
    fn next(&self) -> Option<LaneRefresh> {
        let mut queue = lock_queue(&self.queue);
        if queue.closed {
            return None;
        }
        let pending = queue.lane.pop_front()?;
        let key = queue.next_lane_key;
        queue.next_lane_key += 1;
        let cancel = ScanCancel::new();
        let refresh = LaneRefresh {
            target: pending.request.scope.clone(),
            options: pending.request.options.clone(),
            ancestors: pending.request.ancestors.clone(),
            passes: pending.request.passes,
            cancel: cancel.clone(),
            key,
        };
        queue.serving.insert(
            key,
            ServingRefresh {
                request: pending.request,
                waiters: pending.waiters,
                cancel,
            },
        );
        Some(refresh)
    }

    fn done(
        &self,
        refresh: LaneRefresh,
        outcome: &Result<crate::library_scan::ScanOutcome, ServiceError>,
    ) {
        let result = match outcome {
            Ok(pass) if pass.stopped => ScanResult::Stopped,
            Ok(pass) => {
                tracing::info!(
                    scope = scope_label(&refresh.target),
                    created = pass.created,
                    updated = pass.updated,
                    unchanged = pass.unchanged,
                    removed = pass.removed,
                    "item refresh served inside the running scan"
                );
                ScanResult::Completed
            }
            Err(err) => ScanResult::Failed(err.to_string()),
        };
        let mut queue = lock_queue(&self.queue);
        let serving = queue.serving.remove(&refresh.key);
        let trigger = serving
            .as_ref()
            .map_or(ScanTrigger::Api, |serving| serving.request.trigger);
        let waited = serving.is_some_and(|serving| {
            let waited = !serving.waiters.tickets.is_empty();
            for ticket in serving.waiters.tickets {
                queue.sinks.remove(&ticket);
                queue.finished.insert(ticket, result.clone());
            }
            waited
        });
        drop(queue);
        crate::scan_metrics::lane_refresh_finished(
            trigger,
            result.metric_end(),
            outcome.as_ref().ok(),
        );
        // Logged once, at the outermost layer: a caller waiting for the
        // refresh (Identify → Apply) reports its failure itself; a queued
        // one's is reported here.
        if let (ScanResult::Failed(err), false) = (&result, waited) {
            tracing::error!(%err, scope = scope_label(&refresh.target), "item refresh failed");
        }
        self.progress.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// Releases the worker's claim if its task is dropped before it drained
/// the queue (the runtime shutting down, or the worker panicking), so the
/// queue never waits on a worker that no longer exists. The scan it was
/// running is aborted — it cannot outlive its worker and overlap the next
/// one — and goes back to the head of the queue for its waiters, one of
/// which starts a new worker.
struct WorkerRelease {
    queue: Arc<Mutex<ScanQueue>>,
    progress: Arc<tokio::sync::watch::Sender<u64>>,
    /// The request the worker is running.
    request: Option<ScanRequest>,
    /// The task running it.
    scan_task: Option<tokio::task::AbortHandle>,
    done: bool,
}

impl Drop for WorkerRelease {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(task) = self.scan_task.take() {
            task.abort();
        }
        let mut queue = lock_queue(&self.queue);
        queue.worker = false;
        if let (Some(running), Some(request)) = (queue.running.take(), self.request.take()) {
            running.cancel.cancel();
            if !queue.closed {
                let pending = PendingScan {
                    request,
                    waiters: running.waiters,
                };
                if pending.request.priority {
                    queue.lane.push_front(pending);
                } else {
                    queue.pending.push_front(pending);
                }
            }
        }
        // The lane refreshes the aborted scan was serving go back to the
        // head of the lane for their waiters.
        let serving: Vec<ServingRefresh> = queue.serving.drain().map(|(_, s)| s).collect();
        for refresh in serving {
            refresh.cancel.cancel();
            if !queue.closed {
                queue.lane.push_front(PendingScan {
                    request: refresh.request,
                    waiters: refresh.waiters,
                });
            }
        }
        drop(queue);
        self.progress.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// A waiting caller's ticket, withdrawn however the caller stops waiting.
struct QueuedTicket {
    queue: Arc<Mutex<ScanQueue>>,
    ticket: u64,
}

impl Drop for QueuedTicket {
    fn drop(&mut self) {
        lock_queue(&self.queue).withdraw(self.ticket);
    }
}

impl std::fmt::Debug for FerrofinLibraryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLibraryManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinLibraryManager {
    /// Creates a library manager over the injected persistence repositories.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        counts: Arc<dyn ItemCountService>,
        persistence: Arc<dyn ItemPersistenceService>,
        people: Arc<dyn PeopleRepository>,
    ) -> Self {
        Self {
            items,
            visibility: None,
            virtual_folders: None,
            counts,
            persistence,
            people,
            scanner: None,
            scan_queue: Arc::new(Mutex::new(ScanQueue::default())),
            scan_progress: Arc::new(tokio::sync::watch::Sender::new(0)),
            scan_tracker: None,
            chapters: None,
            user_root: None,
            years: None,
            by_name: None,
            changed: None,
        }
    }

    /// Installs the standalone item-visibility evaluator.
    #[must_use]
    pub fn with_visibility(
        mut self,
        visibility: Arc<crate::item_visibility::ItemVisibility>,
    ) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Attach library options for request-time series grouping decisions.
    #[must_use]
    pub fn with_virtual_folders(
        mut self,
        folders: Arc<dyn ferrofin_traits::library::VirtualFolderManager>,
    ) -> Self {
        self.virtual_folders = Some(folders);
        self
    }

    /// Exposes queued scan scopes to the shared library progress reader.
    #[must_use]
    pub fn with_scan_progress(
        mut self,
        tracker: &crate::scan_progress::ScanProgressTracker,
    ) -> Self {
        let queue = Arc::downgrade(&self.scan_queue);
        tracker.set_queue_reader(Arc::new(move |library, locations| {
            let Some(queue) = queue.upgrade() else {
                return false;
            };
            let queue = lock_queue(&queue);
            queue
                .pending
                .iter()
                .chain(queue.lane.iter())
                .any(|pending| {
                    let paths = match &pending.request.scope {
                        ScanTarget::All => return true,
                        ScanTarget::Library(id) => return *id == library,
                        ScanTarget::Paths(paths)
                        | ScanTarget::Changed(paths)
                        | ScanTarget::Items(paths) => paths,
                        ScanTarget::Artist { folders, .. } => folders,
                    };
                    paths.iter().any(|path| {
                        locations
                            .iter()
                            .any(|location| std::path::Path::new(path).starts_with(location))
                    })
                })
        }));
        self.scan_tracker = Some(tracker.clone());
        self
    }

    /// Attaches the `UserRootFolder` provisioner so `get_user_root_folder`
    /// creates the root on first use, as Jellyfin's `GetUserRootFolder()` does.
    #[must_use]
    pub fn with_user_root(mut self, store: crate::user_root_folder::UserRootFolderStore) -> Self {
        self.user_root = Some(store);
        self
    }

    /// Attaches the `Year` provisioner so a by-name `Year` lookup creates the
    /// item on demand (`GetYear` → `CreateItemByName<Year>`).
    #[must_use]
    pub fn with_years(mut self, store: crate::years::YearStore) -> Self {
        self.years = Some(store);
        self
    }

    /// `GetYear` for every slot of a by-name `Year` resolution that came back
    /// empty: a name that parses as a positive year gets its row created
    /// (directory + item) and the slot filled. No-op without the provisioner.
    async fn create_missing_years(
        &self,
        names: &[String],
        resolved: &mut [Option<BaseItemEntity>],
    ) -> Result<(), ServiceError> {
        let Some(years) = &self.years else {
            return Ok(());
        };
        let missing: Vec<i32> = names
            .iter()
            .zip(resolved.iter())
            .filter(|(_, row)| row.is_none())
            .filter_map(|(name, _)| name.trim().parse::<i32>().ok())
            .filter(|y| *y > 0)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let created = years.ensure_missing(&missing).await?;
        for (name, slot) in names.iter().zip(resolved.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            let Some(year) = name.trim().parse::<i32>().ok().filter(|y| *y > 0) else {
                continue;
            };
            let Some(id) = years.id_of(year) else {
                continue;
            };
            // Prefer the entity just written; a year whose row exists but
            // did not match by CleanName reads back from storage.
            let row = match created
                .iter()
                .find(|e| e.id.eq_ignore_ascii_case(&guid_to_db(id)))
            {
                Some(row) => Some(row.clone()),
                None => self.items.retrieve_item(id).await?,
            };
            *slot = row;
        }
        Ok(())
    }

    /// Attaches the `Genre`/`MusicGenre`/`Studio`/`MusicArtist` provisioner so a
    /// by-name lookup creates the item on demand — the rest of the
    /// `CreateItemByName<T>` family [`with_years`](Self::with_years) covers for
    /// `Year`. Without it those lookups 404 a name the library does not carry,
    /// where Jellyfin answers 200 with a freshly created row.
    #[must_use]
    pub fn with_by_name_store(mut self, store: crate::by_name_store::ByNameStore) -> Self {
        self.by_name = Some(store);
        self
    }

    /// Resolves each of `names` to its by-name row of `kind`, one slot per
    /// input name in order, WITHOUT creating anything.
    ///
    /// One `CleanName IN (…)` query for the whole page (the batch form of the
    /// C# `GetItemList(new InternalItemsQuery { Name = …, IncludeItemTypes =
    /// [kind] }).FirstOrDefault()`). Shared by [`get_named_items`], which then
    /// adds the `CreateItemByName` write, and by `find_named_item`, which does
    /// not — the same split C# makes between `GetGenre` and
    /// `GetItemFromSlugName`.
    ///
    /// [`get_named_items`]: ferrofin_traits::library::LibraryManager::get_named_items
    async fn resolve_named_rows(
        &self,
        kind: BaseItemKind,
        trimmed: &[String],
    ) -> Result<Vec<Option<BaseItemEntity>>, ServiceError> {
        let lookup: Vec<String> = trimmed.iter().filter(|n| !n.is_empty()).cloned().collect();
        if lookup.is_empty() {
            return Ok(vec![None; trimmed.len()]);
        }
        let rows = self
            .items
            .get_item_list(&InternalItemsQuery {
                names: lookup,
                include_item_types: vec![kind],
                ..InternalItemsQuery::default()
            })
            .await?;
        // Key by the row's stored CleanName (what the query matched on); first
        // match wins, mirroring `FirstOrDefault`.
        let mut by_clean: HashMap<String, BaseItemEntity> = HashMap::new();
        for row in rows {
            if let Some(clean) = row.clean_name.clone() {
                by_clean.entry(clean).or_insert(row);
            }
        }
        Ok(trimmed
            .iter()
            .map(|n| {
                if n.is_empty() {
                    None
                } else {
                    by_clean.get(&crate::text_util::get_clean_value(n)).cloned()
                }
            })
            .collect())
    }

    /// `CreateItemByName<T>` for every by-name slot that came back empty.
    ///
    /// `Year` keeps its own provisioner (its names must parse as a positive
    /// year); the other four go through [`ByNameStore`](crate::by_name_store::ByNameStore).
    async fn create_missing_by_name(
        &self,
        kind: BaseItemKind,
        names: &[String],
        resolved: &mut [Option<BaseItemEntity>],
    ) -> Result<(), ServiceError> {
        if kind == BaseItemKind::Year {
            return self.create_missing_years(names, resolved).await;
        }
        let Some(store) = &self.by_name else {
            return Ok(());
        };
        for (name, slot) in names.iter().zip(resolved.iter_mut()) {
            if slot.is_some() || name.is_empty() {
                continue;
            }
            let Some(id) = store.ensure(kind, name).await? else {
                continue;
            };
            *slot = self.items.retrieve_item(id).await?;
        }
        Ok(())
    }

    /// Attaches the chapter seam so chapter thumbnails can be served (their
    /// paths live on the chapter rows, not in the item's image rows).
    #[must_use]
    pub fn with_chapters(
        mut self,
        chapters: Arc<dyn ferrofin_traits::persistence::ChapterRepository>,
    ) -> Self {
        self.chapters = Some(chapters);
        self
    }

    /// Attaches the debounced `LibraryChanged` notifier so item writes
    /// announce themselves to open clients.
    #[must_use]
    pub fn with_change_notifier(
        mut self,
        notifier: Arc<crate::library_changed_notifier::LibraryChangedNotifier>,
    ) -> Self {
        self.changed = Some(notifier);
        self
    }

    /// Attaches the filesystem scanner so `queue_library_scan` actually walks the
    /// libraries. Called once by the composition root.
    #[must_use]
    pub fn with_scanner(mut self, scanner: Arc<crate::library_scan::LibraryScanner>) -> Self {
        self.scanner = Some(scanner);
        self
    }

    /// Attaches a scan stand-in in place of the filesystem scanner.
    #[cfg(test)]
    fn with_scan_runner(mut self, runner: Arc<dyn ScanRunner>) -> Self {
        self.scanner = Some(runner);
        self
    }
}

#[async_trait]
impl crate::library_monitor::LibraryScanTrigger for FerrofinLibraryManager {
    async fn queue_scan_paths(
        &self,
        paths: Vec<String>,
        trigger: ScanTrigger,
    ) -> Result<(), ServiceError> {
        // The monitor's settled change batch (the watcher, the Radarr/Sonarr
        // webhooks): each path's `ChangedExternally` refresh, scoped to the
        // item it refreshes (`FileRefresher.cs:135-208`).
        self.spawn_scan(ScanRequest::changed(paths, trigger));
        Ok(())
    }
}

#[async_trait]
impl LibraryManager for FerrofinLibraryManager {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Ok(None);
        }
        self.items.retrieve_item(id).await
    }

    async fn is_item_visible(
        &self,
        item: &BaseItemEntity,
        user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ServiceError> {
        let service = self
            .visibility
            .as_ref()
            .ok_or_else(|| ServiceError::backend("item visibility is not configured"))?;
        Ok(service
            .visible(std::slice::from_ref(item), user, false)
            .await?[0])
    }

    async fn is_item_visible_standalone(
        &self,
        item: &BaseItemEntity,
        user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<bool, ServiceError> {
        let service = self
            .visibility
            .as_ref()
            .ok_or_else(|| ServiceError::backend("item visibility is not configured"))?;
        Ok(service
            .visible(std::slice::from_ref(item), user, true)
            .await?[0])
    }

    async fn get_visible_item_ids(
        &self,
        ids: &[Uuid],
        user: &ferrofin_db::entities::users::UserEntity,
    ) -> Result<Vec<Uuid>, ServiceError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let service = self
            .visibility
            .as_ref()
            .ok_or_else(|| ServiceError::backend("item visibility is not configured"))?;
        let rows = self.items.retrieve_items(ids).await?;
        let visible: std::collections::HashSet<_> = rows
            .iter()
            .zip(service.visible(&rows, user, true).await?)
            .filter(|(_, visible)| *visible)
            .filter_map(|(row, _)| Uuid::parse_str(&row.id).ok())
            .filter(|id| !id.is_nil() && *id != PLACEHOLDER_ITEM_ID)
            .collect();
        Ok(ids
            .iter()
            .copied()
            .filter(|id| visible.contains(id))
            .collect())
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        // Exactly `get_item_by_id(id).is_some()`, minus the row decode:
        // `get_item_by_id` rejects the nil id and `retrieve_item`'s predicate
        // excludes the seeded placeholder row, so both are "not an item" here
        // too. `ItemRepository::item_exists` answers the rest with a
        // `SELECT 1` existence probe instead of a ~70-column read.
        if id.is_nil() || id == PLACEHOLDER_ITEM_ID {
            return Ok(false);
        }
        self.items.item_exists(id).await
    }

    async fn get_ancestors(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        self.items.get_ancestor_chain(item_id).await
    }

    async fn get_item_images(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
        if item_id.is_nil() {
            return Ok(Vec::new());
        }
        self.items.get_image_infos(item_id).await
    }

    async fn get_chapter_image(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
        let Some(chapters) = &self.chapters else {
            return Ok(None);
        };
        let Ok(index) = usize::try_from(index) else {
            return Ok(None);
        };
        // Chapters come back in position order, which is the index clients
        // address them by (upstream `ChapterManager.GetChapter(id, index)`).
        let rows = chapters.get_chapters(item_id).await?;
        let Some(chapter) = rows.into_iter().nth(index) else {
            return Ok(None);
        };
        let Some(path) = chapter.image_path.filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(ferrofin_traits::options::ItemImageInfo {
            path,
            image_type: ferrofin_model::entities::ImageType::Chapter,
            date_modified: chapter.image_date_modified.unwrap_or_else(chrono::Utc::now),
            width: 0,
            height: 0,
            blur_hash: None,
        }))
    }

    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // Only backdrops and chapters may hold multiple images and thus be
        // reordered; any other type is a bad request (C# `AllowsMultipleImages`
        // guard throwing `ArgumentException` → 400).
        if !image_type_allows_multiple(image_type) {
            return Err(ServiceError::invalid_input(
                "The change index operation is only applicable to backdrops and chapters",
            ));
        }
        self.items
            .swap_item_images(item_id, image_type, index1, index2)
            .await
    }

    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        self.items.get_items(query).await
    }

    async fn get_item_ids(&self, query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.items.get_item_ids(query).await
    }

    async fn get_extra_owner_ids_batch(
        &self,
        items: &[BaseItemEntity],
    ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        let folders = if items.iter().any(|item| item.type_.ends_with(".Series")) {
            if let Some(folders) = &self.virtual_folders {
                folders.get_virtual_folders().await?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let grouped_series: Vec<Uuid> = items
            .iter()
            .filter(|item| {
                item.type_.ends_with(".Series")
                    && crate::item_persistence_service::series_key_scope(
                        &folders,
                        "",
                        item.top_parent_id.as_deref(),
                        item.path.as_deref(),
                        None,
                    )
                    .enable_automatic_series_grouping
            })
            .filter_map(|item| Uuid::parse_str(&item.id).ok())
            .collect();
        self.items
            .get_extra_owner_ids_batch(items, &grouped_series)
            .await
    }

    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items.get_item_list(query).await
    }

    fn empty_by_name_item(&self, kind: BaseItemKind) -> BaseItemEntity {
        BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(Uuid::nil()),
            type_: crate::item_type_lookup::stored_type_name(kind)
                .unwrap_or_default()
                .to_owned(),
            ..BaseItemEntity::default()
        }
    }

    async fn find_named_item(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        // The resolve half of `get_named_items` with the `CreateItemByName`
        // write left off — the slug branch of `GenresController` looks names up
        // and must not materialize a row for one that matches nothing.
        Ok(self
            .resolve_named_rows(kind, std::slice::from_ref(&name.trim().to_owned()))
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    async fn get_named_items(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<BaseItemEntity>>, ServiceError> {
        let trimmed: Vec<String> = names.iter().map(|n| n.trim().to_owned()).collect();
        let mut resolved = self.resolve_named_rows(kind, &trimmed).await?;
        // `CreateItemByName<T>`: a by-name lookup MATERIALIZES the item
        // upstream (directory + row) rather than reporting it missing, so this
        // is the read path's write. Deliberately not done in
        // `get_named_item_ids`, the per-credit hot path — C# splits it the same
        // way (`GetItemByNameId` derives, `CreateItemByName` writes).
        self.create_missing_by_name(kind, &trimmed, &mut resolved)
            .await?;
        Ok(resolved)
    }

    async fn get_named_item(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        // The single-name form of `get_named_items` — same CleanName match,
        // first row wins — so a `Year` lookup also materializes on demand.
        Ok(self
            .get_named_items(kind, std::slice::from_ref(&name.to_owned()))
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    async fn get_user_root_folder(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        // `GetUserRootFolder()`: create the directory + row on first use, then
        // resolve it by its deterministic id. Without the provisioner wired
        // (unit tests) fall back to the persisted-row lookup.
        let Some(root) = &self.user_root else {
            let query = InternalItemsQuery {
                include_item_types: vec![BaseItemKind::UserRootFolder],
                ..InternalItemsQuery::default()
            };
            return Ok(self.items.get_item_list(&query).await?.into_iter().next());
        };
        let id = root.ensure().await?;
        self.items.retrieve_item(id).await
    }

    async fn get_named_item_ids(
        &self,
        kind: BaseItemKind,
        names: &[String],
    ) -> Result<Vec<Option<Uuid>>, ServiceError> {
        // Same resolution as `get_named_items` — same predicates, same ordering,
        // same first-match-wins — over a two-column projection, because the
        // caller wants the id and nothing else. On a cast-heavy page this is the
        // difference between decoding one 72-column row per credited name and
        // decoding two columns.
        let trimmed: Vec<String> = names.iter().map(|n| n.trim().to_owned()).collect();
        let lookup: Vec<String> = trimmed.iter().filter(|n| !n.is_empty()).cloned().collect();
        if lookup.is_empty() {
            return Ok(vec![None; names.len()]);
        }
        let rows = self
            .items
            .get_item_id_clean_names(&InternalItemsQuery {
                names: lookup,
                include_item_types: vec![kind],
                ..InternalItemsQuery::default()
            })
            .await?;
        let mut by_clean: HashMap<String, String> = HashMap::new();
        for (id, clean) in rows {
            if let Some(clean) = clean {
                by_clean.entry(clean).or_insert(id);
            }
        }
        Ok(trimmed
            .into_iter()
            .map(|n| {
                if n.is_empty() {
                    None
                } else {
                    by_clean
                        .get(&crate::text_util::get_clean_value(&n))
                        .and_then(|id| Uuid::parse_str(id).ok())
                }
            })
            .collect())
    }

    async fn get_latest_item_list(
        &self,
        query: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items
            .get_latest_item_list(query, collection_type)
            .await
    }

    async fn create_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        // The parent linkage is already carried on each row's ParentId column; the
        // upsert is the single persistence path (C# CreateItems == save + register).
        self.persistence.save_items(items).await?;
        // `ItemAdded` (LibraryChangedNotifier): announce only after the write
        // succeeded, so a failed save never tells clients to fetch a row that
        // is not there.
        if let Some(changed) = &self.changed {
            changed.record_added(items);
        }
        Ok(())
    }

    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        self.persistence.save_items(items).await?;
        // Re-index each item's genre/tag/studio/artist links: the filter
        // facets and by-name browses read `ItemValues`, not the row columns,
        // so a metadata edit that only saved the row was invisible to them
        // until the next full scan.
        for item in items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            let values = crate::library_scan::item_values_of(item);
            self.persistence.save_item_values(id, &values).await?;
        }
        // No `ItemUpdated` hook here: `save_items` above is the repository save,
        // and the notifier is attached THERE so that every writer announces
        // itself, not just this one. Recording it again here would be a
        // duplicate the fold would have to drop.
        Ok(())
    }

    async fn update_item_provider_ids(
        &self,
        item_id: Uuid,
        provider_ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        // An assignment, not a merge (C# `item.ProviderIds = request.ProviderIds`):
        // `replace_provider_ids` deletes the rows the new set lacks and upserts
        // the rest in one transaction.
        self.persistence
            .replace_provider_ids(item_id, provider_ids)
            .await
    }

    async fn get_locked_fields_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<MetadataField>>, ServiceError> {
        self.persistence.locked_fields_for_items(item_ids).await
    }

    async fn update_item_locked_fields(
        &self,
        item_id: Uuid,
        field_ids: &[i32],
    ) -> Result<(), ServiceError> {
        self.persistence
            .replace_locked_fields(item_id, field_ids)
            .await
    }

    async fn delete_item(&self, id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let Some(row) = self.items.retrieve_item(id).await? else {
            // Already gone — deletion is idempotent.
            return Ok(());
        };
        // C# `LibraryController.DeleteItem`: `!item.CanDelete(user)` is a 401
        // "Unauthorized access". `item_deletion::can_delete` is the single
        // implementation of that rule, and the handler asks it; this kind
        // check is a backstop that only changes the outcome for an API key,
        // where upstream would ask nothing. It keeps the user root, the
        // aggregate root, a library's collection folder, the views and the
        // by-name items undeletable — with `ParentId` a cascading foreign key,
        // deleting the root would delete every library and all of its items.
        let kind = crate::item_type_lookup::kind_from_type_name(&row.type_)
            .unwrap_or(BaseItemKind::Folder);
        let has_parent = row
            .parent_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            .is_some_and(|p| !p.is_nil());
        if !crate::kinds::can_delete(kind, has_parent) {
            return Err(ServiceError::unauthorized("Unauthorized access"));
        }
        let mut ids = vec![id];
        // C# `DeleteItem` cascades to a folder's children; gather the direct-child
        // ids so the row deletion removes the subtree too.
        //
        // TODO(parity, open work item): upstream deletes the media files
        // (`DeleteFileLocation = true`); not ported yet, it waits on scanner
        // parity, so `options.delete_file_location` is not honoured and only
        // rows go. See `brain/plans/PLAN_ITEM_FILE_DELETION.md`.
        if row.is_folder {
            // Cascade to PHYSICAL children only. A box-set/playlist is a folder whose
            // members are LinkedChildren (references), not owned children — deleting the
            // container must never delete the referenced media (data loss). physical_children_only
            // suppresses the LinkedChildren merge the browse path uses.
            let child_query = InternalItemsQuery {
                parent_id: id,
                physical_children_only: true,
                ..Default::default()
            };
            ids.extend(self.items.get_item_ids(&child_query).await?);
        }
        self.persistence.delete_items(&ids).await?;
        // `ItemRemoved` (LibraryChangedNotifier). `ids[0]` is the item itself;
        // the rest are the cascaded children, which have no rows left to read.
        if let Some(changed) = &self.changed {
            changed.record_removed_subtree(&row, &ids[1..]);
        }
        Ok(())
    }

    async fn merge_versions(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        // Resolve each supplied id to a persisted row, dropping any that are
        // missing, then de-duplicate and order by id (C# `.OrderBy(i => i.Id)`).
        let mut items = Vec::new();
        for &id in ids {
            if let Some(row) = self.items.retrieve_item(id).await? {
                items.push(row);
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.dedup_by(|a, b| a.id == b.id);

        if items.len() < 2 {
            return Err(ServiceError::invalid_input(
                "please supply at least two videos to merge",
            ));
        }

        // Pick the primary. C# prefers an item that already owns multiple sources
        // and is itself not an alternate; Ferrofin does not model `MediaSourceCount`,
        // so it falls back to C#'s secondary ordering: a plain video file outranks
        // a special type, then the widest default video stream wins. The item's own
        // `Width` column stands in for the default video stream width.
        let primary_index = items
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.width.unwrap_or(0).cmp(&b.width.unwrap_or(0)))
            .map_or(0, |(i, _)| i);
        let primary_id = items[primary_index].id.clone();

        // Link every non-primary item to the primary by pointer, and ensure the
        // primary itself is a standalone (its own pointer cleared). Targeted
        // single-column writes: the rows were loaded to *decide* the linkage,
        // and a full-row save would write their other columns back stale.
        let primary_uuid = Uuid::parse_str(&primary_id)
            .map_err(|_| ServiceError::invalid_input("malformed item id"))?;
        for item in &items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            if item.id == primary_id {
                if item.primary_version_id.is_some() {
                    self.persistence.set_primary_version_id(id, None).await?;
                }
            } else if item.primary_version_id.as_deref() != Some(primary_id.as_str()) {
                self.persistence
                    .set_primary_version_id(id, Some(primary_uuid))
                    .await?;
            }
        }
        Ok(())
    }

    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(item) = self.items.retrieve_item(item_id).await? else {
            return Err(ServiceError::not_found(format!("item {item_id}")));
        };

        // Resolve the group's primary: either this item (no pointer) or the item it
        // points at (C# hops to `PrimaryVersionId` when the item has no alternates).
        let primary_id = match item.primary_version_id.as_deref() {
            Some(pid) => Uuid::parse_str(pid)
                .map_err(|_| ServiceError::invalid_input("malformed PrimaryVersionId"))?,
            None => item_id,
        };

        // Clear the pointer on every alternate that references the primary, then on
        // the primary itself, so each becomes a standalone version again. Targeted
        // single-column writes — a full-row save of the loaded copies would revert
        // any column another writer changed since the load.
        for alt in self.items.get_items_by_primary_version(primary_id).await? {
            let Ok(id) = Uuid::parse_str(&alt.id) else {
                continue;
            };
            self.persistence.set_primary_version_id(id, None).await?;
        }
        if let Some(primary) = self.items.retrieve_item(primary_id).await?
            && primary.primary_version_id.is_some()
        {
            self.persistence
                .set_primary_version_id(primary_id, None)
                .await?;
        }
        Ok(())
    }

    async fn get_people(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        Ok(self.people.get_people(query).await?.items)
    }

    async fn get_people_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<PeopleEntity>>, ServiceError> {
        self.people.get_people_batch(item_ids).await
    }

    async fn get_people_names(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.people.get_people_names(query).await
    }

    async fn get_count(&self, query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        self.counts.get_count(query).await
    }

    async fn get_item_counts(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        self.counts.get_item_counts(query).await
    }

    async fn get_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_genres(query).await
    }

    async fn get_studios(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_studios(query).await
    }

    async fn get_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_artists(query).await
    }

    async fn get_music_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_music_genres(query).await
    }

    async fn get_album_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_album_artists(query).await
    }

    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.items.get_query_filters_legacy(query).await
    }

    async fn get_distinct_years(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<i32>, ServiceError> {
        self.items.get_distinct_years(query).await
    }

    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
        query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.items
            .get_media_stream_languages(query, stream_type)
            .await
    }

    async fn get_media_stream_languages_by_type(
        &self,
        stream_types: &[MediaStreamType],
        query: &InternalItemsQuery,
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        self.items
            .get_media_stream_languages_by_type(query, stream_types)
            .await
    }

    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        self.spawn_scan(ScanRequest::defaults(ScanTrigger::Api, ScanTarget::All));
        Ok(())
    }

    async fn queue_library_scan_with_trigger(
        &self,
        trigger: ScanTrigger,
    ) -> Result<(), ServiceError> {
        self.spawn_scan(ScanRequest::defaults(trigger, ScanTarget::All));
        Ok(())
    }

    async fn queue_library_scan_scoped(&self, library_id: Uuid) -> Result<(), ServiceError> {
        self.spawn_scan(ScanRequest::defaults(
            ScanTrigger::Api,
            ScanTarget::Library(library_id),
        ));
        Ok(())
    }

    async fn queue_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        if self.scanner.is_none() {
            // The trait's contract: the options cannot be honoured, and a
            // silent no-op would look like a refresh that ran.
            return Err(ServiceError::backend(
                "queue_refresh_scan needs a library scanner, which this library manager has none of",
            ));
        }
        // A file item's own refresh — and a by-name artist's, which has no
        // folder to validate — waits in the priority lane; a folder's
        // validation — as large as a whole library — queues like a scan.
        let item_only = match &target {
            ScanTarget::Items(_) => true,
            ScanTarget::Artist { path, folders, .. } => path.is_none() && folders.is_empty(),
            _ => false,
        };
        let request = if item_only {
            ScanRequest::item_refresh(target, options.clone())
        } else {
            ScanRequest::folder_refresh(target, options.clone())
        };
        self.spawn_scan(request);
        Ok(())
    }

    async fn run_refresh_scan(
        &self,
        target: ScanTarget,
        options: &MetadataRefreshOptions,
    ) -> Result<bool, ServiceError> {
        if self.scanner.is_none() {
            return Err(ServiceError::backend(
                "run_refresh_scan needs a library scanner, which this library manager has none of",
            ));
        }
        // Through the priority lane: it runs ahead of every queued scan, and
        // inside a running one between two of its items, so the caller waits
        // for about one item plus its own refresh — never a library scan.
        self.run_scan(ScanRequest::item_refresh(target, options.clone()), None)
            .await
    }

    async fn run_library_scan(
        &self,
        trigger: ScanTrigger,
        progress: Option<ScanProgressSink>,
    ) -> Result<bool, ServiceError> {
        // The scan runs on the queue's worker; this future only waits for it.
        self.run_scan(ScanRequest::defaults(trigger, ScanTarget::All), progress)
            .await
    }

    async fn shutdown_scans(&self) {
        let mut progress = self.scan_progress.subscribe();
        {
            let mut queue = lock_queue(&self.scan_queue);
            queue.closed = true;
            let mut dropped: Vec<u64> = queue
                .pending
                .drain(..)
                .flat_map(|pending| pending.waiters.tickets)
                .collect();
            dropped.extend(
                queue
                    .lane
                    .drain(..)
                    .flat_map(|pending| pending.waiters.tickets),
            );
            for serving in queue.serving.values() {
                serving.cancel.cancel();
            }
            for ticket in dropped {
                queue.sinks.remove(&ticket);
                queue.finished.insert(ticket, ScanResult::Stopped);
            }
            if let Some(running) = &queue.running {
                running.cancel.cancel();
                tracing::info!("stopping the running library scan for shutdown");
            }
        }
        self.scan_progress.send_modify(|n| *n = n.wrapping_add(1));
        // The running scan stops before its next item (or out of a wait
        // before its current one writes anything), so this is bounded by
        // one item's writes.
        loop {
            if !lock_queue(&self.scan_queue).worker {
                break;
            }
            if progress.changed().await.is_err() {
                break;
            }
        }
        if let Some(tracker) = &self.scan_tracker {
            tracker.shutdown().await;
        }
    }
}

impl FerrofinLibraryManager {
    /// Queues `request` and returns the ticket its caller waits on (`None`
    /// when nobody waits, with no scanner attached, or once the host is
    /// shutting down), starting the queue's worker if none is running.
    fn submit(
        &self,
        request: ScanRequest,
        waiting: bool,
        progress: Option<ScanProgressSink>,
    ) -> Option<u64> {
        let Some(runner) = &self.scanner else {
            tracing::debug!(
                trigger = request.trigger.as_str(),
                "library scan queued (no scanner attached — no-op)"
            );
            return None;
        };
        let request_trigger = request.trigger;
        let trigger = request_trigger.as_str();
        let scope = scope_label(&request.scope);
        let (ticket, start, behind) = {
            let mut queue = lock_queue(&self.scan_queue);
            if queue.closed {
                tracing::debug!(trigger, scope, "library scan refused: shutting down");
                return None;
            }
            let behind = queue.running.is_some();
            let ticket = queue.enqueue(request, waiting);
            if let (Some(ticket), Some(progress)) = (ticket, progress) {
                queue.sinks.insert(ticket, progress);
            }
            let start = !queue.worker;
            queue.worker = true;
            (ticket, start, behind)
        };
        if behind {
            tracing::debug!(
                trigger,
                scope,
                "library scan already running; queued behind it"
            );
        }
        if start {
            self.start_worker(Arc::clone(runner), request_trigger);
        }
        ticket
    }

    /// Starts the queue's worker on a task of its own, inside a
    /// `library_scan` root span tagged with the trigger that started it — a
    /// scan is a background unit of work, never parented under the request
    /// span.
    fn start_worker(&self, runner: Arc<dyn ScanRunner>, trigger: ScanTrigger) {
        let worker = ScanWorker {
            runner,
            queue: Arc::clone(&self.scan_queue),
            progress: Arc::clone(&self.scan_progress),
        };
        let span = tracing::info_span!(parent: None, "library_scan", trigger = trigger.as_str());
        let task = tokio::spawn(worker.drain().instrument(span));
        #[cfg(test)]
        {
            lock_queue(&self.scan_queue).worker_task = Some(task.abort_handle());
        }
        drop(task);
    }

    /// Queues the library scan for `request` and returns at once (Jellyfin's
    /// refresh is fire-and-forget — it must not block the HTTP handler).
    /// Shared by every queueing entry point.
    fn spawn_scan(&self, request: ScanRequest) {
        self.submit(request, false, None);
    }

    /// Queues the scan for `request` and returns only once it has run,
    /// reporting its progress to `progress`.
    ///
    /// Used by the "Scan Media Library" scheduled task, whose upstream
    /// counterpart awaits the validation pass (`RefreshMediaLibraryTask
    /// .ExecuteAsync` → `await ValidateMediaLibraryInternal`), so "the task
    /// finished" means "the library is scanned", and a scan that failed or
    /// panicked fails the task.
    ///
    /// The scan runs on the queue's worker, never on this future.
    /// Cancelling this future — `POST /Library/Refresh`'s
    /// `CancelIfRunningAndQueue` cancelling the scheduled task, or the
    /// dashboard's stop button — withdraws only this caller's request: still
    /// queued, it is dropped unless a fire-and-forget request or another
    /// caller shares it; running, it stops cooperatively once nobody else
    /// wants it, with the item in progress written whole or not at all. Only
    /// then does the next queued scan start, so a refresh's replacement task
    /// waits for the cancelled scan to end: at most one item — plus, when
    /// the scan is serving a priority-lane item refresh at that moment, the
    /// rest of that refresh (it is not the cancelled caller's to stop).
    async fn run_scan(
        &self,
        request: ScanRequest,
        progress: Option<ScanProgressSink>,
    ) -> Result<bool, ServiceError> {
        // Subscribed before the request is queued, so no progress between
        // the check below and the wait is missed.
        let mut changed = self.scan_progress.subscribe();
        // A replacement worker (below) keeps the request's own trigger.
        let trigger = request.trigger;
        // Refused: scanning has shut down.
        let Some(ticket) = self.submit(request, true, progress) else {
            return Ok(false);
        };
        let _withdraw = QueuedTicket {
            queue: Arc::clone(&self.scan_queue),
            ticket,
        };
        loop {
            let (result, restart, lost) = {
                let mut queue = lock_queue(&self.scan_queue);
                let result = queue.finished.remove(&ticket);
                // A worker that went away before it drained the queue (its
                // task dropped) is replaced, so this request still runs —
                // only while the request is still queued: with no worker, a
                // request neither queued nor settled is gone, and starting
                // workers for it would only drain an empty queue over and
                // over.
                let orphaned = result.is_none() && !queue.worker && !queue.closed;
                let queued = queue.is_queued(ticket);
                let restart = orphaned && queued;
                queue.worker |= restart;
                (result, restart, orphaned && !queued)
            };
            match result {
                Some(ScanResult::Completed) => return Ok(true),
                Some(ScanResult::Stopped) => return Ok(false),
                Some(ScanResult::Failed(message)) => return Err(ServiceError::backend(message)),
                None if lost => {
                    return Err(ServiceError::backend(
                        "the scan request was lost before it ran".to_owned(),
                    ));
                }
                None => {}
            }
            if restart && let Some(runner) = &self.scanner {
                self.start_worker(Arc::clone(runner), trigger);
            }
            if changed.changed().await.is_err() {
                return Ok(false);
            }
        }
    }

    /// Whether no scan is running and none is queued.
    #[cfg(test)]
    fn scan_idle(&self) -> bool {
        let queue = lock_queue(&self.scan_queue);
        !queue.worker
            && queue.running.is_none()
            && queue.pending.is_empty()
            && queue.lane.is_empty()
            && queue.serving.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_count_service::FerrofinItemCountService;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::people_repository::FerrofinPeopleRepository;
    use crate::test_support::{
        seed_item, seed_item_genre, seed_named_item, set_clean_name, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ImageType;

    #[tokio::test(flavor = "current_thread")]
    async fn queue_library_scan_exports_a_library_scan_span_tagged_with_trigger() {
        // End-to-end span-coverage smoke: an `api`-triggered scan (empty library,
        // so it finishes fast) exports a `library_scan` root span carrying
        // `trigger`. current-thread + set_default keeps the spawned scan on the
        // scoped subscriber so the `.instrument()`ed span is captured.
        use crate::file_system::FerrofinFileSystem;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_traits::library::VirtualFolderManager;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;

        let db = test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        // No virtual folders added → the scan plans zero items and returns fast.
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        mgr.queue_library_scan().await.expect("queued");
        // Wait for completion rather than assuming the scan finishes within 50 ms.
        // On this current-thread runtime, the worker closes its span before this
        // test can observe the idle queue, so the simple exporter has seen it.
        until_idle(&mgr).await;
        provider.force_flush().expect("flush");

        let spans = exporter.get_finished_spans().expect("spans");
        let span = spans
            .iter()
            .find(|s| s.name == "library_scan")
            .expect("library_scan span exported");
        let trigger = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "trigger")
            .map(|kv| kv.value.to_string());
        assert_eq!(trigger.as_deref(), Some("api"));
    }

    /// One run a [`GatedRunner`] made.
    #[derive(Debug, Clone, PartialEq)]
    struct Run {
        scope: ScanTarget,
        options: MetadataRefreshOptions,
        ancestors: MetadataRefreshOptions,
        /// Items it finished.
        items: usize,
        /// It stopped at an item boundary because it was cancelled.
        stopped: bool,
    }

    /// A scan stand-in of [`GatedRunner::ITEMS`] items: each item waits for a
    /// permit the test releases, and cancellation is checked between items
    /// (as the scanner does). Panics once told to, after its first permit.
    struct GatedRunner {
        runs: Mutex<Vec<Run>>,
        started: tokio::sync::watch::Sender<usize>,
        gate: tokio::sync::Semaphore,
        panic_next: std::sync::atomic::AtomicBool,
        fail_next: std::sync::atomic::AtomicBool,
        /// Serve the priority lane between two items, as the scanner does.
        serves_lane: std::sync::atomic::AtomicBool,
        /// The lane refreshes served inside a run: `(run index, target)`.
        served: Mutex<Vec<(usize, ScanTarget)>>,
        /// The next lane refresh taken panics the run serving it (before
        /// handing it back).
        panic_serving: std::sync::atomic::AtomicBool,
    }

    impl GatedRunner {
        const ITEMS: usize = 2;

        fn new() -> Arc<Self> {
            Arc::new(Self {
                runs: Mutex::new(Vec::new()),
                started: tokio::sync::watch::Sender::new(0),
                gate: tokio::sync::Semaphore::new(0),
                panic_next: std::sync::atomic::AtomicBool::new(false),
                fail_next: std::sync::atomic::AtomicBool::new(false),
                serves_lane: std::sync::atomic::AtomicBool::new(true),
                served: Mutex::new(Vec::new()),
                panic_serving: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn served(&self) -> Vec<(usize, ScanTarget)> {
            self.served.lock().expect("served").clone()
        }

        fn runs(&self) -> Vec<Run> {
            self.runs.lock().expect("runs").clone()
        }

        /// Waits until `n` runs have started.
        async fn started(&self, n: usize) {
            let mut started = self.started.subscribe();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                started.wait_for(|s| *s >= n),
            )
            .await
            .expect("the scan started in time")
            .expect("runner alive");
        }

        fn release(&self, n: usize) {
            self.gate.add_permits(n);
        }
    }

    #[async_trait]
    impl ScanRunner for GatedRunner {
        async fn run(
            &self,
            scope: &ScanTarget,
            run: ScanRun<'_>,
        ) -> Result<crate::library_scan::ScanOutcome, ServiceError> {
            let index = {
                let mut runs = self.runs.lock().expect("runs");
                runs.push(Run {
                    scope: scope.clone(),
                    options: run.options.clone(),
                    ancestors: run.ancestors.clone(),
                    items: 0,
                    stopped: false,
                });
                runs.len() - 1
            };
            self.started.send_modify(|s| *s += 1);
            for item in 1..=Self::ITEMS {
                // Between two items: the lane's refreshes run inline.
                if let Some(lane) = run
                    .lane
                    .filter(|_| self.serves_lane.load(std::sync::atomic::Ordering::SeqCst))
                {
                    while let Some(refresh) = lane.next() {
                        assert!(
                            !self
                                .panic_serving
                                .swap(false, std::sync::atomic::Ordering::SeqCst),
                            "a served refresh panicked"
                        );
                        self.served
                            .lock()
                            .expect("served")
                            .push((index, refresh.target.clone()));
                        lane.done(refresh, &Ok(crate::library_scan::ScanOutcome::default()));
                    }
                }
                if run.cancel.is_cancelled() {
                    self.runs.lock().expect("runs")[index].stopped = true;
                    return Ok(crate::library_scan::ScanOutcome {
                        stopped: true,
                        ..crate::library_scan::ScanOutcome::default()
                    });
                }
                self.gate.acquire().await.expect("gate open").forget();
                assert!(
                    !self
                        .panic_next
                        .swap(false, std::sync::atomic::Ordering::SeqCst),
                    "scan blew up"
                );
                self.runs.lock().expect("runs")[index].items += 1;
                if let Some(progress) = run.progress {
                    let percent = u32::try_from(item * 96 / Self::ITEMS).expect("percent");
                    progress(f64::from(percent));
                }
            }
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(ServiceError::backend("disk full"));
            }
            Ok(crate::library_scan::ScanOutcome::default())
        }
    }

    /// Waits (bounded) until the manager holds no running and no queued scan.
    async fn until_idle(mgr: &FerrofinLibraryManager) {
        for _ in 0..500 {
            if mgr.scan_idle() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the scan queue never went idle");
    }

    /// Waits (bounded) until the queue holds `scans` scans with `tickets`
    /// waiting callers between them.
    async fn until_queued(mgr: &FerrofinLibraryManager, scans: usize, tickets: usize) {
        for _ in 0..500 {
            {
                let queue = lock_queue(&mgr.scan_queue);
                let waiting: usize = queue.pending.iter().map(|p| p.waiters.tickets.len()).sum();
                if queue.pending.len() == scans && waiting == tickets {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{scans} scans with {tickets} waiters never queued");
    }

    fn replace_all() -> MetadataRefreshOptions {
        MetadataRefreshOptions::for_item_refresh(
            ferrofin_traits::providers::MetadataRefreshMode::FullRefresh,
            ferrofin_traits::providers::MetadataRefreshMode::FullRefresh,
            true,
            false,
            false,
        )
    }

    fn none_none() -> MetadataRefreshOptions {
        MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::None,
            image_refresh_mode: MetadataRefreshMode::None,
            ..MetadataRefreshOptions::default()
        }
    }

    /// A finished run of `scope` with the default options.
    fn default_run(scope: ScanTarget) -> Run {
        Run {
            scope,
            options: MetadataRefreshOptions::default(),
            ancestors: MetadataRefreshOptions::default(),
            items: GatedRunner::ITEMS,
            stopped: false,
        }
    }

    fn spawn_scheduled(mgr: &FerrofinLibraryManager) -> tokio::task::JoinHandle<()> {
        let mgr = mgr.clone();
        tokio::spawn(async move {
            mgr.run_library_scan(ScanTrigger::Schedule, None)
                .await
                .expect("scan runs");
        })
    }

    #[tokio::test]
    async fn run_library_scan_returns_only_once_the_scan_has_finished() {
        // Upstream's "Scan Media Library" task awaits the validation pass
        // (`RefreshMediaLibraryTask.ExecuteAsync`:57-64), so the task is still
        // `Running` while the library is being written. This asserts both
        // halves of that: the caller returns once its scan ran and the queue
        // is idle, and a caller queued behind someone else's scan waits until
        // ITS OWN scan has run rather than returning immediately.
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        runner.release(GatedRunner::ITEMS);
        mgr.run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("scan runs");
        until_idle(&mgr).await;
        assert_eq!(runner.runs(), vec![default_run(ScanTarget::All)]);

        // Queued path: a watcher scan runs; the scheduled run queues behind
        // it and returns only once its own scan ran.
        mgr.queue_library_scan().await.expect("queued");
        runner.started(2).await;
        let waiter = spawn_scheduled(&mgr);
        until_queued(&mgr, 1, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "a queued run must wait for its scan, not return at once"
        );
        runner.release(2 * GatedRunner::ITEMS);
        waiter.await.expect("joined");
        assert_eq!(runner.runs().len(), 3, "the queued run scanned");
        until_idle(&mgr).await;
    }

    /// `POST /Library/Refresh` cancels the running scan task and starts a new
    /// one (`CancelIfRunningAndQueue`). Cancelling the task stops its scan
    /// between two items — the item in progress finishes — and releases the
    /// queue when the scan has really ended, so the next scan runs: nothing
    /// queues behind a scan that no longer exists (observed before this: two
    /// refreshes 0.5 s apart wedged scanning until restart).
    #[tokio::test]
    async fn cancelling_a_running_scan_stops_it_between_items_and_the_next_one_runs() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        let first = spawn_scheduled(&mgr);
        runner.started(1).await;
        first.abort();
        assert!(first.await.expect_err("aborted").is_cancelled());
        assert!(
            !mgr.scan_idle(),
            "the scan finishes the item in progress before it stops"
        );
        runner.release(1);
        until_idle(&mgr).await;
        assert_eq!(
            runner.runs(),
            vec![Run {
                items: 1,
                stopped: true,
                ..default_run(ScanTarget::All)
            }],
            "one item finished, then the scan stopped at the boundary"
        );

        runner.release(GatedRunner::ITEMS);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mgr.run_library_scan(ScanTrigger::Schedule, None),
        )
        .await
        .expect("the next scan is not wedged")
        .expect("scan runs");
        assert_eq!(runner.runs()[1], default_run(ScanTarget::All));
        until_idle(&mgr).await;
    }

    /// The refresh race: the new scan task queues behind the one being
    /// cancelled before the cancellation lands. The cancelled scan stops at
    /// its next item boundary, and only then does the queued one run — not
    /// coalesced into nothing — and its caller returns once it has.
    #[tokio::test]
    async fn a_scan_queued_behind_a_cancelled_one_runs_after_it_stops() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        let first = spawn_scheduled(&mgr);
        runner.started(1).await;
        let second = spawn_scheduled(&mgr);
        until_queued(&mgr, 1, 1).await;
        first.abort();
        assert!(first.await.expect_err("aborted").is_cancelled());
        runner.release(1 + GatedRunner::ITEMS);
        tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("the queued run returns")
            .expect("joined");
        assert_eq!(
            runner.runs(),
            vec![
                Run {
                    items: 1,
                    stopped: true,
                    ..default_run(ScanTarget::All)
                },
                default_run(ScanTarget::All),
            ]
        );
        until_idle(&mgr).await;
    }

    /// Two callers waiting on one queued scan: cancelling one of them while
    /// the scan runs does not stop it — the other still wants it.
    #[tokio::test]
    async fn cancelling_one_of_two_waiters_keeps_their_shared_scan_running() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let a = spawn_scheduled(&mgr);
        let b = spawn_scheduled(&mgr);
        until_queued(&mgr, 1, 2).await;
        runner.release(GatedRunner::ITEMS);
        runner.started(2).await;
        a.abort();
        assert!(a.await.expect_err("aborted").is_cancelled());
        runner.release(GatedRunner::ITEMS);
        tokio::time::timeout(std::time::Duration::from_secs(5), b)
            .await
            .expect("the other waiter returns")
            .expect("joined");
        assert_eq!(
            runner.runs(),
            vec![default_run(ScanTarget::All), default_run(ScanTarget::All)],
            "the shared scan ran to the end"
        );
        until_idle(&mgr).await;
    }

    /// A caller cancelled while its request is still queued withdraws it: a
    /// cancelled request does not run.
    #[tokio::test]
    async fn a_cancelled_waiter_withdraws_its_queued_request() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let waiter = spawn_scheduled(&mgr);
        until_queued(&mgr, 1, 1).await;
        waiter.abort();
        assert!(waiter.await.expect_err("aborted").is_cancelled());
        until_queued(&mgr, 0, 0).await;
        runner.release(GatedRunner::ITEMS);
        until_idle(&mgr).await;
        assert_eq!(runner.runs(), vec![default_run(ScanTarget::All)]);
    }

    /// Two callers queued behind a scan that is cancelled: it stops at its
    /// item boundary, then both queued scans run in order and both callers
    /// return.
    #[tokio::test]
    async fn two_callers_queued_behind_a_cancelled_scan_both_run_in_order() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let library = Uuid::from_u128(0xA);

        let first = spawn_scheduled(&mgr);
        runner.started(1).await;
        let replace = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.run_scan(
                    ScanRequest::folder_refresh(ScanTarget::Library(library), replace_all()),
                    None,
                )
                .await
                .expect("scan runs");
            }
        });
        until_queued(&mgr, 1, 1).await;
        let full = spawn_scheduled(&mgr);
        until_queued(&mgr, 2, 2).await;
        first.abort();
        assert!(first.await.expect_err("aborted").is_cancelled());
        runner.release(1 + 2 * GatedRunner::ITEMS);
        for caller in [replace, full] {
            tokio::time::timeout(std::time::Duration::from_secs(5), caller)
                .await
                .expect("the caller returns")
                .expect("joined");
        }
        assert_eq!(
            runner.runs(),
            vec![
                Run {
                    items: 1,
                    stopped: true,
                    ..default_run(ScanTarget::All)
                },
                Run {
                    options: replace_all(),
                    ancestors: none_none(),
                    ..default_run(ScanTarget::Library(library))
                },
                default_run(ScanTarget::All),
            ]
        );
        until_idle(&mgr).await;
    }

    /// The Identify refresh (`run_refresh_scan`) goes into the priority lane:
    /// with a scan running, that scan serves it between two of its items, so
    /// the caller waits about one item — not for the scan — and the scan
    /// then goes on. Without a scanner it refuses rather than pretending.
    #[tokio::test]
    async fn an_identify_is_served_inside_the_running_scan() {
        let db = test_db().await;
        assert!(
            manager(&db)
                .run_refresh_scan(ScanTarget::All, &replace_all())
                .await
                .is_err(),
            "no scanner: refused"
        );
        let runner = GatedRunner::new();
        let mgr = Arc::new(manager(&db).with_scan_runner(runner.clone()));
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let item = ScanTarget::Items(vec!["/media/movies/Heat (1995)/Heat (1995).mkv".into()]);
        let waiting = tokio::spawn({
            let mgr = Arc::clone(&mgr);
            let item = item.clone();
            async move { mgr.run_refresh_scan(item, &replace_all()).await }
        });
        // Not queued behind the running scan: waiting in the lane.
        for _ in 0..500 {
            if !lock_queue(&mgr.scan_queue).lane.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!waiting.is_finished());
        // The running scan finishes one item and serves the lane.
        runner.release(1);
        let ran = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("returned without waiting for the scan")
            .expect("task")
            .expect("refresh");
        assert!(ran);
        assert_eq!(runner.served(), vec![(0, item)]);
        let runs = runner.runs();
        assert_eq!(runs.len(), 1, "no scan of its own");
        assert_eq!(runs[0].items, 1, "the scan is still running");
        runner.release(GatedRunner::ITEMS);
        until_idle(&mgr).await;
        assert_eq!(runner.runs()[0].items, GatedRunner::ITEMS);
    }

    /// A scan that panics while it serves a lane refresh (past the scanner's
    /// own guard) cannot strand the refresh's waiter: the worker settles it
    /// as failed, the waiter gets the error, and no worker is started for a
    /// request that no longer exists (it used to restart workers over an
    /// empty queue in a hot loop).
    #[tokio::test]
    async fn a_refresh_lost_with_a_panicking_scan_fails_its_waiter() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = Arc::new(manager(&db).with_scan_runner(runner.clone()));
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let waiting = tokio::spawn({
            let mgr = Arc::clone(&mgr);
            async move {
                mgr.run_refresh_scan(ScanTarget::Items(vec!["/m/a.mkv".into()]), &replace_all())
                    .await
            }
        });
        for _ in 0..500 {
            if !lock_queue(&mgr.scan_queue).lane.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        runner
            .panic_serving
            .store(true, std::sync::atomic::Ordering::SeqCst);
        runner.release(1);
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the waiter was not stranded")
            .expect("task")
            .expect_err("its refresh was lost");
        assert!(
            err.to_string().contains("ended before it finished"),
            "{err}"
        );
        until_idle(&mgr).await;
        // No worker churn: nothing more runs, and the queue stays quiet.
        let progress = *mgr.scan_progress.borrow();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*mgr.scan_progress.borrow(), progress, "no worker restarted");
        assert_eq!(runner.runs().len(), 1);
        // And the queue still works.
        runner.release(GatedRunner::ITEMS);
        mgr.run_refresh_scan(ScanTarget::Items(vec!["/m/b.mkv".into()]), &replace_all())
            .await
            .expect("the next refresh runs");
    }

    /// With no scan to serve it, a file item's refresh runs ahead of every
    /// queued scan (the lane first), and lane refreshes keep their order.
    #[tokio::test]
    async fn item_refreshes_run_ahead_of_queued_scans_in_order() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        runner
            .serves_lane
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let library = Uuid::from_u128(0xA);
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        mgr.queue_refresh_scan(ScanTarget::Library(library), &replace_all())
            .await
            .expect("folder refresh");
        let first = ScanTarget::Items(vec!["/m/a.mkv".into()]);
        let second = ScanTarget::Items(vec!["/m/b.mkv".into()]);
        for item in [&first, &second] {
            mgr.queue_refresh_scan(item.clone(), &replace_all())
                .await
                .expect("item refresh");
        }
        runner.release(4 * GatedRunner::ITEMS);
        until_idle(&mgr).await;
        let scopes: Vec<ScanTarget> = runner.runs().into_iter().map(|r| r.scope).collect();
        assert_eq!(
            scopes,
            vec![ScanTarget::All, first, second, ScanTarget::Library(library)]
        );
    }

    /// A by-name artist's refresh has no folder to validate: it is an item
    /// refresh, served by a running scan at its next item boundary (so its
    /// music pass never runs beside the scan's); an artist whose albums'
    /// folders must be validated queues like a folder refresh.
    #[tokio::test]
    async fn a_by_name_artist_refresh_rides_the_lane() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let by_name = ScanTarget::Artist {
            id: Uuid::from_u128(0xA1),
            path: None,
            folders: Vec::new(),
        };
        let with_folders = ScanTarget::Artist {
            id: Uuid::from_u128(0xA2),
            path: None,
            folders: vec!["/music/Artist".to_owned()],
        };
        for target in [&by_name, &with_folders] {
            mgr.queue_refresh_scan(target.clone(), &replace_all())
                .await
                .expect("artist refresh");
        }
        runner.release(3 * GatedRunner::ITEMS);
        until_idle(&mgr).await;
        assert_eq!(
            runner.served(),
            vec![(0, by_name)],
            "served inside the scan"
        );
        let scopes: Vec<ScanTarget> = runner.runs().into_iter().map(|r| r.scope).collect();
        assert_eq!(
            scopes,
            vec![ScanTarget::All, with_folders],
            "queued after it"
        );
    }

    /// A running scan serves every waiting lane refresh at its next item
    /// boundary, in the order they were queued.
    #[tokio::test]
    async fn a_running_scan_serves_the_lane_in_order() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        let targets: Vec<ScanTarget> = ["/m/a.mkv", "/m/b.mkv", "/m/c.mkv"]
            .iter()
            .map(|p| ScanTarget::Items(vec![(*p).to_owned()]))
            .collect();
        for target in &targets {
            mgr.queue_refresh_scan(target.clone(), &replace_all())
                .await
                .expect("item refresh");
        }
        runner.release(GatedRunner::ITEMS);
        until_idle(&mgr).await;
        assert_eq!(
            runner.served(),
            targets.into_iter().map(|t| (0, t)).collect::<Vec<_>>()
        );
        assert_eq!(runner.runs().len(), 1);
    }

    /// A scan that panics must not wedge scanning either: it is contained in
    /// its own task, and what was queued behind it still runs.
    #[tokio::test]
    async fn a_panicking_scan_does_not_wedge_the_next_one() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let library = Uuid::from_u128(0xA);

        // Fire-and-forget scan that panics once released.
        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        // A folder refresh queued behind it.
        mgr.queue_refresh_scan(ScanTarget::Library(library), &replace_all())
            .await
            .expect("queued");
        until_queued(&mgr, 1, 0).await;
        runner
            .panic_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        runner.release(1 + GatedRunner::ITEMS);
        until_idle(&mgr).await;
        let runs = runner.runs();
        assert_eq!(runs.len(), 2, "the queued refresh ran after the panic");
        assert_eq!(
            runs[1],
            Run {
                options: replace_all(),
                ancestors: none_none(),
                ..default_run(ScanTarget::Library(library))
            }
        );

        runner.release(GatedRunner::ITEMS);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mgr.run_library_scan(ScanTrigger::Schedule, None),
        )
        .await
        .expect("not wedged")
        .expect("scan runs");
        assert_eq!(runner.runs().len(), 3);
    }

    /// "Replace all metadata" on library A queued with a default full scan
    /// behind a running scan: both run, in order, and the replace applies to
    /// A only — the default scan neither swallows it nor is widened by it.
    #[tokio::test]
    async fn a_replace_on_one_library_and_a_default_full_scan_both_run() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let library = Uuid::from_u128(0xA);

        mgr.queue_library_scan().await.expect("queued");
        runner.started(1).await;
        mgr.queue_refresh_scan(ScanTarget::Library(library), &replace_all())
            .await
            .expect("queued");
        until_queued(&mgr, 1, 0).await;
        mgr.queue_library_scan().await.expect("queued");
        until_queued(&mgr, 2, 0).await;
        runner.release(3 * GatedRunner::ITEMS);
        until_idle(&mgr).await;
        assert_eq!(
            runner.runs(),
            vec![
                default_run(ScanTarget::All),
                Run {
                    options: replace_all(),
                    ancestors: none_none(),
                    ..default_run(ScanTarget::Library(library))
                },
                default_run(ScanTarget::All),
            ]
        );
    }

    /// The scheduled task fails when its scan failed or panicked (upstream
    /// records the exception as a `Failed` run), and gets the scan's
    /// progress while it runs.
    #[tokio::test]
    async fn the_waiting_run_gets_its_scans_progress_and_failure() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink: ScanProgressSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |percent| seen.lock().expect("seen").push(percent))
        };
        runner.release(GatedRunner::ITEMS);
        assert!(
            mgr.run_library_scan(ScanTrigger::Schedule, Some(sink))
                .await
                .expect("scan runs")
        );
        assert_eq!(*seen.lock().expect("seen"), vec![48.0, 96.0]);

        runner
            .fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        runner.release(GatedRunner::ITEMS);
        let err = mgr
            .run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect_err("failed");
        assert!(err.to_string().contains("disk full"), "{err}");

        runner
            .panic_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        runner.release(GatedRunner::ITEMS);
        let err = mgr
            .run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect_err("panicked");
        assert!(err.to_string().contains("panicked"), "{err}");

        runner.release(GatedRunner::ITEMS);
        mgr.run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("scanning goes on");
        until_idle(&mgr).await;
    }

    /// The dashboard's "Scan Media Library": the task's progress moves with
    /// the scan (`GET /ScheduledTasks` shows it while it runs), and a scan
    /// that failed records the run as `Failed`, not `Completed`.
    #[tokio::test]
    async fn the_scan_media_library_task_shows_progress_and_fails_with_its_scan() {
        use crate::scheduled_tasks::{FerrofinTaskManager, RefreshLibraryTask};
        use ferrofin_model::tasks::{TaskCompletionStatus, TaskState};
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let tasks = FerrofinTaskManager::new();
        tasks.register(Arc::new(RefreshLibraryTask::new(Arc::new(mgr))));
        let info = |tasks: &FerrofinTaskManager| tasks.get("RefreshLibrary").expect("task");
        let until_idle = |tasks: FerrofinTaskManager| async move {
            for _ in 0..500 {
                if info(&tasks).state == TaskState::Idle {
                    return info(&tasks);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the task never finished");
        };

        tasks.queue("RefreshLibrary").expect("queued");
        runner.started(1).await;
        runner.release(1);
        for _ in 0..500 {
            if info(&tasks).current_progress_percentage == Some(48.0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let running = info(&tasks);
        assert_eq!(running.state, TaskState::Running);
        assert_eq!(running.current_progress_percentage, Some(48.0));
        runner.release(1);
        let done = until_idle(tasks.clone()).await;
        assert_eq!(
            done.last_execution_result.expect("result").status,
            TaskCompletionStatus::Completed
        );

        runner
            .fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tasks.queue("RefreshLibrary").expect("queued");
        runner.release(GatedRunner::ITEMS);
        let failed = until_idle(tasks.clone()).await;
        let result = failed.last_execution_result.expect("result");
        assert_eq!(result.status, TaskCompletionStatus::Failed);
        assert!(
            result
                .error_message
                .is_some_and(|m| m.contains("disk full")),
            "the scan's error is the run's"
        );
    }

    /// A scan the host's shutdown stops — or refuses, once it has shut
    /// down — ends the "Scan Media Library" run as `Cancelled`, not
    /// `Completed` (upstream's `OperationCanceledException` out of
    /// `ExecuteAsync`).
    #[tokio::test]
    async fn the_scan_media_library_task_is_cancelled_when_shutdown_stops_its_scan() {
        use crate::scheduled_tasks::{FerrofinTaskManager, RefreshLibraryTask};
        use ferrofin_model::tasks::{TaskCompletionStatus, TaskState};
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let tasks = FerrofinTaskManager::new();
        tasks.register(Arc::new(RefreshLibraryTask::new(Arc::new(mgr.clone()))));
        let status = |tasks: FerrofinTaskManager| async move {
            for _ in 0..500 {
                let info = tasks.get("RefreshLibrary").expect("task");
                if info.state == TaskState::Idle {
                    return info.last_execution_result.expect("result").status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the task never finished");
        };

        tasks.queue("RefreshLibrary").expect("queued");
        runner.started(1).await;
        let shutdown = tokio::spawn({
            let mgr = mgr.clone();
            async move { mgr.shutdown_scans().await }
        });
        // The running scan is cancelled under the same lock that closes the
        // queue; then its item in progress may finish.
        while !lock_queue(&mgr.scan_queue).closed {
            tokio::task::yield_now().await;
        }
        runner.release(1);
        shutdown.await.expect("joined");
        assert_eq!(
            status(tasks.clone()).await,
            TaskCompletionStatus::Cancelled,
            "a stopped scan cancels the run"
        );

        tasks.queue("RefreshLibrary").expect("queued");
        assert_eq!(
            status(tasks.clone()).await,
            TaskCompletionStatus::Cancelled,
            "a refused scan cancels the run"
        );
        assert_eq!(runner.runs().len(), 1, "nothing ran after shutdown");
    }

    /// The host's teardown: the running scan stops at its next item
    /// boundary and is awaited, the queued ones are dropped (their callers
    /// return), and nothing is queued any more.
    #[tokio::test]
    async fn shutdown_stops_the_running_scan_and_refuses_new_ones() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        let first = spawn_scheduled(&mgr);
        runner.started(1).await;
        mgr.queue_refresh_scan(ScanTarget::Library(Uuid::from_u128(0xA)), &replace_all())
            .await
            .expect("queued");
        let queued = spawn_scheduled(&mgr);
        until_queued(&mgr, 2, 1).await;

        let shutdown = tokio::spawn({
            let mgr = mgr.clone();
            async move { mgr.shutdown_scans().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !shutdown.is_finished(),
            "shutdown waits for the item in progress"
        );
        runner.release(1);
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown returns")
            .expect("joined");
        assert!(mgr.scan_idle());
        for caller in [first, queued] {
            tokio::time::timeout(std::time::Duration::from_secs(5), caller)
                .await
                .expect("the caller returns")
                .expect("joined");
        }
        assert_eq!(
            runner.runs(),
            vec![Run {
                items: 1,
                stopped: true,
                ..default_run(ScanTarget::All)
            }],
            "the running scan stopped at its boundary; the queued ones never ran"
        );

        mgr.queue_library_scan().await.expect("refused quietly");
        assert!(
            !mgr.run_library_scan(ScanTrigger::Schedule, None)
                .await
                .expect("refused quietly"),
            "a refused scan did not run to its end"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runner.runs().len(), 1, "nothing runs after shutdown");
        assert!(mgr.scan_idle());
    }

    /// A worker whose task dies (the runtime dropping it, or a panic in the
    /// worker itself) aborts the scan it was running — it cannot overlap the
    /// next one — and hands the request back to its waiter, which starts a
    /// new worker.
    #[tokio::test]
    async fn a_worker_that_dies_hands_its_scan_back_to_the_waiter() {
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());

        let waiter = spawn_scheduled(&mgr);
        runner.started(1).await;
        lock_queue(&mgr.scan_queue)
            .worker_task
            .take()
            .expect("a worker")
            .abort();
        // The waiter restarts the request on a new worker.
        runner.started(2).await;
        runner.release(GatedRunner::ITEMS);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the waiter returns")
            .expect("joined");
        let runs = runner.runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].items, 0, "the aborted scan did no more work");
        assert_eq!(runs[1], default_run(ScanTarget::All));
        until_idle(&mgr).await;
    }

    fn request(scope: ScanTarget, options: MetadataRefreshOptions) -> ScanRequest {
        ScanRequest {
            scope,
            options,
            ancestors: MetadataRefreshOptions::default(),
            trigger: ScanTrigger::Api,
            passes: ScanPasses::Library,
            priority: false,
        }
    }

    fn scopes(queue: &ScanQueue) -> Vec<(ScanTarget, bool)> {
        queue
            .pending
            .iter()
            .map(|p| {
                (
                    p.request.scope.clone(),
                    p.request.options == MetadataRefreshOptions::default(),
                )
            })
            .collect()
    }

    fn paths(list: &[&str]) -> ScanTarget {
        ScanTarget::Paths(list.iter().map(|p| (*p).to_owned()).collect())
    }

    #[test]
    fn queued_path_scans_union_only_when_they_refresh_alike() {
        let mut queue = ScanQueue::default();
        let a = queue.enqueue(
            request(paths(&["/m/a"]), MetadataRefreshOptions::default()),
            true,
        );
        let b = queue.enqueue(
            request(paths(&["/m/b", "/m/a"]), MetadataRefreshOptions::default()),
            true,
        );
        queue.enqueue(
            request(paths(&["/m/d"]), MetadataRefreshOptions::default()),
            false,
        );
        let c = queue.enqueue(request(paths(&["/m/c"]), replace_all()), true);
        // A folder refresh's path scan (its ancestors take `None`/`None`)
        // does not join the watcher's, even with equal options.
        queue.enqueue(
            ScanRequest::folder_refresh(paths(&["/m/e"]), MetadataRefreshOptions::default()),
            false,
        );
        assert_eq!(
            scopes(&queue),
            vec![
                (paths(&["/m/a", "/m/b", "/m/d"]), true),
                (paths(&["/m/c"]), false),
                (paths(&["/m/e"]), true),
            ]
        );
        assert_eq!(
            queue.pending[0].waiters.tickets,
            vec![a.unwrap(), b.unwrap()]
        );
        assert_eq!(queue.pending[0].waiters.detached, 1);
        assert_eq!(queue.pending[1].waiters.tickets, vec![c.unwrap()]);
    }

    #[test]
    fn a_queued_full_scan_covers_library_and_path_scans_that_refresh_alike() {
        let mut queue = ScanQueue::default();
        let lib = ScanTarget::Library(Uuid::from_u128(1));
        let replace = queue.enqueue(request(lib.clone(), replace_all()), true);
        let p = queue.enqueue(
            request(paths(&["/m/a"]), MetadataRefreshOptions::default()),
            true,
        );
        let l = queue.enqueue(
            request(lib.clone(), MetadataRefreshOptions::default()),
            false,
        );
        assert_eq!(l, None, "a fire-and-forget request gets no ticket");
        let full = queue.enqueue(
            request(ScanTarget::All, MetadataRefreshOptions::default()),
            true,
        );
        // The full scan takes the earliest covered scan's place and their
        // waiters; the replace (other options) is untouched.
        assert_eq!(
            scopes(&queue),
            vec![(lib.clone(), false), (ScanTarget::All, true)]
        );
        assert_eq!(queue.pending[0].waiters.tickets, vec![replace.unwrap()]);
        assert_eq!(
            queue.pending[1].waiters.tickets,
            vec![full.unwrap(), p.unwrap()]
        );
        assert_eq!(queue.pending[1].waiters.detached, 1);
        // Later library/path scans that refresh alike join it.
        let late_p = queue.enqueue(
            request(paths(&["/m/z"]), MetadataRefreshOptions::default()),
            true,
        );
        let late_l = queue.enqueue(
            request(lib.clone(), MetadataRefreshOptions::default()),
            true,
        );
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(
            queue.pending[1].waiters.tickets,
            vec![full.unwrap(), p.unwrap(), late_p.unwrap(), late_l.unwrap()]
        );
        // A second full scan of the same options joins too.
        queue.enqueue(
            request(ScanTarget::All, MetadataRefreshOptions::default()),
            false,
        );
        assert_eq!(queue.pending.len(), 2);
    }

    #[test]
    fn a_queued_full_scan_does_not_swallow_other_options() {
        let mut queue = ScanQueue::default();
        queue.enqueue(
            request(ScanTarget::All, MetadataRefreshOptions::default()),
            false,
        );
        let lib = ScanTarget::Library(Uuid::from_u128(1));
        queue.enqueue(request(lib.clone(), replace_all()), false);
        queue.enqueue(request(paths(&["/m/a"]), replace_all()), false);
        assert_eq!(
            scopes(&queue),
            vec![
                (ScanTarget::All, true),
                (lib, false),
                (paths(&["/m/a"]), false)
            ]
        );
    }

    #[test]
    fn a_library_scan_never_becomes_a_full_scan() {
        let mut queue = ScanQueue::default();
        let a = ScanTarget::Library(Uuid::from_u128(1));
        let b = ScanTarget::Library(Uuid::from_u128(2));
        queue.enqueue(request(a.clone(), MetadataRefreshOptions::default()), false);
        queue.enqueue(
            request(paths(&["/m/x"]), MetadataRefreshOptions::default()),
            false,
        );
        queue.enqueue(request(b.clone(), MetadataRefreshOptions::default()), false);
        // The same library again joins its queued scan.
        queue.enqueue(request(a.clone(), MetadataRefreshOptions::default()), false);
        assert_eq!(
            scopes(&queue),
            vec![(a, true), (paths(&["/m/x"]), true), (b, true)]
        );
        assert_eq!(queue.pending[0].waiters.detached, 2);
        assert!(
            queue
                .pending
                .iter()
                .all(|p| p.request.scope != ScanTarget::All)
        );
    }

    fn changed(list: &[&str]) -> ScanTarget {
        ScanTarget::Changed(list.iter().map(|p| (*p).to_owned()).collect())
    }

    /// The library monitor's request (the watcher, the *arr webhooks):
    /// `ChangedExternally`'s default refresh of what the paths belong to,
    /// nothing for the folders above it, and only the touched items'
    /// closing passes — never a full scan.
    #[test]
    fn a_watcher_report_is_a_changed_path_scan_of_the_default_options() {
        let request = ScanRequest::changed(vec!["/m/a.mkv".to_owned()], ScanTrigger::Watcher);
        assert_eq!(request.scope, changed(&["/m/a.mkv"]));
        assert_eq!(request.options, MetadataRefreshOptions::default());
        assert_eq!(request.ancestors, none_none());
        assert_eq!(request.passes, ScanPasses::Touched);
        assert_eq!(request.trigger, ScanTrigger::Watcher);
        assert!(!request.priority);
    }

    /// A watcher report never escalates: beside a queued library scan it
    /// queues on its own (no full scan appears), reports union among
    /// themselves only, and a folder refresh's path scan never joins them.
    #[test]
    fn changed_path_scans_union_and_never_turn_a_library_scan_into_a_full_one() {
        let mut queue = ScanQueue::default();
        let lib = ScanTarget::Library(Uuid::from_u128(1));
        queue.enqueue(
            request(lib.clone(), MetadataRefreshOptions::default()),
            false,
        );
        queue.enqueue(
            ScanRequest::changed(vec!["/m/a".to_owned()], ScanTrigger::Watcher),
            false,
        );
        queue.enqueue(
            ScanRequest::folder_refresh(paths(&["/m/f"]), MetadataRefreshOptions::default()),
            false,
        );
        queue.enqueue(
            ScanRequest::changed(
                vec!["/m/b".to_owned(), "/m/a".to_owned()],
                ScanTrigger::Watcher,
            ),
            false,
        );
        // A library scan arriving after the report does not swallow it either.
        let other = ScanTarget::Library(Uuid::from_u128(2));
        queue.enqueue(
            request(other.clone(), MetadataRefreshOptions::default()),
            false,
        );
        assert_eq!(
            scopes(&queue),
            vec![
                (lib, true),
                (changed(&["/m/a", "/m/b"]), true),
                (paths(&["/m/f"]), true),
                (other, true),
            ]
        );
        assert_eq!(queue.pending[1].waiters.detached, 2);
        assert!(
            queue
                .pending
                .iter()
                .all(|p| p.request.scope != ScanTarget::All),
            "no request escalated into a full scan"
        );
    }

    /// A webhook batch joins a queued watcher batch like any two reports
    /// (the trigger never changes what runs); the joined scan keeps the
    /// queued request's trigger.
    #[test]
    fn webhook_and_watcher_batches_join_under_the_queued_trigger() {
        let mut queue = ScanQueue::default();
        queue.enqueue(
            ScanRequest::changed(vec!["/m/a".to_owned()], ScanTrigger::Watcher),
            false,
        );
        queue.enqueue(
            ScanRequest::changed(vec!["/m/b".to_owned()], ScanTrigger::Webhook),
            false,
        );
        queue.enqueue(
            ScanRequest::changed(vec!["/m/c".to_owned()], ScanTrigger::Watcher),
            false,
        );
        queue.enqueue(
            ScanRequest::changed(vec!["/m/d".to_owned()], ScanTrigger::Webhook),
            false,
        );
        let queued: Vec<(ScanTarget, ScanTrigger)> = queue
            .pending
            .iter()
            .map(|p| (p.request.scope.clone(), p.request.trigger))
            .collect();
        assert_eq!(
            queued,
            vec![(
                changed(&["/m/a", "/m/b", "/m/c", "/m/d"]),
                ScanTrigger::Watcher
            )]
        );
    }

    /// A report that arrives while a full scan is still queued joins it (the
    /// full scan will see the file); a full scan queued after reports takes
    /// them over. A full scan of other options covers neither.
    #[test]
    fn a_queued_full_scan_covers_changed_path_scans() {
        let mut queue = ScanQueue::default();
        queue.enqueue(
            ScanRequest::changed(vec!["/m/a".to_owned()], ScanTrigger::Watcher),
            false,
        );
        queue.enqueue(request(ScanTarget::All, replace_all()), false);
        let full = queue.enqueue(
            request(ScanTarget::All, MetadataRefreshOptions::default()),
            true,
        );
        assert_eq!(
            scopes(&queue),
            vec![(ScanTarget::All, true), (ScanTarget::All, false)],
            "the default full scan took the report's place"
        );
        let late = queue.enqueue(
            ScanRequest::changed(vec!["/m/z".to_owned()], ScanTrigger::Watcher),
            false,
        );
        assert_eq!(late, None);
        assert_eq!(
            queue.pending.len(),
            2,
            "the late report joined the full scan"
        );
        assert_eq!(queue.pending[0].waiters.tickets, vec![full.unwrap()]);
        assert_eq!(queue.pending[0].waiters.detached, 2);
    }

    #[test]
    fn a_withdrawn_request_leaves_the_queue_unless_someone_else_wants_it() {
        let mut queue = ScanQueue::default();
        let lib = ScanTarget::Library(Uuid::from_u128(1));
        let alone = queue
            .enqueue(request(lib, MetadataRefreshOptions::default()), true)
            .unwrap();
        let shared_a = queue
            .enqueue(
                request(paths(&["/m/a"]), MetadataRefreshOptions::default()),
                true,
            )
            .unwrap();
        let shared_b = queue
            .enqueue(
                request(paths(&["/m/b"]), MetadataRefreshOptions::default()),
                true,
            )
            .unwrap();
        let detached = queue.enqueue(request(ScanTarget::All, replace_all()), false);
        assert_eq!(detached, None);
        let joined = queue
            .enqueue(request(ScanTarget::All, replace_all()), true)
            .unwrap();
        queue.withdraw(alone);
        queue.withdraw(shared_a);
        queue.withdraw(joined);
        assert_eq!(
            scopes(&queue),
            vec![(paths(&["/m/a", "/m/b"]), true), (ScanTarget::All, false)],
            "the path scan still has a waiter; the full scan a detached request"
        );
        assert_eq!(queue.pending[0].waiters.tickets, vec![shared_b]);
        // A finished ticket is collected by the withdrawal too: no leak.
        queue.finished.insert(shared_b, ScanResult::Completed);
        queue.withdraw(shared_b);
        assert!(queue.finished.is_empty());
    }

    /// Withdrawing from the RUNNING scan stops it only when nobody else —
    /// no other caller, no fire-and-forget request — wants it.
    #[test]
    fn a_running_scan_is_cancelled_only_when_its_last_waiter_withdraws() {
        let running = |tickets: Vec<u64>, detached| ScanQueue {
            worker: true,
            running: Some(RunningScan {
                waiters: Waiters { tickets, detached },
                cancel: ScanCancel::new(),
            }),
            ..ScanQueue::default()
        };
        let cancelled = |queue: &ScanQueue| {
            queue
                .running
                .as_ref()
                .is_some_and(|r| r.cancel.is_cancelled())
        };

        let mut queue = running(vec![1, 2], 0);
        queue.withdraw(1);
        assert!(!cancelled(&queue), "caller 2 still waits");
        queue.withdraw(2);
        assert!(cancelled(&queue));

        let mut queue = running(vec![1], 1);
        queue.withdraw(1);
        assert!(
            !cancelled(&queue),
            "a fire-and-forget request still wants it"
        );

        let mut queue = running(vec![1], 0);
        queue.withdraw(7);
        assert!(!cancelled(&queue), "an unrelated ticket changes nothing");
    }

    #[tokio::test]
    async fn queue_library_scan_scoped_walks_only_that_library() {
        use crate::file_system::FerrofinFileSystem;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
        use ferrofin_model::entities::CollectionTypeOptions;
        use ferrofin_traits::library::VirtualFolderManager;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        std::fs::write(tv.join("Firefly/Season 01/Firefly S01E01.mkv"), b"").unwrap();

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, ct, media) in [
            ("Movies", CollectionTypeOptions::movies, &movies),
            ("TV", CollectionTypeOptions::tvshows, &tv),
        ] {
            vf.add_virtual_folder(
                name,
                Some(ct),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        }
        let tv_cf = vf
            .get_virtual_folders()
            .await
            .unwrap()
            .iter()
            .find(|f| f.name.as_deref() == Some("TV"))
            .and_then(|f| f.item_id.as_deref())
            .map(|s| Uuid::parse_str(s).unwrap())
            .unwrap();
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        mgr.queue_library_scan_scoped(tv_cf).await.expect("queued");
        // The scan runs on a spawned task; poll (through the manager's own query
        // API, not raw SQL — the SQL-boundary ratchet) until it lands the episode.
        let count = |kind| {
            let mgr = &mgr;
            async move {
                mgr.query_items(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..Default::default()
                })
                .await
                .expect("query")
                .items
                .len()
            }
        };
        for _ in 0..200 {
            if count(BaseItemKind::Episode).await == 1 && mgr.scan_idle() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            count(BaseItemKind::Episode).await,
            1,
            "the scoped TV library was scanned"
        );
        assert_eq!(
            count(BaseItemKind::Movie).await,
            0,
            "the movie library must not be scanned"
        );
    }

    /// `LibraryScanTrigger::queue_scan_paths` has no full-scan default any
    /// more; the manager's queues the reported paths as a changed-path scan.
    /// A report that arrives while a full scan RUNS is not folded into it
    /// (the scan may be past its files): it queues and runs, scoped, after.
    #[tokio::test]
    async fn a_report_during_a_running_full_scan_runs_after_it_scoped() {
        use crate::library_monitor::LibraryScanTrigger;
        let db = test_db().await;
        let runner = GatedRunner::new();
        let mgr = manager(&db).with_scan_runner(runner.clone());
        let waiter = spawn_scheduled(&mgr);
        runner.started(1).await;
        mgr.queue_scan_paths(
            vec!["/media/tv/Show/Season 1/e.mkv".to_owned()],
            ScanTrigger::Watcher,
        )
        .await
        .expect("queued");
        until_queued(&mgr, 1, 0).await;
        runner.release(2 * GatedRunner::ITEMS);
        waiter.await.expect("joined");
        until_idle(&mgr).await;
        assert_eq!(
            runner.runs(),
            vec![
                default_run(ScanTarget::All),
                Run {
                    ancestors: none_none(),
                    ..default_run(changed(&["/media/tv/Show/Season 1/e.mkv"]))
                },
            ]
        );
    }

    #[tokio::test]
    async fn queue_scan_paths_ingests_only_the_changed_paths() {
        use crate::file_system::FerrofinFileSystem;
        use crate::library_monitor::LibraryScanTrigger;
        use crate::library_scan::LibraryScanner;
        use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
        use ferrofin_model::entities::CollectionTypeOptions;
        use ferrofin_traits::library::VirtualFolderManager;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        let episode = tv.join("Firefly/Season 01/Firefly S01E01.mkv");
        std::fs::write(&episode, b"").unwrap();

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, ct, media) in [
            ("Movies", CollectionTypeOptions::movies, &movies),
            ("TV", CollectionTypeOptions::tvshows, &tv),
        ] {
            vf.add_virtual_folder(
                name,
                Some(ct),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        }
        let scanner = Arc::new(LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        ));
        let mgr = manager(&db).with_scanner(scanner);

        // Report the episode's path (what the monitor dispatches after a settle
        // window): its hierarchy lands, the movie library is never planned.
        mgr.queue_scan_paths(
            vec![episode.to_string_lossy().into_owned()],
            ScanTrigger::Webhook,
        )
        .await
        .expect("queued");
        let count = |kind| {
            let mgr = &mgr;
            async move {
                mgr.query_items(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..Default::default()
                })
                .await
                .expect("query")
                .items
                .len()
            }
        };
        for _ in 0..200 {
            if count(BaseItemKind::Episode).await == 1 && mgr.scan_idle() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(count(BaseItemKind::Episode).await, 1, "the episode landed");
        assert_eq!(count(BaseItemKind::Series).await, 1, "with its series");
        assert_eq!(
            count(BaseItemKind::Movie).await,
            0,
            "the untouched movie library must not be scanned"
        );
    }

    /// Builds a manager backed by real repositories over the given database.
    fn manager(db: &Database) -> FerrofinLibraryManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinLibraryManager::new(
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup.clone())),
            Arc::new(FerrofinItemCountService::new(db.clone())),
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            Arc::new(FerrofinPeopleRepository::new(db.clone())),
        )
    }

    #[tokio::test]
    async fn extra_series_owners_honor_live_library_grouping_options() {
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
        use ferrofin_traits::library::VirtualFolderManager;
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let vf = Arc::new(crate::FerrofinVirtualFolderManager::new(
            tmp.path().join("views"),
        ));
        vf.add_virtual_folder(
            "TV",
            Some(ferrofin_model::entities::CollectionTypeOptions::tvshows),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: tmp.path().to_string_lossy().into_owned(),
                }],
                enable_automatic_series_grouping: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut items = Vec::new();
        for name in ["A", "B"] {
            items.push(BaseItemEntity {
                id: guid_to_db(Uuid::new_v4()),
                type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
                path: Some(tmp.path().join(name).to_string_lossy().into_owned()),
                name: Some(name.into()),
                presentation_unique_key: Some("same-series".into()),
                ..Default::default()
            });
        }
        let store = FerrofinItemPersistenceService::new(db.clone());
        store.save_items(&items).await.unwrap();
        let mgr = manager(&db).with_virtual_folders(vf.clone());
        let id = Uuid::parse_str(&items[0].id).unwrap();
        let got = mgr.get_extra_owner_ids_batch(&items).await.unwrap();
        assert_eq!(got[&id], vec![id]);
        let folder = vf.get_virtual_folders().await.unwrap().remove(0);
        let mut options = folder.library_options.unwrap();
        options.enable_automatic_series_grouping = true;
        vf.update_library_options("TV", &options).await.unwrap();
        assert_eq!(
            mgr.get_extra_owner_ids_batch(&items).await.unwrap()[&id].len(),
            2
        );
    }

    /// `GET /MusicGenres/{name}` (and its `/Genres`, `/Studios`, `/Artists`
    /// siblings) is `CreateItemByName<T>` upstream: the row is MATERIALIZED by
    /// the lookup, which is why Jellyfin answers 200 for a name the library does
    /// not carry and Ferrofin used to answer 404.
    #[tokio::test]
    async fn a_by_name_lookup_materializes_the_item_the_way_create_item_by_name_does() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = test_db().await;
        let meta = tmp.path().join("metadata");
        let store = crate::by_name_store::ByNameStore::new(
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            meta.join("Genre"),
            meta.join("MusicGenre"),
            meta.join("Studio"),
            meta.join("artists"),
        );
        let mgr = manager(&db).with_by_name_store(store);

        for (kind, dir) in [
            (BaseItemKind::MusicGenre, "MusicGenre"),
            (BaseItemKind::Genre, "Genre"),
            (BaseItemKind::Studio, "Studio"),
            (BaseItemKind::MusicArtist, "artists"),
        ] {
            let row = mgr
                .get_named_item(kind, "Zzznope")
                .await
                .expect("lookup")
                .unwrap_or_else(|| panic!("{kind:?} materialized"));
            assert_eq!(row.name.as_deref(), Some("Zzznope"));
            assert_eq!(
                row.path.as_deref(),
                Some(meta.join(dir).join("Zzznope").to_string_lossy().as_ref()),
                "{kind:?} carries its metadata path"
            );
        }

        // Person is NOT a `CreateItemByName` kind — `GetPerson` is a plain
        // query, and Jellyfin 404s an unknown person. Materializing one here
        // would invent an item Jellyfin does not have.
        assert!(
            mgr.get_named_item(BaseItemKind::Person, "Zzznope")
                .await
                .expect("lookup")
                .is_none()
        );
    }

    /// The id-only resolution is the per-credit hot path (every credited name on
    /// every DTO page). C# splits it the same way — `GetItemByNameId` derives,
    /// only `CreateItemByName` writes — so this must stay read-only.
    #[tokio::test]
    async fn the_by_name_id_lookup_never_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = test_db().await;
        let meta = tmp.path().join("metadata");
        let mgr = manager(&db).with_by_name_store(crate::by_name_store::ByNameStore::new(
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            meta.join("Genre"),
            meta.join("MusicGenre"),
            meta.join("Studio"),
            meta.join("artists"),
        ));

        let ids = mgr
            .get_named_item_ids(BaseItemKind::Genre, &["Zzznope".to_owned()])
            .await
            .expect("ids");
        assert_eq!(ids, vec![None], "an unknown name resolves to nothing");
        assert!(
            mgr.get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Genre],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("list")
            .is_empty(),
            "no row was written on the id path"
        );
    }

    #[tokio::test]
    async fn update_items_reindexes_the_filter_facets() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x77);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        // A metadata edit sets genres/tags on the row; the filter facets read
        // `ItemValues`, so the update must re-index them without a rescan.
        let mut row = mgr.get_item_by_id(id).await.expect("read").expect("row");
        row.genres = Some("Sci-Fi".to_owned());
        row.tags = Some("4K|Christmas".to_owned());
        mgr.update_items(&[row], None).await.expect("update");

        let facets = mgr
            .get_query_filters_legacy(&InternalItemsQuery::default())
            .await
            .expect("facets");
        assert_eq!(facets.genres, vec!["Sci-Fi".to_owned()]);
        assert_eq!(facets.tags, vec!["4K".to_owned(), "Christmas".to_owned()]);
    }

    /// The metadata editor's external ids reach the `BaseItemProviders` table and
    /// REPLACE what was there — the write behind C# `item.ProviderIds = request.ProviderIds`.
    #[tokio::test]
    async fn update_item_provider_ids_replaces_the_stored_set() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x7A);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        mgr.update_item_provider_ids(id, &[("Tvdb".to_owned(), "1".to_owned())])
            .await
            .expect("seed");
        mgr.update_item_provider_ids(
            id,
            &[
                ("Imdb".to_owned(), "tt0069293".to_owned()),
                ("Tmdb".to_owned(), "593".to_owned()),
            ],
        )
        .await
        .expect("replace");

        let ids = mgr
            .get_item_list(&InternalItemsQuery {
                any_provider_id_equals: vec![("Imdb".to_owned(), "tt0069293".to_owned())],
                ..Default::default()
            })
            .await
            .expect("lookup by the new id");
        assert_eq!(ids.len(), 1, "the new id resolves the item");
        let stale = mgr
            .get_item_list(&InternalItemsQuery {
                any_provider_id_equals: vec![("Tvdb".to_owned(), "1".to_owned())],
                ..Default::default()
            })
            .await
            .expect("lookup by the replaced id");
        assert!(stale.is_empty(), "the replaced key is gone, not merged");
    }

    #[tokio::test]
    async fn get_item_by_id_reads_seeded_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(7);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        let item = mgr.get_item_by_id(id).await.expect("read").expect("some");
        assert_eq!(item.name.as_deref(), Some("Solaris"));
        // A nil id short-circuits to None without hitting the pool.
        assert!(
            mgr.get_item_by_id(Uuid::nil())
                .await
                .expect("nil")
                .is_none()
        );
    }

    #[tokio::test]
    async fn scoped_lookup_requires_visibility_and_keeps_raw_access() {
        let db = test_db().await;
        let id = Uuid::from_u128(71);
        seed_named_item(&db, id, BaseItemKind::Movie, "Scoped").await;
        let mgr = manager(&db);
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(72)).await;
        assert!(
            mgr.get_item_by_id_for_user(id, None)
                .await
                .expect("raw")
                .is_some()
        );
        assert!(matches!(
            mgr.get_item_by_id_for_user(id, Some(&user)).await,
            Err(ferrofin_traits::error::ServiceError::Backend(_))
        ));
        assert!(
            mgr.get_item_by_id_for_user(Uuid::nil(), Some(&user))
                .await
                .expect("missing")
                .is_none()
        );
        assert!(
            mgr.get_visible_item_ids(&[], &user)
                .await
                .expect("empty")
                .is_empty()
        );
        assert!(mgr.get_visible_item_ids(&[id], &user).await.is_err());
    }

    #[tokio::test]
    async fn item_exists_agrees_with_get_item_by_id() {
        // The image routes gate their 404 on `item_exists`, so it must answer
        // exactly what `get_item_by_id(..).is_some()` answers — including for
        // the two ids that are "not an item": the nil id and the placeholder
        // row the initial migration seeds.
        let db = test_db().await;
        let id = Uuid::from_u128(11);
        seed_named_item(&db, id, BaseItemKind::Movie, "Stalker").await;
        let mgr = manager(&db);

        for probe in [
            id,
            Uuid::from_u128(0xABBA),
            Uuid::nil(),
            PLACEHOLDER_ITEM_ID,
        ] {
            let by_row = mgr.get_item_by_id(probe).await.expect("row").is_some();
            let by_probe = mgr.item_exists(probe).await.expect("exists");
            assert_eq!(by_probe, by_row, "disagreement on {probe}");
        }
        assert!(mgr.item_exists(id).await.expect("exists"));
    }

    #[tokio::test]
    async fn swap_images_rejects_non_multiple_type_and_swaps_backdrops() {
        let db = test_db().await;
        let item = Uuid::from_u128(0xA100);
        seed_named_item(&db, item, BaseItemKind::Movie, "Swappable").await;
        for (n, path) in [(0u128, "/one.jpg"), (1, "/two.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 0, 2, ?2, ?3, 0)"#,
            )
            .bind(ferrofin_db::store::guid_to_db(Uuid::from_u128(0xA110 + n)))
            .bind(ferrofin_db::store::guid_to_db(item))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }
        let mgr = manager(&db);

        // Primary does not allow multiple images → InvalidInput (the 400).
        let err = mgr
            .swap_images(item, ImageType::Primary, 0, 1)
            .await
            .expect_err("primary rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));

        // Backdrop is reorderable and the swap goes through to the repository.
        mgr.swap_images(item, ImageType::Backdrop, 0, 1)
            .await
            .expect("swap");
        let images = mgr.get_item_images(item).await.expect("images");
        assert_eq!(images[0].path, "/two.jpg");
        assert_eq!(images[1].path, "/one.jpg");
    }

    #[tokio::test]
    async fn query_items_returns_matching_rows() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        let a = Uuid::from_u128(0x101);
        let b = Uuid::from_u128(0x102);
        seed_named_item(&db, a, BaseItemKind::Movie, "MovieA").await;
        seed_named_item(&db, b, BaseItemKind::Movie, "MovieB").await;
        set_clean_name(&db, a, "MovieA").await;
        set_clean_name(&db, b, "MovieB").await;
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Episode).await;
        let mgr = manager(&db);

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            name_contains: Some("Movie".to_owned()),
            ..Default::default()
        };
        let result = mgr.query_items(&query).await.expect("query");
        assert_eq!(result.items.len(), 2);
    }

    #[tokio::test]
    async fn delete_item_removes_the_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(9);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        mgr.delete_item(id, &DeleteOptions::default())
            .await
            .expect("delete");
        assert!(mgr.get_item_by_id(id).await.expect("read").is_none());
    }

    #[tokio::test]
    async fn delete_item_refuses_the_structural_and_by_name_rows() {
        let db = test_db().await;
        let mgr = manager(&db);
        let root = Uuid::from_u128(0x5001);
        seed_named_item(&db, root, BaseItemKind::UserRootFolder, "Media Folders").await;
        let library = Uuid::from_u128(0x5002);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        let year = Uuid::from_u128(0x5003);
        seed_named_item(&db, year, BaseItemKind::Year, "1999").await;
        for id in [root, library, year] {
            let err = mgr
                .delete_item(id, &DeleteOptions::default())
                .await
                .expect_err("refused");
            assert!(matches!(err, ServiceError::Unauthorized(_)), "{err}");
            assert!(mgr.get_item_by_id(id).await.expect("read").is_some());
        }
    }

    #[tokio::test]
    async fn queue_library_scan_is_a_successful_no_op() {
        let db = test_db().await;
        let mgr = manager(&db);
        mgr.queue_library_scan().await.expect("queue");
        mgr.run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("run");
        mgr.shutdown_scans().await;
    }

    /// A folder refresh's options cannot be honoured without a scanner: the
    /// request is refused, as the trait promises, rather than silently
    /// dropped.
    #[tokio::test]
    async fn queue_refresh_scan_without_a_scanner_is_refused() {
        let db = test_db().await;
        let err = manager(&db)
            .queue_refresh_scan(ScanTarget::All, &MetadataRefreshOptions::default())
            .await
            .expect_err("refused");
        assert!(matches!(err, ServiceError::Backend(_)), "{err}");
    }

    #[tokio::test]
    async fn get_named_item_resolves_by_clean_name() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x201);
        seed_named_item(&db, id, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, id, "Science Fiction").await;
        // A different-kind row with the same name must not be returned.
        let other = Uuid::from_u128(0x202);
        seed_named_item(&db, other, BaseItemKind::Studio, "Science Fiction").await;
        set_clean_name(&db, other, "Science Fiction").await;
        let mgr = manager(&db);

        let found = mgr
            .get_named_item(BaseItemKind::Genre, "Science Fiction")
            .await
            .expect("lookup")
            .expect("some");
        assert_eq!(Uuid::parse_str(&found.id).expect("uuid"), id);
        assert_eq!(found.name.as_deref(), Some("Science Fiction"));
    }

    #[tokio::test]
    async fn get_named_items_batches_and_preserves_order() {
        let db = test_db().await;
        let scifi = Uuid::from_u128(0x211);
        let drama = Uuid::from_u128(0x212);
        seed_named_item(&db, scifi, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, scifi, "Science Fiction").await;
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        // A same-name row of a different kind must not leak into Genre results.
        let studio = Uuid::from_u128(0x213);
        seed_named_item(&db, studio, BaseItemKind::Studio, "Drama").await;
        set_clean_name(&db, studio, "Drama").await;
        let mgr = manager(&db);

        // Order follows the input, unresolved names become None, and the
        // wrong-kind "Drama" studio is excluded.
        let names = vec![
            "Drama".to_owned(),
            "Nope".to_owned(),
            "Science Fiction".to_owned(),
        ];
        let got = mgr
            .get_named_items(BaseItemKind::Genre, &names)
            .await
            .expect("batch lookup");
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0].as_ref().and_then(|e| Uuid::parse_str(&e.id).ok()),
            Some(drama)
        );
        assert!(got[1].is_none());
        assert_eq!(
            got[2].as_ref().and_then(|e| Uuid::parse_str(&e.id).ok()),
            Some(scifi)
        );

        // Empty input yields an empty result without a query.
        assert!(
            mgr.get_named_items(BaseItemKind::Genre, &[])
                .await
                .expect("empty")
                .is_empty()
        );
    }

    /// Two rows of the SAME kind sharing a `CleanName` must resolve to the same
    /// id every time — the resolver keeps the FIRST match.
    ///
    /// Person rows have `SortName IS NULL`, so the resolver's bare
    /// `ORDER BY SortName` is a total tie among duplicates and the row order is
    /// whatever the sorter emits. Keeping the first match is what makes the
    /// answer stable and makes it agree with `get_named_items`. Flipping
    /// `or_insert` to `insert` (last-match-wins) passed all 4,221 other tests.
    #[tokio::test]
    async fn duplicate_names_of_one_kind_resolve_to_the_first_match() {
        let db = test_db().await;
        for n in [0x230u128, 0x231, 0x232] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, BaseItemKind::Person, "Jane Doe").await;
            set_clean_name(&db, id, "Jane Doe").await;
        }
        let mgr = manager(&db);

        let first = mgr
            .get_named_item_ids(BaseItemKind::Person, &["Jane Doe".to_owned()])
            .await
            .expect("ids");
        assert_eq!(first.len(), 1, "one name in, one slot out");
        let resolved = first[0].expect("the name resolves");

        // It must be the same id the row-returning resolver picks...
        let rows = mgr
            .get_named_items(BaseItemKind::Person, &["Jane Doe".to_owned()])
            .await
            .expect("rows");
        assert_eq!(
            rows[0].as_ref().map(|r| r.id.clone()),
            Some(ferrofin_db::store::guid_to_db(resolved)),
            "the id-only and row-returning resolvers must agree on the duplicate"
        );

        // ...and it must not drift between calls.
        for round in 0..5 {
            let again = mgr
                .get_named_item_ids(BaseItemKind::Person, &["Jane Doe".to_owned()])
                .await
                .expect("ids");
            assert_eq!(
                again[0].as_ref(),
                Some(&resolved),
                "round {round}: the resolved id must be stable across calls"
            );
        }
    }

    // The id-only twin of the batch resolver: the DTO prefetch resolves a page's
    // whole cast through it and reads nothing but the id, so it must agree with
    // `get_named_items` slot for slot — same order, same wrong-kind exclusion,
    // same `None` for an unresolved name, same blank-name handling — while
    // never materializing a row.
    #[tokio::test]
    async fn get_named_item_ids_matches_get_named_items_slot_for_slot() {
        let db = test_db().await;
        // The same-name row of a WRONG kind is seeded first, so it would win the
        // first-match-wins lookup if the kind filter were ever dropped.
        let studio = Uuid::from_u128(0x220);
        seed_named_item(&db, studio, BaseItemKind::Studio, "Drama").await;
        set_clean_name(&db, studio, "Drama").await;
        let scifi = Uuid::from_u128(0x221);
        let drama = Uuid::from_u128(0x222);
        seed_named_item(&db, scifi, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, scifi, "Science Fiction").await;
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        let mgr = manager(&db);

        // Untrimmed, blank and differently-cased names exercise the same
        // normalization both paths apply before the CleanName join.
        let names = vec![
            "  Drama ".to_owned(),
            "Nope".to_owned(),
            "   ".to_owned(),
            "science fiction".to_owned(),
        ];
        let ids = mgr
            .get_named_item_ids(BaseItemKind::Genre, &names)
            .await
            .expect("id lookup");
        assert_eq!(ids, vec![Some(drama), None, None, Some(scifi)]);

        // And it agrees with the row-returning form it replaces.
        let rows = mgr
            .get_named_items(BaseItemKind::Genre, &names)
            .await
            .expect("row lookup");
        let from_rows: Vec<Option<Uuid>> = rows
            .into_iter()
            .map(|r| r.and_then(|e| Uuid::parse_str(&e.id).ok()))
            .collect();
        assert_eq!(ids, from_rows);

        // Empty input yields an empty result without a query; a blank name is a
        // slot that resolves to nothing rather than a dropped slot.
        assert!(
            mgr.get_named_item_ids(BaseItemKind::Genre, &[])
                .await
                .expect("empty")
                .is_empty()
        );
        assert_eq!(
            mgr.get_named_item_ids(BaseItemKind::Genre, &[String::new(), " ".to_owned()])
                .await
                .expect("blank"),
            vec![None, None]
        );
    }

    #[tokio::test]
    async fn get_ancestors_walks_parent_chain_nearest_first() {
        let db = test_db().await;
        // grandparent <- parent <- child
        let grandparent = Uuid::from_u128(0x301);
        let parent = Uuid::from_u128(0x302);
        let child = Uuid::from_u128(0x303);
        seed_named_item(&db, grandparent, BaseItemKind::Folder, "Library").await;
        seed_named_item(&db, parent, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, child, BaseItemKind::Episode, "Pilot").await;
        for (id, parent_id) in [(child, parent), (parent, grandparent)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .bind(ferrofin_db::store::guid_to_db(parent_id))
                .execute(db.writer())
                .await
                .expect("set parent");
        }
        let mgr = manager(&db);

        let ancestors = mgr
            .get_ancestors(child)
            .await
            .expect("ancestors")
            .expect("item exists");
        // Nearest parent first, then its parent — the seed item is excluded.
        assert_eq!(ancestors.len(), 2);
        assert_eq!(Uuid::parse_str(&ancestors[0].id).expect("uuid"), parent);
        assert_eq!(
            Uuid::parse_str(&ancestors[1].id).expect("uuid"),
            grandparent
        );

        // A root item (no parent) yields an empty list, not None.
        let roots = mgr
            .get_ancestors(grandparent)
            .await
            .expect("ancestors")
            .expect("item exists");
        assert!(roots.is_empty());

        // A missing item yields None so the API maps it to 404.
        assert!(
            mgr.get_ancestors(Uuid::from_u128(0x3ff))
                .await
                .expect("missing")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_ancestors_cycle_terminates_and_deduplicates() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x401);
        let b = Uuid::from_u128(0x402);
        let child = Uuid::from_u128(0x403);
        seed_named_item(&db, a, BaseItemKind::Folder, "A").await;
        seed_named_item(&db, b, BaseItemKind::Folder, "B").await;
        seed_named_item(&db, child, BaseItemKind::Episode, "C").await;
        // child -> A -> B -> A (cycle)
        for (id, parent_id) in [(child, a), (a, b), (b, a)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .bind(ferrofin_db::store::guid_to_db(parent_id))
                .execute(db.writer())
                .await
                .expect("set parent");
        }
        let mgr = manager(&db);
        let anc = mgr
            .get_ancestors(child)
            .await
            .expect("anc")
            .expect("exists");
        let ids: Vec<Uuid> = anc
            .iter()
            .map(|r| Uuid::parse_str(&r.id).expect("uuid"))
            .collect();
        assert_eq!(ids, vec![a, b], "cycle must deduplicate, nearest-first");
    }

    #[tokio::test]
    async fn get_named_item_missing_is_none() {
        let db = test_db().await;
        let mgr = manager(&db);
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "Nope")
                .await
                .expect("lookup")
                .is_none()
        );
        // A blank name short-circuits to None.
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "   ")
                .await
                .expect("blank")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_music_genres_counts_referencing_items() {
        let db = test_db().await;
        // A MusicGenre by-name row plus a song that references it.
        let genre_id = Uuid::from_u128(0x301);
        seed_named_item(&db, genre_id, BaseItemKind::MusicGenre, "Jazz").await;
        set_clean_name(&db, genre_id, "Jazz").await;
        let song = Uuid::from_u128(0x302);
        seed_named_item(&db, song, BaseItemKind::Audio, "Blue in Green").await;
        seed_item_genre(&db, song, "Jazz").await;
        let mgr = manager(&db);

        let result = mgr
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        let jazz = result
            .items
            .iter()
            .find(|iwc| iwc.item.name.as_deref() == Some("Jazz"))
            .expect("jazz present");
        assert_eq!(jazz.counts.item_count, 1);
    }

    #[tokio::test]
    async fn get_media_stream_languages_reads_distinct_codes() {
        use ferrofin_model::entities::MediaStreamType;
        let db = test_db().await;
        let item = Uuid::from_u128(0x501);
        seed_item(&db, item, BaseItemKind::Movie).await;
        // One English audio stream plus one with no language (→ 'und').
        for (idx, lang) in [(0_i64, Some("eng")), (1, None)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos"
                   ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                    "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, ?3)"#,
            )
            .bind(ferrofin_db::store::guid_to_db(item))
            .bind(idx)
            .bind(lang)
            .execute(db.writer())
            .await
            .expect("insert stream");
        }
        let mgr = manager(&db);

        let mut langs = mgr
            .get_media_stream_languages(MediaStreamType::Audio, &InternalItemsQuery::default())
            .await
            .expect("languages");
        langs.sort();
        assert_eq!(langs, vec!["eng".to_owned(), "und".to_owned()]);
    }

    #[tokio::test]
    async fn get_album_artists_returns_artist_rows() {
        let db = test_db().await;
        // A song credits "Miles Davis" as album artist (ItemValues type 1), and the
        // browsable by-name row is materialized sharing the value id — the shape the
        // by-name aggregate now requires (a value referenced by an in-scope item).
        let value_id = ferrofin_db::store::guid_to_db(Uuid::from_u128(0x401));
        let song = Uuid::from_u128(0x402);
        seed_named_item(&db, song, BaseItemKind::Audio, "So What").await;
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId","Type","Value","CleanValue")
               VALUES (?1, 1, 'Miles Davis', 'miles davis')"#,
        )
        .bind(&value_id)
        .execute(db.writer())
        .await
        .expect("value");
        sqlx::query(r#"INSERT INTO "ItemValuesMap" ("ItemId","ItemValueId") VALUES (?1,?2)"#)
            .bind(ferrofin_db::store::guid_to_db(song))
            .bind(&value_id)
            .execute(db.writer())
            .await
            .expect("map");
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","CleanName","IsFolder","IsInMixedFolder",
                "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,'MediaBrowser.Controller.Entities.Audio.MusicArtist',
                       'Miles Davis','miles davis',1,0,0,0,0,0,0)"#,
        )
        .bind(&value_id)
        .execute(db.writer())
        .await
        .expect("by-name row");
        let mgr = manager(&db);

        let result = mgr
            .get_album_artists(&InternalItemsQuery::default())
            .await
            .expect("album artists");
        assert!(
            result
                .items
                .iter()
                .any(|iwc| iwc.item.name.as_deref() == Some("Miles Davis"))
        );
    }

    #[tokio::test]
    async fn get_user_root_folder_resolves_the_root_row() {
        let db = test_db().await;
        let mgr = manager(&db);

        // With no root row materialized, the default resolves to None.
        assert!(mgr.get_user_root_folder().await.expect("none").is_none());

        // Once a UserRootFolder row exists, it is returned.
        let root = Uuid::from_u128(0x5001);
        seed_named_item(&db, root, BaseItemKind::UserRootFolder, "Media Folders").await;
        let resolved = mgr
            .get_user_root_folder()
            .await
            .expect("root")
            .expect("some");
        assert_eq!(Uuid::parse_str(&resolved.id).expect("uuid"), root);
    }

    /// With the provisioners wired (the composition root's shape), the root is
    /// created on first use and a `Year` lookup creates the year — Jellyfin's
    /// `GetUserRootFolder()` / `GetYear` on a database that has neither.
    #[tokio::test]
    async fn root_and_years_are_created_on_first_use() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let mode = crate::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let mgr = manager(&db)
            .with_user_root(crate::user_root_folder::UserRootFolderStore::new(
                Arc::clone(&persistence),
                mode.clone(),
                tmp.path().join("root/default"),
            ))
            .with_years(crate::years::YearStore::new(
                persistence,
                mode,
                tmp.path().join("metadata/Year"),
            ));

        let root = mgr
            .get_user_root_folder()
            .await
            .expect("root")
            .expect("created on first use");
        assert_eq!(root.name.as_deref(), Some("Media Folders"));
        assert!(tmp.path().join("root/default").is_dir());

        // A year with no item and no row resolves (and now exists); a
        // non-year name of the kind does not.
        let year = mgr
            .get_named_item(BaseItemKind::Year, "1999")
            .await
            .expect("year")
            .expect("created on demand");
        assert_eq!(year.name.as_deref(), Some("1999"));
        assert!(tmp.path().join("metadata/Year/1999").is_dir());
        assert!(
            mgr.get_named_item(BaseItemKind::Year, "not-a-year")
                .await
                .expect("lookup")
                .is_none()
        );
        // The batch form fills every slot, reusing the row it already made.
        let batch = mgr
            .get_named_items(BaseItemKind::Year, &["1999".to_owned(), "2004".to_owned()])
            .await
            .expect("batch");
        assert_eq!(batch.len(), 2);
        assert_eq!(
            batch[0].as_ref().map(|r| r.id.clone()),
            Some(year.id.clone()),
            "the existing row is reused"
        );
        assert!(batch[1].is_some(), "the second year was created");
        let years = mgr
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Year],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("list");
        assert_eq!(years.len(), 2);
    }

    /// Sets a row's `Width` column so the merge primary-selection heuristic has a
    /// deterministic winner.
    async fn set_width(db: &Database, id: Uuid, width: i64) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Width" = ?1 WHERE "Id" = ?2"#)
            .bind(width)
            .bind(ferrofin_db::store::guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("set width");
    }

    #[tokio::test]
    async fn merge_versions_links_alternates_to_widest_primary() {
        let db = test_db().await;
        let wide = Uuid::from_u128(0x301);
        let narrow = Uuid::from_u128(0x302);
        seed_item(&db, wide, BaseItemKind::Movie).await;
        seed_item(&db, narrow, BaseItemKind::Movie).await;
        set_width(&db, wide, 1920).await;
        set_width(&db, narrow, 640).await;
        let mgr = manager(&db);

        mgr.merge_versions(&[narrow, wide]).await.expect("merge");

        // The widest becomes the primary (its own pointer stays null); the narrow
        // one points at it.
        let primary = mgr.get_item_by_id(wide).await.expect("read").expect("some");
        assert_eq!(primary.primary_version_id, None);
        let alt = mgr
            .get_item_by_id(narrow)
            .await
            .expect("read")
            .expect("some");
        assert_eq!(
            alt.primary_version_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok()),
            Some(wide)
        );
    }

    #[tokio::test]
    async fn merge_versions_rejects_single_id() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x303);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let err = mgr.merge_versions(&[id]).await.expect_err("too few");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn remove_alternate_sources_clears_the_group() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x311);
        let alt = Uuid::from_u128(0x312);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        set_width(&db, primary, 1920).await;
        set_width(&db, alt, 640).await;
        let mgr = manager(&db);
        mgr.merge_versions(&[primary, alt]).await.expect("merge");

        // Splitting from the *alternate* still clears the whole group.
        mgr.remove_alternate_sources(alt).await.expect("remove");

        assert_eq!(
            mgr.get_item_by_id(primary)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
        assert_eq!(
            mgr.get_item_by_id(alt)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
    }

    /// The `item ??= new Genre()` stand-in: an all-default row whose id is
    /// `Guid.Empty` and whose only meaningful column is the `Type`, so the DTO
    /// the controller serializes says `"Type": "Genre"` and nothing else.
    #[tokio::test]
    async fn the_empty_by_name_item_carries_only_its_kind() {
        let db = test_db().await;
        let mgr = manager(&db);
        let empty = mgr.empty_by_name_item(BaseItemKind::MusicGenre);
        assert_eq!(empty.id, ferrofin_db::store::guid_to_db(Uuid::nil()));
        assert_eq!(
            empty.type_,
            crate::item_type_lookup::stored_type_name(BaseItemKind::MusicGenre).expect("known")
        );
        assert!(empty.name.is_none());
        assert!(empty.path.is_none());
        assert!(empty.sort_name.is_none());
    }

    /// `find_named_item` resolves an existing by-name row and, unlike
    /// `get_named_item`, creates nothing when there is none — the split C#
    /// makes between `GetGenre` (`CreateItemByName`) and the slug branch's
    /// plain `GetItemList` lookups.
    #[tokio::test]
    async fn find_named_item_resolves_without_creating() {
        let db = test_db().await;
        let mgr = manager(&db);
        let genre = Uuid::from_u128(0xE01);
        crate::test_support::seed_named_item(&db, genre, BaseItemKind::Genre, "R&B").await;
        crate::test_support::set_clean_name(&db, genre, "R&B").await;

        let found = mgr
            .find_named_item(BaseItemKind::Genre, "R&B")
            .await
            .expect("lookup");
        assert_eq!(
            found.map(|e| e.id),
            Some(ferrofin_db::store::guid_to_db(genre))
        );

        assert!(
            mgr.find_named_item(BaseItemKind::Genre, "No Such Genre")
                .await
                .expect("lookup")
                .is_none()
        );
        let all_genres = mgr
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Genre],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("genre rows");
        assert_eq!(all_genres.len(), 1, "the miss must not have minted a row");
    }

    #[tokio::test]
    async fn remove_alternate_sources_missing_item_is_not_found() {
        let db = test_db().await;
        let mgr = manager(&db);
        let err = mgr
            .remove_alternate_sources(Uuid::from_u128(0x3FF))
            .await
            .expect_err("missing");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }
}
