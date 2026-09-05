//! The plugin-manager seam — **Tier 1** (compile-time plugins).
//!
//! Port of the object-safe slice of `MediaBrowser.Common.Plugins.IPluginManager`
//! that the `PluginsController` / `PackageController` API surface needs, reduced
//! to Ferrofin's Tier-1 model: plugins are Rust crates compiled into the server and
//! registered at the composition root. There is no runtime assembly loading
//! (that is Tier 2 — a WASM/`libloading` boundary, see `docs/EXTENSIONS.md`),
//! `remove_plugin` and package installation ARE supported for runtime-installed
//! WASM (Tier-1b) plugins: `install_package` downloads a repository package,
//! verifies + validates it, and stages it for the next restart; compiled-in
//! plugins still reject both.
//!
//! The trait traffics only in plain data ([`PluginDescriptor`], [`PluginImage`],
//! and the `ferrofin-model` `RepositoryInfo`/`PackageInfo` wire DTOs) so it stays
//! object-safe and implementable in `ferrofin-core` without any knowledge of the
//! `ferrofin-api` router or `AppState`.

use async_trait::async_trait;
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use uuid::Uuid;

use crate::error::ServiceError;

/// A compiled-in plugin's presentation metadata.
///
/// Stands in for the un-ported `LocalPlugin`/`PluginManifest` domain types. The
/// `ferrofin-api` layer projects this into the `PluginInfo` wire DTO (deriving the
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
    /// The file the plugin persists its configuration into, when it names one
    /// (C# `BasePlugin.ConfigurationFileName`, e.g. `Jellyfin.Plugin.Tmdb.xml`).
    /// Reported verbatim on `GET /Plugins`; `None` omits the field, which is
    /// what upstream does for a plugin that has no configuration.
    pub configuration_file_name: Option<String>,
}

/// An installed plugin with a newer, installable version in the catalog.
///
/// Port of the `InstallationInfo` slice upstream's
/// `IInstallationManager.GetAvailablePluginUpdates` yields to
/// `PluginUpdateTask` — enough to name the package and the version to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateInfo {
    /// The plugin's stable id (the catalog package's guid).
    pub id: Uuid,
    /// The package name, as the repository advertises it.
    pub name: String,
    /// The version currently installed.
    pub installed_version: String,
    /// The newer compatible version available for install.
    pub version: String,
    /// The repository that offers [`version`](Self::version), so the install
    /// resolves the same catalog entry this update was chosen from (upstream's
    /// `InstallationInfo` pins the `SourceUrl` for the same reason). `None`
    /// leaves the repository unconstrained.
    pub repository_url: Option<String>,
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
    /// Port of `IInstallationManager.GetAvailablePackages`: each enabled
    /// repository's manifest, with `repositoryName`/`repositoryUrl` stamped from
    /// the repository actually fetched, versions built against a newer
    /// `targetAbi` removed, packages left with no compatible version dropped, and
    /// same-identity packages from different repositories merged into one entry.
    ///
    /// This is the *installable* catalogue and nothing else — upstream's
    /// `PackageController.GetPackages` returns exactly this. Installed plugins
    /// are `/Plugins`; a compiled-in extension is resolvable through
    /// [`find_package`](Self::find_package) but is not listed here.
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError>;

    /// Resolves one package by name and/or assembly guid — what
    /// `GET /Packages/{name}` serves.
    ///
    /// Port of `InstallationManager.FilterPackages`, whose two predicates are
    /// **alternatives**:
    ///
    /// ```text
    /// if (!id.IsEmpty())          … Where(x => x.Id.Equals(id));
    /// else if (name is not null)  … Where(x => x.Name.Equals(name, OrdinalIgnoreCase));
    /// ```
    ///
    /// So a non-empty guid selects on its own and the name is ignored; an
    /// all-zeros guid is `IsEmpty()` and falls through to the name branch.
    ///
    /// # Errors
    /// Backend errors from reading the catalogue.
    async fn find_package(
        &self,
        name: Option<&str>,
        assembly_guid: Option<Uuid>,
    ) -> Result<Option<PackageInfo>, ServiceError> {
        let catalog = self.list_packages().await?;
        if let Some(id) = assembly_guid.filter(|g| !g.is_nil()) {
            return Ok(catalog.into_iter().find(|p| p.id == id));
        }
        let Some(name) = name else { return Ok(None) };
        Ok(catalog
            .into_iter()
            .find(|p| p.name.to_lowercase() == name.to_lowercase()))
    }

    /// The installed plugins for which a newer, installable version exists in
    /// the configured repositories.
    ///
    /// Port of `IInstallationManager.GetAvailablePluginUpdates`, which backs
    /// upstream's `PluginUpdateTask`: every enabled installed plugin is looked
    /// up in the catalog and offered its newest compatible version above the
    /// one installed. The default implementation reports none (a manager
    /// without an installer has nothing to update).
    ///
    /// # Errors
    /// Backend errors from reading the configured repositories.
    async fn available_plugin_updates(&self) -> Result<Vec<PluginUpdateInfo>, ServiceError> {
        Ok(Vec::new())
    }

    /// Installs a package from the configured repositories: resolve →
    /// download → verify checksum → validate the artifact → stage into the
    /// WASM plugins directory. The plugin activates on the next restart
    /// (the implementation marks restart-required).
    ///
    /// `assembly_guid` wins over `name` when present (names collide across
    /// repositories); `repository_url` filters candidate versions; `version`
    /// pins one, else the newest is chosen.
    ///
    /// # Errors
    /// [`ServiceError::NotFound`] for an unknown package/version;
    /// [`ServiceError::InvalidInput`] for checksum/ABI/validation failures;
    /// backend errors for download or filesystem problems. The default
    /// implementation rejects (a manager without an installer).
    async fn install_package(
        &self,
        _name: &str,
        _assembly_guid: Option<Uuid>,
        _version: Option<&str>,
        _repository_url: Option<&str>,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::invalid_input(
            "runtime plugin installation is not available on this server",
        ))
    }

    /// The plugin configuration pages — the dashboard `GET /web/ConfigurationPages`
    /// list, projected from plugins that ship a settings page. Defaults to empty
    /// (a plugin without a page has no dashboard settings link).
    async fn get_configuration_pages(
        &self,
    ) -> Result<Vec<ferrofin_model::plugins::ConfigurationPageInfo>, ServiceError> {
        Ok(Vec::new())
    }

    /// A configuration page's HTML by its page `name`, for
    /// `GET /web/ConfigurationPage`. Defaults to `None` (no such page).
    async fn get_configuration_page(&self, _name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(None)
    }
}

fn _assert_object_safe_plugin_manager(_: &dyn PluginManager) {}

/// Validates a downloaded plugin artifact before it is committed to disk.
///
/// Implemented by the WASM host crate (which owns wasmtime and the plugin
/// ABI); injected into the plugin manager at the composition root. This seam
/// exists because `ferrofin-core` must not depend on `ferrofin-wasm` (the
/// dependency arrow points the other way).
#[async_trait]
pub trait PluginArtifactValidator: Send + Sync {
    /// The plugin ABI this server supports (e.g. `ferrofin:plugin@0.2.0`),
    /// used for the manifest `targetAbi` gate and error messages.
    fn supported_abi(&self) -> &str;

    /// Checks that `bytes` is a loadable component of the supported world
    /// and returns its self-reported identity + declared egress. Runs in a
    /// throwaway sandbox with the standard limits and no capabilities armed
    /// — the artifact cannot reach the network or the library during
    /// validation.
    ///
    /// # Errors
    /// [`ServiceError::InvalidInput`] describing why the artifact is not a
    /// valid plugin (not a component, wrong world, bad descriptor, …).
    async fn validate(&self, bytes: &[u8]) -> Result<ValidatedArtifact, ServiceError>;
}

/// What install-time validation learns about an artifact.
#[derive(Debug, Clone)]
pub struct ValidatedArtifact {
    /// The plugin's self-reported descriptor id.
    pub id: Uuid,
    /// The plugin's declared public-egress allowlist, verbatim — recorded
    /// at install so an upgrade that GROWS a plugin's reach is loudly
    /// visible in the server log.
    pub declared_egress: Vec<String>,
}

fn _assert_object_safe_plugin_artifact_validator(_: &dyn PluginArtifactValidator) {}

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

/// One HTTP request routed into a plugin's own URL space
/// (`/Plugins/{id}/web/…`), with the caller's identity already resolved —
/// a plugin never sees credentials or tokens.
#[derive(Debug, Clone)]
pub struct PluginWebRequest {
    /// The HTTP method (`GET`, `POST`, …).
    pub method: String,
    /// The path RELATIVE to the plugin's prefix, leading `/` included.
    pub path: String,
    /// The raw query string (`""` when absent).
    pub query: String,
    /// Request headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// The request body (the transport layer caps its size).
    pub body: Option<Vec<u8>>,
    /// The authenticated caller's user id, when there is one.
    pub user_id: Option<uuid::Uuid>,
    /// Whether the caller is an administrator (or an API key).
    pub is_admin: bool,
    /// Whether the caller presented valid credentials at all.
    pub is_authenticated: bool,
}

/// A plugin's response to a [`PluginWebRequest`].
#[derive(Debug, Clone)]
pub struct PluginWebResponse {
    /// The HTTP status code.
    pub status: u16,
    /// Response headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// The response body bytes.
    pub body: Vec<u8>,
}

/// Dispatches requests from the per-plugin URL space to the plugin that
/// owns it. Implemented by the WASM host (the API layer depends only on
/// this seam); absent or unknown/disabled plugin ⇒ the route 404s.
#[async_trait]
pub trait PluginRequestHandler: Send + Sync {
    /// Handles one request for `plugin_id`. `Ok(None)` means "no such
    /// plugin (or it is disabled)" — the transport turns that into `404`.
    ///
    /// # Errors
    /// Backend failures (the plugin trapping, the runtime being gone).
    async fn handle(
        &self,
        plugin_id: uuid::Uuid,
        request: PluginWebRequest,
    ) -> Result<Option<PluginWebResponse>, ServiceError>;
}

/// Object-safety guard.
fn _assert_plugin_request_handler_object_safe(_: &dyn PluginRequestHandler) {}

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
/// rejected. Used as the [`AppState`](../../ferrofin_api/state) default so test
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
