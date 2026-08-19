//! The Maintenance-category scheduled tasks.
//!
//! Faithful ports of the upstream maintenance `IScheduledTask`s (names, keys,
//! categories, descriptions and default triggers match
//! `Emby.Server.Implementations/ScheduledTasks/Tasks/*` and the en-US
//! localization strings):
//!
//! - [`CleanActivityLogTask`] — `CleanActivityLogTask` (`CleanActivityLog`)
//! - [`DeleteCacheFileTask`] — `DeleteCacheFileTask` (`DeleteCacheFiles`)
//! - [`DeleteLogFileTask`] — `DeleteLogFileTask` (`CleanLogFiles`)
//! - [`DeleteTranscodeFileTask`] — `DeleteTranscodeFileTask` (`DeleteTranscodeFiles`)
//! - [`CleanupCollectionAndPlaylistPathsTask`] —
//!   `CleanupCollectionAndPlaylistPathsTask` (`CleanCollectionsAndPlaylists`)
//! - [`OptimizeDatabaseTask`] — `OptimizeDatabaseTask` (`OptimizeDatabaseTask`)
//! - [`CleanupUserDataTask`] — `CleanupUserDataTask` (`CleanupUserDataTask`)
//!
//! The C# `IProgress<double>` maps to [`TaskProgress`]; `CancellationToken`s are
//! dropped (a queued run is cancelled by aborting its tokio task).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::Database;
use ferrofin_db::store::datetime_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::activity::ActivityManager;
use ferrofin_traits::collections::{CollectionManager, PlaylistManager};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::LinkedChildrenService;
use ferrofin_traits::session::SessionManager;
use ferrofin_traits::system::ServerApplicationPaths;
use uuid::Uuid;

use crate::db_error::db_err;
use crate::translate_query::PLACEHOLDER_ID;

use super::{ScheduledTask, TaskProgress};

/// 100-nanosecond ticks per second (the `TaskTriggerInfo` time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Ticks per hour, for trigger intervals.
const TICKS_PER_HOUR: i64 = 3600 * TICKS_PER_SECOND;

/// The upstream Maintenance category display string (`TasksMaintenanceCategory`).
const MAINTENANCE: &str = "Maintenance";

/// An interval trigger firing every `hours` hours.
fn interval_hours(hours: i64) -> TaskTriggerInfo {
    TaskTriggerInfo {
        type_: TaskTriggerInfoType::IntervalTrigger,
        interval_ticks: Some(hours * TICKS_PER_HOUR),
        ..TaskTriggerInfo::default()
    }
}

/// A startup trigger.
fn startup() -> TaskTriggerInfo {
    TaskTriggerInfo {
        type_: TaskTriggerInfoType::StartupTrigger,
        ..TaskTriggerInfo::default()
    }
}

// ---------------------------------------------------------------------------
// file-sweep helpers (the C# FileSystemHelper used by the cleanup tasks)
// ---------------------------------------------------------------------------

/// Recursively collects every file under `dir` (missing dir = empty).
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    files
}

/// Deletes every file under `dir` last modified before `cutoff`, then prunes
/// empty subdirectories (the root is kept). Returns how many files were
/// deleted. Per-file failures are logged and skipped (the C# helper's
/// swallow-and-log behavior). `keep` filters which file names are eligible.
fn delete_files_older_than(
    dir: &Path,
    cutoff: SystemTime,
    progress: &TaskProgress,
    progress_base: f64,
    progress_span: f64,
    keep: &dyn Fn(&Path) -> bool,
) -> u64 {
    let candidates: Vec<PathBuf> = files_under(dir)
        .into_iter()
        .filter(|f| !keep(f))
        .filter(|f| {
            std::fs::metadata(f)
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified < cutoff)
        })
        .collect();
    let total = candidates.len();
    let mut deleted = 0u64;
    for (index, file) in candidates.iter().enumerate() {
        match std::fs::remove_file(file) {
            Ok(()) => deleted += 1,
            Err(e) => tracing::warn!(path = %file.display(), error = %e, "failed to delete file"),
        }
        #[allow(clippy::cast_precision_loss)]
        progress.report(progress_base + progress_span * ((index + 1) as f64 / total as f64));
    }
    delete_empty_dirs(dir, false);
    progress.report(progress_base + progress_span);
    deleted
}

/// Depth-first removal of empty directories under `dir`; removes `dir` itself
/// only when `remove_root` is set (the C# `DeleteEmptyFolders`).
fn delete_empty_dirs(dir: &Path, remove_root: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            delete_empty_dirs(&entry.path(), true);
        }
    }
    if remove_root
        && std::fs::read_dir(dir).is_ok_and(|mut e| e.next().is_none())
        && let Err(e) = std::fs::remove_dir(dir)
    {
        tracing::warn!(path = %dir.display(), error = %e, "failed to remove empty directory");
    }
}

/// Runs a blocking filesystem sweep on the blocking pool.
async fn run_sweep<F>(sweep: F) -> Result<u64, ServiceError>
where
    F: FnOnce() -> u64 + Send + 'static,
{
    tokio::task::spawn_blocking(sweep)
        .await
        .map_err(|e| ServiceError::backend(format!("file sweep panicked: {e}")))
}

// ---------------------------------------------------------------------------
// Clean Activity Log
// ---------------------------------------------------------------------------

/// "Clean Activity Log" — deletes activity log entries older than the
/// configured age. Port of `CleanActivityLogTask`.
pub struct CleanActivityLogTask {
    config: Arc<dyn ServerConfigurationManager>,
    activity: Arc<dyn ActivityManager>,
}

impl CleanActivityLogTask {
    /// Builds the task over the config (retention days) and activity seams.
    #[must_use]
    pub fn new(
        config: Arc<dyn ServerConfigurationManager>,
        activity: Arc<dyn ActivityManager>,
    ) -> Self {
        Self { config, activity }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for CleanActivityLogTask {
    fn key(&self) -> &str {
        "CleanActivityLog"
    }
    fn name(&self) -> &str {
        "Clean Activity Log"
    }
    fn description(&self) -> &str {
        "Deletes activity log entries older than the configured age."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let retention_days = self
            .config
            .configuration()
            .await?
            .activity_log_retention_days;
        let Some(retention_days) = retention_days.filter(|d| *d >= 0) else {
            // The C# task throws when retention is unset/negative.
            return Err(ServiceError::invalid_input(format!(
                "Activity Log Retention days must be at least 0. Currently: {retention_days:?}"
            )));
        };
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(retention_days));
        let removed = self.activity.clean(cutoff).await?;
        tracing::info!(removed, "cleaned activity log entries");
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Clean Cache Directory
// ---------------------------------------------------------------------------

/// "Clean Cache Directory" — deletes cache files no longer needed by the
/// system. Port of `DeleteCacheFileTask`: cache files older than 30 days, temp
/// files (`{cache}/temp`) older than 1 day, then empty-folder pruning.
pub struct DeleteCacheFileTask {
    paths: Arc<dyn ServerApplicationPaths>,
}

impl DeleteCacheFileTask {
    /// Builds the task over the application-paths seam.
    #[must_use]
    pub fn new(paths: Arc<dyn ServerApplicationPaths>) -> Self {
        Self { paths }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for DeleteCacheFileTask {
    fn key(&self) -> &str {
        "DeleteCacheFiles"
    }
    fn name(&self) -> &str {
        "Clean Cache Directory"
    }
    fn description(&self) -> &str {
        "Deletes cache files no longer needed by the system."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let cache = PathBuf::from(self.paths.cache_path());
        let temp = cache.join("temp");
        let progress = progress.clone();
        let deleted = run_sweep(move || {
            let now = SystemTime::now();
            let month_old = now - std::time::Duration::from_hours(30 * 24);
            let day_old = now - std::time::Duration::from_hours(24);
            // The temp subtree gets the shorter retention; exclude it from the
            // 30-day cache pass so its files are judged by the 1-day cutoff.
            let temp_for_filter = temp.clone();
            let deleted = delete_files_older_than(&cache, month_old, &progress, 0.0, 90.0, &|f| {
                f.starts_with(&temp_for_filter)
            });
            deleted + delete_files_older_than(&temp, day_old, &progress, 90.0, 10.0, &|_| false)
        })
        .await?;
        tracing::info!(deleted, "cleaned cache directory");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Clean Log Directory
// ---------------------------------------------------------------------------

/// "Clean Log Directory" — deletes log files past the configured retention.
/// Port of `DeleteLogFileTask`.
pub struct DeleteLogFileTask {
    config: Arc<dyn ServerConfigurationManager>,
    /// The description, formatted with the boot-time retention (the C# formats
    /// `TaskCleanLogsDescription` with the live value on every read; a `&str`
    /// return makes that a construction-time snapshot here).
    description: String,
}

impl DeleteLogFileTask {
    /// Builds the task over the config seam; `retention_days` (the boot-time
    /// `log_file_retention_days`) is embedded in the description string.
    #[must_use]
    pub fn new(config: Arc<dyn ServerConfigurationManager>, retention_days: i32) -> Self {
        Self {
            config,
            description: format!("Deletes log files that are more than {retention_days} days old."),
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for DeleteLogFileTask {
    fn key(&self) -> &str {
        "CleanLogFiles"
    }
    fn name(&self) -> &str {
        "Clean Log Directory"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let retention_days = self.config.configuration().await?.log_file_retention_days;
        let log_dir = PathBuf::from(self.config.application_paths().log_directory_path());
        let progress = progress.clone();
        let deleted = run_sweep(move || {
            let retention_days = u64::try_from(retention_days.max(0)).unwrap_or(0);
            let cutoff =
                SystemTime::now() - std::time::Duration::from_secs(retention_days * 24 * 3600);
            // The C# task skips serilog-managed files (names starting `log_`),
            // which the logging pipeline rotates itself.
            delete_files_older_than(&log_dir, cutoff, &progress, 0.0, 100.0, &|f| {
                f.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("log_"))
            })
        })
        .await?;
        tracing::info!(deleted, "cleaned log directory");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Clean Transcode Directory
// ---------------------------------------------------------------------------

/// "Clean Transcode Directory" — deletes transcode files more than one day
/// old. Port of `DeleteTranscodeFileTask`.
pub struct DeleteTranscodeFileTask {
    paths: Arc<dyn ServerApplicationPaths>,
    sessions: Arc<dyn SessionManager>,
}

impl DeleteTranscodeFileTask {
    /// Builds the task over the application-paths seam and the session manager
    /// (the sweep is skipped while anything is playing — deleting a live
    /// stream's segments mid-playback black-screens the client).
    #[must_use]
    pub fn new(paths: Arc<dyn ServerApplicationPaths>, sessions: Arc<dyn SessionManager>) -> Self {
        Self { paths, sessions }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for DeleteTranscodeFileTask {
    fn key(&self) -> &str {
        "DeleteTranscodeFiles"
    }
    fn name(&self) -> &str {
        "Clean Transcode Directory"
    }
    fn description(&self) -> &str {
        "Deletes transcode files more than one day old."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![startup(), interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Playback guard: a session started >24h ago (paused overnight, a long
        // binge) still owns files in this directory — sweeping now deletes its
        // init/segment files mid-stream. Skip; the 24h interval (and the
        // startup trigger) retries when nothing is playing.
        if self.sessions.has_active_playback().await? {
            tracing::info!("skipping transcode-directory sweep: playback is active");
            return Ok(());
        }
        let dir = PathBuf::from(self.paths.transcode_path());
        let progress = progress.clone();
        let deleted = run_sweep(move || {
            let cutoff = SystemTime::now() - std::time::Duration::from_hours(24);
            delete_files_older_than(&dir, cutoff, &progress, 0.0, 100.0, &|_| false)
        })
        .await?;
        tracing::info!(deleted, "cleaned transcode directory");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Clean up collections and playlists
// ---------------------------------------------------------------------------

/// "Clean up collections and playlists" — removes items from collections and
/// playlists that no longer exist. Port of
/// `CleanupCollectionAndPlaylistPathsTask`: for every box set and playlist,
/// linked children whose item row is gone or whose path no longer exists on
/// disk are removed.
pub struct CleanupCollectionAndPlaylistPathsTask {
    library: Arc<dyn LibraryManager>,
    collections: Arc<dyn CollectionManager>,
    playlists: Arc<dyn PlaylistManager>,
    linked: Arc<dyn LinkedChildrenService>,
}

impl CleanupCollectionAndPlaylistPathsTask {
    /// Builds the task over the library, collection, playlist and
    /// linked-children seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        collections: Arc<dyn CollectionManager>,
        playlists: Arc<dyn PlaylistManager>,
        linked: Arc<dyn LinkedChildrenService>,
    ) -> Self {
        Self {
            library,
            collections,
            playlists,
            linked,
        }
    }

    /// The linked children of `parent_id` whose item row is missing or whose
    /// path no longer exists on disk (the C# `CleanupLinkedChildren` check:
    /// `!File.Exists(path) && !Directory.Exists(path)`).
    async fn dead_children(&self, parent_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
        let mut dead = Vec::new();
        for child_id in self.linked.get_linked_children_ids(parent_id, None).await? {
            let missing = match self.library.get_item_by_id(child_id).await? {
                None => true,
                Some(item) => item
                    .path
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .is_none_or(|p| !Path::new(p).exists()),
            };
            if missing {
                dead.push(child_id);
            }
        }
        Ok(dead)
    }

    /// Cleans one category (box sets or playlists), reporting progress across
    /// `progress_base..progress_base + 50`.
    async fn clean_folders(
        &self,
        kind: BaseItemKind,
        progress: &TaskProgress,
        progress_base: f64,
    ) -> Result<(), ServiceError> {
        let folders = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;
        let total = folders.len();
        for (index, folder) in folders.into_iter().enumerate() {
            let Ok(folder_id) = Uuid::parse_str(&folder.id) else {
                continue;
            };
            let dead = self.dead_children(folder_id).await?;
            if !dead.is_empty() {
                tracing::info!(
                    folder = folder.name.as_deref().unwrap_or_default(),
                    removed = dead.len(),
                    "removing dead linked children"
                );
                if kind == BaseItemKind::BoxSet {
                    self.collections
                        .remove_from_collection(folder_id, &dead)
                        .await?;
                } else {
                    let entry_ids: Vec<String> = dead.iter().map(ToString::to_string).collect();
                    self.playlists
                        .remove_item_from_playlist(&folder.id, &entry_ids)
                        .await?;
                }
            }
            #[allow(clippy::cast_precision_loss)]
            progress.report(progress_base + 50.0 * ((index + 1) as f64 / total as f64));
        }
        progress.report(progress_base + 50.0);
        Ok(())
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for CleanupCollectionAndPlaylistPathsTask {
    fn key(&self) -> &str {
        "CleanCollectionsAndPlaylists"
    }
    fn name(&self) -> &str {
        "Clean up collections and playlists"
    }
    fn description(&self) -> &str {
        "Removes items from collections and playlists that no longer exist."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![startup()]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        self.clean_folders(BaseItemKind::BoxSet, progress, 0.0)
            .await?;
        self.clean_folders(BaseItemKind::Playlist, progress, 50.0)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Optimize database
// ---------------------------------------------------------------------------

/// "Optimize database" — compacts the database and truncates free space. Port
/// of `OptimizeDatabaseTask` + the SQLite provider's `RunScheduledOptimisation`
/// (`PRAGMA optimize`, `VACUUM`, WAL checkpoint truncation).
pub struct OptimizeDatabaseTask {
    db: Database,
    sessions: Arc<dyn SessionManager>,
}

impl OptimizeDatabaseTask {
    /// Builds the task over the database handle and the session manager (the
    /// vacuum is skipped while anything is playing — `VACUUM` and the
    /// truncating checkpoint take exclusive locks that fail live requests; a
    /// real mid-playback black-screen traced to exactly this window).
    #[must_use]
    pub fn new(db: Database, sessions: Arc<dyn SessionManager>) -> Self {
        Self { db, sessions }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for OptimizeDatabaseTask {
    fn key(&self) -> &str {
        "OptimizeDatabaseTask"
    }
    fn name(&self) -> &str {
        "Optimize database"
    }
    fn description(&self) -> &str {
        "Compacts database and truncates free space. Running this task after scanning the \
         library or doing other changes that imply database modifications might improve \
         performance."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(6)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Playback guard: VACUUM + wal_checkpoint(TRUNCATE) take exclusive
        // locks; concurrent requests (HLS auth, playstate) hard-fail once the
        // reader busy-timeout elapses. Skip while anything is playing — the
        // 6h interval retries soon enough.
        if self.sessions.has_active_playback().await? {
            tracing::info!("skipping database optimize/vacuum: playback is active");
            return Ok(());
        }
        tracing::info!("optimizing and vacuuming the database");
        for (statement, pct) in [
            ("PRAGMA optimize", 25.0),
            ("VACUUM", 75.0),
            ("PRAGMA wal_checkpoint(TRUNCATE)", 100.0),
        ] {
            sqlx::query(statement)
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
            progress.report(pct);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// User data cleanup task
// ---------------------------------------------------------------------------

/// How long detached user data is retained before cleanup (the C# `LimitDays`).
const USER_DATA_RETENTION_DAYS: i64 = 90;

/// "User data cleanup task" — cleans user data (watch state, favorite status,
/// …) detached from media that has been gone for at least 90 days. Port of
/// `CleanupUserDataTask`: rows parked on the placeholder item at delete time
/// (see `FerrofinItemPersistenceService`) whose `RetentionDate` passed the limit
/// are removed.
pub struct CleanupUserDataTask {
    db: Database,
}

impl CleanupUserDataTask {
    /// Builds the task over the database handle.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for CleanupUserDataTask {
    fn key(&self) -> &str {
        "CleanupUserDataTask"
    }
    fn name(&self) -> &str {
        "User data cleanup task"
    }
    fn description(&self) -> &str {
        "Cleans all user data (Watch state, favorite status etc) from media that is no longer \
         present for at least 90 days."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let detached: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1"#)
                .bind(PLACEHOLDER_ID)
                .fetch_one(self.db.pool())
                .await
                .map_err(db_err)?;
        tracing::info!(detached, "detached user-data entries");

        let cutoff = Utc::now() - chrono::Duration::days(USER_DATA_RETENTION_DAYS);
        let result = sqlx::query(
            r#"DELETE FROM "UserData"
               WHERE "ItemId" = ?1 AND "RetentionDate" IS NOT NULL AND "RetentionDate" < ?2"#,
        )
        .bind(PLACEHOLDER_ID)
        .bind(datetime_to_db(cutoff))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        tracing::info!(
            removed = result.rows_affected(),
            days = USER_DATA_RETENTION_DAYS,
            "removed expired detached user-data entries"
        );
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Migrate Trickplay Image Location
// ---------------------------------------------------------------------------

/// "Migrate Trickplay Image Location" — moves existing trickplay files
/// according to the library settings. Port of `TrickplayMoveImagesTask`: pages
/// the stored trickplay infos and asks the trickplay manager to consolidate
/// each item's tiles into the configured layout.
pub struct MoveTrickplayImagesTask {
    trickplay: Arc<crate::trickplay_manager::FerrofinTrickplayManager>,
}

impl MoveTrickplayImagesTask {
    /// Builds the task over the concrete trickplay manager (the move helper is
    /// an inherent method — the C# `MoveGeneratedTrickplayDataAsync`).
    #[must_use]
    pub fn new(trickplay: Arc<crate::trickplay_manager::FerrofinTrickplayManager>) -> Self {
        Self { trickplay }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for MoveTrickplayImagesTask {
    fn key(&self) -> &str {
        "MoveTrickplayImages"
    }
    fn name(&self) -> &str {
        "Migrate Trickplay Image Location"
    }
    fn description(&self) -> &str {
        "Moves existing trickplay files according to the library settings."
    }
    fn category(&self) -> &str {
        MAINTENANCE
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        use ferrofin_traits::trickplay::TrickplayManager as _;

        const PAGE: i32 = 100;
        let mut offset = 0i32;
        let mut moved = 0usize;
        loop {
            let infos = self.trickplay.get_trickplay_items(PAGE, offset).await?;
            let count = infos.len();
            let mut seen: Vec<Uuid> = infos
                .iter()
                .filter_map(|i| Uuid::parse_str(&i.item_id).ok())
                .collect();
            seen.dedup();
            for item_id in seen {
                if let Err(e) = self.trickplay.move_generated_trickplay_data(item_id).await {
                    tracing::warn!(item = %item_id, error = %e, "error moving trickplay files");
                } else {
                    moved += 1;
                }
            }
            offset += PAGE;
            #[allow(clippy::cast_precision_loss)]
            progress.report((f64::from(offset) / f64::from(offset + PAGE)) * 100.0);
            if i32::try_from(count).unwrap_or(0) < PAGE {
                break;
            }
        }
        tracing::info!(moved, "trickplay migration complete");
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_model::tasks::TaskTriggerInfoType;
    use ferrofin_traits::activity::{ActivityLogCreate, ActivityLogQuery, ActivityManager};
    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::error::ServiceError;

    use super::*;
    use crate::scheduled_tasks::TaskProgress;
    use crate::test_support::test_db;
    use ferrofin_db::entities::security::DeviceEntity;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::dto::SessionInfoDto;
    use ferrofin_model::session::{
        ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
        PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
    };
    use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData};

    /// A [`SessionManager`] fake for the playback-gated maintenance tasks:
    /// reports a fixed `has_active_playback`; every other method is
    /// unreachable (the tasks only consult the guard).
    struct FakePlaybackSessions(bool);

    #[allow(unused_variables)] // stub bodies are unreachable!(), params deliberately unused
    #[async_trait]
    impl ferrofin_traits::session::SessionManager for FakePlaybackSessions {
        async fn has_active_playback(&self) -> Result<bool, ServiceError> {
            Ok(self.0)
        }
        async fn log_session_activity(
            &self,
            app_name: &str,
            app_version: &str,
            device_id: &str,
            device_name: &str,
            remote_endpoint: &str,
            user: &UserEntity,
        ) -> Result<SessionInfoDto, ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn update_device_name(
            &self,
            session_id: &str,
            reported_device_name: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn on_playback_start(&self, info: &PlaybackStartInfo) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn on_playback_progress(
            &self,
            info: &PlaybackProgressInfo,
            is_automated: bool,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn on_playback_stopped(&self, info: &PlaybackStopInfo) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn report_session_ended(&self, session_id: &str) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_general_command(
            &self,
            controlling_session_id: &str,
            session_id: &str,
            command: &GeneralCommand,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_message_command(
            &self,
            controlling_session_id: &str,
            session_id: &str,
            command: &MessageCommand,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_play_command(
            &self,
            controlling_session_id: &str,
            session_id: &str,
            command: &PlayRequest,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_playstate_command(
            &self,
            controlling_session_id: &str,
            session_id: &str,
            command: &PlaystateRequest,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_message_to_admin_sessions(
            &self,
            message_type: SessionMessageType,
            data: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_message_to_user_sessions(
            &self,
            user_ids: &[Uuid],
            message_type: SessionMessageType,
            data: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_message_to_user_device_sessions(
            &self,
            device_id: &str,
            message_type: SessionMessageType,
            data: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn add_additional_user(
            &self,
            session_id: &str,
            user_id: Uuid,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn remove_additional_user(
            &self,
            session_id: &str,
            user_id: Uuid,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn report_now_viewing_item(
            &self,
            session_id: &str,
            item_id: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn authenticate_new_session(
            &self,
            request: &AuthenticationRequest,
        ) -> Result<AuthenticationResultData, ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn authenticate_direct(
            &self,
            request: &AuthenticationRequest,
        ) -> Result<AuthenticationResultData, ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn report_capabilities(
            &self,
            session_id: &str,
            capabilities: &ClientCapabilities,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn report_transcoding_info(
            &self,
            device_id: &str,
            info: &TranscodingInfo,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn clear_transcoding_info(&self, device_id: &str) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn get_sessions(
            &self,
            user_id: Uuid,
            device_id: Option<&str>,
            active_within_seconds: Option<i32>,
            controllable_user_to_check: Option<Uuid>,
            is_api_key: bool,
        ) -> Result<Vec<SessionInfoDto>, ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn get_session_by_authentication_token(
            &self,
            token: &str,
            device_id: &str,
            remote_endpoint: &str,
        ) -> Result<SessionInfoDto, ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn logout(&self, access_token: &str) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn logout_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn revoke_user_tokens(
            &self,
            user_id: Uuid,
            current_access_token: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
        async fn close_live_stream_if_needed(
            &self,
            live_stream_id: &str,
            session_or_play_session_id: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("not used by maintenance tasks")
        }
    }

    fn idle_sessions() -> Arc<dyn ferrofin_traits::session::SessionManager> {
        Arc::new(FakePlaybackSessions(false))
    }

    fn playing_sessions() -> Arc<dyn ferrofin_traits::session::SessionManager> {
        Arc::new(FakePlaybackSessions(true))
    }

    // -- fakes ------------------------------------------------------------

    struct FakeConfig {
        configuration: ServerConfiguration,
        paths: Arc<crate::FerrofinServerApplicationPaths>,
    }

    impl FakeConfig {
        fn over(dir: &std::path::Path, mutate: impl FnOnce(&mut ServerConfiguration)) -> Self {
            let mut configuration = crate::default_server_configuration();
            mutate(&mut configuration);
            let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
                dir.join("data"),
                dir.join("logs"),
                dir.join("config"),
                dir.join("cache"),
                dir.join("web"),
            ));
            Self {
                configuration,
                paths,
            }
        }
    }

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            Arc::clone(&self.paths) as Arc<dyn ServerApplicationPaths>
        }
        async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
            Ok(self.configuration.clone())
        }
        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            unimplemented!("fake")
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    struct FakeActivity {
        cleaned_before: Mutex<Option<DateTime<Utc>>>,
    }

    #[async_trait]
    impl ActivityManager for FakeActivity {
        async fn get_paged_result(
            &self,
            _query: &ActivityLogQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::activity::ActivityLogEntry>,
            ServiceError,
        > {
            unimplemented!("fake")
        }
        async fn create_entry(&self, _entry: ActivityLogCreate) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn clean(&self, before: DateTime<Utc>) -> Result<u64, ServiceError> {
            *self.cleaned_before.lock().expect("lock") = Some(before);
            Ok(3)
        }
    }

    // -- helpers ----------------------------------------------------------

    /// Creates a file with content under `dir`, returning its path.
    fn touch(dir: &std::path::Path, rel: &str) -> std::path::PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, b"x").expect("write");
        path
    }

    /// Backdates a file's mtime by `days` days.
    fn backdate(path: &std::path::Path, days: u64) {
        let mtime = SystemTime::now() - std::time::Duration::from_secs(days * 24 * 3600 + 60);
        let file = std::fs::File::options()
            .append(true)
            .open(path)
            .expect("open");
        file.set_modified(mtime).expect("set mtime");
    }

    // -- Clean Activity Log ------------------------------------------------

    #[tokio::test]
    async fn clean_activity_log_uses_configured_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(FakeConfig::over(dir.path(), |c| {
            c.activity_log_retention_days = Some(10);
        }));
        let activity = Arc::new(FakeActivity {
            cleaned_before: Mutex::new(None),
        });
        let task = CleanActivityLogTask::new(config, activity.clone());
        assert_eq!(task.key(), "CleanActivityLog");
        assert_eq!(task.category(), "Maintenance");
        assert!(task.default_triggers().is_empty());

        task.execute(&TaskProgress::default()).await.expect("run");
        let before = activity
            .cleaned_before
            .lock()
            .expect("lock")
            .expect("cleaned");
        let expected = Utc::now() - chrono::Duration::days(10);
        assert!((before - expected).num_seconds().abs() < 5);
    }

    #[tokio::test]
    async fn clean_activity_log_rejects_missing_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(FakeConfig::over(dir.path(), |c| {
            c.activity_log_retention_days = None;
        }));
        let activity = Arc::new(FakeActivity {
            cleaned_before: Mutex::new(None),
        });
        let task = CleanActivityLogTask::new(config, activity);
        let err = task
            .execute(&TaskProgress::default())
            .await
            .expect_err("should reject");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    // -- file sweeps -------------------------------------------------------

    #[tokio::test]
    async fn clean_cache_deletes_old_files_and_prunes_empty_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FakeConfig::over(dir.path(), |_| {});
        let paths = config.application_paths();
        let cache = std::path::PathBuf::from(paths.cache_path());

        let old = touch(&cache, "sub/old.bin");
        backdate(&old, 31);
        let fresh = touch(&cache, "fresh.bin");
        let old_temp = touch(&cache, "temp/stale.tmp");
        backdate(&old_temp, 2);
        let fresh_temp = touch(&cache, "temp/live.tmp");

        let task = DeleteCacheFileTask::new(paths);
        assert_eq!(task.key(), "DeleteCacheFiles");
        assert_eq!(task.default_triggers().len(), 1);
        task.execute(&TaskProgress::default()).await.expect("run");

        assert!(!old.exists(), "30-day-old cache file should be deleted");
        assert!(fresh.exists(), "fresh cache file should remain");
        assert!(!old_temp.exists(), "day-old temp file should be deleted");
        assert!(fresh_temp.exists(), "fresh temp file should remain");
        assert!(
            !cache.join("sub").exists(),
            "emptied subdir should be pruned"
        );
        assert!(cache.exists(), "cache root should remain");
    }

    #[tokio::test]
    async fn clean_logs_respects_retention_and_serilog_exclusion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(FakeConfig::over(dir.path(), |c| {
            c.log_file_retention_days = 3;
        }));
        let logs = std::path::PathBuf::from(config.application_paths().log_directory_path());

        let old = touch(&logs, "ferrofin-2026-01-01.log");
        backdate(&old, 4);
        let managed = touch(&logs, "log_managed.log");
        backdate(&managed, 40);
        let fresh = touch(&logs, "ferrofin-today.log");

        let task = DeleteLogFileTask::new(config, 3);
        assert_eq!(task.key(), "CleanLogFiles");
        assert!(task.description().contains("3 days"));
        task.execute(&TaskProgress::default()).await.expect("run");

        assert!(!old.exists(), "old log should be deleted");
        assert!(managed.exists(), "serilog-style log_ files are skipped");
        assert!(fresh.exists(), "fresh log should remain");
    }

    #[tokio::test]
    async fn clean_transcodes_deletes_day_old_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FakeConfig::over(dir.path(), |_| {});
        let paths = config.application_paths();
        let transcodes = std::path::PathBuf::from(paths.transcode_path());

        let old = touch(&transcodes, "abc/segment0.ts");
        backdate(&old, 2);
        let fresh = touch(&transcodes, "live/segment1.ts");

        let task = DeleteTranscodeFileTask::new(paths, idle_sessions());
        assert_eq!(task.key(), "DeleteTranscodeFiles");
        let trigger_types: Vec<TaskTriggerInfoType> =
            task.default_triggers().iter().map(|t| t.type_).collect();
        assert_eq!(
            trigger_types,
            vec![
                TaskTriggerInfoType::StartupTrigger,
                TaskTriggerInfoType::IntervalTrigger
            ]
        );
        task.execute(&TaskProgress::default()).await.expect("run");

        assert!(!old.exists(), "day-old transcode file should be deleted");
        assert!(fresh.exists(), "in-flight transcode file should remain");
    }

    // -- Optimize database -------------------------------------------------

    #[tokio::test]
    async fn optimize_database_runs_pragmas_and_vacuum() {
        let db = test_db().await;
        let task = OptimizeDatabaseTask::new(db, idle_sessions());
        assert_eq!(task.key(), "OptimizeDatabaseTask");
        assert_eq!(
            task.default_triggers()[0].interval_ticks,
            Some(6 * TICKS_PER_HOUR)
        );
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn optimize_database_skips_while_playback_is_active() {
        let db = test_db().await;
        let task = OptimizeDatabaseTask::new(db, playing_sessions());
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("skip is Ok");
        assert!(
            progress.current().abs() < f64::EPSILON,
            "guarded run must not have executed the vacuum steps"
        );
    }

    #[tokio::test]
    async fn transcode_sweep_skips_while_playback_is_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FakeConfig::over(dir.path(), |_| {});
        let paths = config.application_paths();
        let transcodes = std::path::PathBuf::from(paths.transcode_path());
        let old = touch(&transcodes, "abc/segment0.ts");
        backdate(&old, 2);

        let task = DeleteTranscodeFileTask::new(paths, playing_sessions());
        task.execute(&TaskProgress::default())
            .await
            .expect("skip is Ok");
        assert!(
            old.exists(),
            "a live playback session must protect even day-old transcode files"
        );
    }

    // -- Collections/playlists cleanup --------------------------------------

    #[tokio::test]
    async fn collections_cleanup_removes_dead_children_only() {
        use crate::test_support::{item_repository_over, library_manager_over, seed_named_item};
        use crate::{
            FerrofinCollectionManager, FerrofinLinkedChildrenService, FerrofinPlaylistManager,
        };
        use ferrofin_model::data::BaseItemKind;
        use uuid::Uuid;

        let db = test_db().await;
        let media_dir = tempfile::tempdir().expect("tempdir");
        let library = library_manager_over(db.clone());
        let linked: Arc<dyn LinkedChildrenService> =
            Arc::new(FerrofinLinkedChildrenService::new(db.clone()));
        let collections: Arc<dyn CollectionManager> = Arc::new(FerrofinCollectionManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked),
        ));
        let playlists: Arc<dyn PlaylistManager> = Arc::new(FerrofinPlaylistManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked),
            item_repository_over(db.clone()),
        ));

        // A box set with one live child (file on disk) and one dead child
        // (path points nowhere), and a playlist with a dead child.
        let boxset = Uuid::from_u128(0xB0);
        let live = Uuid::from_u128(0xC1);
        let dead = Uuid::from_u128(0xC2);
        let playlist = Uuid::from_u128(0xB1);
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Box").await;
        seed_named_item(&db, live, BaseItemKind::Movie, "Live").await;
        seed_named_item(&db, dead, BaseItemKind::Movie, "Dead").await;
        seed_named_item(&db, playlist, BaseItemKind::Playlist, "List").await;

        let live_path = media_dir.path().join("live.mkv");
        std::fs::write(&live_path, b"x").expect("write");
        let set_path = |id: Uuid, path: String| {
            let db = db.clone();
            async move {
                sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?1 WHERE "Id" = ?2"#)
                    .bind(path)
                    .bind(guid_to_db(id))
                    .execute(db.writer())
                    .await
                    .expect("set path");
            }
        };
        set_path(live, live_path.to_string_lossy().into_owned()).await;
        set_path(dead, "/nonexistent/definitely/gone.mkv".to_owned()).await;

        for (parent, child) in [(boxset, live), (boxset, dead), (playlist, dead)] {
            sqlx::query(
                r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
                   VALUES (?1, ?2, 0, 0)"#,
            )
            .bind(guid_to_db(parent))
            .bind(guid_to_db(child))
            .execute(db.writer())
            .await
            .expect("link");
        }

        let task = CleanupCollectionAndPlaylistPathsTask::new(
            library,
            collections,
            playlists,
            Arc::clone(&linked),
        );
        assert_eq!(task.key(), "CleanCollectionsAndPlaylists");
        assert_eq!(
            task.default_triggers()[0].type_,
            TaskTriggerInfoType::StartupTrigger
        );
        task.execute(&TaskProgress::default()).await.expect("run");

        let remaining = linked
            .get_linked_children_ids(boxset, None)
            .await
            .expect("children");
        assert_eq!(remaining, vec![live], "only the live child survives");
        assert!(
            linked
                .get_linked_children_ids(playlist, None)
                .await
                .expect("children")
                .is_empty(),
            "playlist's dead child is removed"
        );
    }

    // -- Migrate Trickplay Image Location ------------------------------------

    #[tokio::test]
    async fn move_trickplay_task_pages_and_completes() {
        use ferrofin_traits::media_encoding::TrickplayFrameExtractor;

        /// A frame extractor that is never invoked by this test.
        struct NoExtract;
        #[async_trait]
        impl TrickplayFrameExtractor for NoExtract {
            async fn extract_trickplay_frames(
                &self,
                _input_path: &str,
                _interval_ms: i32,
                _max_width: i32,
                _qscale: i32,
                _threads: i32,
                _output_dir: &str,
            ) -> Result<Vec<String>, ServiceError> {
                unimplemented!("not used")
            }
        }

        let db = test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(FakeConfig::over(dir.path(), |_| {}));
        let app_paths = config.paths.clone();
        let trickplay = Arc::new(crate::trickplay_manager::FerrofinTrickplayManager::new(
            db.clone(),
            Arc::new(crate::FerrofinPathManager::new(app_paths)),
            config,
            Arc::new(crate::FerrofinItemRepository::new(
                db,
                Arc::new(crate::ItemTypeLookup::new()),
            )),
            Arc::new(NoExtract),
            Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
        ));

        let task = MoveTrickplayImagesTask::new(trickplay);
        assert_eq!(task.key(), "MoveTrickplayImages");
        assert_eq!(task.name(), "Migrate Trickplay Image Location");
        assert!(task.default_triggers().is_empty());

        // No stored trickplay rows: one empty page, clean completion.
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    // -- User data cleanup -------------------------------------------------

    #[tokio::test]
    async fn user_data_cleanup_removes_expired_detached_rows() {
        use crate::test_support::{seed_item, seed_user};
        use ferrofin_model::data::BaseItemKind;
        use uuid::Uuid;

        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(7)).await;
        let live_item = Uuid::from_u128(0x11);
        seed_item(&db, live_item, BaseItemKind::Movie).await;

        // Three rows: attached (kept), detached-and-expired (removed),
        // detached-but-recent (kept).
        let insert = |item_id: String, key: &str, retention: Option<String>| {
            let db = db.clone();
            let user_id = user.id.clone();
            let key = key.to_owned();
            async move {
                sqlx::query(
                    r#"INSERT INTO "UserData"
                       ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                        "PlaybackPositionTicks", "Played", "RetentionDate")
                       VALUES (?1, ?2, ?3, 0, 0, 0, 1, ?4)"#,
                )
                .bind(item_id)
                .bind(user_id)
                .bind(key)
                .bind(retention)
                .execute(db.writer())
                .await
                .expect("insert userdata");
            }
        };
        insert(guid_to_db(live_item), "live", None).await;
        insert(
            PLACEHOLDER_ID.to_owned(),
            "expired",
            Some(datetime_to_db(Utc::now() - chrono::Duration::days(120))),
        )
        .await;
        insert(
            PLACEHOLDER_ID.to_owned(),
            "recent",
            Some(datetime_to_db(Utc::now() - chrono::Duration::days(5))),
        )
        .await;

        let task = CleanupUserDataTask::new(db.clone());
        assert_eq!(task.key(), "CleanupUserDataTask");
        task.execute(&TaskProgress::default()).await.expect("run");

        let remaining: Vec<String> = sqlx::query_scalar(
            r#"SELECT "CustomDataKey" FROM "UserData" ORDER BY "CustomDataKey""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("query");
        assert_eq!(remaining, vec!["live".to_owned(), "recent".to_owned()]);
    }
}
