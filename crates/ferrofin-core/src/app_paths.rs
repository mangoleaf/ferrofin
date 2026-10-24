//! [`FerrofinServerApplicationPaths`] — the concrete [`ServerApplicationPaths`]
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
//!   subfolder. Path resolution here is pure (no I/O): the directories are
//!   created by [`make_sanity_check`](FerrofinServerApplicationPaths::make_sanity_check),
//!   which the composition root runs at startup, as `Program.StartApp` runs
//!   `MakeSanityCheckOrThrow`. This keeps the accessors — every trait method —
//!   free of filesystem side effects and safe to call from tests over a temp
//!   root.
//! - `InternalMetadataPath` and `CachePath` are mutable properties in C# (the
//!   configuration manager rewrites them when configuration changes). They are held
//!   here behind an [`RwLock`] so the shared `Arc<FerrofinServerApplicationPaths>`
//!   can be updated in place by
//!   [`FerrofinServerConfigurationManager`](crate::configuration_manager::FerrofinServerConfigurationManager)
//!   without breaking the object-safe, `&self` [`ServerApplicationPaths`]
//!   contract.
//!
//! The two virtual-path magic strings (`%AppDataPath%`, `%MetadataPath%`) are
//! exposed as associated constants so the application host's
//! [`expand_virtual_path`](ferrofin_traits::system::ServerApplicationHost::expand_virtual_path)
//! and its reverse can share them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ferrofin_traits::system::ServerApplicationPaths;

/// The concrete server application paths, derived from a fixed set of roots.
///
/// Constructed once at startup; cache and metadata roots follow configuration.
/// Other roots remain fixed. The host and managers share an `Arc` to these
/// paths; mutable cache and metadata roots live behind shared locks.
// Every field names a well-known directory, so the shared `_path` suffix is
// intrinsic to the domain, not a naming smell.
#[allow(clippy::struct_field_names)]
pub struct FerrofinServerApplicationPaths {
    program_data_path: PathBuf,
    log_directory_path: PathBuf,
    configuration_directory_path: PathBuf,
    cache_path: RwLock<PathBuf>,
    transcoding_temp_path: RwLock<Option<PathBuf>>,
    web_path: PathBuf,
    data_path: PathBuf,
    root_folder_path: PathBuf,
    default_user_views_path: PathBuf,
    default_internal_metadata_path: PathBuf,
    /// Mutable: rewritten when `Configuration.MetadataPath` changes.
    internal_metadata_path: RwLock<PathBuf>,
    /// The database file the server opened (`{program-data}/ferrofin.db`
    /// unless the composition root adopted a Jellyfin database elsewhere).
    database_path: RwLock<PathBuf>,
}

impl std::fmt::Debug for FerrofinServerApplicationPaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinServerApplicationPaths")
            .field("program_data_path", &self.program_data_path)
            .finish_non_exhaustive()
    }
}

impl FerrofinServerApplicationPaths {
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
            database_path: RwLock::new(program_data_path.join("ferrofin.db")),
            default_internal_metadata_path,
            default_user_views_path,
            root_folder_path,
            data_path,
            log_directory_path: log_directory_path.into(),
            configuration_directory_path: configuration_directory_path.into(),
            cache_path: RwLock::new(cache_path.into()),
            transcoding_temp_path: RwLock::new(None),
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

    /// Publishes a configured cache root. Clearing an override takes effect on restart.
    pub fn set_cache_path(&self, path: Option<&str>) {
        if let Some(path) = path.filter(|p| !p.trim().is_empty()) {
            *self
                .cache_path
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = path.into();
        }
    }

    /// Selects the live transcode root, resetting to the current cache root for null/empty.
    /// Whitespace is a configured path, matching `GetTranscodePath`'s empty check.
    pub fn set_transcode_path(&self, path: Option<&str>) {
        *self
            .transcoding_temp_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            path.filter(|path| !path.is_empty()).map(PathBuf::from);
    }

    /// Prepares a cache root: its `.jellyfin-cache` marker
    /// ([`Self::create_and_check_marker`]) and its `CACHEDIR.TAG`, which tells
    /// backup tools the directory holds cached data (`CreateCacheDirTag`).
    ///
    /// # Errors
    /// As [`Self::create_and_check_marker`], or an I/O error writing the tag,
    /// naming its path.
    pub fn prepare_cache_path(path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        Self::create_and_check_marker(path, "cache")?;
        let tag = path.join("CACHEDIR.TAG");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tag)
        {
            Ok(mut file) => file
                .write_all(
                    b"Signature: 8a477f597d28d172789f06886806bc55\n# Cache directory created by Ferrofin.\n",
                )
                .map_err(|e| with_path("writing", &tag, &e)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tag.is_file() => Ok(()),
            Err(e) => Err(with_path("creating", &tag, &e)),
        }
    }

    /// Records the database file the server actually opened.
    pub fn set_database_path(&self, path: impl Into<PathBuf>) {
        if let Ok(mut guard) = self.database_path.write() {
            *guard = path.into();
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
        PathBuf::from(self.cache_path()).join("images")
    }

    /// A snapshot of the current internal metadata path.
    #[must_use]
    fn internal_metadata_path_buf(&self) -> PathBuf {
        self.internal_metadata_path.read().map_or_else(
            |_| self.default_internal_metadata_path.clone(),
            |g| g.clone(),
        )
    }

    /// Creates the server's directories and checks their markers: port of
    /// `ServerApplicationPaths.MakeSanityCheckOrThrow` and the base
    /// `BaseApplicationPaths.MakeSanityCheckOrThrow`, in their order. Each of
    /// config, log, plugins, program data, cache, data and root gets its
    /// `.jellyfin-<kind>` marker ([`Self::create_and_check_marker`]), and the
    /// cache its `CACHEDIR.TAG` ([`Self::prepare_cache_path`]).
    ///
    /// Idempotent. The configuration manager prepares the metadata directory,
    /// and a cache root a later configuration change selects.
    ///
    /// # Errors
    ///
    /// Returns the first I/O error, naming its path, or the conflicting-marker
    /// error when a directory carries another kind's marker: two server paths
    /// point at one directory.
    pub fn make_sanity_check(&self) -> std::io::Result<()> {
        Self::create_and_check_marker(&self.configuration_directory_path, "config")?;
        Self::create_and_check_marker(&self.log_directory_path, "log")?;
        Self::create_and_check_marker(&self.program_data_path.join("plugins"), "plugin")?;
        Self::create_and_check_marker(&self.program_data_path, "data")?;
        Self::prepare_cache_path(Path::new(&self.cache_path()))?;
        Self::create_and_check_marker(&self.data_path, "data")?;
        Self::create_and_check_marker(&self.root_folder_path, "root")
    }

    /// Port of `BaseApplicationPaths.CreateAndCheckMarker` (not recursive):
    /// creates `path` and its `.jellyfin-{marker}` file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error creating either, naming its path, or
    /// `CheckOrCreateMarker`'s error when `path` already holds a marker file of
    /// another kind (the marker name compares case-insensitively). A directory
    /// that cannot be listed counts as having no markers, as upstream assumes.
    pub fn create_and_check_marker(path: &Path, marker: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(path).map_err(|e| with_path("creating", path, &e))?;
        let marker_name = format!(".jellyfin-{marker}");
        if let Some(other) = other_marker(path, &marker_name) {
            return Err(std::io::Error::other(format!(
                "Expected to find only {marker_name} but found marker for {}.",
                other.display()
            )));
        }
        let marker_path = path.join(&marker_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(with_path("creating", &marker_path, &e)),
        }
    }
}

/// `CheckOrCreateMarker`'s search: the first `.jellyfin-*` file directly in
/// `path` other than `marker_name`. Files are what `Directory.EnumerateFiles`
/// lists: every entry that is not a directory, a symlink judged by its target
/// (a dangling one counts). An error listing the directory finds none.
fn other_marker(path: &Path, marker_name: &str) -> Option<PathBuf> {
    std::fs::read_dir(path).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".jellyfin-") || name.eq_ignore_ascii_case(marker_name) {
            return None;
        }
        let child = entry.path();
        let is_dir = match entry.file_type() {
            Ok(kind) if !kind.is_symlink() => kind.is_dir(),
            _ => child.is_dir(),
        };
        (!is_dir).then_some(child)
    })
}

/// `error` with what was being done to which path, keeping its kind.
fn with_path(step: &str, path: &Path, error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{step} {}: {error}", path.display()))
}

/// Renders a path as a lossy `String` (the trait returns owned strings).
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl ServerApplicationPaths for FerrofinServerApplicationPaths {
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

    fn configuration_directory_path(&self) -> String {
        path_string(&self.configuration_directory_path)
    }

    fn database_path(&self) -> String {
        self.database_path.read().map_or_else(
            |_| path_string(&self.program_data_path.join("ferrofin.db")),
            |g| path_string(&g),
        )
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
        path_string(
            &self
                .cache_path
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn transcode_path(&self) -> String {
        let configured = self
            .transcoding_temp_path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        configured.map_or_else(
            || path_string(&PathBuf::from(self.cache_path()).join("transcodes")),
            |path| path_string(&path),
        )
    }

    fn log_directory_path(&self) -> String {
        path_string(&self.log_directory_path)
    }
}

/// Builds a [`FerrofinServerApplicationPaths`] rooted at `root/{program,log,…}`
/// for tests.
#[cfg(test)]
pub(crate) fn test_paths(root: &Path) -> Arc<FerrofinServerApplicationPaths> {
    Arc::new(FerrofinServerApplicationPaths::new(
        root.join("program"),
        root.join("log"),
        root.join("config"),
        root.join("cache"),
        root.join("web"),
    ))
}

/// Re-exported so callers can name the shared handle type ergonomically.
pub type SharedApplicationPaths = Arc<FerrofinServerApplicationPaths>;

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> FerrofinServerApplicationPaths {
        FerrofinServerApplicationPaths::new(
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
    fn transcode_root_follows_configured_path_and_live_cache_fallback() {
        let paths = paths();
        assert_eq!(paths.transcode_path(), "/var/cache/jf/transcodes");
        paths.set_transcode_path(Some("/mnt/transcode"));
        let operation = paths.transcode_path();
        paths.set_cache_path(Some("/new/cache"));
        assert_eq!(paths.transcode_path(), "/mnt/transcode");
        paths.set_transcode_path(Some("/other/transcode"));
        assert_eq!(operation, "/mnt/transcode");
        assert_eq!(paths.transcode_path(), "/other/transcode");
        paths.set_transcode_path(Some(""));
        assert_eq!(paths.transcode_path(), "/new/cache/transcodes");
        paths.set_transcode_path(Some("   "));
        assert_eq!(paths.transcode_path(), "   ");
        paths.set_transcode_path(None);
        assert_eq!(paths.transcode_path(), "/new/cache/transcodes");
    }

    #[test]
    fn sanity_check_creates_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = FerrofinServerApplicationPaths::new(
            tmp.path().join("program"),
            tmp.path().join("log"),
            tmp.path().join("config"),
            tmp.path().join("cache"),
            tmp.path().join("web"),
        );
        p.make_sanity_check().expect("sanity check");
        for (dir, marker) in [
            ("config", "config"),
            ("log", "log"),
            ("program/plugins", "plugin"),
            ("program", "data"),
            ("cache", "cache"),
            ("program/data", "data"),
            ("program/root", "root"),
        ] {
            assert!(
                tmp.path()
                    .join(dir)
                    .join(format!(".jellyfin-{marker}"))
                    .is_file(),
                "{dir}"
            );
        }
        assert!(tmp.path().join("cache/CACHEDIR.TAG").is_file());
        // Running it again changes nothing and fails nothing.
        p.make_sanity_check().expect("again");
    }

    #[test]
    fn create_and_check_marker_creates_the_directory_and_its_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("a/b");

        FerrofinServerApplicationPaths::create_and_check_marker(&dir, "log").expect("first");
        FerrofinServerApplicationPaths::create_and_check_marker(&dir, "log").expect("again");

        assert!(dir.join(".jellyfin-log").is_file());
    }

    /// `CheckOrCreateMarker` compares marker names case-insensitively, and
    /// only counts files: unrelated files and a directory named like a
    /// marker do not conflict.
    #[test]
    fn create_and_check_marker_accepts_its_own_marker_and_unrelated_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::write(dir.join(".jellyfin-LOG"), "").expect("marker");
        std::fs::write(dir.join("jellyfin.log"), "").expect("file");
        std::fs::create_dir(dir.join(".jellyfin-data")).expect("dir");

        FerrofinServerApplicationPaths::create_and_check_marker(dir, "log").expect("accepted");
    }

    /// `Directory.EnumerateFiles` lists a symlink to a file, and a dangling
    /// one, but not a symlink to a directory.
    #[cfg(unix)]
    #[rstest::rstest]
    #[case::to_a_file(false)]
    #[case::dangling(true)]
    fn create_and_check_marker_counts_a_symlinked_marker(#[case] dangling: bool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("log");
        std::fs::create_dir(&dir).expect("dir");
        let target = tmp.path().join("target");
        if !dangling {
            std::fs::write(&target, "").expect("target");
        }
        std::os::unix::fs::symlink(&target, dir.join(".jellyfin-data")).expect("symlink");

        assert!(FerrofinServerApplicationPaths::create_and_check_marker(&dir, "log").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn create_and_check_marker_skips_a_symlink_to_a_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("log");
        std::fs::create_dir(&dir).expect("dir");
        std::fs::create_dir(tmp.path().join("target")).expect("target");
        std::os::unix::fs::symlink(tmp.path().join("target"), dir.join(".jellyfin-data"))
            .expect("symlink");

        FerrofinServerApplicationPaths::create_and_check_marker(&dir, "log").expect("accepted");
    }

    /// An I/O failure names the path, so a startup error says which
    /// directory is wrong.
    #[test]
    fn create_and_check_marker_names_the_path_it_cannot_create() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked = tmp.path().join("plugins");
        std::fs::write(&blocked, "a file where the directory goes").expect("file");

        let error = FerrofinServerApplicationPaths::create_and_check_marker(&blocked, "plugin")
            .expect_err("refused");

        assert!(
            error
                .to_string()
                .starts_with(&format!("creating {}: ", blocked.display())),
            "{error}"
        );
    }

    #[test]
    fn create_and_check_marker_refuses_another_kinds_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::write(dir.join(".jellyfin-data"), "").expect("marker");

        let error = FerrofinServerApplicationPaths::create_and_check_marker(dir, "config")
            .expect_err("refused");

        assert_eq!(
            error.to_string(),
            format!(
                "Expected to find only .jellyfin-config but found marker for {}.",
                dir.join(".jellyfin-data").display()
            )
        );
        assert!(!dir.join(".jellyfin-config").exists());
    }
}
