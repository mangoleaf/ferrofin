//! The backup engine behind `BackupController`: create, list, inspect, and
//! restore server backups.
//!
//! Port of Jellyfin's `BackupService`. A backup is a single `.zip` under
//! `{data}/backups/` bundling the configuration directory, the SQLite database
//! file (`Options.Database`) and, per the posted `BackupOptionsDto`, the
//! internal metadata, trickplay and subtitle-cache trees, plus a
//! `manifest.json` describing it. [`FerrofinBackupService`] serves the
//! `/Backup*` routes through the [`BackupService`] trait:
//!
//! - list the available backups' manifests;
//! - read one backup's manifest;
//! - write a new backup and return its manifest;
//! - schedule an archive for restore and restart; the next boot extracts it
//!   over the live tree before the database is opened
//!   ([`apply_pending_restore`], which the composition root calls), exactly
//!   as Jellyfin's `ScheduleRestoreAndRestartServer` does. Restoring
//!   in-process would overwrite an open SQLite file.
//!
//! One divergence beyond upstream: an archive is private (`0600` on Unix), and
//! so is every file a restore puts back under the configuration tree, new or
//! replaced. Upstream's `BackupService` writes the archive with
//! `File.OpenWrite` (the umask mode), and Jellyfin writes its configuration
//! files with the umask mode, so nothing it archives or restores is narrower.
//! Ferrofin writes the configuration tree `0600`, since it holds plugin
//! secrets (LDAP-Auth's bind password), and a backup must not undo that
//! (gl-1233). The other trees keep the umask mode, as do the database, its
//! snapshot and its restored file, whose on-disk mode Ferrofin does not narrow
//! either.

use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::backup::{BackupManifest, BackupOptions};
use ferrofin_traits::backup::{BackupError, BackupService};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::system::{ServerApplicationPaths, SystemManager};

/// The SQLite database file name inside the data directory.
const DB_FILE_NAME: &str = "ferrofin.db";

/// The version of Ferrofin's archive layout, reported as `BackupEngineVersion`.
/// Deliberately not Jellyfin's `0.2.0`: that engine serialises each database
/// table to JSON, while Ferrofin archives the SQLite file itself, so the two
/// archive formats are not interchangeable and must not claim to be.
const BACKUP_ENGINE_VERSION: &str = "1.0.0";

/// The marker Restore leaves next to the archives naming the one to apply on
/// the next boot (Jellyfin keeps the same intent in `RestoreBackupPath`).
const RESTORE_PENDING_FILE: &str = "restore-pending";

/// The on-disk roots of everything an archive holds: the database file
/// (archived under `data/`), the scheduled tasks' last results, and the
/// trees, each archived under its own prefix. `config`, the library root and
/// the box-set and playlist folders are always included, as upstream's
/// `CreateBackupAsync` always copies `Root`, `Data/collections`,
/// `Data/playlists` and `Data/ScheduledTasks`; the others follow
/// `BackupOptions`, the default metadata folder going with the metadata when
/// it is a folder of its own (upstream's `metadata-default`). Built from the
/// live application paths (so a configured `FERROFIN_CONFIG_DIR` /
/// `MetadataPath` is honoured, as Jellyfin archives
/// `ConfigurationDirectoryPath` / `InternalMetadataPath`), or from the default
/// layout at boot, before any configuration is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRoots {
    /// The program-data root (the backups directory hangs off its `data/`).
    pub program_data: PathBuf,
    /// The database file the server opened (archived as `data/ferrofin.db`
    /// whatever its on-disk name — an adopted `jellyfin.db` included).
    pub database: PathBuf,
    /// The configuration directory (`config/` in the archive).
    pub config: PathBuf,
    /// The internal metadata directory (`metadata/`).
    pub metadata: PathBuf,
    /// The default internal metadata directory (`metadata-default/`),
    /// archived beside [`metadata`](Self::metadata) only when that is
    /// configured elsewhere: the default location may still hold data.
    pub metadata_default: PathBuf,
    /// The trickplay directory (`trickplay/`).
    pub trickplay: PathBuf,
    /// The subtitle cache directory (`subtitles/`).
    pub subtitles: PathBuf,
    /// The library root (`root/`): each library's folder with its `.mblink`
    /// locations and `options.json` (upstream's `Root`).
    pub root: PathBuf,
    /// The box-set folders (`data/collections/`).
    pub collections: PathBuf,
    /// The playlist folders (`data/playlists/`).
    pub playlists: PathBuf,
    /// The scheduled tasks' last results (`TASK_RESULTS_ENTRY`; upstream's
    /// `Data/ScheduledTasks`).
    pub task_results: PathBuf,
}

impl TreeRoots {
    /// The roots the running server actually uses.
    fn from_paths(paths: &dyn ferrofin_traits::system::ServerApplicationPaths) -> Self {
        let data = PathBuf::from(paths.data_path());
        Self {
            program_data: PathBuf::from(paths.program_data_path()),
            database: PathBuf::from(paths.database_path()),
            config: PathBuf::from(paths.configuration_directory_path()),
            metadata: PathBuf::from(paths.internal_metadata_path()),
            metadata_default: PathBuf::from(paths.default_internal_metadata_path()),
            trickplay: data.join("trickplay"),
            subtitles: data.join("subtitles"),
            root: PathBuf::from(paths.root_folder_path()),
            collections: data.join("collections"),
            playlists: data.join("playlists"),
            task_results: PathBuf::from(paths.program_data_path()).join("task_results.json"),
        }
    }

    /// The default layout under `program_data` with the given configuration
    /// directory and database file — what the composition root knows before
    /// configuration loads.
    #[must_use]
    pub fn defaults(program_data: &Path, config_dir: &Path, database: &Path) -> Self {
        Self {
            program_data: program_data.to_path_buf(),
            database: database.to_path_buf(),
            config: config_dir.to_path_buf(),
            metadata: program_data.join("metadata"),
            metadata_default: program_data.join("metadata"),
            trickplay: program_data.join("data").join("trickplay"),
            subtitles: program_data.join("data").join("subtitles"),
            root: program_data.join("root"),
            collections: program_data.join("data").join("collections"),
            playlists: program_data.join("data").join("playlists"),
            task_results: program_data.join("task_results.json"),
        }
    }

    /// (archive prefix, on-disk root) for each tree. `metadata-default` is
    /// one only while the default metadata folder is not the configured one:
    /// upstream compares the two full paths ignoring case, and writes and
    /// restores it only when they differ.
    fn trees(&self) -> impl Iterator<Item = (&'static str, &Path)> {
        let separate_default = Path::new(&self.metadata.to_string_lossy().to_lowercase())
            != Path::new(&self.metadata_default.to_string_lossy().to_lowercase());
        let trees: [(&'static str, &Path); 8] = [
            ("config", &self.config),
            ("metadata", &self.metadata),
            ("metadata-default", &self.metadata_default),
            ("trickplay", &self.trickplay),
            ("subtitles", &self.subtitles),
            ("root", &self.root),
            ("data/collections", &self.collections),
            ("data/playlists", &self.playlists),
        ];
        trees
            .into_iter()
            .filter(move |(prefix, _)| *prefix != "metadata-default" || separate_default)
    }
}

/// Serializes `POST /Backup/Create`.
///
/// Two reasons, either sufficient. A create picks its archive name by looking
/// for a free one ([`write_backup`]), which only holds while no other create
/// is picking at the same time. And each create writes a database-sized
/// snapshot beside its archive, so N concurrent creates need N x database-size
/// of free disk on top of the archives.
///
/// Before the create moved onto the blocking pool the runtime's worker count
/// bounded that implicitly; dispatching to a 512-thread pool removed the bound,
/// so it is made explicit here. A caller that loses the race gets the `503` the
/// contract already documents for this operation rather than queueing. Holding
/// the only permit is also what lets a create sweep the scratch files it finds
/// ([`sweep_scratch_files`]): no other create can be writing them.
static BACKUP_IN_FLIGHT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The subdirectory of the data path that holds the backup archives.
const BACKUPS_DIR: &str = "backups";

/// The name prefix of the database snapshot a create zips into its archive.
const SNAPSHOT_PREFIX: &str = ".snapshot-";

/// The name suffix of an archive still being written. Not a `.zip`, so the
/// listing never shows it.
const PARTIAL_SUFFIX: &str = ".partial";

/// The least free space, in bytes, the backups directory must have for
/// `POST /Backup/Create` to start: `BackupService.CreateBackupAsync`'s
/// `FiveGigabyte`, copied verbatim (it is 5 bytes short of 5 GiB).
///
/// A build that cannot read free space (non-unix: `free_space_of` reports
/// `-1`) refuses every create, as upstream's catch arm would.
///
/// FLAGGED as a candidate setting — plausible range 1 GiB…10 GiB, default
/// upstream's.
const BACKUP_MIN_FREE_SPACE: i64 = 5_368_709_115;

/// The archive entry holding the writer's [`ferrofin_db::schema_head`]. It
/// sits beside `manifest.json`, which keeps upstream's `BackupManifestDto`
/// shape. An archive without it predates the entry, so it is no newer than
/// any build that reads it.
const SCHEMA_HEAD_ENTRY: &str = "ferrofin-schema-head";

/// The archive entry holding the scheduled tasks' last results, always
/// archived (upstream's `Data/ScheduledTasks`).
const TASK_RESULTS_ENTRY: &str = "data/task_results.json";

/// Whether the tree stored under `prefix` is selected by `options`.
fn includes(options: &BackupOptions, prefix: &str) -> bool {
    match prefix {
        "config" | "root" | "data/collections" | "data/playlists" => true,
        "metadata" | "metadata-default" => options.metadata,
        "trickplay" => options.trickplay,
        "subtitles" => options.subtitles,
        _ => false,
    }
}

/// Maps an I/O / zip failure onto a backend error (a `500`).
fn io_err(context: &str, e: &impl std::fmt::Display) -> BackupError {
    ServiceError::backend(format!("{context}: {e}")).into()
}

/// Whether a manifest names an archive THIS engine can restore: its own layout
/// version, or a legacy manifest from before the field existed (same layout).
/// A Jellyfin `0.2.0` archive — which shares the backups directory after a
/// drop-in adoption — is refused, as Jellyfin refuses foreign versions.
fn restorable(manifest: &BackupManifest) -> bool {
    manifest.backup_engine_version.is_empty()
        || manifest.backup_engine_version == BACKUP_ENGINE_VERSION
}

/// Why `archive` cannot be restored, or `None` when it can: a foreign or
/// unreadable archive, or one written by a newer Ferrofin. Upstream refuses a
/// newer `ServerVersion`; a Ferrofin build's is a release or `git describe`
/// string, which does not order, so the newer-build test compares the recorded
/// schema head (its database would not open here) instead.
fn restore_refusal(archive: &Path) -> Option<String> {
    let name = archive.file_name().map_or_else(
        || archive.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if !read_manifest(archive).is_ok_and(|m| m.is_some_and(|m| restorable(&m))) {
        return Some(format!(
            "backup {name} is not a Ferrofin {BACKUP_ENGINE_VERSION} archive"
        ));
    }
    let head = ferrofin_db::schema_head();
    match read_schema_head(archive) {
        Ok(Some(written)) if written > head => Some(format!(
            "backup {name} was written by a newer version of Ferrofin (database schema \
             {written}, this version knows up to {head}) and cannot be restored by this version"
        )),
        Ok(_) => None,
        Err(reason) => Some(format!("backup {name} cannot be restored: {reason}")),
    }
}

/// The [`SCHEMA_HEAD_ENTRY`] recorded in one archive: `Ok(None)` when it has
/// none (it predates the entry), `Err` when it has one that cannot be read as
/// a version. That case fails closed: it might come from a newer build.
fn read_schema_head(archive: &Path) -> Result<Option<i64>, String> {
    let unreadable = |e: &dyn std::fmt::Display| format!("unreadable {SCHEMA_HEAD_ENTRY}: {e}");
    let file = std::fs::File::open(archive).map_err(|e| unreadable(&e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| unreadable(&e))?;
    let mut entry = match zip.by_name(SCHEMA_HEAD_ENTRY) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(unreadable(&e)),
    };
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|e| unreadable(&e))?;
    text.trim().parse().map(Some).map_err(|e| unreadable(&e))
}

/// Reads the `manifest.json` embedded in one archive: `Ok(None)` when it has
/// none (upstream's `GetManifest` returns null), `Err` when the archive or its
/// manifest cannot be read (where upstream throws).
fn read_manifest(archive_path: &Path) -> Result<Option<BackupManifest>, String> {
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut entry = match zip.by_name("manifest.json") {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut buf = String::new();
    entry.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    let mut manifest: BackupManifest = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
    // Trust the on-disk location over whatever the manifest recorded.
    manifest.path = archive_path.to_string_lossy().into_owned();
    Ok(Some(manifest))
}

/// [`read_manifest`], with an archive that cannot be read logged and treated
/// as one without a manifest, as `GetBackupManifest` and `EnumerateBackups`
/// do ("Tried to load manifest from archive … but failed").
fn manifest_or_warn(archive: &Path) -> Option<BackupManifest> {
    read_manifest(archive).unwrap_or_else(|error| {
        tracing::warn!(
            path = %archive.display(),
            %error,
            "could not read a backup archive's manifest"
        );
        None
    })
}

/// The manifest of every `.zip` in `dir`, in the order the directory lists
/// them: `EnumerateBackups` over `Directory.EnumerateFiles(path, "*.zip")`,
/// so files only, the extension matched case-sensitively, and unsorted. A
/// missing directory is empty, one that cannot be read an error (upstream
/// throws), and an archive without a readable manifest is left out. Pure over
/// the path so it is unit-testable and shared by the service.
fn list_backups_in(dir: &Path) -> std::io::Result<Vec<BackupManifest>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let unreadable =
        |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    let mut manifests = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(unreadable)? {
        let path = entry.map_err(unreadable)?.path();
        if path.as_os_str().as_encoded_bytes().ends_with(b".zip") && path.is_file() {
            manifests.extend(manifest_or_warn(&path));
        }
    }
    Ok(manifests)
}

/// The file name `BackupController.SanitizePath` keeps of a requested archive:
/// `Path.GetFileName(Path.GetFullPath(path))`. The value is resolved lexically
/// against the working directory, as .NET does on Unix, and its last component
/// kept: empty when it ends in a separator, a directory's own name for `.` or
/// `..`. It never holds a separator or `..`, so joining it to the backups
/// directory stays inside it.
fn sanitized_file_name(path: &str) -> String {
    if path.ends_with('/') {
        return String::new();
    }
    let mut full = std::env::current_dir().unwrap_or_default();
    for part in Path::new(path).components() {
        match part {
            Component::RootDir => full = PathBuf::from("/"),
            Component::ParentDir => {
                full.pop();
            }
            Component::Normal(name) => full.push(name),
            Component::CurDir | Component::Prefix(_) => {}
        }
    }
    full.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Recursively adds a directory's files to the zip under `prefix/…`. A file
/// that cannot be read, or whose name is not UTF-8, fails the whole archive
/// with its path in the error: upstream's `CreateEntryFromFile` throws, and a
/// backup must never be reported complete with a file left out.
fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Path,
    prefix: &str,
    options: zip::write::SimpleFileOptions,
) -> std::io::Result<()> {
    let at = |path: &Path, e: std::io::Error| {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    };
    for entry in std::fs::read_dir(dir).map_err(|e| at(dir, e))? {
        let path = entry.map_err(|e| at(dir, e))?.path();
        let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            let not_utf8 = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the file name is not UTF-8",
            );
            at(&path, not_utf8)
        })?;
        let zip_path = format!("{prefix}/{name}");
        if path.is_dir() {
            add_dir_to_zip(zip, &path, &zip_path, options)?;
        } else {
            let mut file = std::fs::File::open(&path).map_err(|e| at(&path, e))?;
            zip.start_file(zip_path, options)?;
            std::io::copy(&mut file, zip).map_err(|e| at(&path, e))?;
        }
    }
    Ok(())
}

/// The [`BackupService`] over the live application paths: a configured
/// `FERROFIN_CONFIG_DIR` / `MetadataPath` is honoured, as Jellyfin archives
/// `ConfigurationDirectoryPath` / `InternalMetadataPath`.
#[derive(Clone)]
pub struct FerrofinBackupService {
    /// The application paths the archives are written from and to.
    paths: Arc<dyn ServerApplicationPaths>,
    /// Whether a library scan runs (`IsScanRunning`), which refuses a create.
    library: Arc<dyn LibraryManager>,
    /// The backups directory's free space, the database snapshot and the
    /// restart a scheduled restore asks for.
    system: Arc<dyn SystemManager>,
    /// The release recorded as each manifest's `ServerVersion`.
    server_version: &'static str,
}

impl FerrofinBackupService {
    /// A service over `paths` whose manifests record `server_version`.
    #[must_use]
    pub fn new(
        paths: Arc<dyn ServerApplicationPaths>,
        library: Arc<dyn LibraryManager>,
        system: Arc<dyn SystemManager>,
        server_version: &'static str,
    ) -> Self {
        Self {
            paths,
            library,
            system,
            server_version,
        }
    }

    /// The directory holding the archives (`{data}/backups`).
    fn backups_dir(&self) -> PathBuf {
        Path::new(&self.paths.data_path()).join(BACKUPS_DIR)
    }

    /// The body of [`BackupService::create_backup`], run while it holds
    /// [`BACKUP_IN_FLIGHT`].
    async fn run_create(&self, options: BackupOptions) -> Result<BackupManifest, BackupError> {
        // Upstream refuses while a library scan runs (`IsScanRunning`): a backup
        // contends with the scan's writes and could capture the library half
        // written. Its `InvalidOperationException` is a `500`.
        if self.library.is_scan_running() {
            return Err(ServiceError::backend(
                "cannot create a backup while a library scan is running; try again once the \
                 scan has finished",
            )
            .into());
        }
        let roots = TreeRoots::from_paths(self.paths.as_ref());
        let dir = self.backups_dir();
        let now = Utc::now();
        let prepared = dir.clone();
        blocking(move || {
            std::fs::create_dir_all(&prepared)?;
            sweep_scratch_files(&prepared);
            Ok(())
        })
        .await
        .map_err(|e| io_err("create backup", &e))?;
        // Upstream's free-space floor, checked once the directory exists; free
        // space that cannot be read (`-1`) is below it, as upstream.
        let free_space = self.system.free_space_of(&dir).await;
        if free_space < BACKUP_MIN_FREE_SPACE {
            let message = if free_space < 0 {
                format!(
                    "the free space of the backup directory '{}' could not be read; cannot \
                     create backup",
                    dir.display()
                )
            } else {
                format!(
                    "the backup directory '{}' has {free_space} bytes free, under the \
                     {BACKUP_MIN_FREE_SPACE} a backup needs; cannot create backup",
                    dir.display()
                )
            };
            return Err(ServiceError::backend(message).into());
        }
        // The database goes into the archive from a consistent `VACUUM INTO`
        // snapshot, never from the live WAL-mode file (a checkpoint can tear a
        // file copy mid-read). The snapshot lives beside the archive until zipped,
        // and goes with the job that zips it whatever becomes of the create.
        let snapshot = options
            .database
            .then(|| ScratchFile(dir.join(format!("{SNAPSHOT_PREFIX}{}.db", now.timestamp()))));
        if let Some(snapshot) = &snapshot {
            self.system.snapshot_database(&snapshot.0).await?;
        }
        // Deflating the SQLite snapshot (tens to hundreds of MB) plus the whole
        // config tree is seconds of CPU + disk: it must not run on a runtime worker.
        let server_version = self.server_version;
        blocking(move || {
            let snapshot = snapshot.as_ref().map(|s| s.0.as_path());
            write_backup(&dir, &roots, options, now, snapshot, server_version)
        })
        .await
        .map_err(|e| io_err("create backup", &e))
    }
}

#[async_trait]
impl BackupService for FerrofinBackupService {
    /// Port of `BackupController.ListBackups`: reads every `.zip` under the
    /// backups directory and returns its manifest. A missing directory is an
    /// empty list.
    async fn list_backups(&self) -> Result<Vec<BackupManifest>, BackupError> {
        // Opening every retained archive and inflating its manifest is blocking file
        // I/O, unbounded in the number of backups kept and on possibly-network
        // storage — the same reason `create_backup` does not run inline.
        let dir = self.backups_dir();
        blocking(move || list_backups_in(&dir))
            .await
            .map_err(|e| io_err("list backups", &e))
    }

    /// Port of `BackupController.GetBackup`: `path` is reduced to a file name in
    /// the backups directory ([`sanitized_file_name`]), not found when that is
    /// not a file and `None` when the file carries no readable manifest.
    async fn get_backup_manifest(&self, path: &str) -> Result<Option<BackupManifest>, BackupError> {
        let archive = self.backups_dir().join(sanitized_file_name(path));
        // Opening the archive and inflating its manifest is blocking file I/O, as
        // in `list_backups`.
        let found = blocking(move || Ok(archive.is_file().then(|| manifest_or_warn(&archive))))
            .await
            .map_err(|e| io_err("read backup manifest", &e))?;
        found.ok_or_else(|| ServiceError::not_found(format!("backup {path}")).into())
    }

    /// Port of `BackupController.CreateBackup` + `BackupService.CreateBackupAsync`:
    /// zips the SQLite database file and the configuration directory (plus a
    /// `manifest.json`) into a timestamped `.zip` under `{data}/backups/`.
    /// Refused while a library scan runs, and when the backups directory has
    /// less than [`BACKUP_MIN_FREE_SPACE`] free; a create that fails leaves
    /// nothing behind.
    async fn create_backup(&self, options: BackupOptions) -> Result<BackupManifest, BackupError> {
        let Ok(permit) = BACKUP_IN_FLIGHT.try_acquire() else {
            return Err(BackupError::InProgress);
        };
        // The create runs to its end on a task of its own, which holds the permit:
        // a caller that goes away mid-backup (dropping this future) must not free
        // it while the snapshot or the archive is still being written, or the next
        // create would sweep them from under it. Upstream's create runs on
        // regardless of the request too (`CancellationToken.None`), and so, as
        // there, a snapshot that never returns (a stalled filesystem) keeps every
        // later create refused until the server restarts.
        let service = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            service.run_create(options).await
        })
        .await
        .unwrap_or_else(|e| Err(io_err("create backup", &e)))
    }

    /// Port of `BackupController.StartRestoreBackup` +
    /// `BackupService.ScheduleRestoreAndRestartServer`: `archive_file_name` is
    /// reduced to a file name inside the backups directory
    /// ([`sanitized_file_name`]; not found when that is not a file), recorded
    /// in the restore-pending marker, and the server restarts; the next boot
    /// applies it ([`apply_pending_restore`]) before the database is opened.
    async fn schedule_restore(&self, archive_file_name: &str) -> Result<(), BackupError> {
        let requested = archive_file_name.to_owned();
        let name = sanitized_file_name(archive_file_name);
        let dir = self.backups_dir();
        // Checking the archive (two zip reads) and writing the marker is blocking
        // file I/O, as in `list_backups`.
        let refusal = blocking(move || {
            let archive = dir.join(&name);
            if !archive.is_file() {
                let missing = ServiceError::not_found(format!("backup {requested}"));
                return Ok(Some(BackupError::from(missing)));
            }
            // Refuse an archive this engine cannot restore BEFORE scheduling a
            // restart for nothing (a Jellyfin archive in a shared backups dir, a
            // stray zip, an archive from a newer Ferrofin).
            if let Some(reason) = restore_refusal(&archive) {
                return Ok(Some(BackupError::Unrestorable(reason)));
            }
            std::fs::write(dir.join(RESTORE_PENDING_FILE), &name)?;
            Ok(None)
        })
        .await
        .map_err(|e| io_err("schedule restore", &e))?;
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        self.system.restart().await?;
        Ok(())
    }
}

/// A create's scratch file — the database snapshot, the archive being
/// written — or a restored file not moved into place yet, removed when
/// dropped, so no error path and no panic leaves it behind. Dropped once the
/// file is in place, it finds nothing to remove.
struct ScratchFile(PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                let path = self.0.display();
                tracing::warn!(%path, %error, "could not remove a scratch file");
            }
            _ => {}
        }
    }
}

/// Removes the scratch files an interrupted create left in `dir` (the server
/// stopped between the snapshot and the finished archive): the snapshots and
/// the archives still being written. Nothing else is touched. Best effort: a
/// file that cannot be removed only stays where it was.
///
/// Only sound under [`BACKUP_IN_FLIGHT`], which no other create holds: a
/// create keeps it until its job has run, even when its caller went away.
fn sweep_scratch_files(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let scratch = (name.starts_with(SNAPSHOT_PREFIX)
            && Path::new(name)
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("db")))
            || (name.starts_with("ferrofin-backup-") && name.ends_with(PARTIAL_SUFFIX));
        if scratch && entry.file_type().is_ok_and(|t| t.is_file()) {
            let path = entry.path();
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not remove a stale backup scratch file"
                );
            }
        }
    }
}

/// Runs a blocking filesystem job on tokio's blocking pool, mapping a lost pool
/// task to an `io::Error` so callers keep one error type.
async fn blocking<T, F>(job: F) -> std::io::Result<T>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Writes a backup archive (database + `config/` tree + the selected trees +
/// `manifest.json` + the [`SCHEMA_HEAD_ENTRY`]) under `backups_dir`, returning
/// the manifest. The database comes only from `snapshot`, a consistent copy
/// the caller produced, consumed (deleted) here.
///
/// The archive is named `ferrofin-backup-%Y%m%d-%H%M%S.zip`, or, when a
/// create in the same second took that name, the first free `-1`, `-2`, …
/// suffix; it never replaces an archive already there. It is written under a
/// scratch name and renamed into place once complete, so a failure, or a
/// crash, never leaves a truncated archive under a backup's name; on an error
/// the scratch file is removed (upstream deletes its partial archive in
/// `CreateBackupAsync`'s catch). Records `server_version` as the manifest's
/// `ServerVersion`. Pure over paths, so it is unit-testable and shared by
/// the service.
fn write_backup(
    backups_dir: &Path,
    roots: &TreeRoots,
    options: BackupOptions,
    now: DateTime<Utc>,
    snapshot: Option<&Path>,
    server_version: &str,
) -> std::io::Result<BackupManifest> {
    std::fs::create_dir_all(backups_dir)?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    let mut file_name = format!("ferrofin-backup-{stamp}.zip");
    for n in 1.. {
        if !backups_dir.join(&file_name).exists() {
            break;
        }
        file_name = format!("ferrofin-backup-{stamp}-{n}.zip");
    }
    let archive_path = backups_dir.join(&file_name);
    let manifest = BackupManifest {
        server_version: server_version.to_owned(),
        backup_engine_version: BACKUP_ENGINE_VERSION.to_owned(),
        date_created: now,
        path: archive_path.to_string_lossy().into_owned(),
        options,
    };
    let partial = ScratchFile(backups_dir.join(format!("{file_name}{PARTIAL_SUFFIX}")));
    write_archive(&partial.0, roots, &manifest, snapshot)?;
    // Creates are serialised, so only a file placed by hand while this one was
    // written can have taken the name. A hard link is a move that never
    // replaces it (dropping `partial` then removes the scratch name); on a
    // filesystem without hard links, a check and a rename are the fallback.
    let taken = || {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} appeared while the backup was written",
                archive_path.display()
            ),
        )
    };
    match std::fs::hard_link(&partial.0, &archive_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken()),
        Err(_) if archive_path.exists() => return Err(taken()),
        Err(_) => std::fs::rename(&partial.0, &archive_path)?,
    }
    Ok(manifest)
}

/// Writes the archive [`write_backup`] describes to `path`.
fn write_archive(
    path: &Path,
    roots: &TreeRoots,
    manifest: &BackupManifest,
    snapshot: Option<&Path>,
) -> std::io::Result<()> {
    // Private (`0600` on Unix): the archive holds the configuration tree,
    // plugin secrets included. Upstream's `File.OpenWrite` takes the umask
    // mode. The rename into place keeps the mode.
    let file = ferrofin_util::file_helper::create_private(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.json", opts)?;
    let manifest_json = serde_json::to_vec_pretty(manifest).unwrap_or_default();
    zip.write_all(&manifest_json)?;
    zip.start_file(SCHEMA_HEAD_ENTRY, opts)?;
    zip.write_all(ferrofin_db::schema_head().to_string().as_bytes())?;

    // The database, only ever from the snapshot: the live WAL-mode file can
    // tear mid-copy, and an archive whose manifest says `Database` must hold it.
    if manifest.options.database {
        let source = snapshot.filter(|p| p.is_file()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the database snapshot for the backup was not written",
            )
        })?;
        let mut file = std::fs::File::open(source)?;
        zip.start_file(format!("data/{DB_FILE_NAME}"), opts)?;
        std::io::copy(&mut file, &mut zip)?;
    }
    // Zipped: the snapshot's disk goes back before the trees are read.
    if let Some(snapshot) = snapshot {
        let _ = std::fs::remove_file(snapshot);
    }

    if let Ok(mut file) = std::fs::File::open(&roots.task_results) {
        zip.start_file(TASK_RESULTS_ENTRY, opts)?;
        std::io::copy(&mut file, &mut zip)?;
    }

    // The trees always archived, plus every optional tree the options select.
    for (prefix, dir) in roots.trees() {
        if includes(&manifest.options, prefix) && dir.is_dir() {
            add_dir_to_zip(&mut zip, dir, prefix, opts)?;
        }
    }

    zip.finish()?;
    Ok(())
}

/// Why a scheduled restore did not apply.
#[derive(Debug)]
pub struct RestoreFailure {
    /// The archive the marker named, when it named one.
    pub archive: Option<PathBuf>,
    /// What went wrong.
    pub error: std::io::Error,
    /// Whether the trees are partly replaced: the extracted files were being
    /// moved into place (`swap_in`) when it failed. The boot that sees it
    /// refuses to start on them (Jellyfin's failed restore throws out of
    /// startup); the marker is gone by then, so the next boot starts on them,
    /// as a restarted Jellyfin does after any failed restore, its restore path
    /// being held in memory only. `false` when nothing was replaced: a
    /// missing, foreign or unreadable archive, or an extraction that failed
    /// before the move (a corrupt entry, a full disk), its files removed.
    pub partially_applied: bool,
}

impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.archive {
            Some(archive) => write!(f, "restoring {}: {}", archive.display(), self.error),
            None => write!(f, "restoring a backup: {}", self.error),
        }
    }
}

impl std::error::Error for RestoreFailure {}

/// Whether `POST /Backup/Restore` has scheduled a restore that the next boot's
/// [`apply_pending_restore`] will apply.
#[must_use]
pub fn restore_pending(roots: &TreeRoots) -> bool {
    roots
        .program_data
        .join("data")
        .join(BACKUPS_DIR)
        .join(RESTORE_PENDING_FILE)
        .is_file()
}

/// Applies a restore scheduled by `POST /Backup/Restore`, if one is pending.
///
/// The composition root calls this at boot BEFORE the database is opened: it
/// reads and removes the marker under `{program_data}/data/backups/`,
/// extracts the named archive over the tree described by `roots`
/// (`stage_archive`, then `swap_in`), and returns the archive path it
/// applied (`None` when nothing was pending). The marker goes first, so a
/// restore that fails, a stale marker naming a missing or foreign archive
/// included, is never retried by every boot. One deliberate difference from
/// Jellyfin, whose pending path is in-memory and dies with the process: the
/// marker is durable, so a restore scheduled right before a crash still
/// applies on the next boot (logged).
///
/// # Errors
///
/// A [`RestoreFailure`]. The marker is removed before anything else; one that
/// cannot be removed refuses the restore. Only a failure while the extracted
/// files are moved into place leaves the trees partly replaced
/// ([`RestoreFailure::partially_applied`]); any earlier one leaves them as
/// they were.
pub fn apply_pending_restore(roots: &TreeRoots) -> Result<Option<PathBuf>, RestoreFailure> {
    let dir = roots.program_data.join("data").join(BACKUPS_DIR);
    let marker = dir.join(RESTORE_PENDING_FILE);
    let Ok(name) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let untouched = |archive: Option<PathBuf>, error: std::io::Error| RestoreFailure {
        archive,
        error,
        partially_applied: false,
    };
    // A marker that stays would apply the backup again at every boot, undoing
    // everything done since.
    std::fs::remove_file(&marker).map_err(|error| {
        let reason = format!(
            "{} could not be removed, so it was not applied",
            marker.display()
        );
        untouched(
            None,
            std::io::Error::new(error.kind(), format!("{reason}: {error}")),
        )
    })?;
    let name = Path::new(name.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            untouched(
                None,
                std::io::Error::other("restore-pending marker names no archive"),
            )
        })?;
    let archive = dir.join(name);
    if let Some(reason) = restore_refusal(&archive) {
        return Err(untouched(Some(archive), std::io::Error::other(reason)));
    }
    let staged =
        stage_archive(&archive, roots).map_err(|error| untouched(Some(archive.clone()), error))?;
    swap_in(staged, &roots.database).map_err(|error| RestoreFailure {
        archive: Some(archive.clone()),
        error,
        partially_applied: true,
    })?;
    Ok(Some(archive))
}

/// The name suffix of a restored file not moved into place yet. It sits
/// beside its destination, so the move is a rename within one directory.
const RESTORE_STAGING_SUFFIX: &str = ".restore-tmp";

/// A restore's extracted files, each named after the path it replaces plus
/// [`RESTORE_STAGING_SUFFIX`]. Dropping it removes every one not moved into
/// place.
type StagedRestore = Vec<ScratchFile>;

/// Where `dest` is extracted before the move.
fn staging_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(RESTORE_STAGING_SUFFIX);
    PathBuf::from(name)
}

/// [`stage_archive`] then [`swap_in`]: the whole restore.
#[cfg(test)]
fn restore_archive(archive_path: &Path, roots: &TreeRoots) -> std::io::Result<()> {
    swap_in(stage_archive(archive_path, roots)?, &roots.database)
}

/// Extracts a backup archive's entries for a restore over `roots`:
/// `data/ferrofin.db` → the database file, and each tree prefix to its on-disk
/// root. Skips the manifest and anything else. Pure over paths so it is
/// unit-testable and shared by the boot hook.
///
/// Nothing is replaced yet: each file is written beside the one it replaces
/// ([`RESTORE_STAGING_SUFFIX`]), for [`swap_in`] to move into place once the
/// whole archive is extracted. A failure here (a corrupt entry, a full disk)
/// removes what it wrote, so the live tree is left as it was, apart from the
/// directories created for new files. Until the move, the restored files
/// take disk beside the files they replace, as the database always did. The
/// move puts a new file in place, with the umask mode and the server's owner
/// outside the configuration tree, and replaces a link rather than writing
/// through it. The staged names are 12 bytes longer, so a file whose name is
/// within 12 bytes of the filesystem's limit fails the restore. The list of
/// staged paths stays in memory until the move, one path per restored file. A
/// process killed during a restore leaves its staged files behind; restoring
/// the same archive again overwrites and moves them.
fn stage_archive(archive_path: &Path, roots: &TreeRoots) -> std::io::Result<StagedRestore> {
    let file = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;
    let db_entry = format!("data/{DB_FILE_NAME}");
    let mut roots = roots.clone();
    let mut staged = StagedRestore::new();
    // Two passes: everything but the metadata trees first, so the restored
    // `system.json` can name where the metadata belongs. The roots known at
    // boot come from the configuration being replaced; a backup whose
    // configuration relocates the metadata tree must put its metadata where
    // that configuration will look for it, not beside the old one, and its
    // `metadata-default` tree only while that is somewhere else ([`TreeRoots::
    // trees`]), never over the metadata just restored.
    for metadata_pass in [false, true] {
        if metadata_pass {
            let system_json = roots.config.join("system.json");
            let staged_json = staging_path(&system_json);
            let restored = if staged.iter().any(|file| file.0 == staged_json) {
                staged_json
            } else {
                system_json
            };
            if let Some(metadata) = metadata_root_in(&restored) {
                roots.metadata = metadata;
            }
        }
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(std::io::Error::other)?;
            let Some(enclosed) = entry.enclosed_name() else {
                continue; // path-traversal guard (zip-slip)
            };
            let metadata_tree =
                enclosed.starts_with("metadata") || enclosed.starts_with("metadata-default");
            if metadata_tree != metadata_pass {
                continue;
            }
            let (dest, private) = if enclosed == Path::new(&db_entry) {
                (roots.database.clone(), false)
            } else if enclosed == Path::new(TASK_RESULTS_ENTRY) {
                (roots.task_results.clone(), false)
            } else if let Some((prefix, root, rest)) = roots.trees().find_map(|(prefix, root)| {
                enclosed
                    .strip_prefix(prefix)
                    .ok()
                    .filter(|rest| !rest.as_os_str().is_empty()) // a file named as the tree
                    .map(|rest| (prefix, root, rest))
            }) {
                (root.join(rest), prefix == "config")
            } else {
                continue;
            };
            if entry.is_dir() {
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = ScratchFile(staging_path(&dest));
            // The configuration tree goes back private (`0600` on Unix), as
            // Ferrofin writes it: it holds plugin secrets. The move replaces a
            // file already there, so it does not keep its mode, as every
            // Ferrofin config save through `atomic_write` does.
            let mut out = if private {
                ferrofin_util::file_helper::create_private(&file.0)?
            } else {
                std::fs::File::create(&file.0)?
            };
            std::io::copy(&mut entry, &mut out)?;
            // On disk before it moves, as the database and Ferrofin's
            // configuration saves always are.
            if private || dest == roots.database {
                out.sync_all()?;
            }
            staged.push(file);
        }
    }
    Ok(staged)
}

/// Moves the files [`stage_archive`] extracted into place, each by a rename
/// over the file it replaces. The database runs in WAL mode, so its
/// `-wal`/`-shm` sidecars are removed just before it moves: a stale WAL left
/// beside a restored database would be replayed over it on open. A failure
/// part-way leaves the trees partly replaced; the files not moved yet are
/// removed.
fn swap_in(staged: StagedRestore, database: &Path) -> std::io::Result<()> {
    for file in staged {
        // Drops the suffix [`staging_path`] appended.
        let dest = file.0.with_extension("");
        if dest == database {
            for sidecar in ["-wal", "-shm"] {
                let mut name = database.as_os_str().to_owned();
                name.push(sidecar);
                match std::fs::remove_file(&name) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        std::fs::rename(&file.0, &dest)?;
    }
    Ok(())
}

/// The `MetadataPath` the restored configuration names, when it names one:
/// read from `system.json` in the (just restored) configuration directory.
#[must_use]
pub fn restored_metadata_root(config_dir: &Path) -> Option<PathBuf> {
    metadata_root_in(&config_dir.join("system.json"))
}

/// The `MetadataPath` the `system.json` at `path` names, when it names one.
fn metadata_root_in(path: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|cfg| cfg.get("MetadataPath")?.as_str().map(str::to_owned))
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    /// Serializes the tests that create a backup through the service, or hold
    /// its permit.
    ///
    /// They contend on [`BACKUP_IN_FLIGHT`], a process-global single permit
    /// one test deliberately holds. `nextest` gives each test its own process
    /// and hides it; plain `cargo test` runs them as threads in one process,
    /// where they fail.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    use super::{
        BACKUP_IN_FLIGHT, BackupError, BackupManifest, BackupOptions, BackupService as _,
        FerrofinBackupService, ServiceError, TreeRoots, add_dir_to_zip, read_manifest,
    };
    use ferrofin_traits::library::LibraryManager;
    use ferrofin_traits::system::SystemManager;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The release the test services and archives record.
    const TEST_VERSION: &str = "9.9.9-test";

    #[test]
    fn zip_a_dir_and_read_back_its_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // A config-like source tree to archive.
        let src = tmp.path().join("config");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("system.json"), b"{}").unwrap();
        std::fs::write(src.join("sub/network.json"), b"{}").unwrap();

        let archive = tmp.path().join("backup.zip");
        let manifest = BackupManifest {
            path: "backup.zip".to_owned(),
            date_created: "2024-01-02T03:04:05Z".parse().unwrap(),
            server_version: "9.9.9".to_owned(),
            backup_engine_version: super::BACKUP_ENGINE_VERSION.to_owned(),
            options: BackupOptions::default(),
        };
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", opts).unwrap();
            zip.write_all(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
            add_dir_to_zip(&mut zip, &src, "config", opts).unwrap();
            zip.finish().unwrap();
        }
        // The manifest round-trips (path re-derived from the on-disk location).
        let read = read_manifest(&archive).unwrap().expect("manifest");
        assert_eq!(read.path, archive.to_string_lossy());
        assert_eq!(read.server_version, "9.9.9");

        // The directory tree was archived under `config/…`.
        let names: Vec<String> = {
            let file = std::fs::File::open(&archive).unwrap();
            let mut zip = zip::ZipArchive::new(file).unwrap();
            (0..zip.len())
                .map(|i| zip.by_index(i).unwrap().name().unwrap().into_owned())
                .collect()
        };
        assert!(
            names.contains(&"config/system.json".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"config/sub/network.json".to_owned()),
            "{names:?}"
        );

        // A non-archive path cannot be read; an archive without a manifest
        // has none.
        assert!(read_manifest(tmp.path()).is_err());
        let bare = tmp.path().join("bare.zip");
        zip::ZipWriter::new(std::fs::File::create(&bare).unwrap())
            .finish()
            .unwrap();
        assert!(read_manifest(&bare).unwrap().is_none());
    }

    /// A file reaches the archive whole, whatever its size against the copy's
    /// buffer, and an empty one stays an entry.
    #[test]
    fn archived_files_keep_every_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let large: Vec<u8> = (0..1_000_003_u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("large.bin"), &large).unwrap();
        std::fs::write(src.join("empty.bin"), b"").unwrap();

        let archive = tmp.path().join("backup.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        add_dir_to_zip(&mut zip, &src, "config", opts).unwrap();
        zip.finish().unwrap();

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
        let mut read = |name: &str| {
            let mut bytes = Vec::new();
            std::io::copy(&mut zip.by_name(name).unwrap(), &mut bytes).unwrap();
            bytes
        };
        assert_eq!(read("config/large.bin"), large);
        assert_eq!(read("config/empty.bin"), Vec::<u8>::new());
    }

    /// A restore that fails while its files are moved into place leaves the
    /// trees partly replaced, which the boot must not paper over, and removes
    /// the files it had not moved yet; a marker that names no archive touches
    /// nothing.
    #[test]
    fn a_restore_failing_mid_swap_is_reported_as_partial() {
        use super::{apply_pending_restore, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("program");
        std::fs::create_dir_all(program_data.join("config")).unwrap();
        std::fs::write(program_data.join("ferrofin.db"), b"DB").unwrap();
        std::fs::write(program_data.join("config/system.json"), b"{}").unwrap();
        let roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        let manifest = write_backup(
            &tmp.path().join("backups"),
            &roots,
            BackupOptions::default(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            Some(&snapshot(tmp.path(), b"DB")),
            TEST_VERSION,
        )
        .unwrap();

        // A directory sits where a file goes: it extracts beside it, but
        // cannot move over it. The database moves first, so the second case
        // fails after it moved and the first before the configuration did.
        let archive = std::path::PathBuf::from(&manifest.path);
        for (case, obstacle, moved) in [
            ("database", "ferrofin.db", "config/system.json"),
            ("config", "config/system.json", "ferrofin.db"),
        ] {
            let boot = tmp.path().join(case);
            let backups = boot.join("data").join(super::BACKUPS_DIR);
            std::fs::create_dir_all(&backups).unwrap();
            std::fs::create_dir_all(boot.join(obstacle)).unwrap();
            std::fs::copy(&archive, backups.join(archive.file_name().unwrap())).unwrap();
            std::fs::write(
                backups.join(super::RESTORE_PENDING_FILE),
                archive.file_name().unwrap().to_str().unwrap(),
            )
            .unwrap();
            let roots = TreeRoots::defaults(&boot, &boot.join("config"), &boot.join("ferrofin.db"));

            let failure = apply_pending_restore(&roots).unwrap_err();
            assert!(failure.partially_applied, "{case}: {failure}");
            assert!(failure.archive.is_some());
            assert_eq!(boot.join(moved).is_file(), case == "config", "{case}");
            assert_eq!(staged_leftovers(&boot), Vec::<std::path::PathBuf>::new());
            // The marker is consumed either way, so a retry cannot loop the boot.
            assert!(!backups.join(super::RESTORE_PENDING_FILE).exists());
        }
        let boot = tmp.path().join("nameless");
        let backups = boot.join("data").join(super::BACKUPS_DIR);
        std::fs::create_dir_all(&backups).unwrap();
        let boot_roots =
            TreeRoots::defaults(&boot, &boot.join("config"), &boot.join("ferrofin.db"));

        // A marker naming no file at all touches nothing.
        std::fs::write(backups.join(super::RESTORE_PENDING_FILE), "").unwrap();
        let nameless = apply_pending_restore(&boot_roots).unwrap_err();
        assert!(!nameless.partially_applied);
        assert!(nameless.archive.is_none());
        assert!(nameless.to_string().starts_with("restoring a backup:"));
    }

    /// The staged files ([`super::RESTORE_STAGING_SUFFIX`]) anywhere under `dir`.
    fn staged_leftovers(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut left = Vec::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path
                    .to_string_lossy()
                    .ends_with(super::RESTORE_STAGING_SUFFIX)
                {
                    left.push(path);
                }
            }
        }
        left
    }

    /// gl-196: a restore that fails part-way through its archive (a corrupt
    /// entry here; a full disk fails the same way) leaves the live tree as it
    /// was and no staged file behind, and reports that it replaced nothing, so
    /// the boot goes on with the data it had. Once with the database entry
    /// failing, once with an entry that comes after the database and the
    /// configuration.
    #[test]
    fn a_restore_failing_mid_extraction_leaves_the_live_tree_untouched() {
        use super::{apply_pending_restore, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        let library = "root/default/Movies/options.json";
        let program_data = tmp.path().join("program");
        std::fs::create_dir_all(program_data.join("config")).unwrap();
        std::fs::write(program_data.join("config/system.json"), b"RESTORED").unwrap();
        std::fs::create_dir_all(program_data.join(library).parent().unwrap()).unwrap();
        std::fs::write(program_data.join(library), b"RESTORED").unwrap();
        let roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        let manifest = write_backup(
            &tmp.path().join("backups"),
            &roots,
            BackupOptions::default(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            Some(&snapshot(tmp.path(), b"RESTORED DATABASE")),
            TEST_VERSION,
        )
        .unwrap();
        let archive = std::fs::read(&manifest.path).unwrap();

        for (case, broken) in [("database", "data/ferrofin.db"), ("library", library)] {
            // One byte flipped mid-entry: reading it fails its CRC (or its
            // inflate) after the extraction began.
            let mut corrupt = archive.clone();
            let (start, len) = {
                let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&archive)).unwrap();
                let entry = zip.by_name(broken).unwrap();
                (entry.data_start().unwrap(), entry.compressed_size())
            };
            corrupt[usize::try_from(start + len / 2).unwrap()] ^= 0xff;

            let boot = tmp.path().join(case);
            let backups = boot.join("data").join(super::BACKUPS_DIR);
            std::fs::create_dir_all(&backups).unwrap();
            std::fs::create_dir_all(boot.join(library).parent().unwrap()).unwrap();
            let live = [
                ("ferrofin.db", "LIVE DATABASE"),
                ("config/system.json", "{}"),
                (library, "LIVE"),
            ];
            std::fs::create_dir_all(boot.join("config")).unwrap();
            for (rel, contents) in live {
                std::fs::write(boot.join(rel), contents).unwrap();
            }
            std::fs::write(backups.join("corrupt.zip"), &corrupt).unwrap();
            std::fs::write(backups.join(super::RESTORE_PENDING_FILE), "corrupt.zip").unwrap();
            let boot_roots =
                TreeRoots::defaults(&boot, &boot.join("config"), &boot.join("ferrofin.db"));

            let failure = apply_pending_restore(&boot_roots).unwrap_err();
            for (rel, contents) in live {
                assert_eq!(
                    std::fs::read_to_string(boot.join(rel)).unwrap(),
                    contents,
                    "{case}: {rel}"
                );
            }
            let left = staged_leftovers(&boot);
            assert!(left.is_empty(), "{case}: staged files left: {left:?}");
            assert!(!failure.partially_applied, "{case}: {failure}");
            assert!(!backups.join(super::RESTORE_PENDING_FILE).exists());
        }
    }

    /// A marker the boot cannot remove would restore the backup again at
    /// every boot, so it is not applied at all: the failure touches nothing.
    #[cfg(unix)]
    #[test]
    fn a_marker_that_cannot_be_removed_is_not_applied() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let boot = tmp.path().join("boot");
        let backups = boot.join("data").join(super::BACKUPS_DIR);
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(boot.join("ferrofin.db"), b"LIVE").unwrap();
        std::fs::write(backups.join(super::RESTORE_PENDING_FILE), "any.zip").unwrap();
        std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores the mode bits: then there is nothing to show.
        let removable = std::fs::File::create(backups.join("probe")).is_ok();
        let roots = TreeRoots::defaults(&boot, &boot.join("config"), &boot.join("ferrofin.db"));
        let result = super::apply_pending_restore(&roots);
        std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o755)).unwrap();
        if removable {
            return;
        }
        let failure = result.unwrap_err();
        assert!(!failure.partially_applied, "{failure}");
        assert!(
            failure.to_string().contains("could not be removed"),
            "{failure}"
        );
        assert!(backups.join(super::RESTORE_PENDING_FILE).is_file());
        assert_eq!(std::fs::read(boot.join("ferrofin.db")).unwrap(), b"LIVE");
    }

    /// An archive from a newer Ferrofin, whose database is migrated past this
    /// binary's last migration, is refused before anything is touched: the
    /// next boot could not open that database. Upstream refuses an archive
    /// whose `ServerVersion` is newer than the running one; a Ferrofin
    /// build's is a release or `git describe` string, which does not order,
    /// so the schema head stands in.
    #[test]
    fn an_archive_from_a_newer_schema_is_refused_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let boot = tmp.path().join("boot");
        let backups = boot.join("data").join(super::BACKUPS_DIR);
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(boot.join("ferrofin.db"), b"LIVE").unwrap();
        let manifest = format!(
            r#"{{"DateCreated":"2024-01-01T00:00:00Z","ServerVersion":"0.4.1","BackupEngineVersion":"{}","Options":{{}}}}"#,
            super::BACKUP_ENGINE_VERSION
        );
        {
            let file = std::fs::File::create(backups.join("newer.zip")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", opts).unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            // The entry name is part of the archive format: archives already
            // written must keep matching it.
            zip.start_file("ferrofin-schema-head", opts).unwrap();
            zip.write_all(b"999999999999").unwrap();
            zip.start_file("data/ferrofin.db", opts).unwrap();
            zip.write_all(b"NEWER").unwrap();
            zip.finish().unwrap();
        }
        std::fs::write(backups.join(super::RESTORE_PENDING_FILE), "newer.zip").unwrap();
        let roots = TreeRoots::defaults(&boot, &boot.join("config"), &boot.join("ferrofin.db"));

        let refused = super::apply_pending_restore(&roots).unwrap_err();
        assert!(!refused.partially_applied, "{refused}");
        assert!(refused.to_string().contains("newer"), "{refused}");
        assert_eq!(std::fs::read(boot.join("ferrofin.db")).unwrap(), b"LIVE");

        // A head entry that cannot be read fails closed: it may be newer.
        let garbled = backups.join("garbled.zip");
        {
            let file = std::fs::File::create(&garbled).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", opts).unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            zip.start_file("ferrofin-schema-head", opts).unwrap();
            zip.write_all(b"v2").unwrap();
            zip.finish().unwrap();
        }
        let reason = super::restore_refusal(&garbled).expect("refused");
        assert!(reason.contains("unreadable"), "{reason}");

        // An archive this build writes records its own head and is restorable.
        let written = super::write_backup(
            &backups,
            &roots,
            BackupOptions::default(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            Some(&snapshot(tmp.path(), b"DB")),
            TEST_VERSION,
        )
        .unwrap();
        let written = std::path::PathBuf::from(&written.path);
        assert_eq!(
            super::read_schema_head(&written),
            Ok(Some(ferrofin_db::schema_head()))
        );
        assert_eq!(super::restore_refusal(&written), None);

        // An archive from before the entry existed is no newer than this build.
        let legacy = backups.join("legacy.zip");
        {
            let file = std::fs::File::create(&legacy).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        assert_eq!(super::restore_refusal(&legacy), None);
    }

    /// A backup whose configuration relocates the metadata tree restores its
    /// metadata where that restored configuration looks for it, not beside the
    /// roots known before the configuration was replaced.
    #[test]
    fn restored_metadata_follows_the_restored_configuration() {
        use super::{restore_archive, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("program");
        let relocated = tmp.path().join("relocated-metadata");
        std::fs::create_dir_all(program_data.join("config")).unwrap();
        std::fs::create_dir_all(relocated.join("library")).unwrap();
        std::fs::write(program_data.join("ferrofin.db"), b"DB").unwrap();
        std::fs::write(
            program_data.join("config/system.json"),
            serde_json::json!({ "MetadataPath": relocated }).to_string(),
        )
        .unwrap();
        std::fs::write(relocated.join("library/poster.jpg"), b"art").unwrap();
        let backups = tmp.path().join("backups");
        // The backing-up server's metadata root is the relocated one.
        let mut roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        roots.metadata = relocated.clone();
        let manifest = write_backup(
            &backups,
            &roots,
            BackupOptions {
                metadata: true,
                ..no_database()
            },
            "2024-05-06T07:08:09Z".parse().unwrap(),
            None,
            TEST_VERSION,
        )
        .unwrap();

        // Restore on a server whose current configuration says nothing about
        // the relocation: its boot roots put metadata under its own tree.
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let boot_roots =
            TreeRoots::defaults(&target, &target.join("config"), &target.join("ferrofin.db"));
        restore_archive(std::path::Path::new(&manifest.path), &boot_roots).unwrap();
        std::fs::remove_dir_all(&relocated).unwrap();
        // Run again now that the relocated root is gone: it must be recreated.
        restore_archive(std::path::Path::new(&manifest.path), &boot_roots).unwrap();

        assert_eq!(
            std::fs::read(relocated.join("library/poster.jpg")).unwrap(),
            b"art",
            "metadata lands where the restored system.json points"
        );
        assert!(
            !target.join("metadata").exists(),
            "nothing is left under the boot-time metadata root"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one end-to-end story: write → restore → boot-apply → list
    fn backup_then_restore_round_trips_db_and_config() {
        use super::{list_backups_in, restore_archive, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        // A program-data root with a DB file and a config tree.
        let program_data = tmp.path().join("program");
        std::fs::create_dir_all(program_data.join("config/users")).unwrap();
        std::fs::write(program_data.join("ferrofin.db"), b"DBv1").unwrap();
        std::fs::write(program_data.join("config/system.json"), b"{\"v\":1}").unwrap();
        std::fs::write(program_data.join("config/users/u.json"), b"user").unwrap();

        let backups = tmp.path().join("backups");
        let now = "2024-05-06T07:08:09Z".parse().unwrap();
        let roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        let db = snapshot(tmp.path(), b"DBv1");
        let manifest = write_backup(
            &backups,
            &roots,
            BackupOptions::default(),
            now,
            Some(&db),
            TEST_VERSION,
        )
        .unwrap();
        let archive = std::path::PathBuf::from(&manifest.path);
        assert!(!db.exists(), "the snapshot is consumed");
        assert!(
            archive.starts_with(&backups),
            "absolute path: {}",
            manifest.path
        );
        assert!(archive.is_file());
        assert_eq!(manifest.backup_engine_version, super::BACKUP_ENGINE_VERSION);
        // The release the service was given (the composition root passes the
        // build's release, not the crate's placeholder version: gl-235).
        assert_eq!(manifest.server_version, TEST_VERSION);

        // Restore over a fresh root (a stale WAL sidecar beside the DB is dropped).
        let restore_root = tmp.path().join("restored");
        std::fs::create_dir_all(&restore_root).unwrap();
        std::fs::write(restore_root.join("ferrofin.db-wal"), b"stale").unwrap();
        let restore_roots = TreeRoots::defaults(
            &restore_root,
            &restore_root.join("config"),
            &restore_root.join("ferrofin.db"),
        );
        restore_archive(&archive, &restore_roots).unwrap();
        assert!(
            !restore_root.join("ferrofin.db-wal").exists(),
            "stale WAL removed"
        );
        assert_eq!(
            std::fs::read(restore_root.join("ferrofin.db")).unwrap(),
            b"DBv1"
        );
        assert_eq!(
            std::fs::read(restore_root.join("config/system.json")).unwrap(),
            b"{\"v\":1}"
        );
        assert_eq!(
            std::fs::read(restore_root.join("config/users/u.json")).unwrap(),
            b"user"
        );

        // A scheduled restore is applied once at boot: the marker names the archive
        // (by file name), the tree is extracted, the marker is consumed.
        let boot_root = tmp.path().join("boot");
        let boot_backups = boot_root.join("data").join(super::BACKUPS_DIR);
        std::fs::create_dir_all(&boot_backups).unwrap();
        std::fs::copy(&archive, boot_backups.join(archive.file_name().unwrap())).unwrap();
        std::fs::write(
            boot_backups.join(super::RESTORE_PENDING_FILE),
            archive.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        let boot_roots = TreeRoots::defaults(
            &boot_root,
            &boot_root.join("config"),
            &boot_root.join("ferrofin.db"),
        );
        assert!(super::restore_pending(&boot_roots), "the marker is seen");
        let applied = super::apply_pending_restore(&boot_roots).unwrap();
        assert!(!super::restore_pending(&boot_roots), "nothing pends after");
        assert_eq!(
            applied.as_deref(),
            Some(boot_backups.join(archive.file_name().unwrap()).as_path())
        );
        assert_eq!(
            std::fs::read(boot_root.join("ferrofin.db")).unwrap(),
            b"DBv1"
        );
        assert!(
            !boot_backups.join(super::RESTORE_PENDING_FILE).exists(),
            "marker consumed"
        );
        assert_eq!(super::apply_pending_restore(&boot_roots).unwrap(), None);
        // A marker naming a missing archive errors once and is dropped; nothing
        // was written, so the server may still boot.
        std::fs::write(boot_backups.join(super::RESTORE_PENDING_FILE), "gone.zip").unwrap();
        let rejected = super::apply_pending_restore(&boot_roots).unwrap_err();
        assert!(!rejected.partially_applied);
        assert!(rejected.to_string().contains("gone.zip"), "{rejected}");
        assert_eq!(super::apply_pending_restore(&boot_roots).unwrap(), None);
        // A foreign archive (Jellyfin's engine) is refused, marker dropped.
        let foreign = boot_backups.join("jellyfin-backup.zip");
        {
            let file = std::fs::File::create(&foreign).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(br#"{"Path":"x","DateCreated":"2024-01-01T00:00:00Z","ServerVersion":"10.11.8","BackupEngineVersion":"0.2.0","Options":{}}"#).unwrap();
            zip.finish().unwrap();
        }
        std::fs::write(
            boot_backups.join(super::RESTORE_PENDING_FILE),
            "jellyfin-backup.zip",
        )
        .unwrap();
        assert!(
            !super::apply_pending_restore(&boot_roots)
                .unwrap_err()
                .partially_applied
        );
        assert_eq!(super::apply_pending_restore(&boot_roots).unwrap(), None);

        // Listing the backups directory finds the archive: a second, newer
        // backup is listed beside it and a non-zip file is ignored.
        let newer = "2024-05-06T09:00:00Z".parse().unwrap();
        let db = snapshot(tmp.path(), b"DBv2");
        let m2 = write_backup(
            &backups,
            &roots,
            BackupOptions::default(),
            newer,
            Some(&db),
            TEST_VERSION,
        )
        .unwrap();
        std::fs::write(backups.join("notes.txt"), b"ignored").unwrap();
        let listed = list_backups_in(&backups).unwrap();
        assert_eq!(listed.len(), 2, "two archives, txt ignored");
        assert!(listed.iter().any(|m| m.path == m2.path));
        assert!(listed.iter().any(|m| m.path == manifest.path));
        // A missing directory lists nothing.
        assert!(
            list_backups_in(&tmp.path().join("nope"))
                .unwrap()
                .is_empty()
        );
    }

    /// `CreateBackupAsync` always archives the library definitions (`Root`)
    /// and the box-set and playlist folders (`Data/collections`,
    /// `Data/playlists`), and `RestoreBackupAsync` puts them back, so a
    /// restore onto a new host brings back the libraries and their options.
    #[test]
    fn backup_then_restore_round_trips_libraries_box_sets_and_playlists() {
        use super::{restore_archive, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("program");
        let files = [
            "root/default/Movies/movies.mblink",
            "root/default/Movies/options.json",
            "data/collections/Saga [boxset]/poster.jpg",
            "data/playlists/Road Trip/poster.jpg",
            "task_results.json",
        ];
        for rel in files {
            let path = program_data.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, rel).unwrap();
        }
        std::fs::write(program_data.join("ferrofin.db"), b"DB").unwrap();
        let roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        let now = "2024-05-06T07:08:09Z".parse().unwrap();
        let manifest = write_backup(
            &tmp.path().join("backups"),
            &roots,
            BackupOptions::default(),
            now,
            Some(&snapshot(tmp.path(), b"DB")),
            TEST_VERSION,
        )
        .unwrap();

        // A new host's data directory, empty.
        let restored = tmp.path().join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        let restore_roots = TreeRoots::defaults(
            &restored,
            &restored.join("config"),
            &restored.join("ferrofin.db"),
        );
        restore_archive(std::path::Path::new(&manifest.path), &restore_roots).unwrap();
        for rel in files {
            assert_eq!(
                std::fs::read(restored.join(rel)).ok().as_deref(),
                Some(rel.as_bytes()),
                "{rel}"
            );
        }
    }

    /// A write that fails part-way leaves no partial archive: it is written
    /// under a scratch name, renamed into place only once complete, and the
    /// scratch file is removed on every error path (upstream deletes its
    /// partial archive in `CreateBackupAsync`'s catch).
    #[cfg(unix)]
    #[test]
    fn a_write_failing_part_way_leaves_no_partial_archive() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("pd");
        std::fs::create_dir_all(program_data.join("config")).unwrap();
        std::fs::write(program_data.join("config/system.json"), b"{}").unwrap();
        // An unreadable directory in the configuration tree: the walk fails
        // on it with the archive half written.
        let locked = program_data.join("config/locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Permission bits do not bind root; nothing fails to test.
            return;
        }
        let roots = TreeRoots::defaults(
            &program_data,
            &program_data.join("config"),
            &program_data.join("ferrofin.db"),
        );
        let backups = tmp.path().join("backups");

        let written = super::write_backup(
            &backups,
            &roots,
            no_database(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            None,
            TEST_VERSION,
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(written.is_err(), "the configuration tree could not be read");
        assert_eq!(
            file_names(&backups),
            Vec::<String>::new(),
            "no partial archive is left"
        );
    }

    /// Program-data roots under `dir` with one configuration file.
    fn small_roots(dir: &std::path::Path) -> TreeRoots {
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/system.json"), b"{}").unwrap();
        TreeRoots::defaults(dir, &dir.join("config"), &dir.join("ferrofin.db"))
    }

    /// Options without the database, for tests that need no snapshot.
    fn no_database() -> BackupOptions {
        BackupOptions {
            database: false,
            ..BackupOptions::default()
        }
    }

    /// A database snapshot holding `bytes` in `dir`, as `snapshot_database`
    /// leaves one for the archive to consume.
    fn snapshot(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(".snapshot-test.db");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// gl-1233: an archive holds the configuration tree, plugin secrets
    /// included (LDAP-Auth's bind password sits in a `0600` `config.json`), so
    /// the archive is private, not the umask mode. A restore puts the
    /// configuration tree back private, a new file or one already there, as
    /// Ferrofin writes it; the library root, which is not secret, keeps the
    /// umask mode.
    #[cfg(unix)]
    #[test]
    fn archives_and_restored_configuration_are_private() {
        use super::{restore_archive, write_backup};
        use std::os::unix::fs::PermissionsExt as _;

        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("program");
        let roots = small_roots(&program_data);
        let secret = "config/plugins/ldap/config.json";
        let library = "root/default/Movies/options.json";
        for rel in [secret, library] {
            let path = program_data.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, rel).unwrap();
        }
        std::fs::set_permissions(
            program_data.join(secret),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let now = "2024-05-06T07:08:09Z".parse().unwrap();
        let manifest = write_backup(
            &tmp.path().join("backups"),
            &roots,
            no_database(),
            now,
            None,
            TEST_VERSION,
        )
        .unwrap();
        let archive = std::path::PathBuf::from(&manifest.path);
        assert_eq!(mode(&archive), 0o600, "the archive is private");

        // A fresh data directory, then one whose configuration is already
        // there with the umask mode.
        let fresh = tmp.path().join("fresh");
        let existing = tmp.path().join("existing");
        let existing_roots = small_roots(&existing);
        std::fs::create_dir_all(existing.join("config/plugins/ldap")).unwrap();
        std::fs::write(existing.join(secret), b"old").unwrap();
        std::fs::set_permissions(
            existing.join(secret),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        let fresh_roots =
            TreeRoots::defaults(&fresh, &fresh.join("config"), &fresh.join("ferrofin.db"));
        for (dir, restore_roots) in [(&fresh, fresh_roots), (&existing, existing_roots)] {
            restore_archive(&archive, &restore_roots).unwrap();
            for rel in [secret, "config/system.json"] {
                assert_eq!(mode(&dir.join(rel)), 0o600, "{}", dir.join(rel).display());
            }
            assert_eq!(std::fs::read(dir.join(secret)).unwrap(), secret.as_bytes());
            // What a file created with the umask gets: the library root's.
            let reference = dir.join("reference");
            std::fs::File::create(&reference).unwrap();
            assert_eq!(mode(&dir.join(library)), mode(&reference));
            assert_eq!(
                std::fs::read(dir.join(library)).unwrap(),
                library.as_bytes()
            );
            let stray = staged_leftovers(dir);
            assert!(stray.is_empty(), "temporary files left: {stray:?}");
        }
    }

    /// With a `MetadataPath` elsewhere, `CreateBackupAsync` also archives the
    /// default metadata folder (`Data/metadata-default`), which may still hold
    /// data, and `RestoreBackupAsync` puts it back there. When the two are the
    /// same folder it is archived once, as `metadata`.
    #[test]
    fn the_default_metadata_folder_goes_along_when_it_is_separate() {
        use super::{restore_archive, write_backup};

        let tmp = tempfile::tempdir().unwrap();
        let program_data = tmp.path().join("pd");
        let mut roots = small_roots(&program_data);
        roots.metadata = tmp.path().join("custom-metadata");
        for (dir, file) in [
            (&roots.metadata, "new.jpg"),
            (&roots.metadata_default, "old.jpg"),
        ] {
            std::fs::create_dir_all(dir.join("library")).unwrap();
            std::fs::write(dir.join("library").join(file), file).unwrap();
        }
        let with_metadata = BackupOptions {
            metadata: true,
            ..no_database()
        };
        let now = "2024-05-06T07:08:09Z".parse().unwrap();
        let backups = tmp.path().join("backups");
        let manifest = write_backup(
            &backups,
            &roots,
            with_metadata.clone(),
            now,
            None,
            TEST_VERSION,
        )
        .unwrap();

        // Restored onto a host whose metadata is configured elsewhere too.
        let target = tmp.path().join("target");
        let mut target_roots =
            TreeRoots::defaults(&target, &target.join("config"), &target.join("ferrofin.db"));
        target_roots.metadata = target.join("custom");
        restore_archive(std::path::Path::new(&manifest.path), &target_roots).unwrap();
        for (restored, file) in [("custom", "new.jpg"), ("metadata", "old.jpg")] {
            let path = target.join(restored).join("library").join(file);
            assert_eq!(std::fs::read(&path).unwrap(), file.as_bytes(), "{path:?}");
        }

        // Restored onto a host whose metadata is the default folder: the
        // `metadata-default` tree is skipped, never written over the metadata.
        let plain = tmp.path().join("plain");
        let plain_roots =
            TreeRoots::defaults(&plain, &plain.join("config"), &plain.join("ferrofin.db"));
        restore_archive(std::path::Path::new(&manifest.path), &plain_roots).unwrap();
        assert!(plain.join("metadata/library/new.jpg").is_file());
        assert!(!plain.join("metadata/library/old.jpg").exists());

        // The same folder: one tree, archived as `metadata`.
        roots.metadata = roots.metadata_default.clone();
        let once = write_backup(&backups, &roots, with_metadata, now, None, TEST_VERSION).unwrap();
        let file = std::fs::File::open(&once.path).unwrap();
        let names: Vec<String> = zip::ZipArchive::new(file)
            .unwrap()
            .file_names()
            .map(|name| name.unwrap().into_owned())
            .collect();
        assert!(
            names.contains(&"metadata/library/old.jpg".to_owned()),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("metadata-default/")),
            "{names:?}"
        );
    }

    /// The archive name is second-granular: a second create in the same
    /// second takes the first free `-N` suffix instead of replacing the first
    /// archive (upstream's `File.OpenWrite` collides the same way).
    #[test]
    fn creates_in_the_same_second_keep_both_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = small_roots(&tmp.path().join("pd"));
        let backups = tmp.path().join("backups");
        let now = "2024-05-06T07:08:09Z".parse().unwrap();
        let written: Vec<String> = (0..3)
            .map(|_| {
                let manifest =
                    super::write_backup(&backups, &roots, no_database(), now, None, TEST_VERSION);
                manifest.unwrap().path
            })
            .collect();
        assert_eq!(
            file_names(&backups),
            [
                "ferrofin-backup-20240506-070809-1.zip",
                "ferrofin-backup-20240506-070809-2.zip",
                "ferrofin-backup-20240506-070809.zip",
            ]
        );
        assert!(written[1].ends_with("ferrofin-backup-20240506-070809-1.zip"));
    }

    /// `CreateEntryFromFile` throws on a file it cannot read, and the create
    /// deletes its partial archive and fails (Jellyfin 12.2 answers 500 with
    /// a `chmod 000` file under `root/`). A skipped file would be a backup
    /// reported complete that is not.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_fails_the_backup() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let roots = small_roots(&tmp.path().join("pd"));
        let locked = roots.config.join("locked.json");
        std::fs::write(&locked, b"{}").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            // Permission bits do not bind root; nothing fails to test.
            return;
        }
        let backups = tmp.path().join("backups");
        let now = "2024-05-06T07:08:09Z".parse().unwrap();

        let failed = super::write_backup(&backups, &roots, no_database(), now, None, TEST_VERSION)
            .unwrap_err();
        assert!(failed.to_string().contains("locked.json"), "{failed}");
        assert_eq!(file_names(&backups), Vec::<String>::new());
    }

    /// A file whose name is not UTF-8 cannot go into the archive under its
    /// name; the create fails naming it rather than leaving it out.
    #[cfg(unix)]
    #[test]
    fn a_file_with_a_non_utf8_name_fails_the_backup() {
        use std::os::unix::ffi::OsStrExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let roots = small_roots(&tmp.path().join("pd"));
        let odd = roots
            .config
            .join(std::ffi::OsStr::from_bytes(b"caf\xe9.json"));
        if std::fs::write(&odd, b"{}").is_err() {
            // A filesystem that only takes UTF-8 names: nothing to test.
            return;
        }
        let backups = tmp.path().join("backups");
        let now = "2024-05-06T07:08:09Z".parse().unwrap();

        let failed = super::write_backup(&backups, &roots, no_database(), now, None, TEST_VERSION)
            .unwrap_err();
        assert!(failed.to_string().contains("caf"), "{failed}");
        assert_eq!(file_names(&backups), Vec::<String>::new());
    }

    /// The database goes into an archive only from the snapshot: with
    /// `Database` selected and no snapshot written, the create fails instead
    /// of copying the live WAL-mode file or leaving the database out.
    #[test]
    fn a_database_backup_without_a_snapshot_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = small_roots(&tmp.path().join("pd"));
        std::fs::write(&roots.database, b"LIVE").unwrap();
        let backups = tmp.path().join("backups");
        let now = "2024-05-06T07:08:09Z".parse().unwrap();

        for snapshot in [None, Some(tmp.path().join("never-written.db"))] {
            let written = super::write_backup(
                &backups,
                &roots,
                BackupOptions::default(),
                now,
                snapshot.as_deref(),
                TEST_VERSION,
            );
            assert!(written.is_err(), "{snapshot:?}");
        }
        assert_eq!(file_names(&backups), Vec::<String>::new());
    }

    /// A service over the program-data root `dir` (its archives go in
    /// [`backups_of`]`(dir)`), with `library` and `system`.
    fn service_over(
        dir: &Path,
        library: Arc<dyn LibraryManager>,
        system: Arc<dyn SystemManager>,
    ) -> FerrofinBackupService {
        let paths = crate::app_paths::FerrofinServerApplicationPaths::new(
            dir,
            dir.join("log"),
            dir.join("config"),
            dir.join("cache"),
            dir.join("web"),
        );
        FerrofinBackupService::new(Arc::new(paths), library, system, TEST_VERSION)
    }

    /// A library manager with no scan running.
    async fn idle_library() -> Arc<dyn LibraryManager> {
        crate::test_support::library_manager_over(crate::test_support::test_db().await)
    }

    /// [`service_over`] `dir` with an idle library and a disk that never fills.
    async fn idle_service(dir: &Path) -> FerrofinBackupService {
        service_over(
            dir,
            idle_library().await,
            Arc::new(DiskSystem::with_free_space(i64::MAX)),
        )
    }

    /// The backups directory of a service over `dir`.
    fn backups_of(dir: &Path) -> PathBuf {
        dir.join("data").join(super::BACKUPS_DIR)
    }

    /// The file names in `dir`, sorted (none for a missing directory).
    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A [`SystemManager`] fake for the service: reports `free_space` for the
    /// backups directory and counts the restarts asked for; with
    /// `failing_snapshot`, writes part of the snapshot and then fails, as
    /// `VACUUM INTO` does when the disk fills mid-copy. With `stall`, the
    /// snapshot says it started (the first `Notify`) and finishes only once
    /// released (the second).
    struct DiskSystem {
        free_space: i64,
        failing_snapshot: bool,
        stall: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        restarts: AtomicUsize,
    }

    impl DiskSystem {
        /// A disk with `free_space` free, whose snapshots succeed at once.
        fn with_free_space(free_space: i64) -> Self {
            Self {
                free_space,
                failing_snapshot: false,
                stall: None,
                restarts: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl SystemManager for DiskSystem {
        async fn get_system_info(
            &self,
            _request: &ferrofin_traits::net::RequestContext,
        ) -> Result<ferrofin_model::system::SystemInfo, ServiceError> {
            unreachable!("not used by the backup service")
        }
        async fn get_public_system_info(
            &self,
            _request: &ferrofin_traits::net::RequestContext,
        ) -> Result<ferrofin_model::system::PublicSystemInfo, ServiceError> {
            unreachable!("not used by the backup service")
        }
        async fn restart(&self) -> Result<(), ServiceError> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn shutdown(&self) -> Result<(), ServiceError> {
            unreachable!("not used by the backup service")
        }
        async fn get_system_storage_info(
            &self,
        ) -> Result<ferrofin_model::system::SystemStorageInfo, ServiceError> {
            unreachable!("not used by the backup service")
        }
        async fn snapshot_database(&self, dest: &Path) -> Result<(), ServiceError> {
            if self.failing_snapshot {
                std::fs::write(dest, b"half a database").expect("partial snapshot");
                return Err(ServiceError::backend("database or disk is full"));
            }
            if let Some((started, release)) = &self.stall {
                std::fs::write(dest, b"half a database").expect("partial snapshot");
                started.notify_one();
                release.notified().await;
            }
            std::fs::write(dest, b"a database").expect("snapshot");
            Ok(())
        }
        async fn free_space_of(&self, _path: &Path) -> i64 {
            self.free_space
        }
    }

    /// Whether `result` is the not-found answer for `name`.
    fn not_found<T>(result: &Result<T, BackupError>, name: &str) -> bool {
        matches!(
            result,
            Err(BackupError::Service(ServiceError::NotFound(message)))
                if *message == format!("backup {name}")
        )
    }

    /// A second concurrent create is refused (the contract's `503`) rather
    /// than queueing.
    ///
    /// Each create picks a free archive name, which two picking at once could
    /// both take; and each writes a database-sized snapshot, so N in flight
    /// need N x database size of disk.
    #[tokio::test]
    async fn a_second_concurrent_create_is_refused() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let service = idle_service(tmp.path()).await;
        // Hold the only permit, exactly as an in-flight create does.
        let held = BACKUP_IN_FLIGHT
            .try_acquire()
            .expect("the first create takes the permit");

        let refused = service.create_backup(BackupOptions::default()).await;
        assert!(
            matches!(refused, Err(BackupError::InProgress)),
            "{refused:?}"
        );

        drop(held);
    }

    /// A restore of an archive from a newer Ferrofin is refused (a `400`) and
    /// schedules nothing.
    #[tokio::test]
    async fn restore_refuses_an_archive_from_a_newer_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let system = Arc::new(DiskSystem::with_free_space(i64::MAX));
        let service = service_over(tmp.path(), idle_library().await, system.clone());
        let backups = backups_of(tmp.path());
        std::fs::create_dir_all(&backups).unwrap();
        {
            let file = std::fs::File::create(backups.join("newer.zip")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", opts).unwrap();
            let manifest = format!(
                r#"{{"DateCreated":"2024-01-01T00:00:00Z","ServerVersion":"0.4.1","BackupEngineVersion":"{}","Options":{{}}}}"#,
                super::BACKUP_ENGINE_VERSION
            );
            zip.write_all(manifest.as_bytes()).unwrap();
            zip.start_file(super::SCHEMA_HEAD_ENTRY, opts).unwrap();
            zip.write_all(b"999999999999").unwrap();
            zip.finish().unwrap();
        }

        let refused = service.schedule_restore("newer.zip").await;
        // Refused by the schema check, not as a foreign archive.
        assert!(
            matches!(&refused, Err(BackupError::Unrestorable(reason))
                if reason.contains("newer version of Ferrofin")),
            "{refused:?}"
        );
        assert!(!backups.join(super::RESTORE_PENDING_FILE).exists());
        assert_eq!(system.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_list_manifest_and_restore() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let system = Arc::new(DiskSystem::with_free_space(i64::MAX));
        let service = service_over(tmp.path(), idle_library().await, system.clone());
        let backups = backups_of(tmp.path());

        // Create a backup.
        let created = service
            .create_backup(BackupOptions::default())
            .await
            .unwrap();
        let created_name = Path::new(&created.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_owned();
        assert!(
            created_name.starts_with("ferrofin-backup-"),
            "{}",
            created.path
        );
        // The manifest carries the contract's fields with the 10.11.8 defaults.
        let json = serde_json::to_value(&created).unwrap();
        assert_eq!(json["BackupEngineVersion"], super::BACKUP_ENGINE_VERSION);
        assert_eq!(json["ServerVersion"], TEST_VERSION);
        assert!(
            json["DateCreated"].as_str().unwrap().ends_with("+00:00"),
            "{}",
            json["DateCreated"]
        );
        assert_eq!(json["Options"]["Database"], true);
        assert_eq!(json["Options"]["Metadata"], false);
        assert!(json["Options"].get("ManualListIds").is_none());

        // List shows it.
        let listed = service.list_backups().await.unwrap();
        assert!(listed.iter().any(|m| m.path == created.path));

        // Its manifest reads back; an unknown one is not found.
        let read = service.get_backup_manifest(&created_name).await.unwrap();
        assert_eq!(read.map(|m| m.path), Some(created.path.clone()));
        let unknown = service.get_backup_manifest("nope.zip").await;
        assert!(not_found(&unknown, "nope.zip"), "{unknown:?}");

        // Restore schedules the archive (the marker names it) and asks for a
        // restart; an unknown archive is not found.
        service.schedule_restore(&created_name).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(backups.join(super::RESTORE_PENDING_FILE)).unwrap(),
            created_name
        );
        assert_eq!(system.restarts.load(Ordering::SeqCst), 1);
        let unknown = service.schedule_restore("nope.zip").await;
        assert!(not_found(&unknown, "nope.zip"), "{unknown:?}");
        // A foreign (Jellyfin-engine) archive in the same directory is refused up front.
        {
            let file = std::fs::File::create(backups.join("jellyfin-backup.zip")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(br#"{"Path":"x","DateCreated":"2024-01-01T00:00:00Z","ServerVersion":"10.11.8","BackupEngineVersion":"0.2.0","Options":{}}"#).unwrap();
            zip.finish().unwrap();
        }
        let foreign = service.schedule_restore("jellyfin-backup.zip").await;
        assert!(
            matches!(foreign, Err(BackupError::Unrestorable(_))),
            "{foreign:?}"
        );
        assert_eq!(system.restarts.load(Ordering::SeqCst), 1);
    }

    /// `BackupService.CreateBackupAsync` refuses while a library scan runs
    /// (`IsScanRunning`): the backup would contend with the scan's writes and
    /// could capture a library half-written. Its `InvalidOperationException`
    /// is a `500`, and nothing is written.
    #[tokio::test]
    async fn create_is_refused_while_a_library_scan_runs() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let library =
            crate::test_support::library_manager_mid_scan(crate::test_support::test_db().await)
                .await;
        let service = service_over(
            tmp.path(),
            library,
            Arc::new(DiskSystem::with_free_space(i64::MAX)),
        );

        let refused = service.create_backup(BackupOptions::default()).await;
        assert!(
            matches!(refused, Err(BackupError::Service(ServiceError::Backend(_)))),
            "{refused:?}"
        );
        assert_eq!(
            file_names(&backups_of(tmp.path())),
            Vec::<String>::new(),
            "nothing written"
        );
    }

    /// `CreateBackupAsync` refuses when the backups directory has less free
    /// space than its `FiveGigabyte` floor, before anything is written; the
    /// `InvalidOperationException` is upstream's `500`. At the floor it goes
    /// ahead.
    #[tokio::test]
    async fn create_is_refused_below_the_free_space_floor() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let backups = backups_of(tmp.path());
        let library = idle_library().await;
        let with_free_space = |free_space| {
            service_over(
                tmp.path(),
                Arc::clone(&library),
                Arc::new(DiskSystem::with_free_space(free_space)),
            )
        };

        let refused = with_free_space(super::BACKUP_MIN_FREE_SPACE - 1)
            .create_backup(BackupOptions::default())
            .await;
        assert!(
            matches!(refused, Err(BackupError::Service(ServiceError::Backend(_)))),
            "{refused:?}"
        );
        assert_eq!(
            file_names(&backups),
            Vec::<String>::new(),
            "nothing written"
        );
        // Unknown free space (`-1`, the C# catch arm) is below the floor too.
        let unknown = with_free_space(-1)
            .create_backup(BackupOptions::default())
            .await;
        assert!(
            matches!(unknown, Err(BackupError::Service(ServiceError::Backend(_)))),
            "{unknown:?}"
        );

        let at_floor = with_free_space(super::BACKUP_MIN_FREE_SPACE)
            .create_backup(BackupOptions::default())
            .await;
        assert!(at_floor.is_ok(), "{at_floor:?}");
    }

    /// A snapshot that fails part-way is removed: each failed create used to
    /// leave a database-sized `.snapshot-*.db` behind, so retries on a nearly
    /// full disk filled it.
    #[tokio::test]
    async fn a_failed_snapshot_is_removed() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let service = service_over(
            tmp.path(),
            idle_library().await,
            Arc::new(DiskSystem {
                failing_snapshot: true,
                ..DiskSystem::with_free_space(i64::MAX)
            }),
        );

        let failed = service.create_backup(BackupOptions::default()).await;
        assert!(
            matches!(failed, Err(BackupError::Service(ServiceError::Backend(_)))),
            "{failed:?}"
        );
        assert_eq!(
            file_names(&backups_of(tmp.path())),
            Vec::<String>::new(),
            "nothing left"
        );
    }

    /// A create whose caller goes away mid-backup runs on to its end, still
    /// holding the in-flight permit: the next create is refused rather than
    /// sweeping the snapshot and the archive from under it (which would zip
    /// the live database file in the snapshot's place). Upstream's create runs
    /// on regardless of the request too (`CancellationToken.None`).
    #[tokio::test]
    async fn a_create_whose_caller_went_away_still_holds_the_permit() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let backups = backups_of(tmp.path());
        let (started, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let service = service_over(
            tmp.path(),
            idle_library().await,
            Arc::new(DiskSystem {
                stall: Some((Arc::clone(&started), Arc::clone(&release))),
                ..DiskSystem::with_free_space(i64::MAX)
            }),
        );

        // The caller gives up while the snapshot is being written.
        let first = tokio::spawn({
            let service = service.clone();
            async move { service.create_backup(BackupOptions::default()).await }
        });
        started.notified().await;
        first.abort();
        let _ = first.await;

        let second = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            service.create_backup(BackupOptions::default()),
        )
        .await;
        assert!(
            matches!(second, Ok(Err(BackupError::InProgress))),
            "the abandoned create still holds the permit: {second:?}"
        );

        // Released, the abandoned create finishes its archive and cleans up.
        release.notify_one();
        let mut names = Vec::new();
        for _ in 0..500 {
            names = file_names(&backups);
            if names.len() == 1
                && names[0].starts_with("ferrofin-backup-")
                && !names[0].ends_with(super::PARTIAL_SUFFIX)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("ferrofin-backup-"), "{names:?}");
        // The permit comes back once the abandoned create has ended.
        for _ in 0..500 {
            if BACKUP_IN_FLIGHT.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(BACKUP_IN_FLIGHT.available_permits(), 1);
    }

    /// The scratch files an interrupted create left behind (a crash between
    /// the snapshot and the finished archive) are swept by the next create;
    /// anything else in the directory is left alone.
    #[tokio::test]
    async fn a_create_sweeps_the_scratch_files_an_interrupted_one_left() {
        let _serial = TEST_SERIAL.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let backups = backups_of(tmp.path());
        std::fs::create_dir_all(&backups).expect("backups dir");
        for leftover in [
            ".snapshot-1700000000.db",
            "ferrofin-backup-20240101-000000.zip.partial",
            "notes.txt",
        ] {
            std::fs::write(backups.join(leftover), b"x").expect("leftover");
        }

        let created = idle_service(tmp.path())
            .await
            .create_backup(BackupOptions::default())
            .await;
        assert!(created.is_ok(), "{created:?}");
        let names = file_names(&backups);
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(names[1], "notes.txt", "{names:?}");
        assert!(
            names[0].starts_with("ferrofin-backup-")
                && Path::new(&names[0]).extension().is_some_and(|x| x == "zip"),
            "{names:?}"
        );
    }

    /// Writing a backup archive must be dispatched to the blocking pool, never
    /// run inline on a runtime worker.
    ///
    /// Deflating the SQLite file plus the whole config tree is seconds of CPU
    /// and disk on a real library; done inline it parks one tokio worker for
    /// that whole time, and every request already queued behind it waits.
    ///
    /// Discriminates by starving the pool, the same way
    /// `image_info_size_stat_goes_through_the_blocking_pool` does: the runtime
    /// gets exactly one blocking thread, and once the create has prepared its
    /// directory and is inside its snapshot, that thread is held busy, so the
    /// write's `spawn_blocking` job is *queued* and the create cannot finish —
    /// while an inline `write_backup` runs on the worker itself and answers
    /// regardless of the pool. Asserting the create makes no progress is what
    /// fails if the inline call comes back. Starving the pool only after the
    /// snapshot keeps the directory preparation, also on the pool, from
    /// stalling the create in its place.
    #[test]
    fn creating_a_backup_goes_through_the_blocking_pool() {
        use std::time::Duration;

        // Only bounds the failing direction: an inline `write_backup` over the
        // test's (tiny, DB-less) program-data dir returns in well under a
        // millisecond, so any value above the noise floor works. Nothing waits
        // on this when the code is correct.
        const STARVED_WAIT: Duration = Duration::from_millis(250);

        // Taken BEFORE the runtime exists, so `blocking_lock` cannot panic.
        let _serial = TEST_SERIAL.blocking_lock();
        let tmp = tempfile::tempdir().unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();

        runtime.block_on(async {
            let (in_snapshot, finish_snapshot) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            let service = service_over(
                tmp.path(),
                idle_library().await,
                Arc::new(DiskSystem {
                    stall: Some((Arc::clone(&in_snapshot), Arc::clone(&finish_snapshot))),
                    ..DiskSystem::with_free_space(i64::MAX)
                }),
            );
            let mut request = tokio::spawn(async move {
                service
                    .create_backup(BackupOptions::default())
                    .await
                    .is_ok()
            });
            in_snapshot.notified().await;

            // Occupy the single blocking thread, and wait until it is provably
            // busy so the write job cannot win the race for it.
            let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                busy_tx.send(()).ok();
                release_rx.recv().ok();
            });
            busy_rx.await.unwrap();
            finish_snapshot.notify_one();

            let starved = tokio::time::timeout(STARVED_WAIT, &mut request).await;
            assert!(
                starved.is_err(),
                "the create finished with the blocking pool starved, so the archive was \
                 written inline on the async worker thread"
            );

            // Free the pool: the same create now finishes.
            release_tx.send(()).unwrap();
            hog.await.unwrap();
            assert!(request.await.unwrap());
        });
    }

    /// `EnumerateBackups` lists `Directory.EnumerateFiles(BackupPath, "*.zip")`
    /// as it enumerates: files only, in directory order, the extension matched
    /// case-sensitively, and an archive whose manifest cannot be read left out.
    /// Measured on Jellyfin 12.2: `COPY.ZIP` and a junk `.zip` are not listed.
    #[tokio::test]
    async fn listing_keeps_directory_order_and_matches_zip_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let service = idle_service(tmp.path()).await;
        let backups = backups_of(tmp.path());
        let roots = small_roots(&tmp.path().join("pd"));
        // Newer first, so a newest-first sort and creation order disagree.
        for now in ["2024-05-06T09:00:00Z", "2024-05-06T07:00:00Z"] {
            super::write_backup(
                &backups,
                &roots,
                no_database(),
                now.parse().unwrap(),
                None,
                TEST_VERSION,
            )
            .unwrap();
        }
        let first = file_names(&backups).remove(0);
        std::fs::copy(backups.join(&first), backups.join("COPY.ZIP")).unwrap();
        std::fs::write(backups.join("junk.zip"), b"not a zip").unwrap();
        std::fs::create_dir(backups.join("dir.zip")).unwrap();

        let listed: Vec<String> = service
            .list_backups()
            .await
            .unwrap()
            .iter()
            .map(|m| m.path.rsplit('/').next().unwrap().to_owned())
            .collect();
        let in_directory_order: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("ferrofin-backup-"))
            .collect();
        assert_eq!(listed, in_directory_order);
    }

    /// The outcome of a manifest read as the status the route answers it with.
    fn manifest_status(result: &Result<Option<BackupManifest>, BackupError>) -> u16 {
        match result {
            Ok(Some(_)) => 200,
            Ok(None) => 204,
            Err(BackupError::Service(ServiceError::NotFound(_))) => 404,
            Err(_) => 500,
        }
    }

    /// `GetBackup` reduces `path` as `SanitizePath` does
    /// (`Path.GetFileName(Path.GetFullPath(path))`), answers 404 when that is
    /// not a file and 204 when the file is not a readable backup. Each status
    /// was measured on Jellyfin 12.2.
    #[tokio::test]
    async fn manifest_answers_as_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let service = idle_service(tmp.path()).await;
        let backups = backups_of(tmp.path());
        let written = super::write_backup(
            &backups,
            &small_roots(&tmp.path().join("pd")),
            no_database(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            None,
            TEST_VERSION,
        )
        .unwrap();
        let archive = file_names(&backups).remove(0);
        std::fs::write(backups.join("junk.zip"), b"not a zip").unwrap();
        std::fs::write(backups.join("plain.txt"), b"not a zip").unwrap();
        std::fs::create_dir(backups.join("dir.zip")).unwrap();

        for (path, status) in [
            (archive.clone(), 200),
            (format!("/elsewhere/{archive}"), 200),
            (format!("a/../{archive}"), 200),
            (format!("{archive}/."), 200),
            (format!("{archive}/.."), 404),
            ("junk.zip".to_owned(), 204),
            ("plain.txt".to_owned(), 204),
            ("nope.zip".to_owned(), 404),
            ("dir.zip".to_owned(), 404),
            (format!("{archive}/"), 404),
            ("..".to_owned(), 404),
            (".".to_owned(), 404),
            ("/".to_owned(), 404),
            ("a/..".to_owned(), 404),
        ] {
            let read = service.get_backup_manifest(&path).await;
            assert_eq!(manifest_status(&read), status, "path={path}: {read:?}");
            if let Ok(Some(manifest)) = &read {
                assert_eq!(manifest.path, written.path);
            }
            if status == 404 {
                assert!(not_found(&read, &path), "path={path}: {read:?}");
            }
        }
    }

    /// `StartRestoreBackup` reduces `ArchiveFileName` with `SanitizePath` too:
    /// a name that is not a file in the backups directory is a 404 (each
    /// measured on Jellyfin 12.2), and none schedules a restore.
    #[tokio::test]
    async fn restore_answers_as_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let system = Arc::new(DiskSystem::with_free_space(i64::MAX));
        let service = service_over(tmp.path(), idle_library().await, system.clone());
        let backups = backups_of(tmp.path());
        super::write_backup(
            &backups,
            &small_roots(&tmp.path().join("pd")),
            no_database(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            None,
            TEST_VERSION,
        )
        .unwrap();
        let archive = file_names(&backups).remove(0);

        let directory_named = format!("{archive}/");
        for name in ["  ", "..", ".", "/", "a/..", directory_named.as_str()] {
            let scheduled = service.schedule_restore(name).await;
            assert!(not_found(&scheduled, name), "{name:?}: {scheduled:?}");
        }
        assert!(!backups.join(super::RESTORE_PENDING_FILE).exists());
        assert_eq!(system.restarts.load(Ordering::SeqCst), 0);
    }

    /// Reading a manifest and scheduling a restore open the archive on the
    /// blocking pool, as the listing does: with the pool starved, neither
    /// finishes. Inline, they would finish on the runtime worker.
    #[test]
    fn manifest_and_restore_read_the_archive_on_the_blocking_pool() {
        use std::time::Duration;

        // Only bounds the failing direction, as in
        // `creating_a_backup_goes_through_the_blocking_pool`.
        const STARVED_WAIT: Duration = Duration::from_millis(250);

        let tmp = tempfile::tempdir().unwrap();
        let backups = backups_of(tmp.path());
        super::write_backup(
            &backups,
            &small_roots(&tmp.path().join("pd")),
            no_database(),
            "2024-05-06T07:08:09Z".parse().unwrap(),
            None,
            TEST_VERSION,
        )
        .unwrap();
        let archive = file_names(&backups).remove(0);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let service = idle_service(tmp.path()).await;
            for restore in [false, true] {
                let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                let hog = tokio::task::spawn_blocking(move || {
                    busy_tx.send(()).ok();
                    release_rx.recv().ok();
                });
                busy_rx.await.unwrap();
                let (service, archive) = (service.clone(), archive.clone());
                let mut request = tokio::spawn(async move {
                    if restore {
                        service.schedule_restore(&archive).await.is_ok()
                    } else {
                        matches!(service.get_backup_manifest(&archive).await, Ok(Some(_)))
                    }
                });

                let starved = tokio::time::timeout(STARVED_WAIT, &mut request).await;
                assert!(
                    starved.is_err(),
                    "restore={restore} finished with the blocking pool starved, so it read \
                     the archive on the async worker thread"
                );
                release_tx.send(()).unwrap();
                hog.await.unwrap();
                assert!(request.await.unwrap(), "restore={restore}");
            }
        });
    }

    /// `Directory.EnumerateFiles` throws on a backups directory it cannot
    /// read: Jellyfin 12.2 answers `GET /Backup` with a 500, not an empty list.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_backups_directory_fails_the_listing() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let service = idle_service(tmp.path()).await;
        let backups = backups_of(tmp.path());
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&backups).is_ok();

        let listed = service.list_backups().await;
        std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(
                matches!(listed, Err(BackupError::Service(ServiceError::Backend(_)))),
                "{listed:?}"
            );
        }
    }
}
