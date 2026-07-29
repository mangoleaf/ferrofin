//! [`HermitPluginManager`] — the registry-backed [`PluginManager`] for Tier-1
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
//! returns `[]` (no repository fetch yet). See `brain/PLAN_HERMIT_PLUGINS.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use hermit_model::updates::{PackageInfo, RepositoryInfo};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};

/// A compiled-in plugin registered with the manager at the composition root.
///
/// Plain data only (no `hermit-api`/router types) so the manager stays below the
/// dependency arrow; the composition root maps its richer `HermitPlugin` trait
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
    /// The plugin's dashboard settings page as `(page name, HTML)`, if it ships
    /// one — projected into `GET /web/ConfigurationPages` + served by
    /// `GET /web/ConfigurationPage?name=…` so the dashboard shows a Settings link.
    pub config_page: Option<(String, Vec<u8>)>,
}

impl RegisteredPlugin {
    /// Builds a registration, normalizing `has_image`/`can_uninstall` on the
    /// descriptor (image presence drives `has_image`; compiled-in ⇒ never
    /// uninstallable).
    #[must_use]
    pub fn new(mut descriptor: PluginDescriptor, image: Option<PluginImage>) -> Self {
        descriptor.has_image = image.is_some();
        descriptor.can_uninstall = false;
        Self {
            descriptor,
            image,
            default_config: b"{}".to_vec(),
            config_page: None,
        }
    }

    /// Sets the plugin's default configuration JSON.
    #[must_use]
    pub fn with_default_config(mut self, config: Vec<u8>) -> Self {
        self.default_config = config;
        self
    }

    /// Attaches the plugin's dashboard settings page (`page name`, `HTML`).
    #[must_use]
    pub fn with_config_page(mut self, name: impl Into<String>, html: Vec<u8>) -> Self {
        self.config_page = Some((name.into(), html));
        self
    }
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
}

/// The registry-backed plugin manager.
#[derive(Debug)]
pub struct HermitPluginManager {
    /// The compiled-in plugins (immutable after construction).
    plugins: Vec<RegisteredPlugin>,
    /// The `plugins/` directory holding `state.json` and per-plugin config.
    plugins_dir: PathBuf,
    /// The mutable enabled/repository state, mirrored to `state.json`.
    state: Mutex<PersistedState>,
}

impl HermitPluginManager {
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
        }
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
        let tmp = path.with_extension("json.tmp");
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
impl PluginManager for HermitPluginManager {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        let state = self.state.lock().expect("plugin state lock poisoned");
        Ok(self
            .plugins
            .iter()
            .map(|p| {
                let mut d = p.descriptor.clone();
                d.enabled = state
                    .enabled
                    .get(&d.id.to_string())
                    .copied()
                    .unwrap_or(d.enabled);
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
        d.enabled = state
            .enabled
            .get(&id.to_string())
            .copied()
            .unwrap_or(d.enabled);
        Ok(Some(d))
    }

    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        self.set_enabled(id, true)
    }

    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        self.set_enabled(id, false)
    }

    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        if self.find(id).is_none() {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        }
        // Tier-1 plugins are compiled into the binary; there is nothing to remove
        // at runtime (that needs the Tier-2 dynamic host).
        Err(ServiceError::invalid_input(
            "compiled-in plugins cannot be uninstalled at runtime",
        ))
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
        if self.find(id).is_none() {
            return Err(ServiceError::not_found(format!("plugin {id}")));
        }
        // Reject a non-JSON body so a corrupt write can't poison a later read.
        serde_json::from_slice::<serde_json::Value>(&config)
            .map_err(|_| ServiceError::invalid_input("plugin configuration must be valid JSON"))?;
        Self::atomic_write(&self.config_path(id), &config)
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
        // a warning rather than failing the whole catalog. (Runtime installation of
        // what this lists is still unsupported — Hermit has no dynamic plugin host —
        // so this populates the browse catalog only.)
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
        for repo in repos {
            let Some(url) = repo.url.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            match reqwest::get(url).await {
                Ok(resp) => match resp.json::<Vec<PackageInfo>>().await {
                    Ok(list) => packages.extend(list),
                    Err(e) => {
                        tracing::warn!(url, error = %e, "plugin repository manifest was not valid JSON");
                    }
                },
                Err(e) => {
                    tracing::warn!(url, error = %e, "failed to fetch plugin repository manifest");
                }
            }
        }
        Ok(packages)
    }

    async fn get_configuration_pages(
        &self,
    ) -> Result<Vec<hermit_model::plugins::ConfigurationPageInfo>, ServiceError> {
        Ok(self
            .plugins
            .iter()
            .filter_map(|plugin| {
                plugin.config_page.as_ref().map(|(name, _)| {
                    hermit_model::plugins::ConfigurationPageInfo {
                        name: name.clone(),
                        enable_in_main_menu: false,
                        menu_section: None,
                        menu_icon: None,
                        display_name: Some(plugin.descriptor.name.clone()),
                        plugin_id: Some(plugin.descriptor.id),
                    }
                })
            })
            .collect())
    }

    async fn get_configuration_page(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(self.plugins.iter().find_map(|plugin| {
            plugin
                .config_page
                .as_ref()
                .filter(|(page_name, _)| page_name == name)
                .map(|(_, html)| html.clone())
        }))
    }
}

/// Overlays a stored config's top-level keys onto the current defaults, so a
/// config saved by an older plugin version still returns every field (missing
/// keys keep their default). Falls back to the raw stored bytes if either side
/// isn't a JSON object.
fn merge_config(defaults: &[u8], stored: &[u8]) -> Vec<u8> {
    let (Ok(serde_json::Value::Object(mut base)), Ok(serde_json::Value::Object(over))) = (
        serde_json::from_slice::<serde_json::Value>(defaults),
        serde_json::from_slice::<serde_json::Value>(stored),
    ) else {
        return stored.to_vec();
    };
    base.extend(over);
    serde_json::to_vec(&serde_json::Value::Object(base)).unwrap_or_else(|_| stored.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{HermitPluginManager, RegisteredPlugin, merge_config};
    use hermit_model::updates::RepositoryInfo;
    use hermit_traits::error::ServiceError;
    use hermit_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
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
        assert_eq!(merged["D"], "extra"); // stale extra key preserved
        // Non-object stored falls back to raw stored bytes.
        assert_eq!(merge_config(defaults, b"not json"), b"not json");
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

    fn manager(plugins: Vec<RegisteredPlugin>) -> (HermitPluginManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mgr = HermitPluginManager::new(plugins, dir.path().to_path_buf());
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
            let mgr = HermitPluginManager::new(
                vec![RegisteredPlugin::new(descriptor(id, "Demo", true), None)],
                dir.path().to_path_buf(),
            );
            mgr.disable_plugin(id).await.expect("disable");
        }
        // A fresh manager over the same dir sees the persisted disabled flag.
        let mgr = HermitPluginManager::new(
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
}
