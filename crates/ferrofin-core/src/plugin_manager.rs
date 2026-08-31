//! [`FerrofinPluginManager`] — the registry-backed [`PluginManager`] for Tier-1
//! (compile-time) plugins.
//!
//! Plugins are Rust crates compiled into the server and handed to this manager as
//! [`RegisteredPlugin`] entries at the composition root. The manager owns the
//! immutable registry plus the mutable, on-disk state (per-plugin enabled flag,
//! package repositories, per-plugin configuration JSON) rooted at a `plugins/`
//! directory under the server config dir.
//!
//! Runtime installation / removal (a dynamic plugin host — WASM or `libloading`)
//! is **Tier 2** and out of scope: [`remove_plugin`](PluginManager::remove_plugin)
//! rejects a compiled-in plugin, and [`list_packages`](PluginManager::list_packages)
//! returns `[]` (no repository fetch yet). See `docs/PLUGINS_UPSTREAM.md` for
//! the compiled-in plugin design.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::{
    PluginArtifactValidator, PluginDescriptor, PluginImage, PluginManager, PluginUpdateInfo,
};

use crate::system_manager::LifecycleController;

/// A compiled-in plugin registered with the manager at the composition root.
///
/// Plain data only (no `ferrofin-api`/router types) so the manager stays below the
/// dependency arrow; the composition root maps its richer `FerrofinPlugin` trait
/// objects down to these entries.
#[derive(Debug, Clone)]
pub struct RegisteredPlugin {
    /// The plugin's presentation metadata.
    pub descriptor: PluginDescriptor,
    /// The plugin's bundled image, if any.
    pub image: Option<PluginImage>,
    /// The plugin's default configuration (JSON bytes), returned until the admin
    /// writes a value.
    pub default_config: Vec<u8>,
    /// The plugin's dashboard pages/resources — projected into
    /// `GET /web/ConfigurationPages` + served by
    /// `GET /web/ConfigurationPage?name=…` so the dashboard shows a Settings
    /// link. The first page is the plugin's main settings page (jellyfin-web
    /// links the first page whose `PluginId` matches); the rest are typically
    /// the JS/CSS resources that page loads by name.
    pub config_pages: Vec<PluginConfigPage>,
}

/// One dashboard page (or page resource) a plugin ships. Mirrors the C#
/// `PluginPageInfo`: a name the dashboard fetches by, the bytes, and whether the
/// page gets a main-menu (drawer) link.
#[derive(Debug, Clone)]
pub struct PluginConfigPage {
    /// The name the page is fetched by (`?name=…`); resources keep their file
    /// extension (e.g. `introskipper.js`) so the server picks the MIME type.
    pub name: String,
    /// The raw page/resource bytes.
    pub bytes: Vec<u8>,
    /// Whether the dashboard drawer shows a direct link to this page
    /// (`EnableInMainMenu`).
    pub enable_in_main_menu: bool,
}

impl RegisteredPlugin {
    /// Builds a registration, normalizing `has_image`/`can_uninstall` on the
    /// descriptor. Image presence drives `has_image`.
    ///
    /// `can_uninstall` is forced **`true`** even though a compiled-in extension
    /// can't actually be removed at runtime. jellyfin-web gates the dashboard
    /// enable/disable toggle on `CanUninstall` — the *same* flag as the uninstall
    /// button, with no separate "can be disabled" field — so reporting `false`
    /// hides the toggle entirely and the extension is stuck showing "Active".
    /// We report `true` to surface the toggle; [`remove_plugin`] still honestly
    /// rejects the uninstall itself.
    ///
    /// [`remove_plugin`]: FerrofinPluginManager::remove_plugin
    #[must_use]
    pub fn new(mut descriptor: PluginDescriptor, image: Option<PluginImage>) -> Self {
        descriptor.has_image = image.is_some();
        descriptor.can_uninstall = true;
        Self {
            descriptor,
            image,
            default_config: b"{}".to_vec(),
            config_pages: Vec::new(),
        }
    }

    /// Sets the plugin's default configuration JSON.
    #[must_use]
    pub fn with_default_config(mut self, config: Vec<u8>) -> Self {
        self.default_config = config;
        self
    }

    /// Appends a dashboard page/resource. Call once per page; the first call
    /// registers the plugin's main settings page.
    #[must_use]
    pub fn with_config_page(mut self, page: PluginConfigPage) -> Self {
        self.config_pages.push(page);
        self
    }
}

/// Merges runtime-loaded (WASM) plugin registrations into an existing
/// registry, enforcing its two global identifier namespaces:
///
/// - **plugin ids** — a registration whose id is already taken (a
///   compiled-in extension, or an earlier WASM plugin) is skipped whole:
///   two entries with one id would duplicate the dashboard row and make
///   config/enable/uninstall address the wrong plugin. The repository
///   install path refuses such packages up front; this covers the
///   hand-dropped-file door with the same rule.
/// - **page names** — dashboard pages are fetched by name
///   (case-insensitive, first match wins), so a page whose name is already
///   registered is dropped: the plugin loses its Settings button rather
///   than serving another plugin's HTML to the admin. Collisions cannot be
///   attributed (first-wins says nothing about who is malicious), so the
///   warning names both parties.
pub fn merge_plugin_registrations(
    registered: &mut Vec<RegisteredPlugin>,
    incoming: Vec<RegisteredPlugin>,
) {
    let mut taken_ids: std::collections::HashSet<Uuid> =
        registered.iter().map(|p| p.descriptor.id).collect();
    let mut taken_pages: std::collections::HashMap<String, Uuid> = registered
        .iter()
        .flat_map(|p| {
            p.config_pages
                .iter()
                .map(|page| (page.name.to_lowercase(), p.descriptor.id))
        })
        .collect();
    for mut plugin in incoming {
        let id = plugin.descriptor.id;
        if !taken_ids.insert(id) {
            tracing::warn!(
                plugin = %id,
                "wasm plugin id collides with an already-registered plugin; skipping it"
            );
            continue;
        }
        plugin
            .config_pages
            .retain(|page| match taken_pages.entry(page.name.to_lowercase()) {
                std::collections::hash_map::Entry::Occupied(existing) => {
                    tracing::warn!(
                        page = %page.name,
                        held_by = %existing.get(),
                        dropped_from = %id,
                        "plugin page name is already registered; dropping the later copy"
                    );
                    false
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(id);
                    true
                }
            });
        registered.push(plugin);
    }
}

/// The default cap on a plugin artifact download, in MiB
/// (`FERROFIN_MAX_PLUGIN_DOWNLOAD_MB` overrides it). Sized generously for
/// interpreter-language guests bundling their runtime; an abuse guard, not a
/// tuning knob.
const DEFAULT_PLUGIN_DOWNLOAD_MB: u32 = 128;

/// Whether `url` points at a loopback host (`localhost`, `127.x`, `[::1]`).
fn is_loopback_url(url: &str) -> bool {
    // reqwest re-exports Url but not url::Host; host_str + IpAddr parse
    // covers `localhost`, `127.x`, and bracketed `[::1]` alike.
    url.parse::<reqwest::Url>().is_ok_and(|parsed| {
        parsed.host_str().is_some_and(|h| {
            h.eq_ignore_ascii_case("localhost")
                || h.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    })
}

/// Requires an `https://` URL. The manifest checksum proves integrity, not
/// authenticity — transport security is the trust root for plugin downloads.
///
/// `loopback_ok` exempts http-to-loopback, and is granted only when the
/// *admin-configured* repository is itself loopback (a dev/test rig): a
/// remote manifest must not be able to point `sourceUrl` at localhost and
/// use the server as a blind local fetcher.
fn require_https(url: &str, loopback_ok: bool) -> Result<(), ServiceError> {
    let parsed: reqwest::Url = url
        .parse()
        .map_err(|e| ServiceError::invalid_input(format!("invalid sourceUrl `{url}`: {e}")))?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    if loopback_ok && parsed.scheme() == "http" && is_loopback_url(url) {
        return Ok(());
    }
    Err(ServiceError::invalid_input(format!(
        "plugin downloads require https (got `{url}`); http is allowed for loopback repositories only"
    )))
}

/// Cap on a repository *manifest* download. Deliberately hardcoded (unlike
/// the artifact cap): a JSON catalog is not a tuning surface, and 16 MiB is
/// orders of magnitude above any real manifest — this only exists so a
/// hostile repository can't OOM the server with a multi-GB body.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// How long a repository fetch may take end-to-end (manifest) or sit idle
/// between bytes (artifact). Without it reqwest never times out, and a
/// repository that accepts the connection and says nothing pins the task.
const REPO_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Reads a response body in chunks, failing as soon as the running total
/// exceeds `cap_bytes` — `Content-Length` is advisory (absent on chunked
/// responses, and a hostile server can lie), so the streamed count is the
/// enforcement for every repository download.
async fn read_capped(
    mut response: reqwest::Response,
    cap_bytes: u64,
    url: &str,
) -> Result<Vec<u8>, ServiceError> {
    if response.content_length().is_some_and(|len| len > cap_bytes) {
        return Err(ServiceError::invalid_input(format!(
            "download from {url} exceeds the {cap_bytes}-byte limit"
        )));
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ServiceError::backend(format!("downloading {url}: {e}")))?
    {
        if (bytes.len() + chunk.len()) as u64 > cap_bytes {
            return Err(ServiceError::invalid_input(format!(
                "download from {url} exceeds the {cap_bytes}-byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Lowercase-hex SHA-256 of `bytes` (the manifest `sha256` extension field).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Downloads a plugin artifact (HTTPS required on every redirect hop —
/// loopback exempt for dev repositories; the checksum is integrity-only, so
/// the transport is the trust root), enforces `cap_bytes` on the *streamed*
/// byte count, and verifies the checksum: the `sha256` extension wins when
/// the manifest provides it, else the Jellyfin-standard MD5.
async fn download_and_verify(
    source_url: &str,
    name: &str,
    chosen: &ferrofin_model::updates::VersionInfo,
    loopback_ok: bool,
    cap_bytes: u64,
) -> Result<Vec<u8>, ServiceError> {
    require_https(source_url, loopback_ok)?;
    // GitHub release assets 302 to a CDN, so redirects must be followed —
    // but the default policy would happily follow https → http → an internal
    // host. Re-run the transport check on every hop instead. (10 = reqwest's
    // own default hop limit.)
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else if require_https(attempt.url().as_str(), loopback_ok).is_ok() {
            attempt.follow()
        } else {
            attempt.error("insecure redirect target (plugin downloads require https)")
        }
    });
    // Idle timeouts, not a total deadline: a legitimate 100 MiB artifact on
    // a slow link needs minutes of transfer, but must never sit silent.
    let client = reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(REPO_FETCH_TIMEOUT)
        .read_timeout(REPO_FETCH_TIMEOUT)
        .build()
        .map_err(|e| ServiceError::backend(format!("building download client: {e}")))?;
    let response = client
        .get(source_url)
        .send()
        .await
        .map_err(|e| ServiceError::backend(format!("downloading {source_url}: {e}")))?;
    if !response.status().is_success() {
        return Err(ServiceError::backend(format!(
            "downloading {source_url}: HTTP {}",
            response.status()
        )));
    }
    let bytes = read_capped(response, cap_bytes, source_url).await?;

    if let Some(expected) = chosen.sha256.as_deref().filter(|c| !c.is_empty()) {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(ServiceError::invalid_input(format!(
                "sha256 mismatch for {name} v{}: manifest {expected}, artifact {actual}",
                chosen.version
            )));
        }
    } else if let Some(expected) = chosen.checksum.as_deref().filter(|c| !c.is_empty()) {
        let actual = ferrofin_common::extensions::md5_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(ServiceError::invalid_input(format!(
                "checksum mismatch for {name} v{}: manifest {expected}, artifact {actual}",
                chosen.version
            )));
        }
    } else {
        return Err(ServiceError::invalid_input(format!(
            "package {name} v{} declares no checksum",
            chosen.version
        )));
    }
    Ok(bytes)
}

/// Sort key for version strings: numeric segments compared numerically
/// (`10.2.0` > `9.9.9`); a prerelease (`1.0.0-rc1`) sorts *below* the
/// release it precedes (the semver rule), and remaining ties break textually.
fn version_sort_key(version: &str) -> (Vec<u64>, bool, String) {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let nums = core
        .split('.')
        .map_while(|seg| seg.parse::<u64>().ok())
        .collect();
    let is_release = !version.contains('-');
    (nums, is_release, version.to_owned())
}

/// The on-disk mutable plugin state (persisted to `{plugins_dir}/state.json`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PersistedState {
    /// Per-plugin enabled override, keyed by the plugin id's string form. A plugin
    /// absent here uses its descriptor's default `enabled`.
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
    /// The configured package repositories.
    #[serde(default)]
    repositories: Vec<RepositoryInfo>,
    /// Each installed plugin's declared public-egress allowlist (verbatim
    /// from install-time validation), keyed by guid. An upgrade that GROWS
    /// this list is logged loudly — a plugin's network reach changing is a
    /// decision-worthy event, not background noise.
    #[serde(default)]
    installed_egress: BTreeMap<String, Vec<String>>,
    /// sha256 of every artifact ever installed, keyed plugin-guid → version.
    /// A published version is immutable: re-installing a known version whose
    /// artifact digest changed is refused (a compromised repository must
    /// publish a NEW version to ship different code — a visible act — and
    /// can never silently swap bytes under a version the admin vetted).
    #[serde(default)]
    installed_digests: BTreeMap<String, BTreeMap<String, String>>,
}

/// The registry-backed plugin manager.
pub struct FerrofinPluginManager {
    /// The compiled-in plugins (immutable after construction).
    plugins: Vec<RegisteredPlugin>,
    /// The `plugins/` directory holding `state.json` and per-plugin config.
    plugins_dir: PathBuf,
    /// The mutable enabled/repository state, mirrored to `state.json`.
    state: Mutex<PersistedState>,
    /// Where installed WASM plugins are staged (`{data_dir}/plugins`) — the
    /// directory the WASM host loads from at boot. `None` = installs rejected.
    wasm_plugins_dir: Option<PathBuf>,
    /// Validates downloaded artifacts before commit (implemented by the WASM
    /// host crate; see the trait docs for why it is a seam).
    validator: Option<Arc<dyn PluginArtifactValidator>>,
    /// Flags restart-required after an install/uninstall.
    lifecycle: Option<Arc<dyn LifecycleController>>,
    /// The plugin-download size cap, in bytes (`FERROFIN_MAX_PLUGIN_DOWNLOAD_MB`).
    max_download_bytes: u64,
}

impl std::fmt::Debug for FerrofinPluginManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPluginManager")
            .field("plugins", &self.plugins.len())
            .field("plugins_dir", &self.plugins_dir)
            .field("installer_armed", &self.validator.is_some())
            .finish_non_exhaustive()
    }
}

impl FerrofinPluginManager {
    /// Creates a manager over `plugins`, rooting persisted state at `plugins_dir`.
    ///
    /// Loads `{plugins_dir}/state.json` if present; a missing/corrupt file starts
    /// from empty state (every plugin at its descriptor default).
    #[must_use]
    pub fn new(plugins: Vec<RegisteredPlugin>, plugins_dir: PathBuf) -> Self {
        let state = std::fs::read(plugins_dir.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedState>(&bytes).ok())
            .unwrap_or_default();
        Self {
            plugins,
            plugins_dir,
            state: Mutex::new(state),
            wasm_plugins_dir: None,
            validator: None,
            lifecycle: None,
            max_download_bytes: u64::from(DEFAULT_PLUGIN_DOWNLOAD_MB) * 1024 * 1024,
        }
    }

    /// Arms runtime installation: the staging directory the WASM host loads
    /// from, the artifact validator, and the lifecycle handle used to flag
    /// restart-required. Without this, `install_package` rejects.
    #[must_use]
    pub fn with_installer(
        mut self,
        wasm_plugins_dir: PathBuf,
        validator: Arc<dyn PluginArtifactValidator>,
        lifecycle: Arc<dyn LifecycleController>,
    ) -> Self {
        self.wasm_plugins_dir = Some(wasm_plugins_dir);
        self.validator = Some(validator);
        self.lifecycle = Some(lifecycle);
        self
    }

    /// Overrides the plugin-download size cap, in MiB. `None`/zero keeps the
    /// default ([`DEFAULT_PLUGIN_DOWNLOAD_MB`]).
    #[must_use]
    pub fn with_download_cap_mb(mut self, mb: Option<u32>) -> Self {
        if let Some(mb) = mb.filter(|mb| *mb > 0) {
            self.max_download_bytes = u64::from(mb) * 1024 * 1024;
        }
        self
    }

    /// A manager with no plugins — the null shape used before any plugin is wired.
    #[must_use]
    pub fn empty(plugins_dir: PathBuf) -> Self {
        Self::new(Vec::new(), plugins_dir)
    }

    /// Looks up a registered plugin by id.
    fn find(&self, id: Uuid) -> Option<&RegisteredPlugin> {
        self.plugins.iter().find(|p| p.descriptor.id == id)
    }

    /// A plugin's effective enabled flag: the persisted override when the
    /// admin has toggled it, else the descriptor default.
    fn effective_enabled(state: &PersistedState, plugin: &RegisteredPlugin) -> bool {
        state
            .enabled
            .get(&plugin.descriptor.id.to_string())
            .copied()
            .unwrap_or(plugin.descriptor.enabled)
    }

    /// The state-directory path — a test seam (`state.json` assertions).
    #[doc(hidden)]
    #[must_use]
    pub fn plugins_dir_for_test(&self) -> &std::path::Path {
        &self.plugins_dir
    }

    /// The path to a plugin's config file.
    fn config_path(&self, id: Uuid) -> PathBuf {
        self.plugins_dir.join(id.to_string()).join("config.json")
    }

    /// Writes `bytes` to `path` atomically (temp file + rename), creating parents.
    fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), ServiceError> {
        let parent = path
            .parent()
            .ok_or_else(|| ServiceError::backend("plugin path has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| ServiceError::backend(format!("create {}: {e}", parent.display())))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)
            .map_err(|e| ServiceError::backend(format!("write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| ServiceError::backend(format!("rename {}: {e}", path.display())))
    }

    /// Persists the in-memory state to `state.json`.
    fn persist(&self, state: &PersistedState) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| ServiceError::backend(format!("serialize plugin state: {e}")))?;
        Self::atomic_write(&self.plugins_dir.join("state.json"), &bytes)
    }

    /// Refuses an install of `version` when it was installed before with
    /// different bytes — a published version is immutable (see the
    /// `installed_digests` field docs).
    fn refuse_mutated_version(
        &self,
        guid: Uuid,
        name: &str,
        version: &str,
        digest: &str,
    ) -> Result<(), ServiceError> {
        let state = self.state.lock().expect("plugin state lock poisoned");
        if let Some(previous) = state
            .installed_digests
            .get(&guid.to_string())
            .and_then(|versions| versions.get(version))
            && !previous.eq_ignore_ascii_case(digest)
        {
            return Err(ServiceError::invalid_input(format!(
                "artifact for {name} v{version} differs from the one previously installed \
                 (sha256 {previous} != {digest}); a published version is immutable — refusing"
            )));
        }
        Ok(())
    }

    /// Warns when an upgrade's declared egress GROWS beyond what was
    /// recorded at the previous install (see `installed_egress` docs).
    fn warn_on_egress_growth(&self, guid: Uuid, version: &str, declared: &[String]) {
        let state = self.state.lock().expect("plugin state lock poisoned");
        if let Some(previous) = state.installed_egress.get(&guid.to_string()) {
            let grown: Vec<&String> = declared.iter().filter(|h| !previous.contains(h)).collect();
            if !grown.is_empty() {
                tracing::warn!(
                    plugin = %guid,
                    version,
                    added = ?grown,
                    "plugin upgrade DECLARES NEW egress destinations — its \
                     network reach grew; review before trusting"
                );
            }
        }
    }

    /// Records an installed plugin's declared egress (best-effort persist).
    fn record_installed_egress(&self, guid: Uuid, declared: Vec<String>) {
        let mut state = self.state.lock().expect("plugin state lock poisoned");
        state.installed_egress.insert(guid.to_string(), declared);
        let snapshot = state.clone();
        drop(state);
        let _ = self.persist(&snapshot);
    }

    /// Records an installed artifact's digest (best-effort persist — the
    /// staged artifact is the load-bearing outcome).
    fn record_installed_digest(&self, guid: Uuid, version: &str, digest: String) {
        let mut state = self.state.lock().expect("plugin state lock poisoned");
        state
            .installed_digests
            .entry(guid.to_string())
            .or_default()
            .insert(version.to_owned(), digest);
        let snapshot = state.clone();
        drop(state);
        let _ = self.persist(&snapshot);
    }

    /// Flips a plugin's enabled flag, persisting the change.
    fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<(), ServiceError> {
        if self.find(id).is_none() {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        }
        let mut state = self.state.lock().expect("plugin state lock poisoned");
        state.enabled.insert(id.to_string(), enabled);
        self.persist(&state)
    }
}

#[async_trait]
impl PluginManager for FerrofinPluginManager {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        let state = self.state.lock().expect("plugin state lock poisoned");
        Ok(self
            .plugins
            .iter()
            .map(|p| {
                let mut d = p.descriptor.clone();
                d.enabled = Self::effective_enabled(&state, p);
                d
            })
            .collect())
    }

    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        let Some(plugin) = self.find(id) else {
            return Ok(None);
        };
        let state = self.state.lock().expect("plugin state lock poisoned");
        let mut d = plugin.descriptor.clone();
        d.enabled = Self::effective_enabled(&state, plugin);
        Ok(Some(d))
    }

    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        self.set_enabled(id, true)
    }

    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        self.set_enabled(id, false)
    }

    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        // A runtime-installed WASM plugin is a file we staged — deletable.
        // (Look for the file FIRST: a freshly-installed plugin is not in the
        // in-memory registry until the restart that loads it.)
        if let Some(wasm_dir) = &self.wasm_plugins_dir {
            let artifact = wasm_dir.join(format!("{id}.wasm"));
            if artifact.exists() {
                std::fs::remove_file(&artifact).map_err(|e| {
                    ServiceError::backend(format!("removing {}: {e}", artifact.display()))
                })?;
                // Best-effort cleanup of the plugin's KV state, config dir
                // + enabled override; the artifact removal above is the
                // load-bearing part.
                let _ = std::fs::remove_file(wasm_dir.join(format!("{id}.state.json")));
                let _ = std::fs::remove_dir_all(self.plugins_dir.join(id.to_string()));
                {
                    let mut state = self.state.lock().expect("plugin state lock poisoned");
                    state.enabled.remove(&id.to_string());
                    state.installed_egress.remove(&id.to_string());
                    let snapshot = state.clone();
                    drop(state);
                    let _ = self.persist(&snapshot);
                }
                if let Some(lifecycle) = &self.lifecycle {
                    lifecycle.mark_restart_required();
                }
                tracing::info!(plugin = %id, "wasm plugin uninstalled; restart required");
                return Ok(());
            }
        }
        if self.find(id).is_none() {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        }
        // Compiled-in plugins have nothing to remove at runtime.
        Err(ServiceError::invalid_input(
            "compiled-in plugins cannot be uninstalled at runtime",
        ))
    }

    async fn available_plugin_updates(&self) -> Result<Vec<PluginUpdateInfo>, ServiceError> {
        // Port of `InstallationManager.GetAvailablePluginUpdates`: for every
        // installed plugin, the newest catalog version that is *compatible*,
        // strictly newer than the installed one, and not already staged.
        let (Some(wasm_dir), Some(validator)) = (&self.wasm_plugins_dir, &self.validator) else {
            // No installer armed: nothing here could be installed even if the
            // catalog offered it.
            return Ok(Vec::new());
        };
        // Narrow to the updatable plugins BEFORE touching the network: the
        // common deployment has only compiled-in extensions, and fetching every
        // repository at boot and every 24 h to produce an empty list is pure
        // cost.
        let updatable: Vec<PluginDescriptor> = self
            .list_plugins()
            .await?
            .into_iter()
            // Upstream skips a plugin whose manifest disables auto-update or
            // that is disabled. Ferrofin has no per-plugin auto-update flag —
            // its manifests belong to the repository, not the artifact — so
            // only the disabled gate applies; disabling the plugin (or the
            // repository) is the opt-out. See `docs/EXTENSIONS.md`.
            .filter(|plugin| plugin.enabled)
            // Only a runtime-installed (staged) WASM plugin can be replaced —
            // `install_package` refuses a compiled-in extension's id, so
            // offering it an "update" would only produce a failed install.
            .filter(|plugin| wasm_dir.join(format!("{}.wasm", plugin.id)).exists())
            .collect();
        if updatable.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.list_packages().await?;
        let abi = validator.supported_abi();
        let installed_versions = {
            let state = self.state.lock().expect("plugin state lock poisoned");
            state.installed_digests.clone()
        };
        let mut updates = Vec::new();
        for plugin in updatable {
            let installed_key = version_sort_key(&plugin.version);
            // A version already staged in this data directory is not offered
            // again — it is waiting for the activating restart, and re-staging
            // it every run would re-download the same bytes forever (upstream's
            // `CompletedInstallations` guard, made durable).
            let staged = installed_versions.get(&plugin.id.to_string());
            // Every catalog entry for this id, not just the first: a package
            // listed by two repositories keeps both repositories' versions in
            // play, and the winner carries its own repository's URL so the
            // install resolves the same entry this decision was made on.
            let chosen = catalog
                .iter()
                .filter(|package| package.id == plugin.id)
                .flat_map(|package| {
                    package
                        .versions
                        .iter()
                        .map(move |version| (package.name.as_str(), version))
                })
                .filter(|(_, v)| v.target_abi.as_deref() == Some(abi))
                // Releases only: a prerelease sorts above the release it
                // precedes, and staging `1.1.0-rc1` over `1.0.0` with nobody
                // watching is not what "keep my plugins current" means. The
                // admin can still install one by hand.
                .filter(|(_, v)| version_sort_key(&v.version).1)
                .filter(|(_, v)| version_sort_key(&v.version) > installed_key)
                .filter(|(_, v)| !staged.is_some_and(|versions| versions.contains_key(&v.version)))
                .max_by_key(|(_, v)| version_sort_key(&v.version));
            if let Some((name, version)) = chosen {
                updates.push(PluginUpdateInfo {
                    id: plugin.id,
                    name: name.to_owned(),
                    installed_version: plugin.version.clone(),
                    version: version.version.clone(),
                    repository_url: Some(version.repository_url.clone()),
                });
            }
        }
        Ok(updates)
    }

    async fn install_package(
        &self,
        name: &str,
        assembly_guid: Option<Uuid>,
        version: Option<&str>,
        repository_url: Option<&str>,
    ) -> Result<(), ServiceError> {
        let (Some(wasm_dir), Some(validator)) = (&self.wasm_plugins_dir, &self.validator) else {
            return Err(ServiceError::invalid_input(
                "runtime plugin installation is not available on this server",
            ));
        };

        // 1. Resolve the package (guid beats name — names collide across
        //    repositories) and the version (pinned, else newest).
        let catalog = self.list_packages().await?;
        // EVERY catalog entry for this identity, not just the first: two
        // repositories may list the same plugin, and `list_packages` keeps
        // their entries separate. Picking only the first would make a version
        // published by the second repository unreachable — including the one a
        // caller pinned by `version`/`repository_url`.
        let matching: Vec<&PackageInfo> = catalog
            .iter()
            .filter(|p| match assembly_guid {
                Some(guid) => p.id == guid,
                None => p.name.eq_ignore_ascii_case(name),
            })
            .collect();
        let package = *matching
            .first()
            .ok_or_else(|| ServiceError::not_found(format!("package {name}")))?;

        // A repository must not squat a compiled-in plugin's identity: the
        // registry knowing this id *without* a staged artifact means it
        // belongs to a compiled-in extension. (An installed WASM plugin is
        // registered after its activating restart too, but always has its
        // staged file — that case is a legitimate upgrade.)
        if self.find(package.id).is_some()
            && !wasm_dir.join(format!("{}.wasm", package.id)).exists()
        {
            return Err(ServiceError::invalid_input(format!(
                "plugin id {} belongs to a compiled-in extension — refusing",
                package.id
            )));
        }

        let chosen = matching
            .iter()
            .flat_map(|p| p.versions.iter())
            .filter(|v| repository_url.is_none_or(|r| v.repository_url == r))
            .filter(|v| version.is_none_or(|want| v.version == want))
            .max_by_key(|v| version_sort_key(&v.version))
            .ok_or_else(|| {
                ServiceError::not_found(format!(
                    "package {name} has no matching version (version={version:?}, repository={repository_url:?})"
                ))
            })?;
        let source_url = chosen
            .source_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                ServiceError::invalid_input(format!(
                    "package {name} v{} declares no sourceUrl",
                    chosen.version
                ))
            })?;

        // 2. ABI gate: the manifest must target this server's plugin ABI.
        let abi = validator.supported_abi();
        match chosen.target_abi.as_deref() {
            Some(target) if target == abi => {}
            other => {
                return Err(ServiceError::invalid_input(format!(
                    "package {name} v{} targets ABI {:?}, this server supports {abi}",
                    chosen.version, other
                )));
            }
        }

        // 3–4. Download (HTTPS required, size-capped) and verify integrity.
        // The http-loopback exemption is granted only when *this version's
        // own repository* is loopback (a dev rig) — `repository_url` is
        // stamped by `list_packages` from the repo actually fetched, so a
        // remote manifest can neither claim it nor ride a loopback repo
        // configured alongside it. See `require_https`.
        let loopback_ok = is_loopback_url(&chosen.repository_url);
        let bytes = download_and_verify(
            source_url,
            name,
            chosen,
            loopback_ok,
            self.max_download_bytes,
        )
        .await?;

        // 4b. A published version is immutable: if this guid+version was
        //     installed before, the artifact must be byte-identical, so a
        //     compromised repository cannot silently swap the code under a
        //     version the admin already vetted.
        let digest = sha256_hex(&bytes);
        self.refuse_mutated_version(package.id, name, &chosen.version, &digest)?;

        // 5. Validate the artifact is a real plugin component and that its
        //    self-reported id matches the catalog guid (otherwise enable/
        //    disable/uninstall would target a different identity than the
        //    loader registers on boot).
        let artifact = validator.validate(&bytes).await?;
        if artifact.id != package.id {
            return Err(ServiceError::invalid_input(format!(
                "artifact reports plugin id {}, catalog says {} — refusing",
                artifact.id, package.id
            )));
        }
        // Surface egress growth on upgrade: any newly-declared destination
        // is a change in what this plugin can reach.
        self.warn_on_egress_growth(package.id, &chosen.version, &artifact.declared_egress);

        // 6. Commit: atomic write into the WASM host's load directory.
        //    Upgrades are the same filename → overwrite. Known v1 edge: a
        //    manually-copied duplicate under another filename would win or
        //    lose the boot-time duplicate-id skip alphabetically.
        std::fs::create_dir_all(wasm_dir)
            .map_err(|e| ServiceError::backend(format!("create {}: {e}", wasm_dir.display())))?;
        Self::atomic_write(&wasm_dir.join(format!("{}.wasm", package.id)), &bytes)?;

        // Record the installed digest (see step 4b) + declared egress.
        self.record_installed_digest(package.id, &chosen.version, digest);
        self.record_installed_egress(package.id, artifact.declared_egress);

        // 7. Activate on next restart (Jellyfin's model).
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.mark_restart_required();
        }
        tracing::info!(
            package = name,
            version = chosen.version,
            plugin = %package.id,
            "wasm plugin installed; restart required to activate"
        );
        Ok(())
    }

    async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
        let Some(plugin) = self.find(id) else {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        };
        match std::fs::read(self.config_path(id)) {
            // Overlay the stored values onto the current defaults, so a config
            // saved by an older plugin version (missing newly-added keys) still
            // returns every field at its default — matching how C# deserializes a
            // partial `PluginConfiguration`.
            Ok(bytes) => Ok(merge_config(&plugin.default_config, &bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(plugin.default_config.clone()),
            Err(e) => Err(ServiceError::backend(format!("read plugin config: {e}"))),
        }
    }

    async fn set_plugin_configuration(
        &self,
        id: Uuid,
        config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        let Some(plugin) = self.find(id) else {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        };
        let defaults = plugin.default_config.clone();
        // Reject a non-JSON body so a corrupt write can't poison a later read.
        let body = serde_json::from_slice::<serde_json::Value>(&config)
            .map_err(|_| ServiceError::invalid_input("plugin configuration must be valid JSON"))?;
        match body {
            // `if (configuration is not null) { configPlugin.UpdateConfiguration(…); }`
            // (v10.11.8 Jellyfin.Api/Controllers/PluginsController.cs:186-201,
            // unchanged on master): a `null` body is explicitly a NO-OP, and the
            // action still answers 204. Ferrofin stored the literal `null`,
            // after which `merge_config`'s non-object arm made the next GET
            // answer the bare token `null` — an admin-reachable way to destroy a
            // plugin's configuration AND to violate the contract's
            // `BasePluginConfiguration` object shape.
            serde_json::Value::Null => Ok(()),
            serde_json::Value::Object(map) => {
                let projected = project_config(&defaults, &map)?;
                Self::atomic_write(&self.config_path(id), &projected)
            }
            // The C# deserializes into the plugin's own `ConfigurationType`, so
            // an array or a scalar throws and the request fails. Jellyfin's
            // handler lets that escape as a 500 "Error processing request."; a
            // refused body is a 400 here, which is the accepted divergence
            // already recorded for the malformed-body leg.
            _ => Err(ServiceError::invalid_input(
                "plugin configuration must be a JSON object",
            )),
        }
    }

    async fn plugin_image(&self, id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(self.find(id).and_then(|p| p.image.clone()))
    }

    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
        let state = self.state.lock().expect("plugin state lock poisoned");
        Ok(state.repositories.clone())
    }

    async fn set_repositories(
        &self,
        repositories: Vec<RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        let mut state = self.state.lock().expect("plugin state lock poisoned");
        state.repositories = repositories;
        self.persist(&state)
    }

    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
        // Fetch and aggregate the enabled repositories' plugin manifests (each a
        // JSON `PackageInfo[]`), mirroring `InstallationManager.GetAvailablePackages`.
        // A repository that is unreachable or serves malformed JSON is skipped with
        // a warning rather than failing the whole catalog. What this lists is
        // installable via `install_package` when the installer is armed.
        let repos: Vec<RepositoryInfo> = {
            let state = self.state.lock().expect("plugin state lock poisoned");
            state
                .repositories
                .iter()
                .filter(|r| r.enabled)
                .cloned()
                .collect()
        };
        let mut packages: Vec<PackageInfo> = Vec::new();
        // Manifests are small JSON documents: a total deadline plus a
        // streamed size cap, so a hostile/hung repository can neither OOM
        // the server nor pin the task (GET /Packages is plain-auth).
        let client = reqwest::Client::builder()
            .timeout(REPO_FETCH_TIMEOUT)
            .build()
            .map_err(|e| ServiceError::backend(format!("building repository client: {e}")))?;
        for repo in repos {
            let Some(url) = repo.url.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            let repo_name = repo.name.clone().unwrap_or_default();
            let body = match client.get(url).send().await {
                Ok(resp) => match read_capped(resp, MAX_MANIFEST_BYTES, url).await {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::warn!(url, error = %e, "failed to read plugin repository manifest");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(url, error = %e, "failed to fetch plugin repository manifest");
                    continue;
                }
            };
            match serde_json::from_slice::<Vec<PackageInfo>>(&body) {
                // Stamp provenance from the repository we actually fetched
                // from — the manifest's own repositoryName/Url claims are
                // attacker-controlled, and the install path's repositoryUrl
                // filter + loopback exemption rely on these fields being
                // true (Jellyfin stamps them the same way in
                // `InstallationManager.GetPackages`).
                Ok(list) => packages.extend(list.into_iter().map(|mut p| {
                    for v in &mut p.versions {
                        v.repository_name.clone_from(&repo_name);
                        url.clone_into(&mut v.repository_url);
                    }
                    p
                })),
                Err(e) => {
                    tracing::warn!(url, error = %e, "plugin repository manifest was not valid JSON");
                }
            }
        }
        // The compiled-in plugins are real installed packages even when no
        // repository lists them — synthesize a catalog entry for each so
        // `GET /Packages/{name}?assemblyGuid=…` (the dashboard's plugin detail
        // page) resolves instead of 404ing. A repository entry with the same
        // guid wins (it carries richer version/changelog data).
        for plugin in &self.plugins {
            if packages.iter().any(|p| p.id == plugin.descriptor.id) {
                continue;
            }
            packages.push(PackageInfo {
                name: plugin.descriptor.name.clone(),
                description: plugin.descriptor.description.clone(),
                overview: plugin.descriptor.description.clone(),
                owner: "Ferrofin (compiled-in)".to_owned(),
                category: "General".to_owned(),
                id: plugin.descriptor.id,
                versions: vec![ferrofin_model::updates::VersionInfo {
                    version: plugin.descriptor.version.clone(),
                    version_number: Some(plugin.descriptor.version.clone()),
                    changelog: None,
                    target_abi: None,
                    source_url: None,
                    checksum: None,
                    sha256: None,
                    timestamp: None,
                    repository_name: "Ferrofin built-in".to_owned(),
                    repository_url: String::new(),
                }],
                image_url: None,
            });
        }
        Ok(packages)
    }

    async fn get_configuration_pages(
        &self,
    ) -> Result<Vec<ferrofin_model::plugins::ConfigurationPageInfo>, ServiceError> {
        // A DISABLED plugin's pages are hidden entirely — matching Jellyfin,
        // where a disabled plugin is never instantiated so `IHasWebPages`
        // yields nothing. This matters beyond fidelity: a settings page is
        // HTML/JS served to the admin's browser (outside the WASM sandbox),
        // and Disable is the kill switch an admin reaches for — it must
        // disarm this surface too.
        let enabled_flags: Vec<bool> = {
            let state = self.state.lock().expect("plugin state lock poisoned");
            self.plugins
                .iter()
                .map(|p| Self::effective_enabled(&state, p))
                .collect()
        };
        let mut pages = Vec::new();
        for (plugin, enabled) in self.plugins.iter().zip(enabled_flags) {
            if !enabled {
                continue;
            }
            // A page declared main-menu-enabled can be vetoed by the plugin's
            // own stored configuration (the Intro Skipper convention:
            // `EnableInMainMenu = Configuration.EnableMainMenu ?? true`).
            let menu_allowed = self
                .get_plugin_configuration(plugin.descriptor.id)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|v| v.get("EnableMainMenu").and_then(serde_json::Value::as_bool))
                .unwrap_or(true);
            for page in &plugin.config_pages {
                pages.push(ferrofin_model::plugins::ConfigurationPageInfo {
                    name: page.name.clone(),
                    enable_in_main_menu: page.enable_in_main_menu && menu_allowed,
                    menu_section: None,
                    menu_icon: None,
                    display_name: Some(plugin.descriptor.name.clone()),
                    plugin_id: Some(plugin.descriptor.id),
                });
            }
        }
        Ok(pages)
    }

    async fn get_configuration_page(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        // Same disabled-plugin gate as `get_configuration_pages`: this
        // endpoint is unauthenticated (matching Jellyfin), so a disabled
        // plugin's page must not remain fetchable by name.
        let state = self.state.lock().expect("plugin state lock poisoned");
        Ok(self
            .plugins
            .iter()
            .filter(|plugin| Self::effective_enabled(&state, plugin))
            .find_map(|plugin| {
                plugin
                    .config_pages
                    .iter()
                    .find(|page| page.name.eq_ignore_ascii_case(name))
                    .map(|page| page.bytes.clone())
            }))
    }
}

/// Overlays a stored config's top-level keys onto the current defaults, so a
/// config saved by an older plugin version still returns every field (missing
/// keys keep their default).
///
/// A stored document that is not an object falls back to the DEFAULTS rather
/// than to the raw bytes: this endpoint's contract is a
/// `BasePluginConfiguration` object, and answering with whatever a corrupted
/// file happens to hold would hand a client a shape its config type cannot
/// bind. [`FerrofinPluginManager::set_plugin_configuration`] no longer writes a
/// non-object, so this arm only covers a file damaged out of band.
fn merge_config(defaults: &[u8], stored: &[u8]) -> Vec<u8> {
    let (Ok(serde_json::Value::Object(mut base)), Ok(serde_json::Value::Object(over))) = (
        serde_json::from_slice::<serde_json::Value>(defaults),
        serde_json::from_slice::<serde_json::Value>(stored),
    ) else {
        return defaults.to_vec();
    };
    base.extend(over);
    serde_json::to_vec(&serde_json::Value::Object(base)).unwrap_or_else(|_| stored.to_vec())
}

/// Projects a posted configuration object onto the plugin's own schema, the way
/// `JsonSerializer.DeserializeAsync(Request.Body, configPlugin.ConfigurationType)`
/// does (v10.11.8 Jellyfin.Api/Controllers/PluginsController.cs:194).
///
/// A typed deserialize has three consequences a raw byte-store does not, and all
/// three were observable on the wire:
///   * a key the configuration type does not declare is DROPPED — Ferrofin
///     round-tripped `{"Bogus":"zzz"}` for ever;
///   * a value of the wrong JSON kind THROWS — Ferrofin stored
///     `{"Username":123}` and served an integer back to a plugin whose config
///     type declares a string;
///   * a key the body omits falls back to the C# property default rather than to
///     whatever was stored, which is what the merge onto `defaults` gives.
///
/// The schema is the plugin's `default_config`. When a plugin declares none
/// (`RegisteredPlugin::new`'s `{}`, and any WASM guest that ships no default
/// document) there is no type to project onto, so the object is stored as it
/// arrives — the pre-existing behaviour, kept deliberately rather than silently
/// emptying a schemaless plugin's configuration.
fn project_config(
    defaults: &[u8],
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<u8>, ServiceError> {
    let Ok(serde_json::Value::Object(schema)) =
        serde_json::from_slice::<serde_json::Value>(defaults)
    else {
        return serde_json::to_vec(body)
            .map_err(|e| ServiceError::backend(format!("serialize plugin config: {e}")));
    };
    if schema.is_empty() {
        return serde_json::to_vec(body)
            .map_err(|e| ServiceError::backend(format!("serialize plugin config: {e}")));
    }
    let mut out = schema.clone();
    for (key, value) in body {
        let Some(slot) = out.get_mut(key) else {
            continue; // unknown key: the deserializer drops it
        };
        if !same_json_kind(slot, value) {
            return Err(ServiceError::invalid_input(format!(
                "plugin configuration field {key} has the wrong type"
            )));
        }
        *slot = value.clone();
    }
    serde_json::to_vec(&serde_json::Value::Object(out))
        .map_err(|e| ServiceError::backend(format!("serialize plugin config: {e}")))
}

/// Whether `value` can bind to the field `slot` types.
///
/// One `Number` class, as .NET has: a `double RateLimit` accepts the integer
/// `1` and an `int MaxCastMembers` accepts `15`, so the JSON distinction between
/// integer and float is not one the C# deserializer draws. A `null` is accepted
/// for any field — a nullable C# property takes it, and a non-nullable one falls
/// back to its default rather than throwing.
fn same_json_kind(slot: &serde_json::Value, value: &serde_json::Value) -> bool {
    use serde_json::Value::{Array, Bool, Null, Number, Object, String as Str};
    matches!(
        (slot, value),
        (_, Null)
            | (Null, _)
            | (Bool(_), Bool(_))
            | (Number(_), Number(_))
            | (Str(_), Str(_))
            | (Array(_), Array(_))
            | (Object(_), Object(_))
    )
}

#[cfg(test)]
mod tests {
    use super::{FerrofinPluginManager, RegisteredPlugin, merge_config, project_config};
    use ferrofin_model::updates::RepositoryInfo;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
    use uuid::Uuid;

    #[test]
    fn merge_config_fills_missing_keys_and_keeps_stored() {
        let defaults = br#"{"A":1,"B":true,"C":"x"}"#;
        let stored = br#"{"A":9,"D":"extra"}"#;
        let merged: serde_json::Value =
            serde_json::from_slice(&merge_config(defaults, stored)).unwrap();
        assert_eq!(merged["A"], 9); // stored overrides default
        assert_eq!(merged["B"], true); // missing key filled from default
        assert_eq!(merged["C"], "x");
        // A key outside the schema can only be in the file if it was written
        // before `project_config` existed, or by hand; the merge still surfaces
        // it rather than silently dropping a value the admin can see on disk.
        assert_eq!(merged["D"], "extra");
        // A file damaged out of band answers with the schema, not with the
        // damage: the route's contract is a `BasePluginConfiguration` object.
        assert_eq!(merge_config(defaults, b"not json"), defaults);
    }

    /// `POST /Plugins/{id}/Configuration` deserializes into the plugin's own
    /// configuration TYPE (PluginsController.cs:194), which drops keys the type
    /// does not declare and throws on a value of the wrong kind. Measured
    /// against a live 10.11.8: posting `{"RateLimit":3,"Bogus":"zzz"}` to
    /// MusicBrainz reads back without `Bogus`.
    #[test]
    fn a_config_write_is_projected_onto_the_plugin_schema() {
        let defaults =
            br#"{"Server":"https://musicbrainz.org","RateLimit":1,"ReplaceArtistName":false}"#;
        let body = |json: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(json).unwrap()
        };

        let full = project_config(defaults, &body(r#"{"RateLimit":9}"#)).unwrap();
        let full: serde_json::Value = serde_json::from_slice(&full).unwrap();
        assert_eq!(full["RateLimit"], 9);
        // An absent key falls back to the C# property default, not to whatever
        // a previous write stored — the C# does a full replace.
        assert_eq!(full["Server"], "https://musicbrainz.org");
        assert_eq!(full["ReplaceArtistName"], false);

        let dropped = project_config(defaults, &body(r#"{"RateLimit":3,"Bogus":"zzz"}"#)).unwrap();
        let dropped: serde_json::Value = serde_json::from_slice(&dropped).unwrap();
        assert!(dropped.get("Bogus").is_none());

        // A `double` field takes an integer literal, as .NET does.
        assert!(project_config(defaults, &body(r#"{"RateLimit":2.5}"#)).is_ok());
        // …but a string where a number belongs is refused.
        let err = project_config(defaults, &body(r#"{"RateLimit":"abc"}"#)).unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");

        // A schemaless plugin has no type to project onto, so its object is
        // stored as it arrives.
        let raw = project_config(b"{}", &body(r#"{"Anything":[1,2]}"#)).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(raw["Anything"], serde_json::json!([1, 2]));
    }

    fn descriptor(id: Uuid, name: &str, enabled: bool) -> PluginDescriptor {
        PluginDescriptor {
            id,
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            description: "test plugin".to_owned(),
            enabled,
            has_image: false,
            can_uninstall: false,
        }
    }

    fn manager(plugins: Vec<RegisteredPlugin>) -> (FerrofinPluginManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinPluginManager::new(plugins, dir.path().to_path_buf());
        (mgr, dir)
    }

    #[tokio::test]
    async fn empty_manager_lists_nothing() {
        let (mgr, _dir) = manager(Vec::new());
        assert!(mgr.list_plugins().await.expect("list").is_empty());
        assert!(mgr.get_plugin(Uuid::new_v4()).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn lists_and_gets_registered_plugin() {
        let id = Uuid::from_u128(1);
        let (mgr, _dir) = manager(vec![RegisteredPlugin::new(
            descriptor(id, "Demo", true),
            None,
        )]);
        let all = mgr.list_plugins().await.expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Demo");
        assert!(all[0].enabled);
        assert!(mgr.get_plugin(id).await.expect("get").is_some());
    }

    #[tokio::test]
    async fn disable_persists_across_reload() {
        let id = Uuid::from_u128(2);
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mgr = FerrofinPluginManager::new(
                vec![RegisteredPlugin::new(descriptor(id, "Demo", true), None)],
                dir.path().to_path_buf(),
            );
            mgr.disable_plugin(id).await.expect("disable");
        }
        // A fresh manager over the same dir sees the persisted disabled flag.
        let mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(descriptor(id, "Demo", true), None)],
            dir.path().to_path_buf(),
        );
        assert!(
            !mgr.get_plugin(id)
                .await
                .expect("get")
                .expect("some")
                .enabled
        );
    }

    #[tokio::test]
    async fn enable_unknown_plugin_is_not_found() {
        let (mgr, _dir) = manager(Vec::new());
        assert!(matches!(
            mgr.enable_plugin(Uuid::new_v4()).await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn remove_registered_plugin_is_rejected() {
        let id = Uuid::from_u128(3);
        let (mgr, _dir) = manager(vec![RegisteredPlugin::new(
            descriptor(id, "Demo", true),
            None,
        )]);
        assert!(matches!(
            mgr.remove_plugin(id).await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn config_round_trips_with_default() {
        let id = Uuid::from_u128(4);
        let (mgr, _dir) = manager(vec![
            RegisteredPlugin::new(descriptor(id, "Demo", true), None)
                .with_default_config(br#"{"k":1}"#.to_vec()),
        ]);
        // Default until written.
        assert_eq!(
            mgr.get_plugin_configuration(id).await.expect("cfg"),
            br#"{"k":1}"#.to_vec()
        );
        mgr.set_plugin_configuration(id, br#"{"k":2}"#.to_vec())
            .await
            .expect("set");
        assert_eq!(
            mgr.get_plugin_configuration(id).await.expect("cfg"),
            br#"{"k":2}"#.to_vec()
        );
        // Invalid JSON rejected.
        assert!(
            mgr.set_plugin_configuration(id, b"not json".to_vec())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn repositories_persist() {
        let (mgr, _dir) = manager(Vec::new());
        assert!(mgr.get_repositories().await.expect("repos").is_empty());
        let repo = RepositoryInfo {
            name: Some("Main".to_owned()),
            url: Some("https://example.test/manifest.json".to_owned()),
            enabled: true,
        };
        mgr.set_repositories(vec![repo.clone()])
            .await
            .expect("set repos");
        assert_eq!(mgr.get_repositories().await.expect("repos"), vec![repo]);
    }

    #[tokio::test]
    async fn plugin_image_returned_when_present() {
        let id = Uuid::from_u128(5);
        let image = PluginImage {
            content_type: "image/png".to_owned(),
            data: vec![1, 2, 3],
        };
        let (mgr, _dir) = manager(vec![RegisteredPlugin::new(
            descriptor(id, "Demo", true),
            Some(image.clone()),
        )]);
        assert_eq!(mgr.plugin_image(id).await.expect("img"), Some(image));
        // has_image is normalized on the descriptor.
        assert!(
            mgr.get_plugin(id)
                .await
                .expect("get")
                .expect("some")
                .has_image
        );
    }
    // ── repository install/uninstall ────────────────────────────────────

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::system_manager::LifecycleController as _;

    /// A tiny loopback HTTP server: binds first (so the base URL can be
    /// embedded in responses), then answers each request with whatever raw
    /// bytes `respond(request)` returns, until dropped.
    fn raw_server(
        respond_for: impl FnOnce(&str) -> Box<dyn Fn(&str) -> Vec<u8> + Send>,
    ) -> (String, std::sync::mpsc::Sender<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let respond = respond_for(&format!("http://{addr}"));
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let _ = stream.write_all(&respond(&request));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{addr}"), stop_tx)
    }

    /// An HTTP/1.1 200 response with a content-length header.
    fn http_ok(kind: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// The standard rig server: `manifest` at `/manifest.json`, `artifact`
    /// at `/plugin.wasm`.
    fn repo_server(
        manifest_for: impl FnOnce(&str) -> String,
        artifact: Vec<u8>,
    ) -> (String, std::sync::mpsc::Sender<()>) {
        raw_server(move |base| {
            let manifest = manifest_for(base);
            Box::new(move |request| {
                if request.contains("/plugin.wasm") {
                    http_ok("application/wasm", &artifact)
                } else {
                    http_ok("application/json", manifest.as_bytes())
                }
            })
        })
    }

    /// Validator stub: reports a fixed id (or an error).
    struct StubValidator {
        id: Result<Uuid, String>,
        abi: &'static str,
    }
    #[async_trait::async_trait]
    impl ferrofin_traits::plugins::PluginArtifactValidator for StubValidator {
        fn supported_abi(&self) -> &str {
            self.abi
        }
        async fn validate(
            &self,
            _bytes: &[u8],
        ) -> Result<ferrofin_traits::plugins::ValidatedArtifact, ServiceError> {
            self.id
                .clone()
                .map(|id| ferrofin_traits::plugins::ValidatedArtifact {
                    id,
                    declared_egress: vec!["api.example.com".to_owned()],
                })
                .map_err(ServiceError::invalid_input)
        }
    }

    struct FlagLifecycle(AtomicBool);
    #[async_trait::async_trait]
    impl crate::system_manager::LifecycleController for FlagLifecycle {
        async fn stop(&self, _restart: bool) -> Result<(), ServiceError> {
            Ok(())
        }
        fn has_pending_restart(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn mark_restart_required(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn is_shutting_down(&self) -> bool {
            false
        }
    }

    /// The ABI string the stub validator and the test manifests agree on —
    /// deliberately NOT the real `ferrofin_wasm::PLUGIN_ABI` (these tests
    /// prove "manifest must match whatever the validator says", not a
    /// specific version).
    const TEST_ABI: &str = "ferrofin:plugin@0.0.0-test";

    const PKG_ID: Uuid = Uuid::from_u128(0xABCD_EF01);

    fn manifest_json(base: &str, checksum: &str, sha256: Option<&str>, abi: &str) -> String {
        let sha = sha256.map_or(String::new(), |s| format!(r#""sha256":"{s}","#));
        format!(
            r#"[{{"name":"HelloPkg","description":"d","overview":"o","owner":"me","category":"General",
                "guid":"{PKG_ID}",
                "versions":[
                  {{"version":"0.9.0","targetAbi":"{abi}","sourceUrl":"{base}/plugin.wasm","checksum":"{checksum}",{sha}"repositoryName":"test","repositoryUrl":"{base}/manifest.json"}},
                  {{"version":"0.10.0","targetAbi":"{abi}","sourceUrl":"{base}/plugin.wasm","checksum":"{checksum}",{sha}"repositoryName":"test","repositoryUrl":"{base}/manifest.json"}}
                ]}}]"#
        )
    }

    struct InstallRig {
        mgr: FerrofinPluginManager,
        lifecycle: Arc<FlagLifecycle>,
        wasm_dir: std::path::PathBuf,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        _stop: std::sync::mpsc::Sender<()>,
    }

    async fn install_rig(
        artifact: &[u8],
        sha256: Option<&str>,
        checksum: &str,
        abi: &str,
        validator_id: Result<Uuid, String>,
    ) -> InstallRig {
        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let wasm_dir = wasm_root.path().join("plugins");
        let (base, stop2) = repo_server(
            |base| manifest_json(base, checksum, sha256, abi),
            artifact.to_vec(),
        );
        let lifecycle = Arc::new(FlagLifecycle(AtomicBool::new(false)));
        let mgr = FerrofinPluginManager::new(Vec::new(), state_dir.path().to_path_buf())
            .with_installer(
                wasm_dir.clone(),
                Arc::new(StubValidator {
                    id: validator_id,
                    abi: TEST_ABI,
                }),
                lifecycle.clone(),
            );
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        InstallRig {
            mgr,
            lifecycle,
            wasm_dir,
            _dirs: (state_dir, wasm_root),
            _stop: stop2,
        }
    }

    #[tokio::test]
    async fn install_downloads_verifies_stages_and_flags_restart() {
        let artifact = b"pretend-wasm-bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;

        rig.mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .expect("install succeeds");

        let staged = rig.wasm_dir.join(format!("{PKG_ID}.wasm"));
        assert_eq!(std::fs::read(&staged).unwrap(), artifact, "artifact staged");
        assert!(rig.lifecycle.has_pending_restart(), "restart flagged");
        // The validator-reported egress allowlist was recorded (upgrade
        // growth diffs against it).
        let persisted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rig.mgr.plugins_dir_for_test().join("state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            persisted["installed_egress"][PKG_ID.to_string()][0],
            "api.example.com",
            "declared egress persisted: {persisted}"
        );

        // Uninstall removes the staged file and re-flags restart — and the
        // egress record goes with it (a reinstall must not diff stale).
        rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
        assert!(!staged.exists(), "artifact removed");
        let persisted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(rig.mgr.plugins_dir_for_test().join("state.json")).unwrap(),
        )
        .unwrap();
        assert!(
            persisted["installed_egress"]
                .get(PKG_ID.to_string())
                .is_none(),
            "egress record cleared on uninstall: {persisted}"
        );
        // Unknown id after removal → NotFound (not in registry either).
        assert!(rig.mgr.remove_plugin(PKG_ID).await.is_err());
    }

    /// Builds an install rig whose registry already knows `PKG_ID` at
    /// `installed_version`, optionally with its artifact staged on disk (a
    /// runtime-installed WASM plugin) and optionally disabled.
    async fn update_rig(installed_version: &str, staged: bool, enabled: bool) -> InstallRig {
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let mut rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;
        let mut d = descriptor(PKG_ID, "HelloPkg", enabled);
        d.version = installed_version.to_owned();
        rig.mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(d, None)],
            rig.mgr.plugins_dir_for_test().to_path_buf(),
        )
        .with_installer(
            rig.wasm_dir.clone(),
            Arc::new(StubValidator {
                id: Ok(PKG_ID),
                abi: TEST_ABI,
            }),
            rig.lifecycle.clone(),
        );
        if staged {
            std::fs::create_dir_all(&rig.wasm_dir).unwrap();
            std::fs::write(rig.wasm_dir.join(format!("{PKG_ID}.wasm")), b"staged").unwrap();
        }
        rig
    }

    #[tokio::test]
    async fn available_updates_offer_the_newest_compatible_version_for_staged_plugins() {
        // Installed 0.9.0, catalog has 0.9.0 + 0.10.0 → the newer one is offered.
        let rig = update_rig("0.9.0", true, true).await;
        let updates = rig.mgr.available_plugin_updates().await.expect("updates");
        assert_eq!(updates.len(), 1, "{updates:?}");
        assert_eq!(updates[0].id, PKG_ID);
        assert_eq!(updates[0].name, "HelloPkg");
        assert_eq!(updates[0].installed_version, "0.9.0");
        assert_eq!(updates[0].version, "0.10.0");

        // Already on the newest catalog version → nothing to do.
        let rig = update_rig("0.10.0", true, true).await;
        assert!(
            rig.mgr
                .available_plugin_updates()
                .await
                .expect("updates")
                .is_empty()
        );

        // A compiled-in extension (no staged artifact) is never offered an
        // update — `install_package` refuses its id outright.
        let rig = update_rig("0.9.0", false, true).await;
        assert!(
            rig.mgr
                .available_plugin_updates()
                .await
                .expect("updates")
                .is_empty()
        );

        // A disabled plugin is skipped, matching upstream's gate.
        let rig = update_rig("0.9.0", true, false).await;
        assert!(
            rig.mgr
                .available_plugin_updates()
                .await
                .expect("updates")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn available_updates_refuse_wrong_abi_stale_repeats_and_a_disarmed_installer() {
        // A catalog that only offers versions built against another server's
        // plugin ABI is no update at all.
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let mut rig = install_rig(&artifact, None, &md5, "ferrofin:plugin@9.9.9", Ok(PKG_ID)).await;
        let mut d = descriptor(PKG_ID, "HelloPkg", true);
        d.version = "0.9.0".to_owned();
        rig.mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(d.clone(), None)],
            rig.mgr.plugins_dir_for_test().to_path_buf(),
        )
        .with_installer(
            rig.wasm_dir.clone(),
            Arc::new(StubValidator {
                id: Ok(PKG_ID),
                abi: TEST_ABI,
            }),
            rig.lifecycle.clone(),
        );
        std::fs::create_dir_all(&rig.wasm_dir).unwrap();
        std::fs::write(rig.wasm_dir.join(format!("{PKG_ID}.wasm")), b"staged").unwrap();
        assert!(
            rig.mgr
                .available_plugin_updates()
                .await
                .expect("updates")
                .is_empty(),
            "a version targeting another ABI is never offered"
        );

        // A version already staged in this data directory is waiting for the
        // activating restart, so it must not be downloaded and staged again on
        // every run.
        let rig = update_rig("0.9.0", true, true).await;
        rig.mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .expect("install the update");
        assert!(
            rig.mgr
                .available_plugin_updates()
                .await
                .expect("updates")
                .is_empty(),
            "an already-staged version is not offered again"
        );

        // Without an installer armed nothing here is installable, so the task
        // must not even reach the network.
        let dir = tempfile::tempdir().expect("tempdir");
        let bare = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(d, None)],
            dir.path().to_path_buf(),
        );
        assert!(
            bare.available_plugin_updates()
                .await
                .expect("updates")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_second_repository_can_publish_the_update_and_the_install_follows_it() {
        // Two repositories list the same plugin; only the second has the newer
        // release. The offered version must come from that repository AND the
        // install must resolve the same entry — resolving "the first catalog
        // entry with this id" would look for the version in the wrong
        // repository and fail forever.
        let artifact = b"newer-bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let (old_base, _stop_old) = repo_server(
            |base| {
                format!(
                    r#"[{{"name":"HelloPkg","description":"d","overview":"o","owner":"me","category":"General",
                    "guid":"{PKG_ID}",
                    "versions":[
                      {{"version":"0.9.0","targetAbi":"{TEST_ABI}","sourceUrl":"{base}/plugin.wasm","checksum":"deadbeef","repositoryName":"old","repositoryUrl":"{base}/manifest.json"}}
                    ]}}]"#
                )
            },
            b"old-bytes".to_vec(),
        );
        let (new_base, _stop_new) = repo_server(
            move |base| {
                format!(
                    r#"[{{"name":"HelloPkg","description":"d","overview":"o","owner":"me","category":"General",
                    "guid":"{PKG_ID}",
                    "versions":[
                      {{"version":"1.2.0","targetAbi":"{TEST_ABI}","sourceUrl":"{base}/plugin.wasm","checksum":"{md5}","repositoryName":"new","repositoryUrl":"{base}/manifest.json"}}
                    ]}}]"#
                )
            },
            artifact.clone(),
        );

        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let wasm_dir = wasm_root.path().join("plugins");
        std::fs::create_dir_all(&wasm_dir).unwrap();
        std::fs::write(wasm_dir.join(format!("{PKG_ID}.wasm")), b"staged").unwrap();
        let mut installed = descriptor(PKG_ID, "HelloPkg", true);
        installed.version = "0.9.0".to_owned();
        let mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(installed, None)],
            state_dir.path().to_path_buf(),
        )
        .with_installer(
            wasm_dir.clone(),
            Arc::new(StubValidator {
                id: Ok(PKG_ID),
                abi: TEST_ABI,
            }),
            Arc::new(FlagLifecycle(AtomicBool::new(false))),
        );
        mgr.set_repositories(vec![
            RepositoryInfo {
                name: Some("old".to_owned()),
                url: Some(format!("{old_base}/manifest.json")),
                enabled: true,
            },
            RepositoryInfo {
                name: Some("new".to_owned()),
                url: Some(format!("{new_base}/manifest.json")),
                enabled: true,
            },
        ])
        .await
        .unwrap();

        let updates = mgr.available_plugin_updates().await.expect("updates");
        assert_eq!(updates.len(), 1, "{updates:?}");
        assert_eq!(updates[0].version, "1.2.0");
        assert_eq!(
            updates[0].repository_url.as_deref(),
            Some(format!("{new_base}/manifest.json").as_str()),
            "the offer carries the repository that published it"
        );

        mgr.install_package(
            &updates[0].name,
            Some(updates[0].id),
            Some(&updates[0].version),
            updates[0].repository_url.as_deref(),
        )
        .await
        .expect("the pinned repository resolves");
        assert_eq!(
            std::fs::read(wasm_dir.join(format!("{PKG_ID}.wasm"))).unwrap(),
            artifact,
            "the second repository's artifact was staged"
        );
    }

    #[tokio::test]
    async fn no_updatable_plugin_means_no_repository_traffic() {
        // The common deployment has only compiled-in extensions. Fetching every
        // repository at boot and every 24 h to produce an empty list is pure
        // cost, so the eligible set is computed before any request goes out.
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        let (base, _stop) = raw_server(move |_| {
            Box::new(move |_request| {
                counted.fetch_add(1, Ordering::SeqCst);
                http_ok("application/json", b"[]")
            })
        });

        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        // A registered plugin with NO staged artifact — i.e. a compiled-in one.
        let mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(
                descriptor(PKG_ID, "Compiled In", true),
                None,
            )],
            state_dir.path().to_path_buf(),
        )
        .with_installer(
            wasm_root.path().join("plugins"),
            Arc::new(StubValidator {
                id: Ok(PKG_ID),
                abi: TEST_ABI,
            }),
            Arc::new(FlagLifecycle(AtomicBool::new(false))),
        );
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();

        assert!(
            mgr.available_plugin_updates()
                .await
                .expect("updates")
                .is_empty()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no repository was contacted"
        );
    }

    #[tokio::test]
    async fn install_resolves_by_guid_and_prefers_numerically_newest() {
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;
        // Wrong name + right guid still resolves (guid wins).
        rig.mgr
            .install_package("totally-wrong-name", Some(PKG_ID), None, None)
            .await
            .expect("guid resolution");
        // 0.10.0 beats 0.9.0 numerically (would lose lexicographically).
        assert_eq!(
            super::version_sort_key("0.10.0").0,
            vec![0, 10, 0],
            "numeric key"
        );
        assert!(super::version_sort_key("0.10.0") > super::version_sort_key("0.9.0"));
    }

    #[tokio::test]
    async fn install_rejections_cover_every_gate() {
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);

        // Checksum mismatch.
        let rig = install_rig(&artifact, None, "00000000", TEST_ABI, Ok(PKG_ID)).await;
        let err = rig
            .mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert!(!rig.lifecycle.has_pending_restart());

        // sha256 preferred over (correct) md5 — and mismatching.
        let rig = install_rig(&artifact, Some("deadbeef"), &md5, TEST_ABI, Ok(PKG_ID)).await;
        let err = rig
            .mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");

        // ABI mismatch.
        let rig = install_rig(&artifact, None, &md5, "ferrofin:plugin@9.9.9", Ok(PKG_ID)).await;
        let err = rig
            .mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("targets ABI"), "{err}");

        // Validator says the artifact is bogus.
        let rig = install_rig(
            &artifact,
            None,
            &md5,
            TEST_ABI,
            Err("not a component".to_owned()),
        )
        .await;
        let err = rig
            .mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a component"), "{err}");

        // Identity mismatch: artifact reports a different id than the catalog.
        let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(Uuid::from_u128(7))).await;
        let err = rig
            .mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");

        // Unknown package.
        let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;
        let err = rig
            .mgr
            .install_package("NoSuchPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("package"), "{err}");

        // Installer not armed at all.
        let dir = tempfile::tempdir().unwrap();
        let bare = FerrofinPluginManager::new(Vec::new(), dir.path().to_path_buf());
        let err = bare
            .install_package("x", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    #[test]
    fn https_is_required_except_loopback() {
        assert!(super::require_https("https://example.com/p.wasm", false).is_ok());
        assert!(super::require_https("http://127.0.0.1:9/p.wasm", true).is_ok());
        assert!(super::require_https("http://localhost:9/p.wasm", true).is_ok());
        assert!(super::require_https("http://[::1]:9/p.wasm", true).is_ok());
        assert!(super::require_https("http://example.com/p.wasm", true).is_err());
        assert!(super::require_https("http://192.168.1.10/p.wasm", true).is_err());
        assert!(super::require_https("ftp://example.com/p.wasm", true).is_err());
        // The exemption is opt-in (granted only by an admin-configured
        // loopback repository) — a remote manifest pointing sourceUrl at
        // localhost gets refused.
        assert!(super::require_https("http://127.0.0.1:9/p.wasm", false).is_err());
        assert!(super::require_https("http://localhost:9/p.wasm", false).is_err());
    }

    #[test]
    fn prerelease_sorts_below_its_release() {
        assert!(super::version_sort_key("1.0.0") > super::version_sort_key("1.0.0-rc1"));
        assert!(super::version_sort_key("1.0.1-rc1") > super::version_sort_key("1.0.0"));
        assert!(super::version_sort_key("0.10.0") > super::version_sort_key("0.9.9"));
    }

    #[tokio::test]
    async fn download_cap_enforced_on_streamed_bytes_not_content_length() {
        // The server omits content-length entirely (body ends at EOF), so
        // only the running streamed count can enforce the cap.
        let artifact = vec![0u8; 2 * 1024 * 1024];
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let (base, _stop) = raw_server(|base| {
            let manifest = manifest_json(base, &md5, None, TEST_ABI);
            Box::new(move |request| {
                if request.contains("/plugin.wasm") {
                    let mut out =
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/wasm\r\nconnection: close\r\n\r\n"
                            .to_vec();
                    out.extend_from_slice(&vec![0u8; 2 * 1024 * 1024]);
                    out
                } else {
                    http_ok("application/json", manifest.as_bytes())
                }
            })
        });
        let mgr = FerrofinPluginManager::new(Vec::new(), state_dir.path().to_path_buf())
            .with_installer(
                wasm_root.path().join("plugins"),
                Arc::new(StubValidator {
                    id: Ok(PKG_ID),
                    abi: TEST_ABI,
                }),
                Arc::new(FlagLifecycle(AtomicBool::new(false))),
            )
            .with_download_cap_mb(Some(1));
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        let err = mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn insecure_redirects_are_refused() {
        // The artifact endpoint 302s to a cleartext non-loopback host; the
        // per-hop redirect policy must refuse to follow it.
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let (base, _stop) = raw_server(|base| {
            let manifest = manifest_json(base, &md5, None, TEST_ABI);
            Box::new(move |request| {
                if request.contains("/plugin.wasm") {
                    b"HTTP/1.1 302 Found\r\nlocation: http://192.0.2.1/evil.wasm\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_vec()
                } else {
                    http_ok("application/json", manifest.as_bytes())
                }
            })
        });
        let mgr = FerrofinPluginManager::new(Vec::new(), state_dir.path().to_path_buf())
            .with_installer(
                wasm_root.path().join("plugins"),
                Arc::new(StubValidator {
                    id: Ok(PKG_ID),
                    abi: TEST_ABI,
                }),
                Arc::new(FlagLifecycle(AtomicBool::new(false))),
            );
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        let err = mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    #[tokio::test]
    async fn disabled_plugins_hide_their_configuration_pages() {
        let plugin = RegisteredPlugin::new(descriptor(PKG_ID, "Paged", true), None)
            .with_config_page(super::PluginConfigPage {
                name: "paged-settings".to_owned(),
                bytes: b"<div data-role=\"page\">x</div>".to_vec(),
                enable_in_main_menu: false,
            });
        let (mgr, _dir) = manager(vec![plugin]);
        // Enabled: listed and fetchable.
        assert_eq!(mgr.get_configuration_pages().await.unwrap().len(), 1);
        assert!(
            mgr.get_configuration_page("PAGED-SETTINGS")
                .await
                .unwrap()
                .is_some(),
            "case-insensitive fetch while enabled"
        );
        // Disabled: gone from discovery AND from the (unauthenticated)
        // content endpoint — Disable must disarm the browser-side surface.
        mgr.disable_plugin(PKG_ID).await.unwrap();
        assert!(mgr.get_configuration_pages().await.unwrap().is_empty());
        assert!(
            mgr.get_configuration_page("paged-settings")
                .await
                .unwrap()
                .is_none()
        );
        // Re-enabled: back.
        mgr.enable_plugin(PKG_ID).await.unwrap();
        assert_eq!(mgr.get_configuration_pages().await.unwrap().len(), 1);
    }

    #[test]
    fn merge_registrations_enforces_both_identifier_namespaces() {
        let page = |name: &str| super::PluginConfigPage {
            name: name.to_owned(),
            bytes: b"x".to_vec(),
            enable_in_main_menu: false,
        };
        let base_id = Uuid::from_u128(1);
        let mut registered = vec![
            RegisteredPlugin::new(descriptor(base_id, "Compiled", true), None)
                .with_config_page(page("introskipper")),
        ];
        let incoming = vec![
            // Same id as a compiled-in plugin: skipped whole.
            RegisteredPlugin::new(descriptor(base_id, "Squatter", true), None),
            // Page name collides with a compiled-in page (case-insensitively):
            // the plugin survives, the page is dropped.
            RegisteredPlugin::new(descriptor(Uuid::from_u128(2), "PageSquat", true), None)
                .with_config_page(page("IntroSkipper"))
                .with_config_page(page("pagesquat-own")),
            // Claims the previous incoming plugin's page name: later loses.
            RegisteredPlugin::new(descriptor(Uuid::from_u128(3), "Later", true), None)
                .with_config_page(page("pagesquat-own")),
            // Reuses an INCOMING plugin's id: the rule holds within the
            // incoming batch too, not just against the pre-existing registry.
            RegisteredPlugin::new(descriptor(Uuid::from_u128(2), "IncomingDup", true), None),
        ];
        super::merge_plugin_registrations(&mut registered, incoming);
        let ids: Vec<Uuid> = registered.iter().map(|p| p.descriptor.id).collect();
        assert_eq!(
            ids,
            vec![base_id, Uuid::from_u128(2), Uuid::from_u128(3)],
            "both id squatters skipped (vs registry AND within the batch), others kept"
        );
        assert_eq!(
            registered[1].config_pages.len(),
            1,
            "colliding page dropped"
        );
        assert_eq!(registered[1].config_pages[0].name, "pagesquat-own");
        assert!(
            registered[2].config_pages.is_empty(),
            "later claimant of a taken name loses the page, keeps the plugin"
        );
    }

    #[tokio::test]
    async fn reinstalling_a_known_version_with_different_bytes_is_refused() {
        // Install v0.10.0 from a repo serving artifact A…
        let artifact_a = b"artifact-A".to_vec();
        let md5_a = ferrofin_common::extensions::md5_hex(&artifact_a);
        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let wasm_dir = wasm_root.path().join("plugins");
        let installer = |base_url: String| {
            let mgr = FerrofinPluginManager::new(Vec::new(), state_dir.path().to_path_buf())
                .with_installer(
                    wasm_dir.clone(),
                    Arc::new(StubValidator {
                        id: Ok(PKG_ID),
                        abi: TEST_ABI,
                    }),
                    Arc::new(FlagLifecycle(AtomicBool::new(false))),
                );
            (mgr, base_url)
        };
        {
            let (base, _stop) = repo_server(
                |base| manifest_json(base, &md5_a, None, TEST_ABI),
                artifact_a,
            );
            let (mgr, base) = installer(base);
            mgr.set_repositories(vec![RepositoryInfo {
                name: Some("test".to_owned()),
                url: Some(format!("{base}/manifest.json")),
                enabled: true,
            }])
            .await
            .unwrap();
            mgr.install_package("HelloPkg", None, None, None)
                .await
                .expect("first install");
        }
        // …then the repo swaps the bytes under the SAME version. A fresh
        // manager (state.json persisted the digest) must refuse — and a NEW
        // version with new bytes must still install.
        let artifact_b = b"artifact-B-tampered".to_vec();
        let md5_b = ferrofin_common::extensions::md5_hex(&artifact_b);
        let (base, _stop) = repo_server(
            |base| manifest_json(base, &md5_b, None, TEST_ABI),
            artifact_b,
        );
        let (mgr, base) = installer(base);
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        let err = mgr
            .install_package("HelloPkg", None, Some("0.10.0"), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("immutable"), "{err}");
        // The 0.9.0 entry in the manifest was never installed before, so its
        // (new) bytes are fine — version pinning is per version, not global.
        mgr.install_package("HelloPkg", None, Some("0.9.0"), None)
            .await
            .expect("a never-installed version accepts new bytes");
    }

    #[tokio::test]
    async fn repository_cannot_squat_a_compiled_in_id_but_upgrades_pass() {
        // A manifest claiming a compiled-in extension's guid must be
        // refused (otherwise the restart registers two plugins with one id
        // and `find()` silently addresses the wrong one)…
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let state_dir = tempfile::tempdir().unwrap();
        let wasm_root = tempfile::tempdir().unwrap();
        let wasm_dir = wasm_root.path().join("plugins");
        let (base, _stop) = repo_server(
            |base| manifest_json(base, &md5, None, TEST_ABI),
            artifact.clone(),
        );
        let mgr = FerrofinPluginManager::new(
            vec![RegisteredPlugin::new(
                descriptor(PKG_ID, "Compiled-in", true),
                None,
            )],
            state_dir.path().to_path_buf(),
        )
        .with_installer(
            wasm_dir.clone(),
            Arc::new(StubValidator {
                id: Ok(PKG_ID),
                abi: TEST_ABI,
            }),
            Arc::new(FlagLifecycle(AtomicBool::new(false))),
        );
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("test".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        let err = mgr
            .install_package("HelloPkg", None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("compiled-in"), "{err}");

        // …but the same registry state WITH a staged artifact is a loaded
        // WASM plugin being upgraded — that must keep working.
        std::fs::create_dir_all(&wasm_dir).unwrap();
        std::fs::write(wasm_dir.join(format!("{PKG_ID}.wasm")), b"old").unwrap();
        mgr.install_package("HelloPkg", None, None, None)
            .await
            .expect("upgrade of an installed wasm plugin");
        assert_eq!(
            std::fs::read(wasm_dir.join(format!("{PKG_ID}.wasm"))).unwrap(),
            artifact,
            "upgrade replaced the staged artifact"
        );
    }

    #[tokio::test]
    async fn read_capped_enforces_the_streamed_limit() {
        // Body larger than the cap, served WITHOUT content-length — only
        // the running streamed count can catch it.
        let (base, _stop) = raw_server(|_| {
            Box::new(|_| {
                let mut out =
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n"
                        .to_vec();
                out.extend_from_slice(&[b'x'; 2048]);
                out
            })
        });
        let resp = reqwest::get(format!("{base}/big")).await.unwrap();
        let err = super::read_capped(resp, 1024, &base).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        // Under the cap passes through intact.
        let resp = reqwest::get(format!("{base}/big")).await.unwrap();
        let ok = super::read_capped(resp, 4096, &base).await.unwrap();
        assert_eq!(ok.len(), 2048);
    }

    #[tokio::test]
    async fn oversized_manifest_is_skipped_not_fatal() {
        // A repository serving a manifest over MAX_MANIFEST_BYTES is skipped
        // with a warning; the catalog call itself still succeeds.
        let big = usize::try_from(super::MAX_MANIFEST_BYTES).unwrap() + 1;
        let (base, _stop) = raw_server(move |_| {
            Box::new(move |_| {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {big}\r\nconnection: close\r\n\r\n"
                )
                .into_bytes();
                out.extend_from_slice(&vec![b'['; big]);
                out
            })
        });
        let dir = tempfile::tempdir().unwrap();
        let mgr = FerrofinPluginManager::new(Vec::new(), dir.path().to_path_buf());
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("huge".to_owned()),
            url: Some(format!("{base}/manifest.json")),
            enabled: true,
        }])
        .await
        .unwrap();
        let packages = mgr.list_packages().await.expect("catalog still succeeds");
        assert!(
            packages.is_empty(),
            "oversized manifest contributed nothing"
        );
    }

    #[tokio::test]
    async fn list_packages_stamps_repository_provenance() {
        // The manifest lies about its own provenance; the stamped values
        // must come from the repository actually fetched.
        let artifact = b"bytes".to_vec();
        let md5 = ferrofin_common::extensions::md5_hex(&artifact);
        let (base, _stop) = repo_server(
            |base| {
                manifest_json(base, &md5, None, TEST_ABI)
                    .replace("\"repositoryName\":\"test\"", "\"repositoryName\":\"liar\"")
                    .replace(
                        &format!("\"repositoryUrl\":\"{base}/manifest.json\""),
                        "\"repositoryUrl\":\"https://evil.example\"",
                    )
            },
            artifact,
        );
        let dir = tempfile::tempdir().unwrap();
        let mgr = FerrofinPluginManager::new(Vec::new(), dir.path().to_path_buf());
        let repo_url = format!("{base}/manifest.json");
        mgr.set_repositories(vec![RepositoryInfo {
            name: Some("honest".to_owned()),
            url: Some(repo_url.clone()),
            enabled: true,
        }])
        .await
        .unwrap();
        let packages = mgr.list_packages().await.unwrap();
        let pkg = packages.iter().find(|p| p.id == PKG_ID).unwrap();
        for v in &pkg.versions {
            assert_eq!(v.repository_name, "honest");
            assert_eq!(v.repository_url, repo_url);
        }
    }
}
