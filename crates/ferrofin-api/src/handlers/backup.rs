//! `BackupController` — create, list, inspect, and restore server backups.
//!
//! Ports Jellyfin's `BackupController`. A backup is a single `.zip` under
//! `{data}/backups/` bundling the SQLite database file and the whole
//! configuration directory, plus a `manifest.json` describing it:
//!
//! - `GET  /Backup` — list the available backups' manifests.
//! - `GET  /Backup/Manifest?path=` — read one backup's manifest.
//! - `POST /Backup/Create` — write a new backup and return its manifest.
//! - `POST /Backup/Restore?archiveFileName=` — extract a backup back over the
//!   data + config directories (takes effect on the next restart).
//!
//! The DB + config are what Jellyfin's default `BackupOptions` cover; the
//! per-section toggles in the posted options are accepted but the DB/config are
//! always included (they are the restorable state Ferrofin has).

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::state::AppState;

/// The SQLite database file name inside the data directory.
const DB_FILE_NAME: &str = "ferrofin.db";

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
    /// The archive's file name (its handle for restore).
    path: String,
    /// When the backup was created.
    date_created: DateTime<Utc>,
    /// The server version that wrote it.
    server_version: String,
    /// The options the backup was created with (echoed back).
    options: BackupOptions,
}

/// The `POST /Backup/Create` body — which sections to include. Port of
/// `BackupOptionsDto`; the DB + config are always backed up, so these are recorded
/// on the manifest but do not change what is written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackupOptions {
    #[serde(default = "default_true")]
    metadata: bool,
    #[serde(default = "default_true")]
    subtitles: bool,
    #[serde(default = "default_true")]
    trickplay: bool,
    #[serde(default)]
    manual_list_ids: Vec<String>,
}

const fn default_true() -> bool {
    true
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

/// Reads the `manifest.json` embedded in one archive, if it parses.
fn read_manifest(archive_path: &Path) -> Option<BackupManifest> {
    let file = std::fs::File::open(archive_path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("manifest.json").ok()?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    let mut manifest: BackupManifest = serde_json::from_str(&buf).ok()?;
    // Trust the on-disk name over whatever the manifest recorded.
    if let Some(name) = archive_path.file_name().and_then(|n| n.to_str()) {
        name.clone_into(&mut manifest.path);
    }
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
    body: Option<Json<BackupOptions>>,
) -> Result<Json<BackupManifest>, ApiError> {
    let Ok(_permit) = BACKUP_IN_FLIGHT.try_acquire() else {
        return Err(ApiError::ServiceUnavailable(
            "a backup is already in progress".to_owned(),
        ));
    };
    let options = body.map(|Json(b)| b).unwrap_or_default();
    // The restorable state lives at the program-data root: the SQLite DB file and
    // the whole `config/` tree.
    let program_data = PathBuf::from(state.config.application_paths().program_data_path());
    let dir = backups_dir(&state);
    let now = Utc::now();
    // Deflating the SQLite file (tens to hundreds of MB) plus the whole config
    // tree is seconds of CPU + disk: it must not run on a runtime worker.
    blocking(move || write_backup(&dir, &program_data, options, now))
        .await
        .map(Json)
        .map_err(|e| io_err("create backup", &e))
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

/// Writes a backup archive (DB file + `config/` tree + `manifest.json`) under
/// `backups_dir`, returning the manifest. Pure over paths — no `AppState` — so it
/// is unit-testable and shared by the handler.
fn write_backup(
    backups_dir: &Path,
    program_data: &Path,
    options: BackupOptions,
    now: DateTime<Utc>,
) -> std::io::Result<BackupManifest> {
    std::fs::create_dir_all(backups_dir)?;
    let file_name = format!("ferrofin-backup-{}.zip", now.format("%Y%m%d-%H%M%S"));
    let manifest = BackupManifest {
        path: file_name.clone(),
        date_created: now,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        options,
    };

    let file = std::fs::File::create(backups_dir.join(&file_name))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("manifest.json", opts)?;
    let manifest_json = serde_json::to_vec_pretty(&manifest).unwrap_or_default();
    zip.write_all(&manifest_json)?;

    // The database file (skipped if absent — e.g. an in-memory test DB).
    if let Ok(bytes) = std::fs::read(program_data.join(DB_FILE_NAME)) {
        zip.start_file(format!("data/{DB_FILE_NAME}"), opts)?;
        zip.write_all(&bytes)?;
    }

    // The configuration tree.
    let config_dir = program_data.join("config");
    if config_dir.is_dir() {
        add_dir_to_zip(&mut zip, &config_dir, "config", opts)?;
    }

    zip.finish()?;
    Ok(manifest)
}

/// Query for `POST /Backup/Restore`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreQuery {
    /// The archive file name to restore.
    #[serde(default)]
    archive_file_name: Option<String>,
}

/// `POST /Backup/Restore?archiveFileName=` — restores a backup.
///
/// Port of `BackupController.StartRestoreBackup`: extracts the archive's `data/`
/// and `config/` entries back over the data + configuration directories. As with
/// Jellyfin, the running process keeps its open handles, so the restore fully
/// takes effect on the next restart. `404` when the archive is missing.
#[utoipa::path(
    post,
    path = "/Backup/Restore",
    params(("archiveFileName" = String, Query, description = "The archive to restore")),
    responses(
        (status = 204, description = "Restore applied (restart to complete)"),
        (status = 404, description = "Backup not found")
    ),
    tag = "ferrofin"
)]
async fn restore_backup(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<RestoreQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    let name = query
        .archive_file_name
        .as_deref()
        .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing 'archiveFileName'".to_owned()))?;
    let archive_path = backups_dir(&state).join(name);
    if !archive_path.is_file() {
        return Err(ApiError::NotFound(format!("backup {name}")));
    }

    let program_data = PathBuf::from(state.config.application_paths().program_data_path());
    // Inflating the archive back over the data + config tree is the same
    // seconds-long blocking job as creating it.
    blocking(move || restore_archive(&archive_path, &program_data))
        .await
        .map_err(|e| io_err("restore backup", &e))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Extracts a backup archive's `data/` + `config/` entries back under
/// `program_data` (`data/ferrofin.db` → `{root}/ferrofin.db`, `config/…` →
/// `{root}/config/…`), skipping the manifest and anything else. Pure over paths so
/// it is unit-testable and shared by the handler.
fn restore_archive(archive_path: &Path, program_data: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(std::io::Error::other)?;
        let Some(enclosed) = entry.enclosed_name() else {
            continue; // path-traversal guard (zip-slip)
        };
        let dest = if let Ok(rest) = enclosed.strip_prefix("data") {
            program_data.join(rest)
        } else if enclosed.starts_with("config") {
            program_data.join(&enclosed)
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

    use super::{BackupManifest, BackupOptions, add_dir_to_zip, read_manifest};
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

        // The manifest round-trips (path re-derived from the file name).
        let read = read_manifest(&archive).expect("manifest");
        assert_eq!(read.path, "backup.zip");
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
        let manifest =
            write_backup(&backups, &program_data, BackupOptions::default(), now).unwrap();
        assert!(manifest.path.starts_with("ferrofin-backup-"));
        let archive = backups.join(&manifest.path);
        assert!(archive.is_file());

        // Corrupt the live files, then restore over a fresh root.
        let restore_root = tmp.path().join("restored");
        restore_archive(&archive, &restore_root).unwrap();
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

        // Listing the backups directory finds the archive, newest first.
        // A second, newer backup + a non-zip file that is ignored.
        let newer = "2024-05-06T09:00:00Z".parse().unwrap();
        let m2 = write_backup(&backups, &program_data, BackupOptions::default(), newer).unwrap();
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
        assert!(created.path.starts_with("ferrofin-backup-"));

        // List shows it.
        let resp = send("GET", "/Backup".to_owned(), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let listed: Vec<BackupManifest> = serde_json::from_slice(&body).unwrap();
        assert!(listed.iter().any(|m| m.path == created.path));

        // Its manifest reads back; an unknown one is 404.
        let resp = send("GET", format!("/Backup/Manifest?path={}", created.path), "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = send("GET", "/Backup/Manifest?path=nope.zip".to_owned(), "").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Restore the just-created archive.
        let resp = send(
            "POST",
            format!("/Backup/Restore?archiveFileName={}", created.path),
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

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
