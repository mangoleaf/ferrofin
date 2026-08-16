//! The Tier-1b **WASM plugin host** — runtime-installable, sandboxed plugins
//! for Ferrofin (see `brain/plans/PLAN_PLUGIN_TIERS.md`).
//!
//! Users drop `ferrofin:plugin`-world components into `{data_dir}/plugins/`;
//! at startup [`WasmPluginHost::load`] compiles and instantiates each one,
//! and the composition root surfaces them through the **same seams as
//! compiled-in extensions**: [`RegisteredPlugin`] entries for the
//! `/Plugins` API and [`ScheduledTask`] registrations for the dashboard —
//! to `ferrofin-api` a WASM plugin is indistinguishable from a Tier-1a one.
//!
//! # Security model
//! Guests get **no filesystem and no direct network** — the world imports
//! exactly one host interface (`log`, `get-config`, host-mediated
//! `http-fetch`, read-only `query-items`, provider-scoped
//! `write-media-segments`), so `wit/ferrofin-plugin.wit` is the entire
//! attack surface. Every guest call
//! runs under an epoch deadline (`FERROFIN_WASM_CALL_TIMEOUT_SECS`) and a
//! linear-memory cap (`FERROFIN_WASM_MEMORY_LIMIT_MB`); a trap, overrun, or
//! cap hit fails that call only, and repeated failures trip a breaker that
//! deadlines the plugin until restart ([`runtime::BREAKER_LIMIT`]). The
//! server never goes down with a plugin.

pub mod bindings;
pub mod capabilities;
pub mod runtime;

/// The hand-written canonical-ABI component implementing the
/// `ferrofin:plugin@0.4.0` world, as WAT text — the shared test fixture for
/// this crate's host tests and the server-level HTTP test (compiled at test
/// time via the `wat` crate; no `.wasm` binaries in the repo). Not a public
/// API: test support only.
#[doc(hidden)]
pub const TEST_FIXTURE_WAT: &str = include_str!("test_fixture.wat");

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{error, info, warn};
use uuid::Uuid;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config as WasmConfig, Engine};

use ferrofin_core::{
    FerrofinEventManager, PluginConfigPage, RegisteredPlugin, ScheduledTask, TaskProgress,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};

use crate::bindings::HostState;
use crate::runtime::{EPOCH_TICK, InstanceSpec, RuntimeHandle};

/// The domain events forwarded to every enabled plugin's `on-event` export —
/// exactly the set `FerrofinEventManager` publishes today. Extend this list
/// as new publish sites land (plan §3).
pub const FORWARDED_EVENTS: [&str; 9] = [
    "LibraryChanged",
    "RefreshProgress",
    "PlaybackStart",
    "PlaybackProgress",
    "PlaybackStopped",
    "SessionStarted",
    "SessionEnded",
    "AuthenticationSucceeded",
    "TaskCompleted",
];

/// Resolved host settings, with the plan's defaults applied.
///
/// All three are `FERROFIN_*` bootstrap knobs (settings-over-constants,
/// decided 2026-08-06/07); `None`s from the config layer land on the
/// defaults here.
#[derive(Debug, Clone)]
pub struct WasmSettings {
    /// Per-guest-call deadline in seconds (`FERROFIN_WASM_CALL_TIMEOUT_SECS`,
    /// default 30 — confirmed 2026-08-07).
    pub call_timeout_secs: u32,
    /// Per-plugin linear-memory cap in MiB (`FERROFIN_WASM_MEMORY_LIMIT_MB`,
    /// default 128 — confirmed 2026-08-13 after the measured ~6–7 MiB
    /// marginal RSS per plugin; a `memory.grow` ceiling, never a reservation).
    pub memory_limit_mb: u32,
    /// Per-plugin event queue depth (`FERROFIN_WASM_EVENT_QUEUE_CAPACITY`,
    /// default 256 — inherits the approved bus-capacity setting).
    pub event_queue_capacity: u32,
    /// Per-plugin total KV-state cap in MiB
    /// (`FERROFIN_WASM_STATE_LIMIT_MB`, default 8 — settings/cursors fit
    /// easily; stats-heavy plugins may need more).
    pub state_limit_mb: u32,
    /// Plugin ids allowed to reach private/loopback HTTP destinations
    /// (`FERROFIN_WASM_PRIVATE_HTTP_ALLOW`: comma-separated plugin UUIDs, or
    /// `*` for every plugin). Default empty: private destinations denied.
    /// KNOWN UX DEBT: UUIDs are hard to audit later — accepting plugin
    /// names here too is a planned improvement.
    pub private_http_allow: Vec<String>,
}

impl Default for WasmSettings {
    fn default() -> Self {
        Self {
            call_timeout_secs: 30,
            memory_limit_mb: 128,
            event_queue_capacity: 256,
            state_limit_mb: 8,
            private_http_allow: Vec::new(),
        }
    }
}

impl WasmSettings {
    /// Overrides the per-plugin total state cap
    /// (`FERROFIN_WASM_STATE_LIMIT_MB`; `None`/zero keeps the default).
    #[must_use]
    pub fn with_state_limit_mb(mut self, mb: Option<u32>) -> Self {
        if let Some(mb) = mb.filter(|&v| v > 0) {
            self.state_limit_mb = mb;
        }
        self
    }

    /// Whether the allowlist grants plugin `id` private-HTTP access.
    #[must_use]
    pub fn allows_private_http(&self, id: Uuid) -> bool {
        self.private_http_allow.iter().any(|entry| {
            let entry = entry.trim();
            entry == "*" || Uuid::parse_str(entry).is_ok_and(|u| u == id)
        })
    }
}

impl WasmSettings {
    /// Overlays the optional bootstrap-config values onto the defaults,
    /// ignoring zeros (a zero limit would make every call/allocation fail —
    /// treat it as "unset", matching `FERROFIN_METRICS_SAMPLE_INTERVAL`).
    #[must_use]
    pub fn resolve(
        call_timeout_secs: Option<u32>,
        memory_limit_mb: Option<u32>,
        event_queue_capacity: Option<u32>,
        private_http_allow: Option<&str>,
    ) -> Self {
        let d = Self::default();
        Self {
            state_limit_mb: d.state_limit_mb,
            call_timeout_secs: call_timeout_secs
                .filter(|&v| v > 0)
                .unwrap_or(d.call_timeout_secs),
            memory_limit_mb: memory_limit_mb
                .filter(|&v| v > 0)
                .unwrap_or(d.memory_limit_mb),
            event_queue_capacity: event_queue_capacity
                .filter(|&v| v > 0)
                .unwrap_or(d.event_queue_capacity),
            private_http_allow: private_http_allow
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|e| !e.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// One successfully loaded plugin: its identity, seed config, advertised
/// tasks, and the handle into its runtime thread.
pub struct LoadedPlugin {
    /// The `/Plugins` descriptor built from the guest's `descriptor` export.
    pub descriptor: PluginDescriptor,
    /// The guest's `default-config` JSON (validated as JSON at load).
    pub default_config: Vec<u8>,
    /// The guest's advertised tasks.
    pub tasks: Vec<bindings::types::TaskDescriptor>,
    /// The guest's authored dashboard settings pages (may be empty — the
    /// host synthesizes a generic JSON editor in that case; see
    /// [`fallback_config_page`]).
    pub config_pages: Vec<bindings::types::ConfigPage>,
    /// The guest's declared web-file transformations (applied by the host
    /// while the plugin is enabled; see the WIT trust note).
    pub web_transforms: Vec<bindings::types::WebTransform>,
    /// The item kinds this plugin analyzes (empty = not an analyzer).
    pub scan_targets: Vec<String>,
    /// The plugin's named-provider identity, when it is one.
    pub provider_info: Option<bindings::types::ProviderDescriptor>,
    /// Where the plugin's KV state persists — the analysis driver keeps its
    /// offer-once watermark there under a host-reserved key.
    state_path: std::path::PathBuf,
    runtime: RuntimeHandle,
    /// Short-lived enabled-flag snapshot for the event fan-out. Events fire
    /// often (`PlaybackProgress` per session per tick); without this each one
    /// would spawn a task and read the plugin manager just to decide whether
    /// to deliver. The TTL only delays a dashboard toggle taking effect on
    /// event delivery — never correctness (events are droppable hints).
    enabled_cache: std::sync::Mutex<Option<(std::time::Instant, bool)>>,
}

/// How long the event fan-out trusts a cached enabled flag before refreshing
/// — the same seconds-scale window as the metadata gate cache.
const EVENT_ENABLED_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

impl LoadedPlugin {
    /// Drives `run-task` directly with an empty config snapshot, bypassing
    /// the enabled-flag gate — a test seam for exercising containment paths
    /// (traps, deadlines, the limiter, the breaker) that the fixture exposes
    /// as task ids. Not for production call sites: real runs go through the
    /// [`ScheduledTask`] adapter.
    #[doc(hidden)]
    pub async fn run_task_for_test(&self, task_id: String) -> Result<(), String> {
        self.runtime.run_task(task_id, String::from("{}")).await
    }

    /// Drives `handle-request` directly with an empty config — the same
    /// test seam as [`run_task_for_test`], for exercising the request
    /// path's containment (traps, the breaker) without the HTTP layer.
    ///
    /// [`run_task_for_test`]: Self::run_task_for_test
    #[doc(hidden)]
    pub async fn handle_request_for_test(
        &self,
        request: bindings::types::PluginRequest,
    ) -> Result<bindings::types::PluginResponse, String> {
        self.runtime
            .handle_request(request, String::from("{}"))
            .await
    }

    /// The cached enabled flag if it is still fresh, else `None` (caller must
    /// refresh from the plugin manager).
    fn cached_enabled(&self) -> Option<bool> {
        self.enabled_cache
            .lock()
            .expect("enabled cache lock poisoned")
            .and_then(|(at, enabled)| (at.elapsed() < EVENT_ENABLED_CACHE_TTL).then_some(enabled))
    }

    /// Records a freshly-read enabled flag.
    fn store_enabled(&self, enabled: bool) {
        *self
            .enabled_cache
            .lock()
            .expect("enabled cache lock poisoned") = Some((std::time::Instant::now(), enabled));
    }
}

/// The loaded plugin set plus the shared engine machinery.
pub struct WasmPluginHost {
    plugins: Vec<Arc<LoadedPlugin>>,
    /// One cell shared by every plugin's `HostState` (and every rebuild):
    /// filling it arms `query-items`/`write-media-segments` host functions.
    collaborators: Arc<std::sync::OnceLock<capabilities::Collaborators>>,
}

impl WasmPluginHost {
    /// Scans `plugins_dir` for `*.wasm` components and loads each one.
    ///
    /// **Blocking** (compilation is CPU-heavy) — call via `spawn_blocking`
    /// from async contexts. A missing directory is an empty host, not an
    /// error. A component that fails to compile, links against a different
    /// world version, reports an invalid descriptor, or duplicates an id is
    /// **skipped with an `error!`** — one bad file never blocks the server
    /// or the other plugins.
    ///
    /// # Errors
    /// Only on engine construction failure (a wasmtime misconfiguration —
    /// effectively a bug, but the caller decides whether to boot without
    /// WASM support rather than us panicking).
    ///
    /// # Panics
    /// Only if the OS refuses to spawn the epoch-ticker thread (startup-time
    /// resource exhaustion — the process is already unviable).
    pub fn load(plugins_dir: &Path, settings: &WasmSettings) -> Result<Self, ServiceError> {
        let (engine, linker) = build_runtime_parts(settings)?;
        // load() runs on a blocking thread (spawn_blocking at the composition
        // root), so eager client construction is safe here.
        let http = build_guest_http_client(settings)?;
        let collaborators: Arc<std::sync::OnceLock<capabilities::Collaborators>> =
            Arc::new(std::sync::OnceLock::new());

        let mut plugins: Vec<Arc<LoadedPlugin>> = Vec::new();
        let mut paths: Vec<_> = match std::fs::read_dir(plugins_dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "wasm"))
                .collect(),
            // A missing directory is the normal no-plugins state; anything
            // else (permissions, I/O) must not masquerade as it.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                warn!(
                    dir = %plugins_dir.display(),
                    %err,
                    "cannot read the wasm plugins directory; continuing without plugins"
                );
                Vec::new()
            }
        };
        paths.sort();

        for path in paths {
            match load_one(&engine, &linker, &path, settings, &http, &collaborators) {
                Ok(loaded) => {
                    if plugins
                        .iter()
                        .any(|p| p.descriptor.id == loaded.descriptor.id)
                    {
                        error!(
                            path = %path.display(),
                            plugin_id = %loaded.descriptor.id,
                            "duplicate wasm plugin id; skipping this file"
                        );
                        continue;
                    }
                    info!(
                        plugin = loaded.descriptor.name,
                        plugin_id = %loaded.descriptor.id,
                        version = loaded.descriptor.version,
                        tasks = loaded.tasks.len(),
                        path = %path.display(),
                        "loaded wasm plugin"
                    );
                    plugins.push(Arc::new(loaded));
                }
                Err(err) => {
                    error!(
                        path = %path.display(),
                        error = format!("{err:#}"),
                        "failed to load wasm plugin (expected a ferrofin:plugin@0.5.0 \
                         component); skipping this file"
                    );
                }
            }
        }

        Ok(Self {
            plugins,
            collaborators,
        })
    }

    /// A host with no plugins (no directory scanned, no engine started).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            collaborators: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Builds the [`DynamicMetadataProvider`] adapters for every loaded
    /// plugin — the scanner's dynamic metadata pass (`metadata-lookup` in
    /// the world). Each adapter self-gates on the plugin's enabled flag via
    /// the collaborators cell, so it is inert until
    /// [`set_runtime_collaborators`](Self::set_runtime_collaborators) and
    /// while the plugin is disabled.
    ///
    /// [`DynamicMetadataProvider`]: ferrofin_traits::providers::DynamicMetadataProvider
    #[must_use]
    pub fn metadata_providers(
        &self,
    ) -> Vec<Arc<dyn ferrofin_traits::providers::DynamicMetadataProvider>> {
        self.plugins
            .iter()
            .map(|plugin| {
                Arc::new(WasmMetadataProvider {
                    plugin: Arc::clone(plugin),
                    collaborators: Arc::clone(&self.collaborators),
                    gate_cache: std::sync::Mutex::new(None),
                }) as Arc<dyn ferrofin_traits::providers::DynamicMetadataProvider>
            })
            .collect()
    }

    /// Arms the `query-items` / `write-media-segments` host functions with
    /// their backing managers. Called once by the composition root after the
    /// managers exist; a guest calling these before that (i.e. during its
    /// own load) gets a clean error, never a hang. A second call is a no-op.
    pub fn set_runtime_collaborators(&self, collaborators: capabilities::Collaborators) {
        let _ = self.collaborators.set(collaborators);
    }

    /// The loaded plugins.
    #[must_use]
    pub fn plugins(&self) -> &[Arc<LoadedPlugin>] {
        &self.plugins
    }

    /// Projects every loaded plugin into a [`RegisteredPlugin`] for the
    /// shared `/Plugins` registry — the same projection
    /// `ferrofin_extensions::registered_plugins` does for Tier 1a.
    #[must_use]
    pub fn registered_plugins(&self) -> Vec<RegisteredPlugin> {
        self.plugins
            .iter()
            .map(|p| {
                let mut registered = RegisteredPlugin::new(p.descriptor.clone(), None)
                    .with_default_config(p.default_config.clone());
                for page in config_pages_for(&p.descriptor, &p.config_pages) {
                    registered = registered.with_config_page(page);
                }
                registered
            })
            .collect()
    }

    /// Builds the [`ScheduledTask`] adapters for every task every plugin
    /// advertised. Each adapter self-gates on the plugin's enabled flag at
    /// execute time (the Tier-1a pattern), so toggling needs no restart.
    #[must_use]
    pub fn scheduled_tasks(
        &self,
        plugin_manager: &Arc<dyn PluginManager>,
    ) -> Vec<Arc<dyn ScheduledTask>> {
        let mut out: Vec<Arc<dyn ScheduledTask>> = Vec::new();
        for plugin in &self.plugins {
            for task in &plugin.tasks {
                out.push(Arc::new(WasmTask {
                    key: format!("wasm-{}-{}", plugin.descriptor.id, task.id),
                    plugin: Arc::clone(plugin),
                    task: task.clone(),
                    plugin_manager: Arc::clone(plugin_manager),
                }));
            }
        }
        out
    }

    /// Subscribes every loaded plugin to the [`FORWARDED_EVENTS`] on the
    /// event manager. Delivery follows the composition root's established
    /// non-blocking pattern: the consumer spawns, checks the enabled flag,
    /// and `try_send`s into the plugin's bounded queue — publication never
    /// waits on a guest.
    pub fn subscribe_events(
        &self,
        events: &FerrofinEventManager,
        plugin_manager: &Arc<dyn PluginManager>,
    ) {
        for event_name in FORWARDED_EVENTS {
            for plugin in &self.plugins {
                let plugin = Arc::clone(plugin);
                let plugin_manager = Arc::clone(plugin_manager);
                events.subscribe(
                    event_name,
                    Arc::new(move |payload: &str| {
                        if plugin.runtime.is_dead() {
                            return Ok(());
                        }
                        // Fast path: a fresh enabled flag needs no manager read
                        // and no spawn — deliver (or skip) synchronously. This
                        // is what most events hit.
                        if let Some(enabled) = plugin.cached_enabled() {
                            if enabled {
                                plugin.runtime.deliver_event(event_name, payload);
                            }
                            return Ok(());
                        }
                        // Slow path (once per TTL): refresh the flag off-thread
                        // (the manager call is async), cache it, then deliver.
                        let plugin = Arc::clone(&plugin);
                        let plugin_manager = Arc::clone(&plugin_manager);
                        let payload = payload.to_owned();
                        tokio::spawn(async move {
                            let enabled = matches!(
                                plugin_manager.get_plugin(plugin.descriptor.id).await,
                                Ok(Some(descriptor)) if descriptor.enabled
                            );
                            plugin.store_enabled(enabled);
                            if enabled {
                                plugin.runtime.deliver_event(event_name, &payload);
                            }
                        });
                        Ok(())
                    }),
                );
            }
        }
    }
}

/// Compiles, instantiates and interrogates a single component file.
fn load_one(
    engine: &Engine,
    linker: &Arc<Linker<HostState>>,
    path: &Path,
    settings: &WasmSettings,
    http: &Arc<reqwest::blocking::Client>,
    collaborators: &Arc<std::sync::OnceLock<capabilities::Collaborators>>,
) -> Result<LoadedPlugin, wasmtime::Error> {
    let component = Component::from_file(engine, path)?;
    let spec = InstanceSpec {
        engine: engine.clone(),
        component,
        linker: Arc::clone(linker),
        plugin_name: path.file_stem().map_or_else(
            || "unknown".to_owned(),
            |s| s.to_string_lossy().into_owned(),
        ),
        plugin_id: String::new(), // filled from the descriptor below
        memory_limit_bytes: settings.memory_limit_mb as usize * 1024 * 1024,
        timeout_ticks: u64::from(settings.call_timeout_secs),
        http: Arc::clone(http),
        // Loading calls (`descriptor` etc.) run before the id is known, so
        // the safe default applies; the real grant is applied below.
        private_http_allowed: false,
        collaborators: Arc::clone(collaborators),
        state_path: None, // id-derived; set below once the descriptor is read
        // Load-time calls can't fetch at all; the real policy is read below.
        egress: Arc::new(capabilities::EgressPolicy::default()),
        state_total_cap: settings.state_limit_mb as usize * 1024 * 1024,
    };

    let (mut store, instance) = spec.instantiate(String::from("{}"))?;
    let wire = instance.call_descriptor(&mut store)?;
    let id: Uuid = wire.id.parse().map_err(|_| {
        wasmtime::Error::msg(format!(
            "plugin descriptor id `{}` is not a valid UUID",
            wire.id
        ))
    })?;
    // State is available from here on (the plan allows it at load — it is
    // local, with no exfil channel): the id names the file.
    let state_path = path.with_file_name(format!("{id}.state.json"));
    store.data_mut().state_path = Some(state_path.clone());
    let default_config = instance.call_default_config(&mut store)?;
    serde_json::from_str::<serde_json::Value>(&default_config).map_err(|e| {
        wasmtime::Error::msg(format!("plugin default-config is not valid JSON: {e}"))
    })?;
    let tasks = instance.call_tasks(&mut store)?;
    let config_pages = instance.call_config_pages(&mut store)?;
    let web_transforms = instance.call_web_transforms(&mut store)?;
    let scan_targets = instance.call_scan_targets(&mut store)?;
    let provider_info = instance.call_provider_info(&mut store)?;
    let declared_egress = instance.call_declared_egress(&mut store)?;
    let egress = Arc::new(capabilities::EgressPolicy::parse(&declared_egress));
    if egress.allow_any {
        warn!(
            plugin = %path.display(),
            "plugin declares UNRESTRICTED public egress (`*`) — it may contact \
             any internet host; install only if you trust it"
        );
    } else if !declared_egress.is_empty() {
        info!(
            plugin = %path.display(),
            hosts = ?declared_egress,
            "plugin declared public-egress allowlist"
        );
    }

    let descriptor = PluginDescriptor {
        id,
        name: wire.name.clone(),
        version: wire.version,
        description: wire.description,
        enabled: true, // the plugin manager overlays persisted state
        has_image: false,
        can_uninstall: false,
    };

    // Re-tag the spec with the real identity + its private-HTTP grant, then
    // hand the warm instance to its runtime thread.
    let spec = InstanceSpec {
        plugin_name: wire.name,
        plugin_id: id.to_string(),
        private_http_allowed: settings.allows_private_http(id),
        state_path: Some(state_path.clone()),
        egress: Arc::clone(&egress),
        ..spec
    };
    // The already-made calls used the placeholder identity in HostState; fix
    // the live store's copy too so guest log lines carry the real name and
    // the real network grant.
    store.data_mut().plugin_name.clone_from(&spec.plugin_name);
    store.data_mut().plugin_id.clone_from(&spec.plugin_id);
    store.data_mut().private_http_allowed = spec.private_http_allowed;
    store.data_mut().egress = egress;

    let runtime = runtime::spawn(
        spec,
        store,
        instance,
        settings.event_queue_capacity as usize,
    );

    Ok(LoadedPlugin {
        descriptor,
        default_config: default_config.into_bytes(),
        tasks,
        config_pages,
        web_transforms,
        scan_targets,
        provider_info,
        state_path,
        runtime,
        enabled_cache: std::sync::Mutex::new(None),
    })
}

/// Builds the shared wasm runtime pieces: an epoch-ticked engine (weak-held
/// by its ticker thread) and the world+WASI linker. Shared by
/// [`WasmPluginHost::load`] and [`WasmArtifactValidator`] so they can never
/// drift. The guest HTTP client is built separately
/// ([`build_guest_http_client`]) because `reqwest::blocking::Client`
/// construction panics inside an async runtime context — callers must build
/// it on a blocking thread.
fn build_runtime_parts(
    settings: &WasmSettings,
) -> Result<(Engine, Arc<Linker<HostState>>), ServiceError> {
    let _ = settings; // engine construction has no tunables today
    let mut config = WasmConfig::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config)
        .map_err(|e| ServiceError::backend(format!("wasm engine init failed: {e:#}")))?;

    // One ticker advances the epoch for every plugin of this engine.
    // 1 tick == 1 second == the unit of FERROFIN_WASM_CALL_TIMEOUT_SECS.
    // The ticker holds only a WEAK engine handle: when the last real handle
    // drops, the upgrade fails and the thread exits instead of pinning the
    // engine and its compiled code forever.
    {
        let engine = engine.weak();
        std::thread::Builder::new()
            .name("wasm-epoch-ticker".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(EPOCH_TICK);
                    let Some(engine) = engine.upgrade() else {
                        break;
                    };
                    engine.increment_epoch();
                }
            })
            .expect("spawning the wasm epoch ticker cannot fail");
    }

    let mut linker: Linker<HostState> = Linker::new(&engine);
    bindings::Plugin::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state| state,
    )
    .map_err(|e| ServiceError::backend(format!("wasm linker setup failed: {e:#}")))?;
    // Locked-down WASI so std guests link (the ctx grants nothing — see
    // HostState).
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| ServiceError::backend(format!("wasi linker setup failed: {e:#}")))?;
    let linker = Arc::new(linker);

    Ok((engine, linker))
}

/// The guest HTTP client: call-timeout bound, redirects off. MUST be built
/// on a blocking thread — `reqwest::blocking::Client` construction (and
/// drop) panics inside an async runtime context.
fn build_guest_http_client(
    settings: &WasmSettings,
) -> Result<Arc<reqwest::blocking::Client>, ServiceError> {
    Ok(Arc::new(
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(u64::from(
                settings.call_timeout_secs,
            )))
            // No transparent redirects: any destination policy would be
            // bypassed by a 302 — a guest can follow Location itself.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ferrofin-wasm/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ServiceError::backend(format!("wasm http client init: {e:#}")))?,
    ))
}

/// Projects a plugin's authored settings pages into the shared registry
/// shape — or, when the guest ships none, a synthesized generic JSON editor
/// over its configuration, so every WASM plugin is configurable from the
/// dashboard (jellyfin-web only shows a Settings button for plugins with a
/// page carrying their `PluginId`).
fn config_pages_for(
    descriptor: &PluginDescriptor,
    authored: &[bindings::types::ConfigPage],
) -> Vec<PluginConfigPage> {
    // Guest page bytes are held resident and cloned per (anonymous) request
    // to `GET /web/ConfigurationPage`, so they must be bounded at load —
    // otherwise a hostile plugin turns that endpoint into an unauthenticated
    // memory amplifier. Hardcoded like the manifest cap: real plugin pages
    // are tens of KB, so these are abuse guards, not tuning knobs.
    const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
    const MAX_PLUGIN_PAGES_BYTES: usize = 16 * 1024 * 1024;
    let mut total = 0usize;
    let kept: Vec<PluginConfigPage> = authored
        .iter()
        .filter(|page| {
            let within = page.content.len() <= MAX_PAGE_BYTES
                && total + page.content.len() <= MAX_PLUGIN_PAGES_BYTES;
            if within {
                total += page.content.len();
            } else {
                tracing::warn!(
                    plugin = %descriptor.id,
                    page = %page.name,
                    bytes = page.content.len(),
                    "wasm plugin settings page exceeds the size cap; dropping it"
                );
            }
            within
        })
        .map(|page| PluginConfigPage {
            name: page.name.clone(),
            bytes: page.content.clone(),
            enable_in_main_menu: page.enable_in_main_menu,
        })
        .collect();
    if kept.is_empty() {
        // No usable authored page (none shipped, or all oversized) — the
        // synthesized editor keeps the plugin configurable either way.
        return vec![fallback_config_page(descriptor)];
    }
    kept
}

/// The synthesized settings page: the canonical jellyfin-web plugin-page
/// shape (`data-role="page"` root + inline script using the `ApiClient` /
/// `Dashboard` globals) wrapping a JSON editor over the plugin's config —
/// loaded via `ApiClient.getPluginConfiguration` and saved via
/// `ApiClient.updatePluginConfiguration`, exactly like an authored page.
///
/// The page name is `wasm-settings-{id}` — unique per plugin and stable, so
/// dashboard bookmarks survive restarts.
fn fallback_config_page(descriptor: &PluginDescriptor) -> PluginConfigPage {
    // The id is a validated UUID (safe to embed raw); the display name is
    // guest-supplied and must be escaped for the HTML context.
    let id = descriptor.id;
    let title = escape_html(&descriptor.name);
    let html = format!(
        r#"<div id="wasmConfig-{id}" data-role="page" class="page type-interior pluginConfigurationPage">
  <div data-role="content"><div class="content-primary">
    <form class="wasmConfigForm">
      <h1>{title}</h1>
      <p>This plugin ships no settings page of its own; edit its configuration JSON directly.</p>
      <div class="inputContainer">
        <textarea is="emby-textarea" class="textarea-mono wasmConfigJson" rows="16" spellcheck="false" style="width:100%;font-family:monospace;"></textarea>
      </div>
      <button is="emby-button" type="submit" class="raised button-submit block"><span>Save</span></button>
    </form>
  </div></div>
  <script type="text/javascript">
  (function () {{
    var pluginId = '{id}';
    var page = document.querySelector('#wasmConfig-{id}');
    page.addEventListener('pageshow', function () {{
      Dashboard.showLoadingMsg();
      ApiClient.getPluginConfiguration(pluginId).then(function (config) {{
        page.querySelector('.wasmConfigJson').value = JSON.stringify(config, null, 2);
        Dashboard.hideLoadingMsg();
      }}).catch(Dashboard.processErrorResponse);
    }});
    page.querySelector('.wasmConfigForm').addEventListener('submit', function (e) {{
      e.preventDefault();
      var parsed;
      try {{
        parsed = JSON.parse(page.querySelector('.wasmConfigJson').value);
      }} catch (err) {{
        Dashboard.alert('Configuration must be valid JSON: ' + err.message);
        return false;
      }}
      Dashboard.showLoadingMsg();
      ApiClient.updatePluginConfiguration(pluginId, parsed).then(
        Dashboard.processPluginConfigurationUpdateResult
      ).catch(Dashboard.processErrorResponse);
      return false;
    }});
  }})();
  </script>
</div>
"#
    );
    PluginConfigPage {
        name: format!("wasm-settings-{id}"),
        bytes: html.into_bytes(),
        enable_in_main_menu: false,
    }
}

/// Minimal HTML text-context escaping for guest-supplied strings embedded
/// in the synthesized page.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The plugin ABI this build of Ferrofin supports — the `ferrofin:plugin`
/// world version from `wit/ferrofin-plugin.wit` (a test guards against
/// drift). Repository manifests must declare this exact `targetAbi` at 0.x.
pub const PLUGIN_ABI: &str = "ferrofin:plugin@0.5.0";

/// The install-time artifact validator: proves a downloaded `.wasm` is a
/// loadable `ferrofin:plugin` component and reports its self-declared id,
/// in a throwaway store with the standard limits. The store's capability
/// cell is never armed and `http-fetch` is denied during load, so the
/// artifact is **network-mute** while being validated — a malicious
/// component cannot phone home from its `descriptor` export.
pub struct WasmArtifactValidator {
    engine: Engine,
    linker: Arc<Linker<HostState>>,
    /// Built lazily on the first validation's blocking thread —
    /// `reqwest::blocking::Client` construction panics in async contexts,
    /// and `new()` is called from the (async) composition root.
    http: Arc<std::sync::OnceLock<Arc<reqwest::blocking::Client>>>,
    settings: WasmSettings,
}

impl WasmArtifactValidator {
    /// Builds a validator with its own engine (same construction as the
    /// plugin host, via the shared builder). Safe to call from async
    /// contexts — the blocking HTTP client is deferred to first use.
    ///
    /// # Errors
    /// Engine/linker construction failure (a wasmtime misconfiguration).
    pub fn new(settings: &WasmSettings) -> Result<Self, ServiceError> {
        let (engine, linker) = build_runtime_parts(settings)?;
        Ok(Self {
            engine,
            linker,
            http: Arc::new(std::sync::OnceLock::new()),
            settings: settings.clone(),
        })
    }
}

#[async_trait]
impl ferrofin_traits::plugins::PluginArtifactValidator for WasmArtifactValidator {
    fn supported_abi(&self) -> &str {
        PLUGIN_ABI
    }

    async fn validate(
        &self,
        bytes: &[u8],
    ) -> Result<ferrofin_traits::plugins::ValidatedArtifact, ServiceError> {
        let engine = self.engine.clone();
        let linker = Arc::clone(&self.linker);
        let http_cell = Arc::clone(&self.http);
        let settings = self.settings.clone();
        let bytes = bytes.to_vec();
        // Compilation + the descriptor call are CPU-bound guest work — off
        // the async workers, exactly like the plugin runtime threads.
        tokio::task::spawn_blocking(move || {
            // Blocking thread: safe place to build the blocking client.
            let http = if let Some(client) = http_cell.get() {
                Arc::clone(client)
            } else {
                let client = build_guest_http_client(&settings)?;
                let _ = http_cell.set(Arc::clone(&client));
                client
            };
            let component = Component::new(&engine, &bytes).map_err(|e| {
                ServiceError::invalid_input(format!(
                    "artifact is not a valid WebAssembly component for {PLUGIN_ABI}: {e:#}"
                ))
            })?;
            let spec = InstanceSpec {
                engine,
                component,
                linker,
                plugin_name: "install-validation".to_owned(),
                plugin_id: String::new(),
                memory_limit_bytes: settings.memory_limit_mb as usize * 1024 * 1024,
                timeout_ticks: u64::from(settings.call_timeout_secs),
                http,
                private_http_allowed: false,
                state_path: None, // validation is throwaway — no persistence
                egress: Arc::new(capabilities::EgressPolicy::default()),
                state_total_cap: settings.state_limit_mb as usize * 1024 * 1024,
                // Never armed: query-items/write-media-segments/http-fetch
                // all refuse during validation.
                collaborators: Arc::new(std::sync::OnceLock::new()),
            };
            let (mut store, instance) = spec.instantiate(String::from("{}")).map_err(|e| {
                ServiceError::invalid_input(format!(
                    "artifact does not instantiate as a {PLUGIN_ABI} plugin: {e:#}"
                ))
            })?;
            let wire = instance.call_descriptor(&mut store).map_err(|e| {
                ServiceError::invalid_input(format!("artifact descriptor call failed: {e:#}"))
            })?;
            let id = wire.id.parse::<Uuid>().map_err(|_| {
                ServiceError::invalid_input(format!(
                    "artifact descriptor id `{}` is not a valid UUID",
                    wire.id
                ))
            })?;
            let declared_egress = instance.call_declared_egress(&mut store).map_err(|e| {
                ServiceError::invalid_input(format!("artifact declared-egress call failed: {e:#}"))
            })?;
            Ok(ferrofin_traits::plugins::ValidatedArtifact {
                id,
                declared_egress,
            })
        })
        .await
        .map_err(|e| ServiceError::backend(format!("validation task failed: {e}")))?
    }
}

/// The [`DynamicMetadataProvider`] adapter for one loaded plugin: converts
/// the scanner's lookup into the guest's `metadata-lookup` export and back.
///
/// [`DynamicMetadataProvider`]: ferrofin_traits::providers::DynamicMetadataProvider
struct WasmMetadataProvider {
    plugin: Arc<LoadedPlugin>,
    collaborators: Arc<std::sync::OnceLock<capabilities::Collaborators>>,
    /// Short-lived (enabled, config) snapshot so a 100k-item scan does one
    /// flag/config read per interval instead of two per item. The TTL only
    /// delays a mid-scan dashboard toggle taking effect — never correctness.
    gate_cache: std::sync::Mutex<Option<(std::time::Instant, bool, String)>>,
}

/// How long a [`WasmMetadataProvider`] trusts its (enabled, config)
/// snapshot before re-reading. Seconds-scale: any value ≫ per-item cost and
/// ≪ human toggle latency works; not worth a setting.
const METADATA_GATE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[async_trait]
impl ferrofin_traits::providers::DynamicMetadataProvider for WasmMetadataProvider {
    fn name(&self) -> &str {
        &self.plugin.descriptor.name
    }

    async fn lookup(
        &self,
        item: &ferrofin_traits::providers::DynamicMetadataLookup,
    ) -> Result<Option<ferrofin_traits::providers::DynamicMetadataResult>, ServiceError> {
        // Inert until the composition root arms the collaborators (a scan
        // cannot run before that) and while the plugin is disabled.
        let Some(cx) = self.collaborators.get() else {
            return Ok(None);
        };
        let cached = self
            .gate_cache
            .lock()
            .expect("gate cache lock poisoned")
            .clone()
            .filter(|(at, _, _)| at.elapsed() < METADATA_GATE_CACHE_TTL);
        let (enabled, config) = if let Some((_, enabled, config)) = cached {
            (enabled, config)
        } else {
            {
                let enabled = cx
                    .plugins
                    .get_plugin(self.plugin.descriptor.id)
                    .await?
                    .is_some_and(|d| d.enabled);
                let config = cx
                    .plugins
                    .get_plugin_configuration(self.plugin.descriptor.id)
                    .await
                    .map_or_else(
                        |_| String::from("{}"),
                        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
                    );
                *self.gate_cache.lock().expect("gate cache lock poisoned") =
                    Some((std::time::Instant::now(), enabled, config.clone()));
                (enabled, config)
            }
        };
        if !enabled {
            return Ok(None);
        }

        let wire_item = bindings::types::ItemSummary {
            id: item.item_id.to_string(),
            name: item.name.clone(),
            kind: item.kind.clone(),
            path: item.path.clone(),
            parent_id: None,
            run_time_ticks: None,
            // The scan offer carries identity only — the plugin queries for
            // anything richer; per-user fields never apply to a scan.
            genres: Vec::new(),
            premiere_date: None,
            date_created: None,
            community_rating: None,
            production_year: None,
            is_folder: false,
            played: None,
            is_favorite: None,
            playback_position_ticks: None,
        };
        let offer = self
            .plugin
            .runtime
            .metadata_lookup(wire_item, item.provider_ids.clone(), config)
            .await
            .map_err(ServiceError::backend)?;

        Ok(
            offer.map(|m| ferrofin_traits::providers::DynamicMetadataResult {
                tagline: m.tagline,
                studios: m.studios,
                tags: m.tags,
                official_rating: m.official_rating,
                end_date: m.end_date,
                overview: m.overview,
                production_year: m.production_year,
                community_rating: m.community_rating,
                genres: m.genres,
                provider_ids: m.provider_ids,
            }),
        )
    }
}

/// The [`ScheduledTask`] adapter for one advertised guest task.
/// Caps on declared web transforms — hardcoded abuse guards (a transform
/// is a script/style injection snippet, not a payload channel).
const MAX_TRANSFORMS_PER_PLUGIN: usize = 16;
/// Max bytes for one transform's search or replace text.
const MAX_TRANSFORM_TEXT: usize = 256 * 1024;

/// A declared literal search/replace applied to served `/web` files.
struct LiteralTransform {
    search: String,
    replace: String,
}

#[async_trait]
impl ferrofin_traits::plugins::FileTransformer for LiteralTransform {
    async fn transform(&self, _path: &str, contents: String) -> String {
        contents.replace(&self.search, &self.replace)
    }
}

impl WasmPluginHost {
    /// Registers every ENABLED plugin's declared web transforms into the
    /// server's transformation pipeline (capped; see the WIT trust note —
    /// this is client-side script injection, the largest grant a plugin
    /// has). A disabled plugin's transforms are not registered; toggling
    /// takes effect on the next restart, like everything decided at boot.
    pub async fn register_web_transforms(
        &self,
        service: &Arc<dyn ferrofin_traits::plugins::FileTransformationService>,
        plugin_manager: &Arc<dyn PluginManager>,
    ) {
        for plugin in &self.plugins {
            if plugin.web_transforms.is_empty() {
                continue;
            }
            let enabled = plugin_manager
                .get_plugin(plugin.descriptor.id)
                .await
                .ok()
                .flatten()
                .is_some_and(|d| d.enabled);
            if !enabled {
                continue;
            }
            for transform in plugin.web_transforms.iter().take(MAX_TRANSFORMS_PER_PLUGIN) {
                if transform.search.len() > MAX_TRANSFORM_TEXT
                    || transform.replace.len() > MAX_TRANSFORM_TEXT
                {
                    warn!(
                        plugin = %plugin.descriptor.id,
                        pattern = transform.path_pattern,
                        "wasm plugin web transform exceeds the size cap; skipping it"
                    );
                    continue;
                }
                info!(
                    plugin = %plugin.descriptor.id,
                    pattern = transform.path_pattern,
                    "registering wasm plugin web transform"
                );
                service
                    .add_transformation(
                        plugin.descriptor.id,
                        &transform.path_pattern,
                        Arc::new(LiteralTransform {
                            search: transform.search.clone(),
                            replace: transform.replace.clone(),
                        }),
                    )
                    .await;
            }
            if plugin.web_transforms.len() > MAX_TRANSFORMS_PER_PLUGIN {
                warn!(
                    plugin = %plugin.descriptor.id,
                    declared = plugin.web_transforms.len(),
                    "wasm plugin declared more than the transform cap; extras skipped"
                );
            }
        }
    }
}

/// The enabled-gate + config + parsed scan-target kinds for one analyzer's
/// pass; `None` = skip this plugin (disabled, or no parseable targets).
async fn plugin_pass_prelude(
    plugin_manager: &Arc<dyn PluginManager>,
    plugin: &LoadedPlugin,
) -> Result<Option<(String, Vec<ferrofin_model::data::BaseItemKind>)>, ServiceError> {
    let enabled = plugin_manager
        .get_plugin(plugin.descriptor.id)
        .await?
        .is_some_and(|d| d.enabled);
    if !enabled {
        return Ok(None);
    }
    let config = plugin_manager
        .get_plugin_configuration(plugin.descriptor.id)
        .await
        .map_or_else(
            |_| String::from("{}"),
            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
        );
    let kinds: Vec<ferrofin_model::data::BaseItemKind> = plugin
        .scan_targets
        .iter()
        .filter_map(|k| serde_json::from_value(serde_json::Value::String(k.clone())).ok())
        .collect();
    if kinds.is_empty() {
        return Ok(None);
    }
    Ok(Some((config, kinds)))
}

/// The host-reserved KV key holding the analysis driver's offer-once
/// watermark (unix microseconds of the newest item already offered).
const SCAN_WATERMARK_KEY: &str = "host:scan-watermark";

/// The dashboard task driving every analyzer plugin's `scan-media` pass:
/// for each ENABLED plugin with non-empty `scan-targets`, offers each item
/// newer than the plugin's watermark exactly once (host-tracked, in the
/// plugin's own state file under a reserved key, unreadable and unwritable
/// from the guest). An orderly guest error is logged (aggregated per pass)
/// and never retried; TRAPS — not orderly errors — count toward the
/// plugin's breaker, like every other guest call. The task itself never
/// fails on a plugin's behavior.
struct WasmMediaAnalysisTask {
    plugins: Vec<Arc<LoadedPlugin>>,
    plugin_manager: Arc<dyn PluginManager>,
    collaborators: Arc<std::sync::OnceLock<capabilities::Collaborators>>,
}

#[allow(clippy::unnecessary_literal_bound)] // the trait pins `-> &str`
#[async_trait]
impl ScheduledTask for WasmMediaAnalysisTask {
    fn key(&self) -> &str {
        "WasmMediaAnalysis"
    }
    fn name(&self) -> &str {
        "Plugin media analysis"
    }
    fn description(&self) -> &str {
        "Offers new library items to analysis plugins (scan-media)."
    }
    fn category(&self) -> &str {
        "Library"
    }

    async fn execute(&self, _progress: &TaskProgress) -> Result<(), ServiceError> {
        let Some(cx) = self.collaborators.get() else {
            return Err(ServiceError::backend(
                "analysis collaborators are not armed",
            ));
        };
        for plugin in &self.plugins {
            let Some((config, kinds)) = plugin_pass_prelude(&self.plugin_manager, plugin).await?
            else {
                continue;
            };
            let watermark: i64 =
                capabilities::get_state(Some(&plugin.state_path), SCAN_WATERMARK_KEY)
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(i64::MIN);
            let query = ferrofin_traits::options::InternalItemsQuery {
                include_item_types: kinds,
                // Push the watermark into the QUERY — materializing the
                // whole library per pass per plugin would defeat the point.
                // FIRST pass (no watermark yet) stays UNFILTERED so items
                // with a NULL DateCreated get their one offer (SQL `>=`
                // would exclude them forever); afterwards the filter
                // applies and NULL-dated items sit behind the watermark. A
                // NULL-dated item added AFTER the first pass is never
                // offered — the scanner always stamps DateCreated, so that
                // is a non-case in practice.
                min_date_created: (watermark > i64::MIN)
                    .then(|| chrono::DateTime::from_timestamp_micros(watermark))
                    .flatten(),
                ..Default::default()
            };
            let items = cx
                .library
                .get_item_list(&query)
                .await
                .map_err(|e| ServiceError::backend(format!("analysis item query: {e}")))?;
            let mut new_watermark = watermark;
            let mut offered = 0u32;
            let mut failed = 0u32;
            let mut first_failure: Option<String> = None;
            for entity in items {
                // NULL-dated items read as epoch: offered on the first
                // (unfiltered) pass, then permanently behind the watermark.
                let created = entity.date_created.map_or(0, |d| d.timestamp_micros());
                if created <= watermark {
                    continue;
                }
                let item = capabilities::summarize(&entity, None);
                if let Err(e) = plugin.runtime.scan_media(item, config.clone()).await {
                    // Per-item detail stays at debug (volume scales with
                    // library size); one aggregated warn per pass below.
                    tracing::debug!(
                        plugin = plugin.descriptor.name,
                        item = %entity.id,
                        error = %e,
                        "scan-media failed for one item (offered once; not retried)"
                    );
                    failed += 1;
                    if first_failure.is_none() {
                        first_failure = Some(format!("{} -> {e}", entity.id));
                    }
                }
                offered += 1;
                new_watermark = new_watermark.max(created);
            }
            if failed > 0 {
                warn!(
                    plugin = plugin.descriptor.name,
                    failed,
                    offered,
                    first = first_failure.as_deref().unwrap_or(""),
                    "scan-media failures this pass (items offered once; not retried)"
                );
            }
            if new_watermark > watermark
                && let Err(e) = capabilities::set_state(
                    Some(&plugin.state_path),
                    SCAN_WATERMARK_KEY,
                    Some(new_watermark.to_string().into_bytes()),
                )
            {
                warn!(
                    plugin = plugin.descriptor.name,
                    error = %e,
                    "could not persist the analysis watermark; items will re-offer"
                );
            }
            if offered > 0 {
                info!(
                    plugin = plugin.descriptor.name,
                    offered, "analysis pass offered new items"
                );
            }
        }
        Ok(())
    }
}

impl WasmPluginHost {
    /// The named-provider identities of every loaded plugin that declares
    /// one, as (name, supported kinds) — surfaced in the dashboard's
    /// library-options fetcher lists.
    #[must_use]
    pub fn provider_names(&self) -> Vec<(String, Vec<String>)> {
        self.plugins
            .iter()
            .filter_map(|p| p.provider_info.as_ref())
            .map(|info| (info.name.clone(), info.supported_kinds.clone()))
            .collect()
    }

    /// The analysis driver task, when any loaded plugin declares
    /// `scan-targets` (`None` otherwise — no task registered, no overhead).
    #[must_use]
    pub fn analysis_task(
        &self,
        plugin_manager: &Arc<dyn PluginManager>,
    ) -> Option<Arc<dyn ScheduledTask>> {
        let analyzers: Vec<Arc<LoadedPlugin>> = self
            .plugins
            .iter()
            .filter(|p| !p.scan_targets.is_empty())
            .cloned()
            .collect();
        if analyzers.is_empty() {
            return None;
        }
        Some(Arc::new(WasmMediaAnalysisTask {
            plugins: analyzers,
            plugin_manager: Arc::clone(plugin_manager),
            collaborators: Arc::clone(&self.collaborators),
        }))
    }
}

/// The [`PluginRequestHandler`] implementation: routes requests from
/// `/Plugins/{id}/web/…` to the owning plugin's `handle-request` export.
/// Unknown or DISABLED plugins yield `Ok(None)` (the transport 404s) — the
/// kill switch disarms this surface exactly like settings pages.
pub struct WasmRequestDispatcher {
    plugins_by_id: std::collections::HashMap<Uuid, Arc<LoadedPlugin>>,
    plugin_manager: Arc<dyn PluginManager>,
}

impl WasmRequestDispatcher {
    /// Builds the dispatcher over the host's loaded plugins.
    #[must_use]
    pub fn new(host: &WasmPluginHost, plugin_manager: Arc<dyn PluginManager>) -> Self {
        Self {
            plugins_by_id: host
                .plugins()
                .iter()
                .map(|p| (p.descriptor.id, Arc::clone(p)))
                .collect(),
            plugin_manager,
        }
    }
}

#[async_trait]
impl ferrofin_traits::plugins::PluginRequestHandler for WasmRequestDispatcher {
    async fn handle(
        &self,
        plugin_id: Uuid,
        request: ferrofin_traits::plugins::PluginWebRequest,
    ) -> Result<Option<ferrofin_traits::plugins::PluginWebResponse>, ServiceError> {
        let Some(plugin) = self.plugins_by_id.get(&plugin_id) else {
            return Ok(None);
        };
        let enabled = self
            .plugin_manager
            .get_plugin(plugin_id)
            .await?
            .is_some_and(|d| d.enabled);
        if !enabled {
            return Ok(None);
        }
        let config = self
            .plugin_manager
            .get_plugin_configuration(plugin_id)
            .await
            .map_or_else(
                |_| String::from("{}"),
                |bytes| String::from_utf8_lossy(&bytes).into_owned(),
            );
        let wire = bindings::types::PluginRequest {
            method: request.method,
            path: request.path,
            query: request.query,
            headers: request.headers,
            body: request.body,
            user_id: request.user_id.map(|u| u.to_string()),
            is_admin: request.is_admin,
            is_authenticated: request.is_authenticated,
        };
        let response = plugin
            .runtime
            .handle_request(wire, config)
            .await
            .map_err(ServiceError::backend)?;
        Ok(Some(ferrofin_traits::plugins::PluginWebResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        }))
    }
}

struct WasmTask {
    /// Registry key: `wasm-{plugin-uuid}-{task-id}` (stable across restarts).
    key: String,
    plugin: Arc<LoadedPlugin>,
    task: bindings::types::TaskDescriptor,
    plugin_manager: Arc<dyn PluginManager>,
}

#[async_trait]
impl ScheduledTask for WasmTask {
    fn key(&self) -> &str {
        &self.key
    }

    fn name(&self) -> &str {
        &self.task.name
    }

    fn description(&self) -> &str {
        &self.task.description
    }

    fn category(&self) -> &str {
        &self.task.category
    }

    async fn execute(&self, _progress: &TaskProgress) -> Result<(), ServiceError> {
        // Self-gate on the enabled flag, like every Tier-1a extension task.
        let enabled = self
            .plugin_manager
            .get_plugin(self.plugin.descriptor.id)
            .await?
            .is_some_and(|d| d.enabled);
        if !enabled {
            warn!(
                plugin = self.plugin.descriptor.name,
                task = self.task.name,
                "wasm plugin task skipped: plugin is disabled"
            );
            return Ok(());
        }
        let config = self
            .plugin_manager
            .get_plugin_configuration(self.plugin.descriptor.id)
            .await
            .map_or_else(
                |_| String::from("{}"),
                |bytes| String::from_utf8_lossy(&bytes).into_owned(),
            );

        self.plugin
            .runtime
            .run_task(self.task.id.clone(), config)
            .await
            .map_err(ServiceError::backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> PluginDescriptor {
        PluginDescriptor {
            id: Uuid::from_u128(7),
            name: "Evil <img src=x onerror=alert(1)> & \"Co\"".to_owned(),
            version: "1.0.0".to_owned(),
            description: "d".to_owned(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
        }
    }

    #[test]
    fn authored_pages_pass_through_unchanged() {
        let authored = vec![bindings::types::ConfigPage {
            name: "mypage".to_owned(),
            content: b"<div data-role=\"page\">x</div>".to_vec(),
            enable_in_main_menu: true,
        }];
        let pages = config_pages_for(&descriptor(), &authored);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, "mypage");
        assert!(pages[0].enable_in_main_menu);
        assert_eq!(pages[0].bytes, authored[0].content);
    }

    #[test]
    fn oversized_pages_are_dropped_and_the_fallback_covers_a_total_loss() {
        let d = descriptor();
        let big = bindings::types::ConfigPage {
            name: "big".to_owned(),
            content: vec![0u8; 4 * 1024 * 1024 + 1],
            enable_in_main_menu: false,
        };
        let small = bindings::types::ConfigPage {
            name: "small".to_owned(),
            content: b"<div data-role=\"page\">ok</div>".to_vec(),
            enable_in_main_menu: false,
        };
        // Mixed: the oversized page is dropped, the small one survives.
        let pages = config_pages_for(&d, &[big.clone(), small]);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, "small");
        // All oversized: the synthesized editor takes over.
        let pages = config_pages_for(&d, &[big]);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, format!("wasm-settings-{}", d.id));
    }

    #[test]
    fn missing_pages_synthesize_the_json_editor() {
        let d = descriptor();
        let pages = config_pages_for(&d, &[]);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, format!("wasm-settings-{}", d.id));
        assert!(!pages[0].enable_in_main_menu);
        let html = String::from_utf8(pages[0].bytes.clone()).unwrap();
        // The canonical jellyfin-web page shape + the save round-trip calls.
        assert!(html.contains("data-role=\"page\""), "{html}");
        assert!(html.contains("ApiClient.getPluginConfiguration"), "{html}");
        assert!(
            html.contains("ApiClient.updatePluginConfiguration"),
            "{html}"
        );
        assert!(html.contains(&d.id.to_string()), "plugin id embedded");
        // Guest-supplied name is escaped for the HTML context.
        assert!(!html.contains("<img"), "unescaped guest name: {html}");
        assert!(html.contains("&lt;img"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
    }
}
