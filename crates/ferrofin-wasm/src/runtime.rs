//! The per-plugin runtime actor: one dedicated OS thread that owns the
//! plugin's [`Store`] and instance for the life of the server.
//!
//! wasmtime stores are single-threaded by design, and guest calls can burn
//! CPU for up to the configured deadline — both reasons they must never run
//! on the tokio workers. Each loaded plugin therefore gets one long-lived
//! thread (cost is per-plugin, not per-call) fed by a bounded command queue:
//!
//! - scheduled-task runs and metadata lookups `send().await` on the queue
//!   and await a typed reply;
//! - events are fire-and-forget `try_send`s — when a slow guest's queue is
//!   full the event is dropped (with a debug log), never the server's time;
//! - web requests (`/Plugins/{id}/web/*`, reachable without authentication)
//!   never wait either: they `try_send`, and together they may hold at most
//!   [`web_request_slots`] of the queue (half), queued or running. Beyond
//!   that, or when the queue is full, a request is turned away at once with
//!   [`RequestError::Busy`], so a flood of them cannot fill the queue that
//!   scans, tasks and events share.
//!
//! The async `send().await` suspends (rather than parking a tokio worker)
//! when the queue is full, which is the right trade — but it is an
//! *unbounded* wait. A plugin that is slow without ever failing can keep its
//! queue full and stall a `run-task` call or a scan's per-item
//! `metadata-lookup` until it drains. That is bounded in practice by the
//! per-call deadline (each queued call still trips it, driving the
//! breaker; [`InstanceSpec::arm_deadline`] holds wasm code, WASI clock
//! waits and the bounded host waits to it), but a genuinely
//! slow-not-failing metadata plugin can drag a scan. If that ever bites,
//! give lookups a `try_send` + "plugin busy — skip this item" fast path
//! rather than making them wait.
//!
//! **Containment:** a trap, deadline overrun, or memory-cap hit fails only
//! the one call; the actor rebuilds a fresh instance for the next call. After
//! [`BREAKER_LIMIT`] consecutive failures the plugin is declared dead for the
//! rest of the process (commands are refused, events discarded) — the same
//! circuit-breaker shape as the HLS transcode restart breaker.
//!
//! **The watchdog:** a host function that blocks past the deadline (one with
//! no bound of its own, such as a database write) parks the thread where the
//! deadline cannot reach it. So every caller waits under a [`CallWatch`]: once
//! the call the thread is running outlives its deadline by the grace
//! (`FERROFIN_WASM_CALL_GRACE_SECS`), the call is lost, the plugin is
//! declared dead and its thread and store are abandoned until restart. The
//! watch times a call from when it starts running, never from when it was
//! queued.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};

use tracing::{debug, warn};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store, StoreLimitsBuilder};

use crate::bindings::{HostState, Plugin};

/// Consecutive guest-call failures after which a plugin is declared dead
/// until restart. Mirrors the HLS transcode breaker's `RESTART_FAILURE_LIMIT`
/// (3): one flake retries, a pattern trips.
pub const BREAKER_LIMIT: u32 = 3;

/// wasmtime's `memory_size` limit applies to EACH linear memory, and the
/// store default allows 10,000 of them — which would make the documented
/// per-plugin ceiling meaningless. Rust wasip2 components link to exactly
/// one linear memory, so one is what a plugin gets (a multi-memory
/// component fails instantiation with a clear error; raise deliberately if
/// such plugins ever appear).
pub const MEMORIES_PER_PLUGIN: usize = 1;

/// Function-reference tables a component may create. Components carry a few
/// (call-indirect table + adapter shims); 8 is generous headroom.
pub const TABLES_PER_PLUGIN: usize = 8;

/// Total table elements per table (~8 bytes of host memory each, so the cap
/// bounds table growth at a few MiB — the same runaway class as memory).
pub const TABLE_ELEMENTS_PER_PLUGIN: usize = 500_000;

/// Core-module instances per plugin store. A wasip2 component instantiates
/// a handful (main module + WASI adapters); 64 is generous headroom.
pub const INSTANCES_PER_PLUGIN: usize = 64;

/// How often the epoch ticker advances the engine epoch. One tick is the
/// resolution of the call deadline; 1 s keeps the ticker negligible while
/// making `FERROFIN_WASM_CALL_TIMEOUT_SECS` map 1:1 onto ticks.
pub const EPOCH_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// How many of a plugin's `queue_capacity` command slots its web requests
/// may hold at once, queued or running: half, and at least one (owner
/// decision 2026-10-20). The anonymous web route can then never fill the
/// queue that scans, tasks and events share.
#[must_use]
pub fn web_request_slots(queue_capacity: usize) -> usize {
    (queue_capacity / 2).max(1)
}

/// The deadline of the guest call a plugin's thread is running, shared with
/// everyone waiting on that thread: the caller-side watchdog.
///
/// [`InstanceSpec::arm_deadline`] sets it as each guest call (or
/// instantiation) starts; the runtime thread clears it between calls. A
/// waiter declares the call lost once it outlives that deadline by the grace,
/// which must exceed one [`EPOCH_TICK`]: a guest within its budget is
/// interrupted within a tick of its deadline and every bounded host wait is
/// held to the deadline, so only a wait that ignores it is ever declared lost.
#[derive(Debug)]
pub struct CallWatch {
    deadline: std::sync::Mutex<Option<Instant>>,
    budget: Duration,
    grace: Duration,
}

impl CallWatch {
    /// A watch for calls of `budget` (the per-call timeout) that are lost
    /// `grace` past their deadline.
    #[must_use]
    pub fn new(budget: Duration, grace: Duration) -> Self {
        Self {
            deadline: std::sync::Mutex::new(None),
            budget,
            grace,
        }
    }

    /// The grace a call gets past its deadline.
    #[must_use]
    pub fn grace(&self) -> Duration {
        self.grace
    }

    fn set(&self, deadline: Option<Instant>) {
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
    }

    /// When the running call counts as lost; `None` between calls.
    fn lost_at(&self) -> Option<Instant> {
        let deadline = *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        deadline.and_then(|deadline| deadline.checked_add(self.grace))
    }

    /// When a waiter looks again: when the running call would be lost, or,
    /// between calls, one whole budget and grace from now (a call that starts
    /// meanwhile cannot be lost any sooner).
    fn next_check(&self) -> Instant {
        self.lost_at()
            .unwrap_or_else(|| Instant::now() + self.budget + self.grace)
    }

    /// Whether the running call has outlived its deadline by the grace.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost_at().is_some_and(|at| Instant::now() >= at)
    }
}

/// Why [`run_watched`] has no result.
#[derive(Debug)]
pub(crate) enum Unwatched {
    /// The guest call outlived its deadline by the grace; the thread was
    /// abandoned.
    Lost,
    /// The work panicked (the panic hook logged it).
    Panicked,
    /// The OS refused a thread.
    NoThread(std::io::Error),
}

/// Runs `work` (loading or validating one plugin) on a thread of its own and
/// waits while the guest calls it makes, stamped on `watch`, keep within
/// their deadline plus the grace. A lost call abandons the thread with
/// whatever store it holds, so the caller never waits on it again. The time
/// before the first guest call (compiling the component) is not watched.
///
/// # Errors
/// [`Unwatched`]: the call was lost, the work panicked, or no thread could
/// be spawned.
pub(crate) fn run_watched<T: Send + 'static>(
    watch: &CallWatch,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unwatched> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("wasm-plugin-load".to_owned())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .map_err(Unwatched::NoThread)?;
    loop {
        let wait = watch.next_check().saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(out) => return Ok(out),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Unwatched::Panicked);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if watch.is_lost() => {
                return Err(Unwatched::Lost);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Why [`RuntimeHandle::handle_request`] produced no guest response.
#[derive(Debug)]
pub enum RequestError {
    /// The plugin cannot take another web request now: web requests already
    /// hold their [`web_request_slots`], or the queue is full. Not a plugin
    /// failure; the caller may retry later.
    Busy(String),
    /// The guest or host failed the call, the breaker is open, or the
    /// runtime thread is gone.
    Failed(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

/// The guest analysis result plus a host-only live-policy observation.
#[derive(Debug)]
pub struct ScanMediaOutcome {
    /// Guest success or its orderly/host error.
    pub result: Result<(), String>,
    /// A live library/plugin gate suppressed at least one output write during the call.
    pub policy_skipped: bool,
}

/// A command sent to a plugin's runtime thread.
pub enum Command {
    /// Run the guest task with the given id and reply with its outcome.
    RunTask {
        /// The plugin-local task id (the WIT `task-descriptor.id`).
        task_id: String,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives `Ok(())` or the guest/host error text.
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Deliver a domain event (fire-and-forget).
    OnEvent {
        /// The stable event type name (`LibraryChanged`, …).
        name: String,
        /// The event's JSON payload.
        json: String,
    },
    /// Route one HTTP request from the plugin's URL space to the guest.
    HandleRequest {
        /// The request, already identity-resolved by the host.
        request: crate::bindings::types::PluginRequest,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives the guest's response (or the host/trap error text).
        reply: tokio::sync::oneshot::Sender<Result<crate::bindings::types::PluginResponse, String>>,
        /// One of the plugin's [`web_request_slots`], freed once the guest is
        /// done with this request.
        slot: tokio::sync::OwnedSemaphorePermit,
    },
    /// Offer the guest one library item for media analysis.
    ScanMedia {
        /// The item, as the shared summary projection.
        item: crate::bindings::types::ItemSummary,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives the guest's outcome (or the host/trap error text).
        reply: tokio::sync::oneshot::Sender<Result<ScanMediaOutcome, String>>,
    },
    /// Ask the guest for remote-artwork candidates for one item.
    RemoteImages {
        /// The item, as the shared summary projection.
        item: crate::bindings::types::ItemSummary,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives the guest's candidates (or the host/trap error text).
        reply: tokio::sync::oneshot::Sender<
            Result<Vec<crate::bindings::types::ImageCandidate>, String>,
        >,
    },
    /// Ask the guest for metadata on one item (the scan's dynamic pass).
    MetadataLookup {
        /// The item as scanned so far.
        item: crate::bindings::types::ItemSummary,
        /// The item's known external ids.
        provider_ids: Vec<(String, String)>,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives the guest's offer (or its error text).
        reply: tokio::sync::oneshot::Sender<
            Result<Option<crate::bindings::types::MetadataResult>, String>,
        >,
    },
}

/// Everything needed to (re)build a plugin's store + instance from scratch.
pub struct InstanceSpec {
    /// The shared engine (cheap to clone; one per host).
    pub engine: Engine,
    /// The plugin's compiled component.
    pub component: Component,
    /// The linker with the host interface registered.
    pub linker: Arc<Linker<HostState>>,
    /// Display name for log tagging.
    pub plugin_name: String,
    /// Stable id (canonical UUID string) for log tagging.
    pub plugin_id: String,
    /// Linear-memory ceiling in bytes.
    pub memory_limit_bytes: usize,
    /// Guest-call deadline in epoch ticks (1 tick = [`EPOCH_TICK`]).
    pub timeout_ticks: u64,
    /// Where each guest call's deadline is published for the watchdog.
    pub watch: Arc<CallWatch>,
    /// The shared blocking HTTP client behind `http-fetch`.
    pub http: Arc<crate::capabilities::GuestHttp>,
    /// Whether this plugin may reach private/loopback HTTP destinations.
    pub private_http_allowed: bool,
    /// The manager handles behind the E2 capabilities (installed post-load).
    pub collaborators: Arc<std::sync::OnceLock<crate::capabilities::Collaborators>>,
    /// Where the plugin's key/value state persists (`None` until the id is
    /// known / in validation stores).
    pub state_path: Option<std::path::PathBuf>,
    /// The plugin's declared public-egress allowlist.
    pub egress: Arc<crate::capabilities::EgressPolicy>,
    /// The operator-configured total state cap, in bytes.
    pub state_total_cap: usize,
    /// The operator-configured lyric/subtitle write cap, in bytes.
    pub write_content_cap: usize,
    /// The operator-configured extracted-subtitle-track cap, in bytes.
    pub subtitle_extract_cap: usize,
}

impl InstanceSpec {
    /// Builds a fresh store and world instance, with the memory limiter and
    /// an initial config snapshot installed.
    ///
    /// # Errors
    /// Any instantiation failure — including a component built against a
    /// different `ferrofin:plugin` world version, which surfaces here as a
    /// missing/mismatched import or export.
    pub fn instantiate(&self, config_json: String) -> wasmtime::Result<(Store<HostState>, Plugin)> {
        let state = HostState {
            plugin_name: self.plugin_name.clone(),
            plugin_id: self.plugin_id.clone(),
            config_json,
            segment_policy_skipped: false,
            limits: StoreLimitsBuilder::new()
                .memory_size(self.memory_limit_bytes)
                .memories(MEMORIES_PER_PLUGIN)
                .tables(TABLES_PER_PLUGIN)
                .table_elements(TABLE_ELEMENTS_PER_PLUGIN)
                .instances(INSTANCES_PER_PLUGIN)
                .build(),
            memory_limit_bytes: self.memory_limit_bytes,
            http: Arc::clone(&self.http),
            http_timeout: std::time::Duration::from_secs(self.timeout_ticks),
            state_path: self.state_path.clone(),
            egress: Arc::clone(&self.egress),
            state_total_cap: self.state_total_cap,
            write_content_cap: self.write_content_cap,
            subtitle_extract_cap: self.subtitle_extract_cap,
            private_http_allowed: self.private_http_allowed,
            collaborators: Arc::clone(&self.collaborators),
            call_deadline: std::time::Instant::now(), // placeholder; armed below
            wasi: HostState::empty_wasi(),
            table: wasmtime::component::ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // Instantiation itself runs guest code (start sections, allocators),
        // so it gets a deadline too. Loading's identity calls share it.
        self.arm_deadline(&mut store);
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)?;
        Ok((store, plugin))
    }

    /// Starts a guest call's time budget of [`timeout_ticks`](Self::timeout_ticks)
    /// [`EPOCH_TICK`]s: the epoch deadline that interrupts wasm code (to
    /// within one tick), and the wall-clock instant that caps the guest's
    /// WASI clock waits and bounded host waits (host code, which epochs never
    /// interrupt; see [`DeadlineClocks`](crate::bindings::DeadlineClocks)).
    /// The instant is also published on the [`watch`](Self::watch).
    pub fn arm_deadline(&self, store: &mut Store<HostState>) {
        store.set_epoch_deadline(self.timeout_ticks);
        let budget =
            EPOCH_TICK.saturating_mul(u32::try_from(self.timeout_ticks).unwrap_or(u32::MAX));
        let now = Instant::now();
        // Unrepresentable only for a budget of centuries; fail closed (no waits).
        let deadline = now.checked_add(budget).unwrap_or(now);
        store.data_mut().call_deadline = deadline;
        self.watch.set(Some(deadline));
    }
}

/// The sending half owned by the host: a bounded queue into the plugin's
/// runtime thread plus the shared dead flag and call watch.
pub struct RuntimeHandle {
    sender: Sender<Command>,
    dead: Arc<AtomicBool>,
    /// The deadline of the call the thread is running (the watchdog).
    watch: Arc<CallWatch>,
    /// The [`web_request_slots`] that web requests take before queueing.
    web_slots: Arc<tokio::sync::Semaphore>,
    plugin_name: String,
    plugin_id: String,
}

impl RuntimeHandle {
    /// Whether the plugin is disabled until restart: the breaker tripped, or
    /// a call was lost. Asking is enough to find a lost call, so no reader
    /// (event delivery, the analysis pass) takes a stuck plugin for alive
    /// just because no caller is waiting on it.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed) || self.lost()
    }

    /// Whether the call the thread is running is lost; the first to find it
    /// disables the plugin and says so.
    fn lost(&self) -> bool {
        if !self.watch.is_lost() {
            return false;
        }
        if !self.dead.swap(true, Ordering::Relaxed) {
            warn!(
                plugin = self.plugin_name,
                plugin_id = self.plugin_id,
                grace_secs = self.watch.grace().as_secs(),
                "wasm plugin call outlived its deadline plus the grace; \
                 plugin disabled until restart and its thread abandoned"
            );
        }
        true
    }

    /// The error every call to a disabled plugin gets.
    fn disabled(&self) -> String {
        format!("plugin `{}` is disabled until restart", self.plugin_name)
    }

    /// Awaits `step` (room in the queue, or a reply) while the call the
    /// plugin's thread is running stays within its deadline plus the grace.
    /// Past that the call is lost: the plugin is disabled until restart and
    /// its thread, stuck in host code, is abandoned with its store. Every
    /// other waiter fails at the same moment, so nothing queued behind the
    /// stuck call waits forever either.
    async fn watched<T>(&self, step: impl std::future::Future<Output = T>) -> Result<T, String> {
        let mut step = std::pin::pin!(step);
        loop {
            tokio::select! {
                biased;
                out = &mut step => return Ok(out),
                () = tokio::time::sleep_until(self.watch.next_check().into()) => {}
            }
            if self.lost() {
                return Err(format!(
                    "plugin `{}` did not finish a call within its deadline plus the {} s \
                     grace; it is disabled until restart",
                    self.plugin_name,
                    self.watch.grace().as_secs()
                ));
            }
            // Lost by another waiter: the thread may never answer.
            if self.is_dead() {
                return Err(self.disabled());
            }
        }
    }

    /// Queues `command` and awaits `rx`, both under the watchdog.
    async fn call<T>(
        &self,
        command: Command,
        rx: tokio::sync::oneshot::Receiver<Result<T, String>>,
        what: &str,
    ) -> Result<T, String> {
        if self.is_dead() {
            return Err(self.disabled());
        }
        // Async send: a queue full of pending events suspends this future —
        // it never parks the tokio worker the way a blocking send would.
        self.watched(self.sender.send(command))
            .await?
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        self.watched(rx)
            .await?
            .map_err(|_| format!("plugin runtime dropped the {what} reply"))?
    }

    /// Queues a task run and awaits its outcome.
    ///
    /// # Errors
    /// The guest's error text, the plugin being disabled, a lost call, or the
    /// runtime thread being gone.
    pub async fn run_task(&self, task_id: String, config: String) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let command = Command::RunTask {
            task_id,
            config,
            reply,
        };
        self.call(command, rx, "task").await
    }

    /// Routes one HTTP request to the guest and awaits its response.
    ///
    /// The request never waits for queue room: when web requests already
    /// hold all their [`web_request_slots`], or the queue is full, it is
    /// turned away at once. Its slot travels with the command and frees when
    /// the guest is done, so a caller that hangs up keeps holding it until
    /// then.
    ///
    /// # Errors
    /// [`RequestError::Busy`] when the plugin has no room for the request;
    /// [`RequestError::Failed`] with the guest/host error text, the plugin
    /// being disabled, a lost call, or the runtime thread being gone.
    pub async fn handle_request(
        &self,
        request: crate::bindings::types::PluginRequest,
        config: String,
    ) -> Result<crate::bindings::types::PluginResponse, RequestError> {
        if self.is_dead() {
            return Err(RequestError::Failed(self.disabled()));
        }
        let busy = || {
            RequestError::Busy(format!(
                "plugin `{}` is busy; retry the request later",
                self.plugin_name
            ))
        };
        let Ok(slot) = Arc::clone(&self.web_slots).try_acquire_owned() else {
            return Err(busy());
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        match self.sender.try_send(Command::HandleRequest {
            request,
            config,
            reply,
            slot,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(busy()),
            Err(TrySendError::Closed(_)) => {
                return Err(RequestError::Failed(
                    "plugin runtime thread has exited".to_owned(),
                ));
            }
        }
        self.watched(rx)
            .await
            .map_err(RequestError::Failed)?
            .map_err(|_| {
                RequestError::Failed("plugin runtime dropped the request reply".to_owned())
            })?
            .map_err(RequestError::Failed)
    }

    /// Offers the guest one item for media analysis and awaits its outcome.
    ///
    /// # Errors
    /// The guest's error text, the plugin being disabled, a lost call, or the
    /// runtime thread being gone.
    pub async fn scan_media(
        &self,
        item: crate::bindings::types::ItemSummary,
        config: String,
    ) -> Result<ScanMediaOutcome, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let command = Command::ScanMedia {
            item,
            config,
            reply,
        };
        self.call(command, rx, "scan").await
    }

    /// Asks the guest for artwork candidates and awaits them.
    ///
    /// # Errors
    /// The guest's error text, the plugin being disabled, a lost call, or the
    /// runtime thread being gone.
    pub async fn remote_images(
        &self,
        item: crate::bindings::types::ItemSummary,
        config: String,
    ) -> Result<Vec<crate::bindings::types::ImageCandidate>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let command = Command::RemoteImages {
            item,
            config,
            reply,
        };
        self.call(command, rx, "images").await
    }

    /// Asks the guest for metadata on one item and awaits its offer.
    ///
    /// # Errors
    /// The guest's error text, the plugin being disabled, a lost call, or the
    /// runtime thread being gone.
    pub async fn metadata_lookup(
        &self,
        item: crate::bindings::types::ItemSummary,
        provider_ids: Vec<(String, String)>,
        config: String,
    ) -> Result<Option<crate::bindings::types::MetadataResult>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let command = Command::MetadataLookup {
            item,
            provider_ids,
            config,
            reply,
        };
        self.call(command, rx, "lookup").await
    }

    /// Delivers an event without waiting. A full queue or dead plugin drops
    /// the event (debug-logged) — event delivery must never apply
    /// back-pressure to the server.
    pub fn deliver_event(&self, name: &str, json: &str) {
        if self.is_dead() {
            return;
        }
        match self.sender.try_send(Command::OnEvent {
            name: name.to_owned(),
            json: json.to_owned(),
        }) {
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(_)) => {
                debug!(
                    plugin = self.plugin_name,
                    event = name,
                    "wasm plugin event queue full; event dropped"
                );
            }
        }
    }
}

/// Spawns the runtime thread for one plugin and returns its handle.
///
/// `store` and `plugin` are the already-instantiated pair from loading (the
/// loader has just called `descriptor`/`default-config`/`tasks` on them), so
/// the first command reuses the warm instance instead of paying a rebuild.
///
/// # Panics
/// Only if the OS refuses to spawn a thread (resource exhaustion at startup —
/// the process is already unviable at that point).
#[must_use]
pub fn spawn(
    spec: InstanceSpec,
    store: Store<HostState>,
    plugin: Plugin,
    queue_capacity: usize,
) -> RuntimeHandle {
    let (sender, receiver) = tokio::sync::mpsc::channel(queue_capacity);
    let dead = Arc::new(AtomicBool::new(false));
    let handle = RuntimeHandle {
        sender,
        dead: Arc::clone(&dead),
        watch: Arc::clone(&spec.watch),
        web_slots: Arc::new(tokio::sync::Semaphore::new(web_request_slots(
            queue_capacity,
        ))),
        plugin_name: spec.plugin_name.clone(),
        plugin_id: spec.plugin_id.clone(),
    };
    // Named from the validated id: the descriptor name is guest-controlled,
    // and an interior NUL in a thread name panics `spawn`.
    std::thread::Builder::new()
        .name(format!("wasm-plugin-{}", spec.plugin_id))
        .spawn(move || {
            let mut receiver = receiver;
            run_loop(&spec, store, plugin, &mut receiver, &dead);
        })
        .expect("spawning a wasm plugin runtime thread cannot fail");
    handle
}

/// The actor loop: execute commands against the live instance, rebuilding it
/// after any failure, until the breaker trips or the channel closes.
// One arm per world export; the dispatch table IS the function.
#[allow(clippy::too_many_lines)]
fn run_loop(
    spec: &InstanceSpec,
    store: Store<HostState>,
    plugin: Plugin,
    receiver: &mut Receiver<Command>,
    dead: &AtomicBool,
) {
    let mut live = Some((store, plugin));
    let mut consecutive_failures: u32 = 0;

    loop {
        // Between calls nothing runs, so there is no deadline to watch.
        spec.watch.set(None);
        let Some(command) = receiver.blocking_recv() else {
            break;
        };
        if dead.load(Ordering::Relaxed) {
            refuse(command, &spec.plugin_name);
            continue;
        }

        // Rebuild the instance if the previous call wrecked it.
        if live.is_none() {
            match spec.instantiate(String::from("{}")) {
                Ok(pair) => live = Some(pair),
                Err(err) => {
                    warn!(
                        plugin = spec.plugin_name,
                        error = %err,
                        "wasm plugin re-instantiation failed"
                    );
                    consecutive_failures += 1;
                    trip_if_due(spec, dead, consecutive_failures);
                    refuse(command, &spec.plugin_name);
                    continue;
                }
            }
        }
        let (store, instance) = live.as_mut().expect("instance was just ensured");

        let failed = match command {
            Command::RunTask {
                task_id,
                config,
                reply,
            } => {
                store.data_mut().config_json = config;
                spec.arm_deadline(store);
                match instance.call_run_task(&mut *store, &task_id) {
                    // Guest-reported failure: an orderly `err(string)`, not a
                    // trap — the instance is still healthy.
                    Ok(Err(guest_err)) => {
                        let _ = reply.send(Err(guest_err));
                        false
                    }
                    Ok(Ok(())) => {
                        let _ = reply.send(Ok(()));
                        false
                    }
                    Err(trap) => {
                        let _ = reply.send(Err(format!("plugin call failed: {trap:#}")));
                        true
                    }
                }
            }
            Command::HandleRequest {
                request,
                config,
                reply,
                slot,
            } => dispatch_request(spec, store, instance, &request, config, reply, slot),
            Command::RemoteImages {
                item,
                config,
                reply,
            } => {
                store.data_mut().config_json = config;
                spec.arm_deadline(store);
                match instance.call_remote_images(&mut *store, &item) {
                    Ok(outcome) => {
                        let _ = reply.send(outcome);
                        false
                    }
                    Err(trap) => {
                        let _ = reply.send(Err(format!("plugin call failed: {trap:#}")));
                        true
                    }
                }
            }
            Command::ScanMedia {
                item,
                config,
                reply,
            } => dispatch_scan(spec, store, instance, &item, config, reply),
            Command::MetadataLookup {
                item,
                provider_ids,
                config,
                reply,
            } => {
                store.data_mut().config_json = config;
                spec.arm_deadline(store);
                match instance.call_metadata_lookup(&mut *store, &item, &provider_ids) {
                    // An orderly guest err(string) leaves the instance healthy.
                    Ok(outcome) => {
                        let _ = reply.send(outcome);
                        false
                    }
                    Err(trap) => {
                        let _ = reply.send(Err(format!("plugin call failed: {trap:#}")));
                        true
                    }
                }
            }
            Command::OnEvent { name, json } => {
                spec.arm_deadline(store);
                match instance.call_on_event(&mut *store, &name, &json) {
                    Ok(()) => false,
                    Err(trap) => {
                        warn!(
                            plugin = spec.plugin_name,
                            event = name,
                            error = %trap,
                            "wasm plugin trapped handling an event"
                        );
                        true
                    }
                }
            }
        };

        if failed {
            // A trap (including epoch timeout and memory-cap hits) leaves the
            // instance suspect: drop it and rebuild lazily on the next call.
            live = None;
            consecutive_failures += 1;
            trip_if_due(spec, dead, consecutive_failures);
        } else {
            consecutive_failures = 0;
        }
    }
}

/// Trips the breaker once the consecutive-failure count reaches the limit.
fn trip_if_due(spec: &InstanceSpec, dead: &AtomicBool, consecutive_failures: u32) {
    if consecutive_failures >= BREAKER_LIMIT {
        warn!(
            plugin = spec.plugin_name,
            plugin_id = spec.plugin_id,
            failures = consecutive_failures,
            "wasm plugin disabled until restart (circuit breaker)"
        );
        dead.store(true, Ordering::Relaxed);
    }
}

/// Answers a command that will not be executed (dead plugin / broken state).
#[allow(clippy::match_same_arms)] // arms differ by reply TYPE, not intent
fn refuse(command: Command, plugin_name: &str) {
    let message = format!("plugin `{plugin_name}` is disabled until restart");
    match command {
        Command::RunTask { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Command::HandleRequest { reply, slot, .. } => {
            drop(slot);
            let _ = reply.send(Err(message));
        }
        Command::RemoteImages { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Command::ScanMedia { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Command::MetadataLookup { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Command::OnEvent { .. } => {}
    }
}

/// Runs one `handle-request` guest call (extracted from the actor loop to
/// keep it readable). Returns whether the call trapped.
fn dispatch_request(
    spec: &InstanceSpec,
    store: &mut Store<HostState>,
    instance: &Plugin,
    request: &crate::bindings::types::PluginRequest,
    config: String,
    reply: tokio::sync::oneshot::Sender<Result<crate::bindings::types::PluginResponse, String>>,
    slot: tokio::sync::OwnedSemaphorePermit,
) -> bool {
    store.data_mut().config_json = config;
    spec.arm_deadline(store);
    let outcome = instance.call_handle_request(&mut *store, request);
    // Free the web slot before answering, so the caller's next request never
    // finds it still taken.
    drop(slot);
    match outcome {
        Ok(response) => {
            let _ = reply.send(Ok(response));
            false
        }
        Err(trap) => {
            let _ = reply.send(Err(format!("plugin call failed: {trap:#}")));
            true
        }
    }
}

/// Runs one `scan-media` guest call (extracted from the actor loop for the
/// same readability reason as [`dispatch_request`]). Returns whether the
/// call trapped.
fn dispatch_scan(
    spec: &InstanceSpec,
    store: &mut Store<HostState>,
    instance: &Plugin,
    item: &crate::bindings::types::ItemSummary,
    config: String,
    reply: tokio::sync::oneshot::Sender<Result<ScanMediaOutcome, String>>,
) -> bool {
    store.data_mut().config_json = config;
    store.data_mut().segment_policy_skipped = false;
    spec.arm_deadline(store);
    match instance.call_scan_media(&mut *store, item) {
        Ok(outcome) => {
            let _ = reply.send(Ok(ScanMediaOutcome {
                result: outcome,
                policy_skipped: store.data().segment_policy_skipped,
            }));
            false
        }
        Err(trap) => {
            let _ = reply.send(Ok(ScanMediaOutcome {
                result: Err(format!("plugin call failed: {trap:#}")),
                policy_skipped: store.data().segment_policy_skipped,
            }));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rstest::rstest;

    use super::{CallWatch, Unwatched, run_watched, web_request_slots};

    const BUDGET: Duration = Duration::from_millis(200);
    const GRACE: Duration = Duration::from_millis(800);

    /// Work that starts a guest call (publishes its deadline) and then
    /// blocks for `blocked`, the way a guest stuck in a host function does.
    fn call_blocking_for(
        watch: &Arc<CallWatch>,
        blocked: Duration,
    ) -> impl FnOnce() -> &'static str + Send + 'static {
        let watch = Arc::clone(watch);
        move || {
            watch.set(Some(Instant::now() + BUDGET));
            std::thread::sleep(blocked);
            "done"
        }
    }

    #[test]
    fn a_call_within_its_deadline_plus_grace_finishes() {
        let watch = Arc::new(CallWatch::new(BUDGET, GRACE));
        // Past the deadline, well inside the grace.
        let work = call_blocking_for(&watch, BUDGET + GRACE / 4);
        assert_eq!(run_watched(&watch, work).unwrap(), "done");
    }

    #[test]
    fn a_call_past_its_deadline_plus_grace_is_lost_and_abandoned() {
        let watch = Arc::new(CallWatch::new(BUDGET, GRACE));
        let started = Instant::now();
        let work = call_blocking_for(&watch, Duration::from_secs(30));
        assert!(matches!(run_watched(&watch, work), Err(Unwatched::Lost)));
        let waited = started.elapsed();
        assert!(waited >= BUDGET + GRACE, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[test]
    fn work_before_the_first_guest_call_is_not_timed() {
        // Compiling a component publishes no deadline: however long it
        // takes, nothing is lost.
        let watch = Arc::new(CallWatch::new(BUDGET, GRACE));
        let work = || {
            std::thread::sleep(BUDGET + GRACE * 2);
            "compiled"
        };
        assert_eq!(run_watched(&watch, work).unwrap(), "compiled");
    }

    #[test]
    fn panicking_work_is_reported() {
        let watch = CallWatch::new(BUDGET, GRACE);
        let work = || -> u8 { panic!("load blew up") };
        assert!(matches!(
            run_watched(&watch, work),
            Err(Unwatched::Panicked)
        ));
    }

    #[rstest]
    #[case::default_queue(256, 128)]
    #[case::even(4, 2)]
    #[case::odd_rounds_down(3, 1)]
    #[case::one_slot_queue_still_serves_the_web(1, 1)]
    fn web_requests_get_half_the_queue_and_at_least_one_slot(
        #[case] queue_capacity: usize,
        #[case] slots: usize,
    ) {
        assert_eq!(web_request_slots(queue_capacity), slots);
    }
}
