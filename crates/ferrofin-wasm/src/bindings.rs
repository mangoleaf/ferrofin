//! Generated component-model bindings for the `ferrofin:plugin` world, plus
//! the host-side state each plugin [`Store`](wasmtime::Store) carries.
//!
//! The [`bindgen!`](wasmtime::component::bindgen) macro turns
//! `wit/ferrofin-plugin.wit` into typed Rust: the [`Plugin`] world struct
//! (typed wrappers over the guest's exports) and the
//! [`host::Host`](ferrofin::plugin::host::Host) trait this module implements
//! for [`HostState`]. Synchronous bindings are deliberate: every guest call
//! already runs on the plugin's dedicated runtime thread (see
//! [`runtime`](crate::runtime)), so async plumbing here would buy nothing.

use tracing::{debug, error, info, trace, warn};
use wasmtime::StoreLimits;
use wasmtime::component::bindgen;

/// The bindgen output — a generated module tree we cannot document item-by-
/// item, hence the lint carve-outs (the WIT file carries the real docs).
#[allow(missing_docs, clippy::pedantic, clippy::all)]
mod generated {
    use super::bindgen;
    bindgen!({
        path: "wit",
        world: "plugin",
    });
}

pub use generated::Plugin;
pub use generated::ferrofin::plugin::host;
pub use generated::ferrofin::plugin::types;

/// The per-plugin data behind each `Store<HostState>`: the identity used to
/// tag log lines, the latest configuration snapshot, the resource limits
/// wasmtime enforces on the guest's linear memory, and a **locked-down**
/// WASI context.
///
/// The WASI context exists only so guests written against `std` (the
/// `wasm32-wasip2` target) can link their runtime imports — it grants
/// nothing: no preopened directories (⇒ no filesystem), no socket
/// capability (⇒ no network), no environment, no CLI args, null stdio. The
/// only real capabilities remain the `ferrofin:plugin/host` functions.
pub struct HostState {
    /// The plugin's display name, prefixed onto every guest log line.
    pub plugin_name: String,
    /// The plugin's stable id, tagged onto every guest log line.
    pub plugin_id: String,
    /// The most recent persisted configuration JSON, refreshed by the runtime
    /// before each guest call so `get-config` never needs an async hop.
    pub config_json: String,
    /// The memory ceiling handed to [`wasmtime::Store::limiter`].
    pub limits: StoreLimits,
    /// The memory ceiling in bytes — also caps `http-fetch` response bodies
    /// (a body that can't fit in guest memory is refused before the host
    /// buffers it).
    pub memory_limit_bytes: usize,
    /// The shared blocking HTTP client behind `http-fetch` (per host, with
    /// the call timeout baked in at construction).
    pub http: std::sync::Arc<reqwest::blocking::Client>,
    /// The per-call timeout, needed when `http-fetch` builds a one-off
    /// DNS-pinned client (the shared client's timeout is not readable).
    pub http_timeout: std::time::Duration,
    /// Where this plugin's key/value state persists
    /// (`{plugins_dir}/{id}.state.json`). `None` until the plugin's id is
    /// known (during the identity calls at load) and in throwaway
    /// validation stores — state ops fail cleanly there.
    pub state_path: Option<std::path::PathBuf>,
    /// Whether THIS plugin may reach private/loopback destinations
    /// (`FERROFIN_WASM_PRIVATE_HTTP_ALLOW` names it or is `*`).
    pub private_http_allowed: bool,
    /// The plugin's declared public-egress allowlist (deny-by-default).
    pub egress: std::sync::Arc<crate::capabilities::EgressPolicy>,
    /// The manager handles behind `query-items`/`write-media-segments`,
    /// installed by the composition root after loading (empty during load).
    pub collaborators: std::sync::Arc<std::sync::OnceLock<crate::capabilities::Collaborators>>,
    /// The empty WASI context (see the struct docs).
    pub wasi: wasmtime_wasi::WasiCtx,
    /// The resource table WASI's generated bindings require.
    pub table: wasmtime::component::ResourceTable,
}

impl HostState {
    /// The locked-down WASI context every plugin store gets: nothing
    /// attached, nothing inherited.
    #[must_use]
    pub fn empty_wasi() -> wasmtime_wasi::WasiCtx {
        wasmtime_wasi::WasiCtxBuilder::new().build()
    }
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// The `types` interface carries only type definitions; its generated host
// trait is empty but must still be implemented for the linker bound.
impl types::Host for HostState {}

impl host::Host for HostState {
    fn log(&mut self, level: types::LogLevel, message: String) {
        // Guest text is data, not format: log it as a field so a malicious
        // message can never smuggle formatting or fake fields into our lines.
        let plugin = self.plugin_name.as_str();
        let plugin_id = self.plugin_id.as_str();
        match level {
            types::LogLevel::Trace => trace!(plugin, plugin_id, message, "wasm plugin log"),
            types::LogLevel::Debug => debug!(plugin, plugin_id, message, "wasm plugin log"),
            types::LogLevel::Info => info!(plugin, plugin_id, message, "wasm plugin log"),
            types::LogLevel::Warn => warn!(plugin, plugin_id, message, "wasm plugin log"),
            types::LogLevel::Error => error!(plugin, plugin_id, message, "wasm plugin log"),
        }
    }

    fn get_config(&mut self) -> String {
        self.config_json.clone()
    }

    fn http_fetch(&mut self, request: types::HttpRequest) -> Result<types::HttpResponse, String> {
        // Same load-time gate as query-items/write-media-segments: the
        // collaborators cell is armed only after ALL plugins have loaded, so
        // an unarmed cell means we are inside a `descriptor`/`default-config`/
        // `tasks` call. Those metadata exports run at boot even for a plugin
        // the admin disabled (they must, to appear in `/Plugins`), and
        // http-fetch is the one capability with outbound reach — deny it there
        // so "disabled" (and load itself) can never phone home.
        if self.collaborators.get().is_none() {
            return Err("http-fetch is not available during plugin load".to_owned());
        }
        crate::capabilities::http_fetch(
            &self.http,
            &self.plugin_name,
            self.memory_limit_bytes,
            self.private_http_allowed,
            &self.egress,
            self.http_timeout,
            &request,
        )
    }

    fn get_state(&mut self, key: String) -> Option<Vec<u8>> {
        crate::capabilities::get_state(self.state_path.as_deref(), &key)
    }

    fn set_state(&mut self, key: String, value: Option<Vec<u8>>) -> Result<(), String> {
        crate::capabilities::set_state(self.state_path.as_deref(), &key, value)
    }

    fn next_up(&mut self, user_id: String, limit: u32) -> Result<Vec<types::ItemSummary>, String> {
        let cx = self
            .collaborators
            .get()
            .ok_or("next-up is not available during plugin load")?;
        crate::capabilities::next_up(cx, &user_id, limit)
    }

    fn query_items(&mut self, query: types::ItemQuery) -> Result<Vec<types::ItemSummary>, String> {
        let cx = self
            .collaborators
            .get()
            .ok_or("query-items is not available during plugin load")?;
        crate::capabilities::query_items(cx, &query)
    }

    fn write_media_segments(
        &mut self,
        item_id: String,
        segments: Vec<types::MediaSegment>,
    ) -> Result<(), String> {
        let cx = self
            .collaborators
            .get()
            .ok_or("write-media-segments is not available during plugin load")?;
        // Each plugin writes under its own provider id, so replacement can
        // never touch another provider's (or a user's) segments.
        let provider_id = format!("wasm:{}", self.plugin_id);
        crate::capabilities::write_media_segments(cx, &provider_id, &item_id, &segments)
    }
}
