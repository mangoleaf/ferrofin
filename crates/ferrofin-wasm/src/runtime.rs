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
//!   full the event is dropped (with a debug log), never the server's time.
//!
//! The async `send().await` suspends (rather than parking a tokio worker)
//! when the queue is full, which is the right trade — but it is an
//! *unbounded* wait. A plugin that is slow without ever failing can keep its
//! queue full and stall a `run-task` call or a scan's per-item
//! `metadata-lookup` until it drains. That is bounded in practice by the
//! per-call epoch deadline (each queued call still trips it, driving the
//! breaker), but a genuinely slow-not-failing metadata plugin can drag a
//! scan. If that ever bites, give lookups a `try_send` + "plugin busy — skip
//! this item" fast path rather than making them wait.
//!
//! **Containment:** a trap, deadline overrun, or memory-cap hit fails only
//! the one call; the actor rebuilds a fresh instance for the next call. After
//! [`BREAKER_LIMIT`] consecutive failures the plugin is declared dead for the
//! rest of the process (commands are refused, events discarded) — the same
//! circuit-breaker shape as the HLS transcode restart breaker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    },
    /// Offer the guest one library item for media analysis.
    ScanMedia {
        /// The item, as the shared summary projection.
        item: crate::bindings::types::ItemSummary,
        /// Fresh configuration JSON to snapshot before the call.
        config: String,
        /// Receives the guest's outcome (or the host/trap error text).
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
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
    /// The shared blocking HTTP client behind `http-fetch`.
    pub http: Arc<reqwest::blocking::Client>,
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
            wasi: HostState::empty_wasi(),
            table: wasmtime::component::ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // Instantiation itself runs guest code (start sections, allocators),
        // so it gets a deadline too.
        store.set_epoch_deadline(self.timeout_ticks);
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)?;
        Ok((store, plugin))
    }
}

/// The sending half owned by the host: a bounded queue into the plugin's
/// runtime thread plus the shared dead flag.
pub struct RuntimeHandle {
    sender: Sender<Command>,
    dead: Arc<AtomicBool>,
    plugin_name: String,
}

impl RuntimeHandle {
    /// Whether the breaker has permanently tripped for this plugin.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Queues a task run and awaits its outcome.
    ///
    /// # Errors
    /// The guest's error text, the breaker being open, or the runtime thread
    /// being gone.
    pub async fn run_task(&self, task_id: String, config: String) -> Result<(), String> {
        if self.is_dead() {
            return Err(format!(
                "plugin `{}` is disabled until restart after {BREAKER_LIMIT} consecutive failures",
                self.plugin_name
            ));
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        // Async send: a queue full of pending events suspends this future
        // (bounded wait, the actor is draining) — it never parks the tokio
        // worker the way a blocking send would.
        self.sender
            .send(Command::RunTask {
                task_id,
                config,
                reply,
            })
            .await
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        rx.await
            .map_err(|_| "plugin runtime dropped the task reply".to_owned())?
    }

    /// Routes one HTTP request to the guest and awaits its response.
    ///
    /// # Errors
    /// The guest/host error text, the breaker being open, or the runtime
    /// thread being gone.
    pub async fn handle_request(
        &self,
        request: crate::bindings::types::PluginRequest,
        config: String,
    ) -> Result<crate::bindings::types::PluginResponse, String> {
        if self.is_dead() {
            return Err(format!(
                "plugin `{}` is disabled until restart after {BREAKER_LIMIT} consecutive failures",
                self.plugin_name
            ));
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::HandleRequest {
                request,
                config,
                reply,
            })
            .await
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        rx.await
            .map_err(|_| "plugin runtime dropped the request reply".to_owned())?
    }

    /// Offers the guest one item for media analysis and awaits its outcome.
    ///
    /// # Errors
    /// The guest's error text, the breaker being open, or the runtime
    /// thread being gone.
    pub async fn scan_media(
        &self,
        item: crate::bindings::types::ItemSummary,
        config: String,
    ) -> Result<(), String> {
        if self.is_dead() {
            return Err(format!(
                "plugin `{}` is disabled until restart after {BREAKER_LIMIT} consecutive failures",
                self.plugin_name
            ));
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::ScanMedia {
                item,
                config,
                reply,
            })
            .await
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        rx.await
            .map_err(|_| "plugin runtime dropped the scan reply".to_owned())?
    }

    /// Asks the guest for artwork candidates and awaits them.
    ///
    /// # Errors
    /// The guest's error text, the breaker being open, or the runtime
    /// thread being gone.
    pub async fn remote_images(
        &self,
        item: crate::bindings::types::ItemSummary,
        config: String,
    ) -> Result<Vec<crate::bindings::types::ImageCandidate>, String> {
        if self.is_dead() {
            return Err(format!(
                "plugin `{}` is disabled until restart after {BREAKER_LIMIT} consecutive failures",
                self.plugin_name
            ));
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::RemoteImages {
                item,
                config,
                reply,
            })
            .await
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        rx.await
            .map_err(|_| "plugin runtime dropped the images reply".to_owned())?
    }

    /// Asks the guest for metadata on one item and awaits its offer.
    ///
    /// # Errors
    /// The guest's error text, the breaker being open, or the runtime thread
    /// being gone.
    pub async fn metadata_lookup(
        &self,
        item: crate::bindings::types::ItemSummary,
        provider_ids: Vec<(String, String)>,
        config: String,
    ) -> Result<Option<crate::bindings::types::MetadataResult>, String> {
        if self.is_dead() {
            return Err(format!(
                "plugin `{}` is disabled until restart after {BREAKER_LIMIT} consecutive failures",
                self.plugin_name
            ));
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Command::MetadataLookup {
                item,
                provider_ids,
                config,
                reply,
            })
            .await
            .map_err(|_| "plugin runtime thread has exited".to_owned())?;
        rx.await
            .map_err(|_| "plugin runtime dropped the lookup reply".to_owned())?
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
        plugin_name: spec.plugin_name.clone(),
    };
    std::thread::Builder::new()
        .name(format!("wasm-plugin-{}", spec.plugin_name))
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

    while let Some(command) = receiver.blocking_recv() {
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
                store.set_epoch_deadline(spec.timeout_ticks);
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
            } => dispatch_request(spec, store, instance, &request, config, reply),
            Command::RemoteImages {
                item,
                config,
                reply,
            } => {
                store.data_mut().config_json = config;
                store.set_epoch_deadline(spec.timeout_ticks);
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
                store.set_epoch_deadline(spec.timeout_ticks);
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
                store.set_epoch_deadline(spec.timeout_ticks);
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
    let message = format!("plugin `{plugin_name}` is disabled until restart (circuit breaker)");
    match command {
        Command::RunTask { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Command::HandleRequest { reply, .. } => {
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
) -> bool {
    store.data_mut().config_json = config;
    store.set_epoch_deadline(spec.timeout_ticks);
    match instance.call_handle_request(&mut *store, request) {
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
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> bool {
    store.data_mut().config_json = config;
    store.set_epoch_deadline(spec.timeout_ticks);
    match instance.call_scan_media(&mut *store, item) {
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
