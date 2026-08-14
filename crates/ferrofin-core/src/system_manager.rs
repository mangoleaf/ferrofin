//! [`FerrofinSystemManager`] — the concrete [`SystemManager`].
//!
//! Port of `Emby.Server.Implementations.SystemManager`. Assembles the
//! [`SystemInfo`]/[`PublicSystemInfo`] responses, reports storage usage, and
//! drives the restart/shutdown lifecycle.
//!
//! Collaborators, and how the C# constructor arguments map here:
//! - `IServerApplicationHost` → the injected [`ServerApplicationHost`]
//!   (`Version`, friendly name, ports, smart API URL, pending-restart flag).
//! - `IServerConfigurationManager` → the injected [`ServerConfigurationManager`]
//!   (startup-wizard flag, cast-receiver apps, transcode temp path).
//! - `IServerApplicationPaths` → the concrete
//!   [`FerrofinServerApplicationPaths`](crate::app_paths::FerrofinServerApplicationPaths)
//!   (program-data/web/log/cache/metadata folders for storage info).
//! - `IHostApplicationLifetime` → the injected [`LifecycleController`] seam
//!   (`StopApplication`, restart flag). Not a ferrofin-traits trait — the C#
//!   equivalent is ASP.NET Core hosting — so it is defined here.
//! - `IInstallationManager` (completed installations) and `IStartupOptions`
//!   (package name), plus the host facts that are *not* on the
//!   [`ServerApplicationHost`] trait (server version string, system id, product
//!   name), are supplied as a plain [`SystemHostFacts`] value — the composition
//!   root fills them.
//! - Storage free-space and the per-library folders come through the
//!   [`StorageProbe`] and [`LibraryStorageProvider`] seams so the manager can be
//!   tested without real disks or a library manager (which is out of this unit's
//!   scope). Defaults probe the real filesystem and report no libraries.
//!
//! Restart/shutdown are best-effort fire-and-forget in C# (`Task.Run` after a
//! 100 ms delay); here they set the restart flag and call `stop`, returning once
//! the request is issued.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::system::{
    FolderStorageInfo, LibraryStorageInfo, PublicSystemInfo, SystemInfo, SystemStorageInfo,
};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::RequestContext;
use ferrofin_traits::system::{ServerApplicationHost, ServerApplicationPaths};

use crate::app_paths::FerrofinServerApplicationPaths;

/// Static host facts not exposed by the [`ServerApplicationHost`] trait.
///
/// Filled by the composition root. Mirrors the C# `IServerApplicationHost`
/// `ApplicationVersionString`/`SystemId`/`Name` and `IStartupOptions.PackageName`
/// plus the completed-installations list from `IInstallationManager`.
#[derive(Debug, Clone, Default)]
pub struct SystemHostFacts {
    /// The server version string (`ApplicationVersionString`).
    pub version: Option<String>,
    /// The product name (`Name`).
    pub product_name: Option<String>,
    /// The stable server id (`SystemId`).
    pub system_id: Option<String>,
    /// The package name (`IStartupOptions.PackageName`).
    pub package_name: Option<String>,
    /// The transcoding temp path override (`EncodingOptions.TranscodingTempPath`,
    /// owned by the encoding layer); `None` falls back to `{cache}/transcodes`.
    pub transcoding_temp_path: Option<String>,
    /// The list of completed installations (`IInstallationManager`).
    pub completed_installations: Vec<ferrofin_model::updates::InstallationInfo>,
}

/// Drives application restart/shutdown.
///
/// Port of the slice of `IHostApplicationLifetime`/`IServerApplicationHost` the
/// system manager uses. `restart` sets the restart flag then stops; `shutdown`
/// just stops.
#[async_trait]
pub trait LifecycleController: Send + Sync {
    /// Requests application stop, restarting afterwards when `restart` is set.
    async fn stop(&self, restart: bool) -> Result<(), ServiceError>;

    /// Whether a restart is currently pending (`HasPendingRestart`).
    fn has_pending_restart(&self) -> bool;

    /// Flags that a restart is required WITHOUT stopping (e.g. a plugin was
    /// installed/uninstalled and activates on the next restart). Surfaces as
    /// `SystemInfo.HasPendingRestart` until the restart happens.
    fn mark_restart_required(&self);

    /// Whether the application is currently shutting down.
    fn is_shutting_down(&self) -> bool;
}

/// Probes a folder's storage usage.
///
/// A seam over the OS free-/used-space query (C# `StorageHelper.GetFreeSpaceOf`)
/// so storage-info assembly is testable. The default [`FsStorageProbe`] queries
/// the real filesystem via `statvfs` on unix.
pub trait StorageProbe: Send + Sync {
    /// Returns the storage info for `path` (resolved path, free/used bytes).
    fn probe(&self, path: &str) -> FolderStorageInfo;
}

/// The default storage probe: reports the path with the free/used bytes of the
/// filesystem it lives on.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsStorageProbe;

impl StorageProbe for FsStorageProbe {
    fn probe(&self, path: &str) -> FolderStorageInfo {
        let resolved_path = std::fs::canonicalize(path)
            .map_or_else(|_| path.to_owned(), |p| p.to_string_lossy().into_owned());
        let (free_space, used_space) = disk_usage(&resolved_path);
        FolderStorageInfo {
            path: path.to_owned(),
            resolved_path,
            free_space,
            used_space,
            ..Default::default()
        }
    }
}

/// Returns `(free_space, used_space)` in bytes for the filesystem containing
/// `path`, mirroring C# `DriveInfo` (free = space available to unprivileged
/// callers; used = total − total-free). Both are `0` if the query fails.
#[cfg(unix)]
fn disk_usage(path: &str) -> (i64, i64) {
    use std::os::unix::ffi::OsStrExt;
    // A configured folder may not exist on disk yet (e.g. the log dir before the
    // first write); walk up to the nearest existing ancestor so we still report
    // the containing filesystem, as C# `DriveInfo` does.
    let mut current = Some(std::path::Path::new(path));
    while let Some(dir) = current {
        let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return (0, 0);
        };
        // SAFETY: `stat` is written by `statvfs` before we read it; `c_path` is a
        // valid NUL-terminated C string that outlives the call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), std::ptr::from_mut(&mut stat)) } == 0 {
            let block = i128::from(stat.f_frsize);
            let bytes = |blocks: u64| -> i64 {
                i64::try_from(i128::from(blocks) * block).unwrap_or(i64::MAX)
            };
            let free = bytes(stat.f_bavail);
            let used = bytes(stat.f_blocks.saturating_sub(stat.f_bfree));
            return (free, used);
        }
        current = dir.parent();
    }
    (0, 0)
}

/// Non-unix fallback: no portable `statvfs`, so usage is unknown.
///
// ponytail: Windows would use `GetDiskFreeSpaceExW`; add it if a Windows build
// ships. The user's server is unix, where this is real.
#[cfg(not(unix))]
fn disk_usage(_path: &str) -> (i64, i64) {
    (0, 0)
}

/// Supplies the per-library storage folders.
///
/// Stands in for `ILibraryManager.GetVirtualFolders()`. Async because the real
/// provider reads the virtual folders from disk; the default [`NoLibraries`]
/// reports none.
#[async_trait]
pub trait LibraryStorageProvider: Send + Sync {
    /// The libraries and their on-disk locations, as `(id, name, paths)`.
    async fn libraries(&self) -> Vec<(uuid::Uuid, String, Vec<String>)>;
}

/// A [`LibraryStorageProvider`] that reports no libraries.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLibraries;

#[async_trait]
impl LibraryStorageProvider for NoLibraries {
    async fn libraries(&self) -> Vec<(uuid::Uuid, String, Vec<String>)> {
        Vec::new()
    }
}

/// The concrete system manager.
pub struct FerrofinSystemManager {
    application_host: Arc<dyn ServerApplicationHost>,
    configuration_manager: Arc<dyn ServerConfigurationManager>,
    paths: Arc<FerrofinServerApplicationPaths>,
    lifecycle: Arc<dyn LifecycleController>,
    storage_probe: Arc<dyn StorageProbe>,
    library_storage: Arc<dyn LibraryStorageProvider>,
    facts: SystemHostFacts,
}

impl std::fmt::Debug for FerrofinSystemManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSystemManager")
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl FerrofinSystemManager {
    /// Creates a system manager with the default real-filesystem storage probe
    /// and no library storage provider.
    #[must_use]
    pub fn new(
        application_host: Arc<dyn ServerApplicationHost>,
        configuration_manager: Arc<dyn ServerConfigurationManager>,
        paths: Arc<FerrofinServerApplicationPaths>,
        lifecycle: Arc<dyn LifecycleController>,
        facts: SystemHostFacts,
    ) -> Self {
        Self {
            application_host,
            configuration_manager,
            paths,
            lifecycle,
            storage_probe: Arc::new(FsStorageProbe),
            library_storage: Arc::new(NoLibraries),
            facts,
        }
    }

    /// Overrides the storage probe (tests inject a fake).
    #[must_use]
    pub fn with_storage_probe(mut self, probe: Arc<dyn StorageProbe>) -> Self {
        self.storage_probe = probe;
        self
    }

    /// Overrides the library storage provider (tests / the composition root
    /// inject a real one).
    #[must_use]
    pub fn with_library_storage(mut self, provider: Arc<dyn LibraryStorageProvider>) -> Self {
        self.library_storage = provider;
        self
    }

    /// The transcode temp path.
    ///
    /// In C# this is `EncodingOptions.TranscodingTempPath` (a separate config
    /// store owned by the encoding layer, out of this unit's scope). The
    /// override is injected via [`SystemHostFacts::transcoding_temp_path`]; absent
    /// that, it defaults to `{cache}/transcodes`, matching Jellyfin's default.
    fn transcode_path(&self) -> String {
        self.facts
            .transcoding_temp_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| {
                std::path::Path::new(&self.paths.cache_path())
                    .join("transcodes")
                    .to_string_lossy()
                    .into_owned()
            })
    }
}

/// The host OS name in Jellyfin's `SystemInfo.OperatingSystem` vocabulary
/// (`Windows` / `OSX` / `BSD` / `Linux`).
///
/// jellyfin-web's directory browser calls `.toLowerCase()` on this field
/// unconditionally when opening the folder picker, so it must never be null —
/// a `None` here throws `undefined.toLowerCase()` and the picker never opens.
fn host_operating_system() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "OSX",
        "windows" => "Windows",
        "freebsd" | "openbsd" | "netbsd" | "dragonfly" => "BSD",
        other => other,
    }
}

#[async_trait]
impl ferrofin_traits::system::SystemManager for FerrofinSystemManager {
    async fn get_system_info(&self, request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        let cfg = self.configuration_manager.configuration().await?;
        let local_address = self.application_host.get_smart_api_url(request).await.ok();
        let transcode = self.transcode_path();
        let internal_metadata = self.paths.internal_metadata_path();

        #[allow(deprecated)]
        Ok(SystemInfo {
            local_address,
            server_name: Some(self.application_host.friendly_name()),
            version: self.facts.version.clone(),
            product_name: self.facts.product_name.clone(),
            id: self.facts.system_id.clone(),
            startup_wizard_completed: Some(cfg.is_startup_wizard_completed),
            package_name: self.facts.package_name.clone(),
            has_pending_restart: self.lifecycle.has_pending_restart(),
            is_shutting_down: self.lifecycle.is_shutting_down(),
            supports_library_monitor: true,
            web_socket_port_number: i32::from(self.application_host.http_port()),
            completed_installations: self.facts.completed_installations.clone(),
            program_data_path: Some(self.paths.program_data_path()),
            web_path: Some(self.paths.web_path()),
            items_by_name_path: Some(internal_metadata.clone()),
            internal_metadata_path: Some(internal_metadata),
            cache_path: Some(self.paths.cache_path()),
            log_path: Some(self.paths.log_directory_path()),
            transcoding_temp_path: Some(transcode),
            cast_receiver_applications: Some(cfg.cast_receiver_applications),
            operating_system: Some(host_operating_system().to_owned()),
            ..Default::default()
        })
    }

    async fn get_public_system_info(
        &self,
        request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        let cfg = self.configuration_manager.configuration().await?;
        let local_address = self.application_host.get_smart_api_url(request).await.ok();
        #[allow(deprecated)]
        Ok(PublicSystemInfo {
            version: self.facts.version.clone(),
            product_name: self.facts.product_name.clone(),
            id: self.facts.system_id.clone(),
            server_name: Some(self.application_host.friendly_name()),
            local_address,
            startup_wizard_completed: Some(cfg.is_startup_wizard_completed),
            operating_system: Some(host_operating_system().to_owned()),
        })
    }

    async fn restart(&self) -> Result<(), ServiceError> {
        self.lifecycle.stop(true).await
    }

    async fn shutdown(&self) -> Result<(), ServiceError> {
        self.lifecycle.stop(false).await
    }

    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError> {
        let probe = |p: String| self.storage_probe.probe(&p);
        let libraries = self
            .library_storage
            .libraries()
            .await
            .into_iter()
            .map(|(id, name, folders)| LibraryStorageInfo {
                id,
                name,
                folders: folders.into_iter().map(&probe).collect(),
            })
            .collect();

        Ok(SystemStorageInfo {
            program_data_folder: probe(self.paths.program_data_path()),
            web_folder: probe(self.paths.web_path()),
            image_cache_folder: probe(self.paths.image_cache_path()),
            cache_folder: probe(self.paths.cache_path()),
            log_folder: probe(self.paths.log_directory_path()),
            internal_metadata_folder: probe(self.paths.internal_metadata_path()),
            transcoding_temp_folder: probe(self.transcode_path()),
            libraries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::app_paths::test_paths;
    use crate::application_host::{FerrofinServerApplicationHost, HostNetworkInfo};
    use crate::configuration_manager::FerrofinServerConfigurationManager;
    use ferrofin_traits::system::SystemManager as _;

    #[derive(Default)]
    struct FakeLifecycle {
        stopped_restart: std::sync::Mutex<Option<bool>>,
        pending: AtomicBool,
    }
    #[async_trait]
    impl LifecycleController for FakeLifecycle {
        async fn stop(&self, restart: bool) -> Result<(), ServiceError> {
            *self.stopped_restart.lock().unwrap() = Some(restart);
            Ok(())
        }
        fn has_pending_restart(&self) -> bool {
            self.pending.load(Ordering::SeqCst)
        }
        fn mark_restart_required(&self) {
            self.pending.store(true, Ordering::SeqCst);
        }
        fn is_shutting_down(&self) -> bool {
            false
        }
    }

    struct SizedProbe;
    impl StorageProbe for SizedProbe {
        fn probe(&self, path: &str) -> FolderStorageInfo {
            FolderStorageInfo {
                path: path.to_owned(),
                resolved_path: path.to_owned(),
                free_space: 100,
                used_space: 50,
                ..Default::default()
            }
        }
    }

    async fn build() -> (FerrofinSystemManager, Arc<FakeLifecycle>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.keep();
        let paths = test_paths(&root);
        let cfg = Arc::new(
            FerrofinServerConfigurationManager::load(Arc::clone(&paths))
                .await
                .expect("config"),
        );
        let host: Arc<dyn ServerApplicationHost> = Arc::new(FerrofinServerApplicationHost::new(
            Arc::clone(&paths),
            Arc::clone(&cfg) as Arc<dyn ServerConfigurationManager>,
            HostNetworkInfo::default(),
            "test-machine",
        ));
        let lifecycle = Arc::new(FakeLifecycle::default());
        let facts = SystemHostFacts {
            version: Some("10.9.0".to_owned()),
            product_name: Some("Ferrofin".to_owned()),
            system_id: Some("abc123".to_owned()),
            package_name: Some("ferrofin-docker".to_owned()),
            transcoding_temp_path: None,
            completed_installations: vec![],
        };
        let mgr = FerrofinSystemManager::new(
            host,
            Arc::clone(&cfg) as Arc<dyn ServerConfigurationManager>,
            paths,
            Arc::clone(&lifecycle) as Arc<dyn LifecycleController>,
            facts,
        )
        .with_storage_probe(Arc::new(SizedProbe));
        (mgr, lifecycle)
    }

    #[tokio::test]
    async fn system_info_carries_host_and_config_facts() {
        let (mgr, _) = build().await;
        let info = mgr
            .get_system_info(&RequestContext::default())
            .await
            .unwrap();
        assert_eq!(info.version.as_deref(), Some("10.9.0"));
        assert_eq!(info.id.as_deref(), Some("abc123"));
        assert_eq!(info.server_name.as_deref(), Some("test-machine"));
        assert_eq!(info.web_socket_port_number, 8096);
        assert!(info.supports_library_monitor);
        assert_eq!(info.startup_wizard_completed, Some(false));
    }

    #[tokio::test]
    async fn public_info_is_the_unauthenticated_subset() {
        let (mgr, _) = build().await;
        let info = mgr
            .get_public_system_info(&RequestContext::default())
            .await
            .unwrap();
        assert_eq!(info.version.as_deref(), Some("10.9.0"));
        assert_eq!(info.product_name.as_deref(), Some("Ferrofin"));
        assert_eq!(info.server_name.as_deref(), Some("test-machine"));
    }

    #[tokio::test]
    async fn restart_and_shutdown_set_the_flag() {
        let (mgr, lifecycle) = build().await;
        mgr.restart().await.unwrap();
        assert_eq!(*lifecycle.stopped_restart.lock().unwrap(), Some(true));
        mgr.shutdown().await.unwrap();
        assert_eq!(*lifecycle.stopped_restart.lock().unwrap(), Some(false));
    }

    #[tokio::test]
    async fn storage_info_probes_every_folder() {
        let (mgr, _) = build().await;
        let storage = mgr.get_system_storage_info().await.unwrap();
        assert_eq!(storage.program_data_folder.free_space, 100);
        assert_eq!(storage.cache_folder.used_space, 50);
        assert!(storage.libraries.is_empty());
        assert!(storage.transcoding_temp_folder.path.ends_with("transcodes"));
    }

    // The real filesystem probe reports the live free/used bytes of the disk the
    // path is on — a booted server no longer shows every folder as 0 bytes.
    #[cfg(unix)]
    #[test]
    fn fs_probe_reports_real_disk_usage() {
        let dir = std::env::temp_dir();
        let info = FsStorageProbe.probe(&dir.to_string_lossy());
        assert!(info.free_space > 0, "free space should be non-zero");
        assert!(info.used_space > 0, "used space should be non-zero");
    }
}
