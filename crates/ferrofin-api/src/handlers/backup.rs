//! `BackupController` — create, list, inspect, and restore server backups.
//!
//! Ports Jellyfin's `BackupController`. A backup is a single `.zip` under
//! `{data}/backups/` bundling the configuration directory, the SQLite database
//! file (`Options.Database`) and, per the posted `BackupOptionsDto`, the
//! internal metadata, trickplay and subtitle-cache trees, plus a
//! `manifest.json` describing it:
//!
//! - `GET  /Backup` — list the available backups' manifests.
//! - `GET  /Backup/Manifest?path=` — read one backup's manifest.
//! - `POST /Backup/Create` — write a new backup and return its manifest.
//! - `POST /Backup/Restore` (`BackupRestoreRequestDto` body) — schedule the
//!   archive for restore and restart; the next boot extracts it over the live
//!   tree before the database is opened ([`apply_pending_restore`]), exactly as
//!   Jellyfin's `ScheduleRestoreAndRestartServer` does. Restoring in-process
//!   would overwrite an open SQLite file.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::state::AppState;

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

/// The on-disk roots of everything an archive holds: the directory of the
/// database file (archived under `data/`) and the four trees, each archived
/// under its own prefix. `config` is always included; the others follow
/// `BackupOptions`. Built from the live application paths (so a configured
/// `FERROFIN_CONFIG_DIR` / `MetadataPath` is honoured, as Jellyfin archives
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
    /// The trickplay directory (`trickplay/`).
    pub trickplay: PathBuf,
    /// The subtitle cache directory (`subtitles/`).
    pub subtitles: PathBuf,
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
            trickplay: data.join("trickplay"),
            subtitles: data.join("subtitles"),
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
            trickplay: program_data.join("data").join("trickplay"),
            subtitles: program_data.join("data").join("subtitles"),
        }
    }

    /// (archive prefix, on-disk root) for each tree.
    fn trees(&self) -> [(&'static str, &Path); 4] {
        [
            ("config", &self.config),
            ("metadata", &self.metadata),
            ("trickplay", &self.trickplay),
            ("subtitles", &self.subtitles),
        ]
    }
}

/// Serializes `POST /Backup/Create`.
///
/// Two reasons, either sufficient. The archive name is second-granular
/// (`ferrofin-backup-%Y%m%d-%H%M%S.zip`), so two creates in the same second
/// write the *same* path and one silently destroys the other. And
/// [`write_backup`] reads the whole SQLite file into memory, so N concurrent
/// creates hold N x database-size — on a large library that is an OOM.
///
/// Before this handler moved onto the blocking pool the runtime's worker count
/// bounded that implicitly; dispatching to a 512-thread pool removed the bound,
/// so it is made explicit here. A caller that loses the race gets the `503` the
/// contract already documents for this operation rather than queueing.
static BACKUP_IN_FLIGHT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The subdirectory of the data path that holds the backup archives.
const BACKUPS_DIR: &str = "backups";

/// The manifest entry stored inside each archive (as `manifest.json`) and returned
/// by the list/create/inspect routes. Port of `BackupManifestDto`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackupManifest {
    /// The archive's absolute path (Jellyfin reports the full path; its file
    /// name is the handle Restore / Manifest accept). Always re-derived from the
    /// on-disk location on read — and defaulted, because a Jellyfin manifest
    /// carries no `Path` at all and must still parse so `restorable` can refuse
    /// it (and the listing can show it, as Jellyfin lists foreign archives).
    #[serde(default)]
    path: String,
    /// When the backup was created.
    #[serde(with = "ferrofin_model::json::datetime")]
    date_created: DateTime<Utc>,
    /// The server version that wrote it.
    server_version: String,
    /// The archive-layout version ([`BACKUP_ENGINE_VERSION`]). Defaulted on read
    /// so archives written before the field existed (same layout) still list.
    #[serde(default)]
    backup_engine_version: String,
    /// The options the backup was created with (echoed back).
    options: BackupOptions,
}

/// The `POST /Backup/Create` body — which sections to include. Port of
/// `BackupOptionsDto` with its defaults: only the database is on unless asked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // the contract's DTO is four flags
struct BackupOptions {
    metadata: bool,
    trickplay: bool,
    subtitles: bool,
    database: bool,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            metadata: false,
            trickplay: false,
            subtitles: false,
            database: true,
        }
    }
}

impl BackupOptions {
    /// Whether the tree stored under `prefix` is selected by these options.
    fn includes(&self, prefix: &str) -> bool {
        match prefix {
            "config" => true,
            "metadata" => self.metadata,
            "trickplay" => self.trickplay,
            "subtitles" => self.subtitles,
            _ => false,
        }
    }
}

/// The `POST /Backup/Restore` body. Port of `BackupRestoreRequestDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackupRestoreRequest {
    /// The archive file name (must be present in the backups directory).
    #[serde(default)]
    archive_file_name: Option<String>,
}

/// The directory holding the archives (`{data}/backups`).
fn backups_dir(state: &AppState) -> PathBuf {
    Path::new(&state.config.application_paths().data_path()).join(BACKUPS_DIR)
}

/// Maps an I/O / zip failure onto a `500`.
fn io_err(context: &str, e: &impl std::fmt::Display) -> ApiError {
    ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
        "{context}: {e}"
    )))
}

/// Whether a manifest names an archive THIS engine can restore: its own layout
/// version, or a legacy manifest from before the field existed (same layout).
/// A Jellyfin `0.2.0` archive — which shares the backups directory after a
/// drop-in adoption — is refused, as Jellyfin refuses foreign versions.
fn restorable(manifest: &BackupManifest) -> bool {
    manifest.backup_engine_version.is_empty()
        || manifest.backup_engine_version == BACKUP_ENGINE_VERSION
}

/// Reads the `manifest.json` embedded in one archive, if it parses.
fn read_manifest(archive_path: &Path) -> Option<BackupManifest> {
    let file = std::fs::File::open(archive_path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("manifest.json").ok()?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    let mut manifest: BackupManifest = serde_json::from_str(&buf).ok()?;
    // Trust the on-disk location over whatever the manifest recorded.
    manifest.path = archive_path.to_string_lossy().into_owned();
    Some(manifest)
}

/// `GET /Backup` — the available backups, newest first.
///
/// Port of `BackupController.ListBackups`: reads every `.zip` under the backups
/// directory and returns its manifest. A missing directory is an empty list.
#[utoipa::path(
    get,
    path = "/Backup",
    responses((status = 200, description = "Backups available (BackupManifestDto[])")),
    tag = "ferrofin"
)]
async fn list_backups(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Json<Vec<BackupManifest>> {
    // Opening every retained archive and inflating its manifest is blocking file
    // I/O, unbounded in the number of backups kept and on possibly-network
    // storage — the same reason `create_backup` does not run inline.
    let dir = backups_dir(&state);
    Json(
        blocking(move || Ok(list_backups_in(&dir)))
            .await
            .unwrap_or_default(),
    )
}

/// Reads every `.zip` in `dir` and returns its manifest, newest first. A missing
/// directory (or a `.zip` with no readable manifest) contributes nothing. Pure over
/// the path so it is unit-testable and shared by the handler.
fn list_backups_in(dir: &Path) -> Vec<BackupManifest> {
    let mut manifests: Vec<BackupManifest> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("zip"))
        })
        .filter_map(|e| read_manifest(&e.path()))
        .collect();
    manifests.sort_by_key(|m| std::cmp::Reverse(m.date_created));
    manifests
}

/// Query for `GET /Backup/Manifest`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestQuery {
    /// The archive file name (or path) to inspect.
    #[serde(default)]
    path: Option<String>,
}

/// `GET /Backup/Manifest?path=` — one backup's manifest.
///
/// Port of `BackupController.GetBackup`. `404` when the named archive is missing or
/// carries no readable manifest.
#[utoipa::path(
    get,
    path = "/Backup/Manifest",
    params(("path" = String, Query, description = "The archive file name")),
    responses(
        (status = 200, description = "Backup manifest returned"),
        (status = 404, description = "Backup not found")
    ),
    tag = "ferrofin"
)]
async fn get_backup_manifest(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<ManifestQuery>,
) -> Result<Json<BackupManifest>, ApiError> {
    let name = query
        .path
        .as_deref()
        .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing 'path'".to_owned()))?;
    let archive = backups_dir(&state).join(name);
    read_manifest(&archive)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("backup {name}")))
}

/// Recursively adds a directory's files to the zip under `prefix/…`.
fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Path,
    prefix: &str,
    options: zip::write::SimpleFileOptions,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let zip_path = format!("{prefix}/{name}");
        if path.is_dir() {
            add_dir_to_zip(zip, &path, &zip_path, options)?;
        } else if let Ok(bytes) = std::fs::read(&path) {
            zip.start_file(zip_path, options)?;
            zip.write_all(&bytes)?;
        }
    }
    Ok(())
}

/// `POST /Backup/Create` — writes a backup archive and returns its manifest.
///
/// Port of `BackupController.CreateBackup`: zips the SQLite database file and the
/// configuration directory (plus a `manifest.json`) into a timestamped `.zip`
/// under `{data}/backups/`.
#[utoipa::path(
    post,
    path = "/Backup/Create",
    responses((status = 200, description = "Backup created (BackupManifestDto)")),
    tag = "ferrofin"
)]
async fn create_backup(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    body: Option<JsonBody<BackupOptions>>,
) -> Result<Json<BackupManifest>, ApiError> {
    let Ok(_permit) = BACKUP_IN_FLIGHT.try_acquire() else {
        return Err(ApiError::ServiceUnavailable(
            "a backup is already in progress".to_owned(),
        ));
    };
    let options = body.map(|JsonBody(b)| b).unwrap_or_default();
    let roots = TreeRoots::from_paths(state.config.application_paths().as_ref());
    let dir = backups_dir(&state);
    let now = Utc::now();
    // The database goes into the archive from a consistent `VACUUM INTO`
    // snapshot, never from the live WAL-mode file (a checkpoint can tear a
    // file copy mid-read). The snapshot lives beside the archive until zipped.
    let snapshot = options
        .database
        .then(|| dir.join(format!(".snapshot-{}.db", now.timestamp())));
    if let Some(snapshot) = &snapshot {
        std::fs::create_dir_all(&dir).map_err(|e| io_err("create backup", &e))?;
        let _ = std::fs::remove_file(snapshot);
        state.system.snapshot_database(snapshot).await?;
    }
    // Deflating the SQLite snapshot (tens to hundreds of MB) plus the whole
    // config tree is seconds of CPU + disk: it must not run on a runtime worker.
    let result =
        blocking(move || write_backup(&dir, &roots, options, now, snapshot.as_deref())).await;
    result.map(Json).map_err(|e| io_err("create backup", &e))
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
/// `manifest.json`) under `backups_dir`, returning the manifest. The database
/// comes from `snapshot` when given (a consistent copy the caller produced,
/// consumed — deleted — here), else from the live file (tests with no server).
/// Pure over paths — no `AppState` — so it is unit-testable and shared by the
/// handler.
fn write_backup(
    backups_dir: &Path,
    roots: &TreeRoots,
    options: BackupOptions,
    now: DateTime<Utc>,
    snapshot: Option<&Path>,
) -> std::io::Result<BackupManifest> {
    std::fs::create_dir_all(backups_dir)?;
    let file_name = format!("ferrofin-backup-{}.zip", now.format("%Y%m%d-%H%M%S"));
    let archive_path = backups_dir.join(&file_name);
    let manifest = BackupManifest {
        path: archive_path.to_string_lossy().into_owned(),
        date_created: now,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        backup_engine_version: BACKUP_ENGINE_VERSION.to_owned(),
        options,
    };

    let file = std::fs::File::create(&archive_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("manifest.json", opts)?;
    let manifest_json = serde_json::to_vec_pretty(&manifest).unwrap_or_default();
    zip.write_all(&manifest_json)?;

    // The database (skipped if absent — e.g. an in-memory test DB).
    if manifest.options.database {
        let source = snapshot.filter(|p| p.is_file()).unwrap_or(&roots.database);
        if let Ok(mut file) = std::fs::File::open(source) {
            zip.start_file(format!("data/{DB_FILE_NAME}"), opts)?;
            std::io::copy(&mut file, &mut zip)?;
        }
    }
    if let Some(snapshot) = snapshot {
        let _ = std::fs::remove_file(snapshot);
    }

    // The configuration tree, plus every optional tree the options select.
    for (prefix, dir) in roots.trees() {
        if manifest.options.includes(prefix) && dir.is_dir() {
            add_dir_to_zip(&mut zip, dir, prefix, opts)?;
        }
    }

    zip.finish()?;
    Ok(manifest)
}

/// `POST /Backup/Restore` — schedules a backup restore and restarts.
///
/// Port of `BackupController.StartRestoreBackup` +
/// `BackupService.ScheduleRestoreAndRestartServer`: the body's `ArchiveFileName`
/// is reduced to a file name inside the backups directory (`404` when absent),
/// recorded in the restore-pending marker, and the server restarts; the next
/// boot applies it ([`apply_pending_restore`]) before the database is opened.
#[utoipa::path(
    post,
    path = "/Backup/Restore",
    responses(
        (status = 204, description = "Restore scheduled; the server restarts to apply it"),
        (status = 404, description = "Backup not found")
    ),
    tag = "ferrofin"
)]
async fn restore_backup(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    body: Option<JsonBody<BackupRestoreRequest>>,
) -> Result<axum::http::StatusCode, ApiError> {
    let name = body
        .and_then(|JsonBody(b)| b.archive_file_name)
        .as_deref()
        .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::BadRequest("missing 'ArchiveFileName'".to_owned()))?;
    let dir = backups_dir(&state);
    let archive = dir.join(&name);
    if !archive.is_file() {
        return Err(ApiError::NotFound(format!("backup {name}")));
    }
    // Refuse an archive this engine cannot restore BEFORE scheduling a restart
    // for nothing (a Jellyfin archive in a shared backups dir, a stray zip).
    if !read_manifest(&archive).is_some_and(|m| restorable(&m)) {
        return Err(ApiError::BadRequest(format!(
            "backup {name} is not a Ferrofin {BACKUP_ENGINE_VERSION} archive"
        )));
    }
    std::fs::write(dir.join(RESTORE_PENDING_FILE), &name)
        .map_err(|e| io_err("schedule restore", &e))?;
    state.system.restart().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Applies a restore scheduled by `POST /Backup/Restore`, if one is pending.
///
/// The composition root calls this at boot BEFORE the database is opened: it
/// reads the marker under `{program_data}/data/backups/`, extracts the named
/// archive over the tree described by `roots`, removes the marker, and returns the archive path
/// it applied (`None` when nothing was pending). A marker naming a missing or
/// foreign archive is dropped with an error so a stale marker cannot block
/// every boot. One deliberate difference from Jellyfin, whose pending path is
/// in-memory and dies with the process: the marker is durable, so a restore
/// scheduled right before a crash still applies on the next boot (logged).
///
/// # Errors
///
/// The extraction's I/O error; the marker is removed either way.
pub fn apply_pending_restore(roots: &TreeRoots) -> std::io::Result<Option<PathBuf>> {
    let dir = roots.program_data.join("data").join(BACKUPS_DIR);
    let marker = dir.join(RESTORE_PENDING_FILE);
    let Ok(name) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let _ = std::fs::remove_file(&marker);
    let name = Path::new(name.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("restore-pending marker names no archive"))?;
    let archive = dir.join(name);
    if !read_manifest(&archive).is_some_and(|m| restorable(&m)) {
        return Err(std::io::Error::other(format!(
            "{} is not a Ferrofin {BACKUP_ENGINE_VERSION} archive",
            archive.display()
        )));
    }
    restore_archive(&archive, roots)?;
    Ok(Some(archive))
}

/// Extracts a backup archive's entries back over `roots`: `data/ferrofin.db` →
/// the database file, and each tree prefix to its on-disk root. Skips the
/// manifest and anything else. Pure over paths so it is unit-testable and
/// shared by the boot hook.
///
/// The database is written to a temporary file beside the target and renamed
/// into place only once fully extracted, so a failure mid-way (disk full, a
/// corrupt archive) cannot leave a truncated database behind. It runs in WAL
/// mode, so its `-wal`/`-shm` sidecars are removed at the same moment: a stale
/// WAL left beside a restored database would be replayed over it on open.
fn restore_archive(archive_path: &Path, roots: &TreeRoots) -> std::io::Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;
    let db_entry = format!("data/{DB_FILE_NAME}");
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(std::io::Error::other)?;
        let Some(enclosed) = entry.enclosed_name() else {
            continue; // path-traversal guard (zip-slip)
        };
        if enclosed == Path::new(&db_entry) {
            let tmp = roots.database.with_extension("restore-tmp");
            {
                let mut out = std::fs::File::create(&tmp)?;
                std::io::copy(&mut entry, &mut out)?;
                out.sync_all()?;
            }
            for sidecar in ["-wal", "-shm"] {
                let mut name = roots.database.as_os_str().to_owned();
                name.push(sidecar);
                match std::fs::remove_file(&name) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            std::fs::rename(&tmp, &roots.database)?;
            continue;
        }
        let dest = if let Some((root, rest)) = roots
            .trees()
            .into_iter()
            .find_map(|(prefix, root)| enclosed.strip_prefix(prefix).ok().map(|r| (root, r)))
        {
            root.join(rest)
        } else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Backup", get(list_backups))
        .route("/Backup/Manifest", get(get_backup_manifest))
        .route("/Backup/Create", post(create_backup))
        .route("/Backup/Restore", post(restore_backup))
}

#[cfg(test)]
mod tests {
    /// Serializes the backup tests against each other.
    ///
    /// They contend on two process-global resources: [`BACKUP_IN_FLIGHT`],
    /// whose single permit one test deliberately holds, and the shared
    /// `ferrofin-api-test-data/backups` directory that another wipes. `nextest`
    /// gives each test its own process and hides both; plain `cargo test` runs
    /// them as threads in one process, where they fail.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    use super::{BackupManifest, BackupOptions, TreeRoots, add_dir_to_zip, read_manifest};
    use std::io::Write as _;

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
        let read = read_manifest(&archive).expect("manifest");
        assert_eq!(read.path, archive.to_string_lossy());
        assert_eq!(read.server_version, "9.9.9");

        // The directory tree was archived under `config/…`.
        let names: Vec<String> = {
            let file = std::fs::File::open(&archive).unwrap();
            let mut zip = zip::ZipArchive::new(file).unwrap();
            (0..zip.len())
                .map(|i| zip.by_index(i).unwrap().name().to_owned())
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

        // A non-archive path yields no manifest.
        assert!(read_manifest(tmp.path()).is_none());
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
        let manifest = write_backup(&backups, &roots, BackupOptions::default(), now, None).unwrap();
        let archive = std::path::PathBuf::from(&manifest.path);
        assert!(
            archive.starts_with(&backups),
            "absolute path: {}",
            manifest.path
        );
        assert!(archive.is_file());
        assert_eq!(manifest.backup_engine_version, super::BACKUP_ENGINE_VERSION);

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
        let applied = super::apply_pending_restore(&boot_roots).unwrap();
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
        // A marker naming a missing archive errors once and is dropped.
        std::fs::write(boot_backups.join(super::RESTORE_PENDING_FILE), "gone.zip").unwrap();
        assert!(super::apply_pending_restore(&boot_roots).is_err());
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
        assert!(super::apply_pending_restore(&boot_roots).is_err());
        assert_eq!(super::apply_pending_restore(&boot_roots).unwrap(), None);

        // Listing the backups directory finds the archive, newest first.
        // A second, newer backup + a non-zip file that is ignored.
        let newer = "2024-05-06T09:00:00Z".parse().unwrap();
        let m2 = write_backup(&backups, &roots, BackupOptions::default(), newer, None).unwrap();
        std::fs::write(backups.join("notes.txt"), b"ignored").unwrap();
        let listed = list_backups_in(&backups);
        assert_eq!(listed.len(), 2, "two archives, txt ignored");
        assert_eq!(listed[0].path, m2.path, "newest first");
        // A missing directory lists nothing.
        assert!(list_backups_in(&tmp.path().join("nope")).is_empty());
    }

    /// Every `/Backup*` route is `RequiresElevation` in the contract (each
    /// documents a `403`), and restore in particular replaces the live
    /// database — an ordinary account must never reach it.
    #[tokio::test]
    async fn backup_routes_reject_a_non_elevated_caller() {
        use crate::create_router;
        use crate::test_support::authed_fake_state;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        // `authed_fake_state` authenticates a plain user — not an API key and
        // not an administrator.
        let router = create_router(authed_fake_state());
        for (method, uri) in [
            ("GET", "/Backup"),
            ("GET", "/Backup/Manifest?path=x.zip"),
            ("POST", "/Backup/Create"),
            ("POST", "/Backup/Restore"),
        ] {
            let res = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "{method} {uri} must require elevation"
            );
        }
    }

    /// A second concurrent create is refused with the contract's `503` rather
    /// than queueing.
    ///
    /// Two creates in the same second write the SAME archive path (the name is
    /// second-granular), so one would silently destroy the other; and each
    /// holds the whole database in memory, so N in flight is N x database size.
    #[tokio::test]
    async fn a_second_concurrent_create_is_refused() {
        use crate::create_router;
        use crate::test_support::elevated_fake_state;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let _serial = TEST_SERIAL.lock().await;
        // Hold the only permit, exactly as an in-flight create does.
        let held = super::BACKUP_IN_FLIGHT
            .try_acquire()
            .expect("the first create takes the permit");

        let res = create_router(elevated_fake_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/Backup/Create")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(held);
    }

    #[tokio::test]
    async fn create_list_and_manifest_via_router() {
        use crate::create_router;
        use crate::test_support::elevated_fake_state;
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let _serial = TEST_SERIAL.lock().await;

        // The authed fake state's paths point under the temp dir; start clean.
        let backups = std::env::temp_dir()
            .join("ferrofin-api-test-data")
            .join("backups");
        let _ = std::fs::remove_dir_all(&backups);

        let router = create_router(elevated_fake_state());
        let send = |method: &str, uri: String, body: &str| {
            let router = router.clone();
            let (method, body) = (method.to_owned(), body.to_owned());
            async move {
                router
                    .oneshot(
                        Request::builder()
                            .method(method.as_str())
                            .uri(uri)
                            .header("Content-Type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        };

        // Create a backup.
        let resp = send("POST", "/Backup/Create".to_owned(), "{}").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let created: BackupManifest = serde_json::from_slice(&body).unwrap();
        let created_name = std::path::Path::new(&created.path)
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
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["BackupEngineVersion"], super::BACKUP_ENGINE_VERSION);
        assert_eq!(json["Options"]["Database"], true);
        assert_eq!(json["Options"]["Metadata"], false);
        assert!(json["Options"].get("ManualListIds").is_none());

        // List shows it.
        let resp = send("GET", "/Backup".to_owned(), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let listed: Vec<BackupManifest> = serde_json::from_slice(&body).unwrap();
        assert!(listed.iter().any(|m| m.path == created.path));

        // Its manifest reads back; an unknown one is 404.
        let resp = send("GET", format!("/Backup/Manifest?path={created_name}"), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = send("GET", "/Backup/Manifest?path=nope.zip".to_owned(), "").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Restore takes the `BackupRestoreRequestDto` body, schedules the archive
        // (the marker names it) and asks for a restart; an unknown archive is 404
        // and a missing name is 400.
        let resp = send(
            "POST",
            "/Backup/Restore".to_owned(),
            &format!("{{\"ArchiveFileName\":\"{created_name}\"}}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            std::fs::read_to_string(backups.join(super::RESTORE_PENDING_FILE)).unwrap(),
            created_name
        );
        let resp = send(
            "POST",
            "/Backup/Restore".to_owned(),
            "{\"ArchiveFileName\":\"nope.zip\"}",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // A foreign (Jellyfin-engine) archive in the same directory is refused up front.
        {
            let file = std::fs::File::create(backups.join("jellyfin-backup.zip")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(br#"{"Path":"x","DateCreated":"2024-01-01T00:00:00Z","ServerVersion":"10.11.8","BackupEngineVersion":"0.2.0","Options":{}}"#).unwrap();
            zip.finish().unwrap();
        }
        let resp = send(
            "POST",
            "/Backup/Restore".to_owned(),
            "{\"ArchiveFileName\":\"jellyfin-backup.zip\"}",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = send("POST", "/Backup/Restore".to_owned(), "{}").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let _ = std::fs::remove_dir_all(&backups);
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
    /// gets exactly one blocking thread and that thread is held busy, so a
    /// `spawn_blocking` job is *queued* and the request cannot finish — while an
    /// inline `write_backup` runs on the worker itself and answers regardless of
    /// the pool. Asserting the request makes no progress is what fails if the
    /// inline call comes back.
    #[test]
    fn creating_a_backup_goes_through_the_blocking_pool() {
        use crate::create_router;
        use crate::test_support::elevated_fake_state;
        use axum::body::Body;
        use axum::http::Request;
        use std::time::Duration;
        use tower::ServiceExt as _;

        // Only bounds the failing direction: an inline `write_backup` over the
        // fake state's (tiny, DB-less) program-data dir returns in well under a
        // millisecond, so any value above the noise floor works. Nothing waits
        // on this when the code is correct.
        const STARVED_WAIT: Duration = Duration::from_millis(250);

        // Taken BEFORE the runtime exists, so `blocking_lock` cannot panic.
        let _serial = TEST_SERIAL.blocking_lock();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();

        runtime.block_on(async {
            let router = create_router(elevated_fake_state());

            // Occupy the single blocking thread, and wait until it is provably
            // busy so the backup job cannot win the race for it.
            let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                busy_tx.send(()).ok();
                release_rx.recv().ok();
            });
            busy_rx.await.unwrap();

            let create = |router: axum::Router| async move {
                router
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/Backup/Create")
                            .header("Content-Type", "application/json")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            };

            let starved = tokio::time::timeout(STARVED_WAIT, create(router.clone())).await;
            assert!(
                starved.is_err(),
                "POST /Backup/Create answered with the blocking pool starved, so the archive \
                 was written inline on the async worker thread"
            );

            // Free the pool and confirm the same request now answers.
            release_tx.send(()).unwrap();
            hog.await.unwrap();
            let _ = create(router).await;
        });
    }
}
