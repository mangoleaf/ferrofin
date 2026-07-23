//! [`HermitServerApplicationPaths`] — the concrete [`ServerApplicationPaths`]
//! over a fixed program-data root.
//!
//! Port of `Emby.Server.Implementations.ServerApplicationPaths` and the common
//! members of its base `BaseApplicationPaths`. The C# type derives every
//! well-known directory from five constructor roots (program data, log,
//! configuration, cache, web) using `Path.Combine`; that layout is reproduced
//! here with [`std::path::Path::join`].
//!
//! Two departures from the C#:
//! - The C# constructor eagerly `Directory.CreateDirectory`s the `data`
//!   subfolder. Path resolution here is pure (no I/O), so directory creation is
//!   deferred to [`make_sanity_check`](Self::make_sanity_check), which the host
//!   calls at startup. This keeps the accessors — every trait method — free of
//!   filesystem side effects and safe to call from tests over a temp root.
//! - `InternalMetadataPath` is a *mutable* property in C# (the configuration
//!   manager rewrites it when `Configuration.MetadataPath` changes). It is held
//!   here behind an [`RwLock`] so the shared `Arc<HermitServerApplicationPaths>`
//!   can be updated in place by
//!   [`HermitServerConfigurationManager`](crate::configuration_manager::HermitServerConfigurationManager)
//!   without breaking the object-safe, `&self` [`ServerApplicationPaths`]
//!   contract.
//!
//! The two virtual-path magic strings (`%AppDataPath%`, `%MetadataPath%`) are
//! exposed as associated constants so the application host's
//! [`expand_virtual_path`](hermit_traits::system::ServerApplicationHost::expand_virtual_path)
//! and its reverse can share them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hermit_traits::system::ServerApplicationPaths;

/// The concrete server application paths, derived from a fixed set of roots.
///
/// Constructed once at startup (the roots cannot change while the server runs)
/// and shared as an `Arc` by the host, configuration, path, and system
/// managers. Cloning is cheap: the resolved fields are shared, and the mutable
/// internal-metadata path lives behind a shared lock.
// Every field names a well-known directory, so the shared `_path` suffix is
// intrinsic to the domain, not a naming smell.
#[allow(clippy::struct_field_names)]
pub struct HermitServerApplicationPaths {
    program_data_path: PathBuf,
    log_directory_path: PathBuf,
    configuration_directory_path: PathBuf,
    cache_path: PathBuf,
    web_path: PathBuf,
    data_path: PathBuf,
    root_folder_path: PathBuf,
    default_user_views_path: PathBuf,
    default_internal_metadata_path: PathBuf,
    /// Mutable: rewritten when `Configuration.MetadataPath` changes.
    internal_metadata_path: RwLock<PathBuf>,
}

impl std::fmt::Debug for HermitServerApplicationPaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitServerApplicationPaths")
            .field("program_data_path", &self.program_data_path)
            .finish_non_exhaustive()
    }
}

impl HermitServerApplicationPaths {
    /// The virtual placeholder for the data path (`VirtualDataPath` in C#).
    pub const VIRTUAL_DATA_PATH: &'static str = "%AppDataPath%";

    /// The virtual placeholder for the internal metadata path
    /// (`VirtualInternalMetadataPath` in C#).
    pub const VIRTUAL_INTERNAL_METADATA_PATH: &'static str = "%MetadataPath%";

    /// Creates the application paths from their five roots.
    ///
    /// Mirrors the `ServerApplicationPaths` constructor: `data`, `root`,
    /// `default`, and `metadata` are derived from the program-data path, and the
    /// internal metadata path starts equal to the default (the configuration
    /// manager overrides it if `Configuration.MetadataPath` is set).
    #[must_use]
    pub fn new(
        program_data_path: impl Into<PathBuf>,
        log_directory_path: impl Into<PathBuf>,
        configuration_directory_path: impl Into<PathBuf>,
        cache_path: impl Into<PathBuf>,
        web_path: impl Into<PathBuf>,
    ) -> Self {
        let program_data_path = program_data_path.into();
        let data_path = program_data_path.join("data");
        let root_folder_path = program_data_path.join("root");
        let default_user_views_path = root_folder_path.join("default");
        let default_internal_metadata_path = program_data_path.join("metadata");
        Self {
            internal_metadata_path: RwLock::new(default_internal_metadata_path.clone()),
            default_internal_metadata_path,
            default_user_views_path,
            root_folder_path,
            data_path,
            log_directory_path: log_directory_path.into(),
            configuration_directory_path: configuration_directory_path.into(),
            cache_path: cache_path.into(),
            web_path: web_path.into(),
            program_data_path,
        }
    }

    /// The default internal metadata path (`{program-data}/metadata`), used when
    /// `Configuration.MetadataPath` is blank.
    #[must_use]
    pub fn default_internal_metadata_path(&self) -> String {
        path_string(&self.default_internal_metadata_path)
    }

    /// Overrides the internal metadata path, or resets it to the default when
    /// `path` is `None`/blank.
    ///
    /// Called by the configuration manager whenever the configuration is
    /// (re)loaded, reproducing C# `ServerConfigurationManager.UpdateMetadataPath`.
    pub fn set_internal_metadata_path(&self, path: Option<&str>) {
        let resolved = match path {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => self.default_internal_metadata_path.clone(),
        };
        if let Ok(mut guard) = self.internal_metadata_path.write() {
            *guard = resolved;
        }
    }

    /// The plugins directory (`{program-data}/plugins`).
    #[must_use]
    pub fn plugins_path(&self) -> String {
        path_string(&self.program_data_path.join("plugins"))
    }

    /// The trickplay directory (`{data}/trickplay`).
    #[must_use]
    pub fn trickplay_path(&self) -> String {
        path_string(&self.data_path.join("trickplay"))
    }

    /// The image cache directory as a [`PathBuf`].
    #[must_use]
    fn image_cache_path_buf(&self) -> PathBuf {
        self.cache_path.join("images")
    }

    /// A snapshot of the current internal metadata path.
    #[must_use]
    fn internal_metadata_path_buf(&self) -> PathBuf {
        self.internal_metadata_path.read().map_or_else(
            |_| self.default_internal_metadata_path.clone(),
            |g| g.clone(),
        )
    }

    /// Creates the well-known base directories, mirroring
    /// `BaseApplicationPaths.MakeSanityCheckOrThrow`.
    ///
    /// Idempotent (`create_dir_all`). The C# variant additionally drops
    /// `.jellyfin-*` marker and `CACHEDIR.TAG` files; those are diagnostic only
    /// and are not reproduced here.
    ///
    /// # Errors
    ///
    /// Returns the first I/O error creating any base directory.
    pub fn make_sanity_check(&self) -> std::io::Result<()> {
        for dir in [
            &self.configuration_directory_path,
            &self.log_directory_path,
            &self.program_data_path,
            &self.cache_path,
            &self.data_path,
            &self.root_folder_path,
            &self.internal_metadata_path_buf(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Renders a path as a lossy `String` (the trait returns owned strings).
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl ServerApplicationPaths for HermitServerApplicationPaths {
    fn root_folder_path(&self) -> String {
        path_string(&self.root_folder_path)
    }

    fn default_user_views_path(&self) -> String {
        path_string(&self.default_user_views_path)
    }

    fn people_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("People"))
    }

    fn genre_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("Genre"))
    }

    fn music_genre_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("MusicGenre"))
    }

    fn studio_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("Studio"))
    }

    fn year_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("Year"))
    }

    fn artists_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf().join("artists"))
    }

    fn user_configuration_directory_path(&self) -> String {
        path_string(&self.configuration_directory_path.join("users"))
    }

    fn internal_metadata_path(&self) -> String {
        path_string(&self.internal_metadata_path_buf())
    }

    fn program_data_path(&self) -> String {
        path_string(&self.program_data_path)
    }

    fn web_path(&self) -> String {
        path_string(&self.web_path)
    }

    fn data_path(&self) -> String {
        path_string(&self.data_path)
    }

    fn image_cache_path(&self) -> String {
        path_string(&self.image_cache_path_buf())
    }

    fn cache_path(&self) -> String {
        path_string(&self.cache_path)
    }

    fn log_directory_path(&self) -> String {
        path_string(&self.log_directory_path)
    }
}

/// Builds a [`HermitServerApplicationPaths`] rooted at `root/{program,log,…}`
/// for tests.
#[cfg(test)]
pub(crate) fn test_paths(root: &Path) -> Arc<HermitServerApplicationPaths> {
    Arc::new(HermitServerApplicationPaths::new(
        root.join("program"),
        root.join("log"),
        root.join("config"),
        root.join("cache"),
        root.join("web"),
    ))
}

/// Re-exported so callers can name the shared handle type ergonomically.
pub type SharedApplicationPaths = Arc<HermitServerApplicationPaths>;

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> HermitServerApplicationPaths {
        HermitServerApplicationPaths::new(
            "/srv/jellyfin",
            "/var/log/jf",
            "/etc/jf",
            "/var/cache/jf",
            "/usr/share/jf-web",
        )
    }

    #[test]
    fn derives_program_data_children() {
        let p = paths();
        assert_eq!(p.data_path(), "/srv/jellyfin/data");
        assert_eq!(p.root_folder_path(), "/srv/jellyfin/root");
        assert_eq!(p.default_user_views_path(), "/srv/jellyfin/root/default");
        assert_eq!(p.image_cache_path(), "/var/cache/jf/images");
        assert_eq!(p.user_configuration_directory_path(), "/etc/jf/users");
        assert_eq!(p.plugins_path(), "/srv/jellyfin/plugins");
        assert_eq!(p.trickplay_path(), "/srv/jellyfin/data/trickplay");
    }

    #[test]
    fn internal_metadata_defaults_then_overrides() {
        let p = paths();
        assert_eq!(p.internal_metadata_path(), "/srv/jellyfin/metadata");
        assert_eq!(p.people_path(), "/srv/jellyfin/metadata/People");

        p.set_internal_metadata_path(Some("/mnt/meta"));
        assert_eq!(p.internal_metadata_path(), "/mnt/meta");
        assert_eq!(p.people_path(), "/mnt/meta/People");
        assert_eq!(p.year_path(), "/mnt/meta/Year");

        // Blank resets to the default.
        p.set_internal_metadata_path(Some("   "));
        assert_eq!(p.internal_metadata_path(), "/srv/jellyfin/metadata");
        p.set_internal_metadata_path(Some("/mnt/meta"));
        p.set_internal_metadata_path(None);
        assert_eq!(p.internal_metadata_path(), "/srv/jellyfin/metadata");
    }

    #[test]
    fn sanity_check_creates_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = HermitServerApplicationPaths::new(
            tmp.path().join("program"),
            tmp.path().join("log"),
            tmp.path().join("config"),
            tmp.path().join("cache"),
            tmp.path().join("web"),
        );
        p.make_sanity_check().expect("sanity check");
        assert!(tmp.path().join("program/data").is_dir());
        assert!(tmp.path().join("program/root").is_dir());
        assert!(tmp.path().join("program/metadata").is_dir());
        assert!(tmp.path().join("config").is_dir());
    }
}
