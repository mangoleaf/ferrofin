//! The plugin-manager seam — **Tier 1** (compile-time plugins).
//!
//! Port of the object-safe slice of `MediaBrowser.Common.Plugins.IPluginManager`
//! that the `PluginsController` / `PackageController` API surface needs, reduced
//! to Hermit's Tier-1 model: plugins are Rust crates compiled into the server and
//! registered at the composition root. There is no runtime assembly loading
//! (that is Tier 2 — a WASM/`libloading` boundary, see `PLAN_HERMIT_PLUGINS.md`),
//! so `remove_plugin` and package *installation* are not supported here.
//!
//! The trait traffics only in plain data ([`PluginDescriptor`], [`PluginImage`],
//! and the `hermit-model` `RepositoryInfo`/`PackageInfo` wire DTOs) so it stays
//! object-safe and implementable in `hermit-core` without any knowledge of the
//! `hermit-api` router or `AppState`.

use async_trait::async_trait;
use hermit_model::updates::{PackageInfo, RepositoryInfo};
use uuid::Uuid;

use crate::error::ServiceError;

/// A compiled-in plugin's presentation metadata.
///
/// Stands in for the un-ported `LocalPlugin`/`PluginManifest` domain types. The
/// `hermit-api` layer projects this into the `PluginInfo` wire DTO (deriving the
/// `PluginStatus` from [`enabled`](Self::enabled)).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginDescriptor {
    /// The plugin's stable id.
    pub id: Uuid,
    /// The plugin's display name.
    pub name: String,
    /// The plugin's version string.
    pub version: String,
    /// A human-readable description.
    pub description: String,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// Whether the plugin ships a valid image (served by `GetPluginImage`).
    pub has_image: bool,
    /// Whether the plugin can be uninstalled at runtime. Always `false` for a
    /// compiled-in Tier-1 plugin.
    pub can_uninstall: bool,
}

/// A plugin's bundled image, served by `GET /Plugins/{id}/{version}/Image`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginImage {
    /// The image's MIME type (e.g. `image/png`).
    pub content_type: String,
    /// The raw image bytes.
    pub data: Vec<u8>,
}

/// Manages the server's compiled-in plugins and package repositories.
///
/// Port of `IPluginManager` (the Tier-1 slice). Every method is `async fn ->
/// Result<_, ServiceError>`; `Guid` → [`Uuid`]. Plugin *configuration* is opaque
/// JSON bytes (the wire `BasePluginConfiguration` is an open object each plugin
/// subclasses), stored per plugin.
#[async_trait]
pub trait PluginManager: Send + Sync {
    /// Lists all installed (compiled-in) plugins.
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError>;

    /// Gets a single plugin by id, if installed.
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError>;

    /// Enables the plugin with the given id (persisted across restarts).
    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError>;

    /// Disables the plugin with the given id (persisted across restarts).
    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError>;

    /// Removes the plugin with the given id.
    ///
    /// Tier-1 plugins are compiled in and cannot be removed at runtime; the real
    /// manager rejects with [`ServiceError::InvalidInput`].
    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError>;

    /// Returns a plugin's stored configuration as JSON bytes (`{}` when unset).
    ///
    /// [`ServiceError::NotFound`] when no such plugin is installed.
    async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError>;

    /// Persists a plugin's configuration (opaque JSON bytes).
    ///
    /// [`ServiceError::NotFound`] when no such plugin is installed;
    /// [`ServiceError::InvalidInput`] when `config` is not valid JSON.
    async fn set_plugin_configuration(&self, id: Uuid, config: Vec<u8>)
    -> Result<(), ServiceError>;

    /// Returns a plugin's bundled image, or `None` when it has none.
    async fn plugin_image(&self, id: Uuid) -> Result<Option<PluginImage>, ServiceError>;

    /// Lists the configured package repositories.
    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError>;

    /// Replaces the configured package repositories (persisted).
    async fn set_repositories(&self, repositories: Vec<RepositoryInfo>)
    -> Result<(), ServiceError>;

    /// Lists the packages available from the enabled repositories.
    ///
    /// Tier-1 does not fetch remote repository manifests, so this returns `[]`
    /// until repository browsing lands (faithful — an empty catalog, never a
    /// faked package).
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError>;

    /// The plugin configuration pages — the dashboard `GET /web/ConfigurationPages`
    /// list, projected from plugins that ship a settings page. Defaults to empty
    /// (a plugin without a page has no dashboard settings link).
    async fn get_configuration_pages(
        &self,
    ) -> Result<Vec<hermit_model::plugins::ConfigurationPageInfo>, ServiceError> {
        Ok(Vec::new())
    }

    /// A configuration page's HTML by its page `name`, for
    /// `GET /web/ConfigurationPage`. Defaults to `None` (no such page).
    async fn get_configuration_page(&self, _name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(None)
    }
}

fn _assert_object_safe_plugin_manager(_: &dyn PluginManager) {}

/// One step of a web-file transformation pipeline.
///
/// Port of the File Transformation plugin's `TransformFile` delegate: given the
/// served file's (web-root-relative) path and its current textual contents,
/// returns the transformed contents. Implementations must be pure over their
/// inputs plus their own configuration — the pipeline may run them on every
/// request of a matching file.
#[async_trait]
pub trait FileTransformer: Send + Sync {
    /// Transforms `contents` for the file at `path`.
    async fn transform(&self, path: &str, contents: String) -> String;
}

fn _assert_object_safe_file_transformer(_: &dyn FileTransformer) {}

/// The web-file transformation pipeline — the File Transformation plugin's
/// `IWebFileTransformation{Read,Write}Service`, as one object-safe seam.
///
/// Registrations map a file-name pattern (an exact web-root-relative path, or a
/// regex) to an ordered pipeline of [`FileTransformer`]s. The static web server
/// consults [`needs_transformation`](Self::needs_transformation) per request and
/// routes matching files through [`run_transformation`](Self::run_transformation).
#[async_trait]
pub trait FileTransformationService: Send + Sync {
    /// Whether any registered transformation matches `path` (leading `/`
    /// ignored). Port of `NeedsTransformation`.
    async fn needs_transformation(&self, path: &str) -> bool;

    /// Runs the matching pipeline over `contents`, returning the transformed
    /// text (unchanged when nothing matches). Port of `RunTransformation`.
    async fn run_transformation(&self, path: &str, contents: String) -> String;

    /// Registers an in-process transformer for `file_name_pattern` under `id`
    /// (idempotent per id within a pattern). Port of `AddTransformation`.
    async fn add_transformation(
        &self,
        id: Uuid,
        file_name_pattern: &str,
        transformer: std::sync::Arc<dyn FileTransformer>,
    );

    /// Registers an HTTP-callback transformer: the pipeline POSTs
    /// `{"contents": …}` to `endpoint` (a relative endpoint resolves against
    /// this server's own base URL) and uses the response body as the
    /// transformed contents. Port of the `TransformationEndpoint` callback in
    /// `TransformationHelper.ApplyTransformation`.
    async fn add_endpoint_transformation(&self, id: Uuid, file_name_pattern: &str, endpoint: &str);

    /// Removes every registration made under `id`. Port of `RemoveTransformation`.
    async fn remove_transformation(&self, id: Uuid);
}

fn _assert_object_safe_file_transformation_service(_: &dyn FileTransformationService) {}

/// A disabled [`PluginManager`]: no plugins installed, no repositories, mutators
/// rejected. Used as the [`AppState`](../../hermit_api/state) default so test
/// constructors compile; the composition root injects the real manager.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledPluginManager;

impl DisabledPluginManager {
    /// The shared "no such plugin" error for id-addressed methods.
    fn not_found(id: Uuid) -> ServiceError {
        ServiceError::not_found(format!("plugin {id}"))
    }
}

#[async_trait]
impl PluginManager for DisabledPluginManager {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, _id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(None)
    }
    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }
    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }
    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }
    async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
        Err(Self::not_found(id))
    }
    async fn set_plugin_configuration(
        &self,
        id: Uuid,
        _config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }
    async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(None)
    }
    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_repositories(
        &self,
        _repositories: Vec<RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::backend("plugin subsystem is not configured"))
    }
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{DisabledPluginManager, PluginDescriptor, PluginManager};
    use uuid::Uuid;

    #[test]
    fn descriptor_default_is_empty_and_disabled() {
        let d = PluginDescriptor::default();
        assert!(d.name.is_empty());
        assert!(!d.enabled);
        assert!(!d.can_uninstall);
        assert!(d.id.is_nil());
    }

    #[tokio::test]
    async fn disabled_manager_has_no_plugins() {
        let mgr = DisabledPluginManager;
        assert!(mgr.list_plugins().await.expect("list").is_empty());
        assert!(mgr.get_plugin(Uuid::new_v4()).await.expect("get").is_none());
        assert!(mgr.get_repositories().await.expect("repos").is_empty());
        assert!(mgr.list_packages().await.expect("pkgs").is_empty());
        assert!(mgr.enable_plugin(Uuid::new_v4()).await.is_err());
        assert!(mgr.set_repositories(Vec::new()).await.is_err());
    }
}
