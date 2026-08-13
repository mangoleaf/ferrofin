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
//! Guests get **no filesystem and no network** — the world imports exactly
//! one host interface (`log` + `get-config` in 0.1), so
//! `wit/ferrofin-plugin.wit` is the entire attack surface. Every guest call
//! runs under an epoch deadline (`FERROFIN_WASM_CALL_TIMEOUT_SECS`) and a
//! linear-memory cap (`FERROFIN_WASM_MEMORY_LIMIT_MB`); a trap, overrun, or
//! cap hit fails that call only, and repeated failures trip a breaker that
//! deadlines the plugin until restart ([`runtime::BREAKER_LIMIT`]). The
//! server never goes down with a plugin.

pub mod bindings;
pub mod runtime;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{error, info, warn};
use uuid::Uuid;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config as WasmConfig, Engine};

use ferrofin_core::{FerrofinEventManager, RegisteredPlugin, ScheduledTask, TaskProgress};
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
#[derive(Debug, Clone, Copy)]
pub struct WasmSettings {
    /// Per-guest-call deadline in seconds (`FERROFIN_WASM_CALL_TIMEOUT_SECS`,
    /// default 30 — confirmed 2026-08-07).
    pub call_timeout_secs: u32,
    /// Per-plugin linear-memory cap in MiB (`FERROFIN_WASM_MEMORY_LIMIT_MB`,
    /// default 128 — provisional until the E1 RSS measurement).
    pub memory_limit_mb: u32,
    /// Per-plugin event queue depth (`FERROFIN_WASM_EVENT_QUEUE_CAPACITY`,
    /// default 256 — inherits the approved bus-capacity setting).
    pub event_queue_capacity: u32,
}

impl Default for WasmSettings {
    fn default() -> Self {
        Self {
            call_timeout_secs: 30,
            memory_limit_mb: 128,
            event_queue_capacity: 256,
        }
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
    ) -> Self {
        let d = Self::default();
        Self {
            call_timeout_secs: call_timeout_secs
                .filter(|&v| v > 0)
                .unwrap_or(d.call_timeout_secs),
            memory_limit_mb: memory_limit_mb
                .filter(|&v| v > 0)
                .unwrap_or(d.memory_limit_mb),
            event_queue_capacity: event_queue_capacity
                .filter(|&v| v > 0)
                .unwrap_or(d.event_queue_capacity),
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
    runtime: RuntimeHandle,
}

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
}

/// The loaded plugin set plus the shared engine machinery.
pub struct WasmPluginHost {
    plugins: Vec<Arc<LoadedPlugin>>,
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
    pub fn load(plugins_dir: &Path, settings: WasmSettings) -> Result<Self, ServiceError> {
        let mut config = WasmConfig::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)
            .map_err(|e| ServiceError::backend(format!("wasm engine init failed: {e:#}")))?;

        // One ticker advances the epoch for every plugin of this engine.
        // 1 tick == 1 second == the unit of FERROFIN_WASM_CALL_TIMEOUT_SECS.
        {
            let engine = engine.clone();
            std::thread::Builder::new()
                .name("wasm-epoch-ticker".to_owned())
                .spawn(move || {
                    loop {
                        std::thread::sleep(EPOCH_TICK);
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
        // HostState). Failure here is the same misconfiguration class as the
        // world linker above.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| ServiceError::backend(format!("wasi linker setup failed: {e:#}")))?;
        let linker = Arc::new(linker);

        let mut plugins: Vec<Arc<LoadedPlugin>> = Vec::new();
        let mut paths: Vec<_> = match std::fs::read_dir(plugins_dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "wasm"))
                .collect(),
            Err(_) => Vec::new(), // no plugins dir → no plugins
        };
        paths.sort();

        for path in paths {
            match load_one(&engine, &linker, &path, settings) {
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
                        "failed to load wasm plugin (expected a ferrofin:plugin@0.1.0 \
                         component); skipping this file"
                    );
                }
            }
        }

        Ok(Self { plugins })
    }

    /// A host with no plugins (no directory scanned, no engine started).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
        }
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
                RegisteredPlugin::new(p.descriptor.clone(), None)
                    .with_default_config(p.default_config.clone())
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
                        let plugin = Arc::clone(&plugin);
                        let plugin_manager = Arc::clone(&plugin_manager);
                        let payload = payload.to_owned();
                        tokio::spawn(async move {
                            match plugin_manager.get_plugin(plugin.descriptor.id).await {
                                Ok(Some(descriptor)) if descriptor.enabled => {
                                    plugin.runtime.deliver_event(event_name, &payload);
                                }
                                _ => {} // disabled or unknown: skip silently
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
    settings: WasmSettings,
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
    };

    let (mut store, instance) = spec.instantiate(String::from("{}"))?;
    let wire = instance.call_descriptor(&mut store)?;
    let id: Uuid = wire.id.parse().map_err(|_| {
        wasmtime::Error::msg(format!(
            "plugin descriptor id `{}` is not a valid UUID",
            wire.id
        ))
    })?;
    let default_config = instance.call_default_config(&mut store)?;
    serde_json::from_str::<serde_json::Value>(&default_config).map_err(|e| {
        wasmtime::Error::msg(format!("plugin default-config is not valid JSON: {e}"))
    })?;
    let tasks = instance.call_tasks(&mut store)?;

    let descriptor = PluginDescriptor {
        id,
        name: wire.name.clone(),
        version: wire.version,
        description: wire.description,
        enabled: true, // the plugin manager overlays persisted state
        has_image: false,
        can_uninstall: false,
    };

    // Re-tag the spec with the real identity, then hand the warm instance to
    // its runtime thread.
    let spec = InstanceSpec {
        plugin_name: wire.name,
        plugin_id: id.to_string(),
        ..spec
    };
    // The already-made calls used the placeholder identity in HostState; fix
    // the live store's copy too so guest log lines carry the real name.
    store.data_mut().plugin_name.clone_from(&spec.plugin_name);
    store.data_mut().plugin_id.clone_from(&spec.plugin_id);

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
        runtime,
    })
}

/// The [`ScheduledTask`] adapter for one advertised guest task.
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
