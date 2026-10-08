//! The Library-category scheduled tasks.
//!
//! Faithful ports of the upstream library `IScheduledTask`s (names, keys,
//! categories, descriptions and default triggers match the upstream classes and
//! en-US localization strings):
//!
//! - [`KeyframeExtractionTask`] — `KeyframeExtractionScheduledTask`
//!   (`KeyframeExtraction`)
//! - [`AudioNormalizationTask`] — `AudioNormalizationTask` (`AudioNormalization`)
//! - [`ChapterImagesTask`] — `ChapterImagesTask` (`RefreshChapterImages`)
//! - [`PeopleValidationTask`] — `PeopleValidationTask` (`RefreshPeople`)
//! - [`SubtitleDownloadTask`] — `SubtitleScheduledTask` (`DownloadSubtitles`)
//! - [`LyricDownloadTask`] — `LyricScheduledTask` (`DownloadLyrics`)
//! - [`TrickplayImagesTask`] — `TrickplayImagesTask` (`RefreshTrickplayImages`)
//! - [`MediaSegmentExtractionTask`] — `MediaSegmentExtractionTask`
//!   (`TaskExtractMediaSegments`)
//!
//! The media-segment task runs the registered providers; season fingerprinting
//! and guest analysis retain their own producer passes and persistent output.
//!
//! The C# `IProgress<double>` maps to [`TaskProgress`]; `CancellationToken`s
//! are dropped (a queued run is cancelled by aborting its tokio task).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, KeyframeDataEntity};
use ferrofin_db::store::{datetime_to_db, guid_to_db};
#[cfg(test)]
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::data::{BaseItemKind, MediaType};
#[cfg(test)]
use ferrofin_model::dto::MediaSourceInfo;
#[cfg(test)]
use ferrofin_model::entities_media::MediaStream;
#[cfg(test)]
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::chapters::ChapterManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::options::{InternalItemsQuery, SourceType};
use ferrofin_traits::persistence::KeyframeRepository;
#[cfg(test)]
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshOptions, ProviderManager};
use ferrofin_traits::stubs::LyricManager;
#[cfg(test)]
use ferrofin_traits::subtitles::{SubtitleManager, SubtitleSearchRequest};
#[cfg(test)]
use ferrofin_traits::system::PathManager;
use ferrofin_traits::system::ServerApplicationPaths;
use ferrofin_traits::trickplay::TrickplayManager;
use uuid::Uuid;

use crate::db_error::db_err;

use super::{ScheduledTask, TaskProgress};

/// 100-nanosecond ticks per second (the `TaskTriggerInfo` time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The upstream Library category display string (`TasksLibraryCategory`).
const LIBRARY: &str = "Library";

/// Items examined per page (the upstream `QueryPageLimit`).
const PAGE_SIZE: i32 = 100;

/// An interval trigger firing every `hours` hours.
fn interval_hours(hours: i64) -> TaskTriggerInfo {
    TaskTriggerInfo {
        type_: TaskTriggerInfoType::IntervalTrigger,
        interval_ticks: Some(hours * 3600 * TICKS_PER_SECOND),
        ..TaskTriggerInfo::default()
    }
}

/// Fetches one page of items for a query.
async fn page(
    library: &Arc<dyn LibraryManager>,
    base: &InternalItemsQuery,
    start_index: i32,
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    library
        .get_item_list(&InternalItemsQuery {
            start_index: Some(start_index),
            limit: Some(PAGE_SIZE),
            ..base.clone()
        })
        .await
}

// ---------------------------------------------------------------------------
// Keyframe Extractor
// ---------------------------------------------------------------------------

/// "Keyframe Extractor" — extracts keyframes from video files to create more
/// precise HLS playlists. Port of `KeyframeExtractionScheduledTask` over the
/// ffprobe extractor in `ferrofin-keyframes` (the upstream cache decorator's
/// "already extracted" check becomes a stored-row check).
pub struct KeyframeExtractionTask {
    library: Arc<dyn LibraryManager>,
    keyframes: Arc<dyn KeyframeRepository>,
    encoder: Arc<dyn MediaEncoder>,
}

impl KeyframeExtractionTask {
    /// Builds the task over the library, keyframe-repository and encoder seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        keyframes: Arc<dyn KeyframeRepository>,
        encoder: Arc<dyn MediaEncoder>,
    ) -> Self {
        Self {
            library,
            keyframes,
            encoder,
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for KeyframeExtractionTask {
    fn key(&self) -> &str {
        "KeyframeExtraction"
    }
    fn name(&self) -> &str {
        "Keyframe Extractor"
    }
    fn description(&self) -> &str {
        "Extracts keyframes from video files to create more precise HLS playlists. This task \
         may run for a long time."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Movie],
            is_virtual_item: Some(false),
            recursive: true,
            ..InternalItemsQuery::default()
        };
        let total = self.library.get_count(&query).await?.max(0);
        let probe = self.encoder.probe_path();
        let mut done = 0i32;
        let mut start_index = 0i32;
        while start_index < total {
            let items = page(&self.library, &query, start_index).await?;
            if items.is_empty() {
                break;
            }
            for item in &items {
                done += 1;
                progress.report(100.0 * f64::from(done) / f64::from(total.max(1)));
                let Ok(item_id) = Uuid::parse_str(&item.id) else {
                    continue;
                };
                let Some(path) = item.path.clone().filter(|p| Path::new(p).exists()) else {
                    continue;
                };
                // Already extracted → skip (the C# cache decorator's role).
                if !self.keyframes.get_keyframe_data(item_id).await?.is_empty() {
                    continue;
                }
                let probe = probe.clone();
                let extracted = tokio::task::spawn_blocking(move || {
                    ferrofin_keyframes::ff_probe::get_keyframe_data(&probe, &path)
                })
                .await
                .map_err(|e| ServiceError::backend(format!("keyframe task panicked: {e}")))?;
                match extracted {
                    Ok(data) => {
                        let entity = KeyframeDataEntity {
                            item_id: guid_to_db(item_id),
                            keyframe_ticks: Some(
                                serde_json::to_string(&data.keyframe_ticks)
                                    .map_err(|e| ServiceError::backend(e.to_string()))?,
                            ),
                            total_duration: data.total_duration,
                        };
                        self.keyframes.save_keyframe_data(item_id, &entity).await?;
                    }
                    Err(e) => {
                        tracing::warn!(item = %item.id, error = %e, "keyframe extraction failed");
                    }
                }
            }
            start_index += PAGE_SIZE;
        }
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Audio Normalization
// ---------------------------------------------------------------------------

/// Runs an ffmpeg process and returns its stderr text.
///
/// The seam that keeps the real process spawn out of unit tests (the pattern
/// `ferrofin-mediaencoding` uses for its `Transcoder`); [`TokioFfmpegRunner`] is
/// the real impl.
#[async_trait]
pub trait FfmpegRunner: Send + Sync {
    /// Runs `program` with `args`, returning captured stderr on exit.
    async fn run_stderr(&self, program: &str, args: &[String]) -> Result<String, ServiceError>;
}

/// The real [`FfmpegRunner`] over `tokio::process`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioFfmpegRunner;

#[async_trait]
impl FfmpegRunner for TokioFfmpegRunner {
    async fn run_stderr(&self, program: &str, args: &[String]) -> Result<String, ServiceError> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            // Task cancel = future drop; the child must not outlive it.
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| ServiceError::backend(format!("failed to start {program}: {e}")))?;
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Parses the integrated-loudness value from ffmpeg `ebur128` stderr output.
///
/// Port of the C# `^\s+I:\s+(.*?)\s+LUFS` regex: the first summary line of the
/// form `I: -23.1 LUFS` wins.
#[must_use]
pub fn parse_lufs(stderr: &str) -> Option<f64> {
    for line in stderr.lines() {
        if !line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut parts = line.split_whitespace();
        if parts.next() == Some("I:")
            && let Some(value) = parts.next()
            && parts.next() == Some("LUFS")
            && let Ok(lufs) = value.parse::<f64>()
        {
            return Some(lufs);
        }
    }
    None
}

/// "Audio Normalization" — scans files for audio normalization data. Port of
/// `AudioNormalizationTask`: for every library with `EnableLUFSScan`, measures
/// integrated loudness (ffmpeg `ebur128`) for multi-track albums (via a concat
/// list) and tracks that don't have one yet, storing it on the item's `LUFS`
/// column.
pub struct AudioNormalizationTask {
    db: Database,
    library: Arc<dyn LibraryManager>,
    folders: Arc<dyn VirtualFolderManager>,
    encoder: Arc<dyn MediaEncoder>,
    runner: Arc<dyn FfmpegRunner>,
    paths: Arc<dyn ServerApplicationPaths>,
}

impl AudioNormalizationTask {
    /// Builds the task over the database, library, encoder-path,
    /// process-runner and paths seams.
    #[must_use]
    pub fn new(
        db: Database,
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        encoder: Arc<dyn MediaEncoder>,
        runner: Arc<dyn FfmpegRunner>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            db,
            library,
            folders,
            encoder,
            runner,
            paths,
        }
    }

    /// Measures integrated LUFS for the given ffmpeg input arguments.
    async fn measure(&self, input_args: Vec<String>) -> Result<Option<f64>, ServiceError> {
        let mut args = vec!["-hide_banner".to_owned()];
        args.extend(input_args);
        args.extend(
            ["-af", "ebur128=framelog=verbose", "-f", "null", "-"]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        let stderr = match self
            .runner
            .run_stderr(&self.encoder.encoder_path(), &args)
            .await
        {
            Ok(stderr) => stderr,
            Err(error) => {
                tracing::warn!(%error, "failed to start audio normalization analysis");
                return Ok(None);
            }
        };
        let lufs = parse_lufs(&stderr);
        if lufs.is_none() {
            tracing::warn!("failed to find LUFS value in ffmpeg output");
        }
        Ok(lufs)
    }

    /// Stores a measured LUFS value on an item.
    async fn save_lufs(&self, item_id: &str, lufs: f64) -> Result<(), ServiceError> {
        sqlx::query(r#"UPDATE "BaseItems" SET "LUFS" = ?1 WHERE "Id" = ?2"#)
            .bind(lufs)
            .bind(item_id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Album pass: concat every track of a multi-track album and measure once.
    async fn scan_albums(
        &self,
        folder_ids: &[Uuid],
        progress: &TaskProgress,
        base: f64,
        span: f64,
    ) -> Result<(), ServiceError> {
        let albums = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicAlbum],
                recursive: true,
                ancestor_ids: folder_ids.to_vec(),
                ..InternalItemsQuery::default()
            })
            .await?;
        let total = albums.len().max(1);
        for (index, album) in albums.iter().enumerate() {
            if album.lufs.is_none()
                && album.normalization_gain.is_none()
                && let Ok(album_id) = Uuid::parse_str(&album.id)
            {
                let tracks = self
                    .library
                    .get_item_list(&InternalItemsQuery {
                        include_item_types: vec![BaseItemKind::Audio],
                        recursive: true,
                        ancestor_ids: vec![album_id],
                        ..InternalItemsQuery::default()
                    })
                    .await?;
                let track_paths: Vec<String> = tracks
                    .iter()
                    .filter_map(|track| track.path.as_deref())
                    .filter(|path| crate::media_info_resolver::is_file_protocol(path))
                    .map(str::to_owned)
                    .collect();
                // Album gain is useless for single-track albums (upstream skip).
                if track_paths.len() > 1 {
                    tracing::info!(
                        album = album.name.as_deref().unwrap_or_default(),
                        "calculating album LUFS"
                    );
                    if let Some(lufs) = self.measure_album(&album.id, &track_paths).await? {
                        self.save_lufs(&album.id, lufs).await?;
                    }
                }
            }
            #[allow(clippy::cast_precision_loss)]
            progress.report(base + span * ((index + 1) as f64 / total as f64));
        }
        Ok(())
    }

    /// Measures an album's LUFS over an ffmpeg concat list of its tracks.
    async fn measure_album(
        &self,
        album_id: &str,
        track_paths: &[String],
    ) -> Result<Option<f64>, ServiceError> {
        // Same scratch directory the frame extractor uses, named once on the
        // paths trait so the two cannot drift apart.
        let temp_dir = std::path::PathBuf::from(self.paths.temp_path());
        ferrofin_util::file_helper::ensure_writable_dir(&temp_dir).map_err(|e| {
            ServiceError::backend(format!("temp directory `{}`: {e}", temp_dir.display()))
        })?;
        let concat = temp_dir.join(format!("{album_id}.concat"));
        // ffmpeg concat-list quoting: single quotes with '\'' escapes.
        let lines: Vec<String> = track_paths
            .iter()
            .map(|p| format!("file '{}'", p.replace('\'', "'\\''")))
            .collect();
        std::fs::write(&concat, lines.join("\n"))
            .map_err(|e| ServiceError::backend(e.to_string()))?;
        let measured = self
            .measure(vec![
                "-f".to_owned(),
                "concat".to_owned(),
                "-safe".to_owned(),
                "0".to_owned(),
                "-i".to_owned(),
                concat.to_string_lossy().into_owned(),
            ])
            .await;
        if let Err(e) = std::fs::remove_file(&concat) {
            tracing::warn!(path = %concat.display(), error = %e, "failed to delete concat file");
        }
        measured
    }

    /// Track pass: measure each track that has no LUFS yet.
    async fn scan_tracks(
        &self,
        folder_ids: &[Uuid],
        progress: &TaskProgress,
        base: f64,
        span: f64,
    ) -> Result<(), ServiceError> {
        let tracks = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Audio],
                recursive: true,
                ancestor_ids: folder_ids.to_vec(),
                ..InternalItemsQuery::default()
            })
            .await?;
        let total = tracks.len().max(1);
        for (index, track) in tracks.iter().enumerate() {
            if track.lufs.is_none()
                && track.normalization_gain.is_none()
                && let Some(path) = track.path.clone()
                && crate::media_info_resolver::is_file_protocol(&path)
                && let Some(lufs) = self.measure(vec!["-i".to_owned(), path]).await?
            {
                self.save_lufs(&track.id, lufs).await?;
            }
            #[allow(clippy::cast_precision_loss)]
            progress.report(base + span * ((index + 1) as f64 / total as f64));
        }
        Ok(())
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for AudioNormalizationTask {
    fn key(&self) -> &str {
        "AudioNormalization"
    }
    fn name(&self) -> &str {
        "Audio Normalization"
    }
    fn description(&self) -> &str {
        "Scans files for audio normalization data."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Libraries opted into the LUFS scan (per-library `EnableLUFSScan`).
        let folder_ids: Vec<Uuid> = self
            .folders
            .get_virtual_folders()
            .await?
            .into_iter()
            .filter(|f| {
                f.library_options
                    .as_ref()
                    .is_some_and(|o| o.enable_lufs_scan)
            })
            .filter_map(|f| f.item_id.as_deref().and_then(|id| Uuid::parse_str(id).ok()))
            .collect();
        if folder_ids.is_empty() {
            progress.report(100.0);
            return Ok(());
        }
        self.scan_albums(&folder_ids, progress, 0.0, 50.0).await?;
        self.scan_tracks(&folder_ids, progress, 50.0, 50.0).await?;
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Extract Chapter Images
// ---------------------------------------------------------------------------

/// "Extract Chapter Images" — reconciles every video's saved chapter images.
/// Missing images are extracted only for eligible videos in enabled libraries;
/// disabled and previously failed videos still reconcile paths and prune files.
pub struct ChapterImagesTask {
    library: Arc<dyn LibraryManager>,
    folders: Arc<dyn VirtualFolderManager>,
    chapters: Arc<dyn ChapterManager>,
    extractor: Arc<crate::chapter_image_extractor::ChapterImageExtractor>,
    paths: Arc<dyn ServerApplicationPaths>,
}

impl ChapterImagesTask {
    /// Builds the task over the shared chapter-image extractor and row manager.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        chapters: Arc<dyn ChapterManager>,
        extractor: Arc<crate::chapter_image_extractor::ChapterImageExtractor>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            library,
            folders,
            chapters,
            extractor,
            paths,
        }
    }

    async fn refresh_video(
        &self,
        video: &BaseItemEntity,
        extract: bool,
    ) -> Result<bool, ServiceError> {
        let item_id = Uuid::parse_str(&video.id)
            .map_err(|error| ServiceError::invalid_input(error.to_string()))?;
        let mut chapters = self.chapters.get_chapters(item_id).await?;
        let outcome = self
            .extractor
            .refresh(video, &mut chapters, extract, &crate::ScanCancel::default())
            .await?;
        if outcome.changed {
            // ChapterManager.SaveChapters retains only markers inside runtime.
            let saved = chapters
                .iter()
                .filter(|chapter| chapter.start_position_ticks < video.run_time_ticks.unwrap_or(0))
                .cloned()
                .collect::<Vec<_>>();
            self.chapters.save_chapters(item_id, &saved).await?;
        }
        outcome.prune_unused_images(&chapters);
        Ok(outcome.success)
    }
}

impl ChapterImagesTask {
    /// Proves the run can write everywhere it needs to before touching a single
    /// video.
    ///
    /// An unwritable directory fails EVERY extraction, and without this the run
    /// records the whole library as permanently failed — which is exactly what
    /// happened: a server whose cache volume had a root-owned `temp/`
    /// blocklisted ~3000 videos, then could not even rewrite the blocklist. A
    /// misconfigured server must fail the task, loudly and once, and leave the
    /// history untouched.
    fn preflight_writable_dirs(&self, fail_history_path: &Path) -> Result<(), ServiceError> {
        for dir in [
            // ffmpeg writes the frame here ...
            std::path::PathBuf::from(self.paths.temp_path()),
            // ... and it is then moved under the internal metadata tree. Both
            // live on the cache/metadata volume, and the outage this guards
            // against - root-owned directories left by a container that once
            // ran as root - hits whichever of them it happens to have created.
            // Probing only the first leaves the same failure one directory
            // over: extraction succeeds, every move fails, the library is
            // blocklisted again.
            std::path::PathBuf::from(self.paths.internal_metadata_path()),
            fail_history_path
                .parent()
                .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf),
        ] {
            // Returned, not logged: the task runner logs a failed task once, at
            // the outermost layer, and the message names the directory.
            ferrofin_util::file_helper::ensure_writable_dir(&dir).map_err(|e| {
                ServiceError::backend(format!(
                    "chapter image extraction needs a writable `{}`, so no images can be \
                     produced until this is fixed: {e}",
                    dir.display()
                ))
            })?;
        }
        Ok(())
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for ChapterImagesTask {
    fn key(&self) -> &str {
        "RefreshChapterImages"
    }
    fn name(&self) -> &str {
        "Extract Chapter Images"
    }
    fn description(&self) -> &str {
        "Creates thumbnails for videos that have chapters."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::DailyTrigger,
            time_of_day_ticks: Some(2 * 3600 * TICKS_PER_SECOND),
            max_runtime_ticks: Some(4 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        }]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let folders = self.folders.get_virtual_folders().await?;
        let videos = self
            .library
            .get_item_list(&InternalItemsQuery {
                media_types: vec![MediaType::Video],
                is_folder: Some(false),
                is_virtual_item: Some(false),
                include_owned_items: true,
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;

        // Failure history: videos whose extraction failed before are skipped
        // until their file changes (the key embeds the mtime).
        let fail_history_path = Path::new(&self.paths.cache_path()).join("chapter-failures.txt");

        if folders.iter().any(|folder| {
            folder
                .library_options
                .as_ref()
                .is_some_and(|options| options.enable_chapter_image_extraction)
        }) {
            self.preflight_writable_dirs(&fail_history_path)?;
        }

        // A set, and lowercased once: the lookup is per video, and a history
        // that has grown to thousands of entries turns a linear scan per video
        // into O(videos × history).
        let mut failed: std::collections::BTreeSet<String> =
            std::fs::read_to_string(&fail_history_path)
                .map(|text| {
                    text.split('|')
                        .filter(|s| !s.is_empty())
                        .map(str::to_lowercase)
                        .collect()
                })
                .unwrap_or_default();

        // A blocklist this large is almost always the fingerprint of a past
        // systemic failure (an unwritable temp directory failing every
        // extraction), not that many unreadable files. Nothing here can tell
        // the two apart — the history records only path+mtime — so say how many
        // videos are being skipped and let the operator judge. Silence is what
        // let ~2950 wrongly-blocklisted videos look like a working task.
        if !failed.is_empty() {
            tracing::info!(
                skipped = failed.len(),
                path = %fail_history_path.display(),
                "skipping videos recorded as previously failed; delete this file to retry them"
            );
        }

        let total = videos.len().max(1);
        let mut history_dirty = false;
        for (index, video) in videos.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            progress.report(100.0 * (index as f64) / total as f64);
            let Some(path) = video.path.as_deref().filter(|path| !path.is_empty()) else {
                continue;
            };
            let modified = crate::chapter_image_extractor::date_modified_ticks(video.date_modified);
            let history_key = format!("{path}{modified}").to_lowercase();
            let success = match self
                .refresh_video(video, !failed.contains(&history_key))
                .await
            {
                Ok(success) => success,
                Err(error) => {
                    // Source records extraction failures, not repository/service
                    // errors. Flush genuine failures already seen before failing.
                    write_failure_history(&fail_history_path, &failed, history_dirty)?;
                    return Err(error);
                }
            };
            if !success {
                failed.insert(history_key);
                history_dirty = true;
            }
        }
        write_failure_history(&fail_history_path, &failed, history_dirty)?;
        progress.report(100.0);
        Ok(())
    }
}

/// Persists the chapter-image failure history, if the run added to it.
///
/// Written once per run rather than per failure: rewriting the whole file each
/// time made a systemic failure quadratic in the history's own size.
fn write_failure_history(
    path: &Path,
    failed: &std::collections::BTreeSet<String>,
    dirty: bool,
) -> Result<(), ServiceError> {
    if dirty {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ServiceError::backend(format!(
                    "cannot create chapter failure history directory `{}`: {error}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(path, failed.iter().cloned().collect::<Vec<_>>().join("|")).map_err(
            |error| {
                ServiceError::backend(format!(
                    "cannot write chapter failure history `{}`: {error}",
                    path.display()
                ))
            },
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Refresh People
// ---------------------------------------------------------------------------

/// How long since the last refresh before a person is re-examined (upstream's
/// 30-day window in `RefreshPeopleImagesAsync`).
const PEOPLE_REFRESH_DAYS: i64 = 30;

/// The stored `Type` name of a `Person` item row.
const PERSON_TYPE: &str = "MediaBrowser.Controller.Entities.Person";

/// "Refresh People" — updates metadata for actors and directors in the media
/// library. Port of `PeopleValidationTask`:
///
/// 1. deduplicates `Peoples` rows sharing (name, type), re-pointing their item
///    links to one survivor, and removes people with no item links;
/// 2. `PeopleValidator`: removes `Person` items whose people row is gone,
///    and refreshes the new and never-refreshed ones (`isNew ||
///    neverRefreshed`, `PeopleValidator.cs:76-80`) with the default options;
/// 3. `RefreshPeopleImagesAsync`: refreshes the `Person` items missing a
///    primary image or overview that were not refreshed in the last 30 days,
///    fetching only what each lacks (a `FullRefresh` of the missing half,
///    `ValidationOnly` of the other).
pub struct PeopleValidationTask {
    db: Database,
    providers: Arc<dyn ProviderManager>,
}

impl PeopleValidationTask {
    /// Builds the task over the database and provider-manager seams.
    #[must_use]
    pub fn new(db: Database, providers: Arc<dyn ProviderManager>) -> Self {
        Self { db, providers }
    }

    /// Phase 1: merge duplicate people and drop orphans.
    async fn dedupe_and_orphans(&self) -> Result<(), ServiceError> {
        let dup_groups: Vec<String> = sqlx::query_scalar(
            r#"SELECT GROUP_CONCAT("Id")
               FROM "Peoples"
               GROUP BY "Name", "PersonType"
               HAVING COUNT(*) > 1"#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        for group in dup_groups {
            let ids: Vec<&str> = group.split(',').collect();
            let Some((keep, dups)) = ids.split_first() else {
                continue;
            };
            for dup in dups {
                // Re-point links to the survivor; a link that would collide
                // with an existing (item, person, role) row is dropped.
                sqlx::query(
                    r#"UPDATE OR IGNORE "PeopleBaseItemMap" SET "PeopleId" = ?1
                       WHERE "PeopleId" = ?2"#,
                )
                .bind(keep)
                .bind(dup)
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
                sqlx::query(r#"DELETE FROM "PeopleBaseItemMap" WHERE "PeopleId" = ?1"#)
                    .bind(dup)
                    .execute(self.db.writer())
                    .await
                    .map_err(db_err)?;
                sqlx::query(r#"DELETE FROM "Peoples" WHERE "Id" = ?1"#)
                    .bind(dup)
                    .execute(self.db.writer())
                    .await
                    .map_err(db_err)?;
            }
        }
        let orphans = sqlx::query(
            r#"DELETE FROM "Peoples"
               WHERE "Id" NOT IN (SELECT DISTINCT "PeopleId" FROM "PeopleBaseItemMap")"#,
        )
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        tracing::info!(removed = orphans.rows_affected(), "removed orphaned people");
        Ok(())
    }

    /// Phase 2: drop `Person` items whose people row no longer exists.
    async fn validate_person_items(&self) -> Result<(), ServiceError> {
        let removed =
            crate::item_persistence_service::remove_orphaned_person_items(&self.db).await?;
        tracing::info!(removed, "removed dead person items");
        Ok(())
    }

    /// The rest of phase 2 (`PeopleValidator.Run`): every person item that is
    /// new or was never refreshed (`DateLastRefreshed` unset — a person item
    /// is created unrefreshed) refreshes with the default options, whose
    /// first refresh runs its providers. A person already refreshed is left
    /// alone here.
    async fn refresh_new_person_items(
        &self,
        progress: &TaskProgress,
        base: f64,
        span: f64,
    ) -> Result<(), ServiceError> {
        let ids =
            crate::item_persistence_service::never_refreshed_items(&self.db, PERSON_TYPE).await?;
        let refreshed = self
            .refresh_people(&ids, progress, base, span, |_| {
                MetadataRefreshOptions::default()
            })
            .await;
        tracing::info!(refreshed, total = ids.len(), "refreshed new people");
        Ok(())
    }

    /// Refreshes the person items `ids` with the options `options_for`
    /// builds for each, reporting progress over `base..base + span`; returns
    /// how many were refreshed. Stops at the first failure: a provider-manager
    /// failure (no network, no image store) repeats identically for every
    /// person, and the next scheduled run retries.
    async fn refresh_people(
        &self,
        ids: &[String],
        progress: &TaskProgress,
        base: f64,
        span: f64,
        options_for: impl Fn(&str) -> MetadataRefreshOptions,
    ) -> usize {
        let total = ids.len().max(1);
        let mut refreshed = 0usize;
        for (index, id) in ids.iter().enumerate() {
            if let Ok(item_id) = Uuid::parse_str(id) {
                match self
                    .providers
                    .refresh_single_item(item_id, &options_for(id))
                    .await
                {
                    Ok(_) => refreshed += 1,
                    Err(e) => {
                        tracing::warn!(person = id, error = %e, "person refresh failed");
                        break;
                    }
                }
            }
            #[allow(clippy::cast_precision_loss)]
            progress.report(base + span * ((index + 1) as f64 / total as f64));
        }
        refreshed
    }

    /// Phase 3 (`RefreshPeopleImagesAsync`): refresh the person items missing
    /// a primary image or an overview that were not refreshed in the last 30
    /// days, each with `ImageRefreshMode`/`MetadataRefreshMode` a
    /// `FullRefresh` for the half it lacks and `ValidationOnly` for the half
    /// it has.
    async fn refresh_person_items(
        &self,
        progress: &TaskProgress,
        base: f64,
        span: f64,
    ) -> Result<(), ServiceError> {
        use ferrofin_traits::providers::MetadataRefreshMode;
        let cutoff = datetime_to_db(Utc::now() - chrono::Duration::days(PEOPLE_REFRESH_DAYS));
        let rows = crate::item_persistence_service::items_lacking_overview_or_primary(
            &self.db,
            PERSON_TYPE,
            &cutoff,
        )
        .await?;
        tracing::info!(count = rows.len(), "people needing image/overview refresh");
        let has: std::collections::HashMap<String, (bool, bool)> = rows
            .iter()
            .map(|(id, overview, image)| (id.clone(), (*overview, *image)))
            .collect();
        let ids: Vec<String> = rows.into_iter().map(|(id, _, _)| id).collect();
        let mode = |present: bool| {
            if present {
                MetadataRefreshMode::ValidationOnly
            } else {
                MetadataRefreshMode::FullRefresh
            }
        };
        let refreshed = self
            .refresh_people(&ids, progress, base, span, |id| {
                let (overview, image) = has.get(id).copied().unwrap_or_default();
                MetadataRefreshOptions {
                    metadata_refresh_mode: mode(overview),
                    image_refresh_mode: mode(image),
                    ..MetadataRefreshOptions::default()
                }
            })
            .await;
        tracing::info!(refreshed, "refreshed people missing images or overview");
        Ok(())
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for PeopleValidationTask {
    fn key(&self) -> &str {
        "RefreshPeople"
    }
    /// C# `PeopleValidationTask` implements `IConfigurableScheduledTask`, so the
    /// `GET /ScheduledTasks` `isHidden`/`isEnabled` filters apply to it.
    fn is_configurable(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "Refresh People"
    }
    fn description(&self) -> &str {
        "Updates metadata for actors and directors in your media library."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(7 * 24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        self.dedupe_and_orphans().await?;
        progress.report(33.0);
        self.validate_person_items().await?;
        self.refresh_new_person_items(progress, 50.0, 16.0).await?;
        progress.report(66.0);
        self.refresh_person_items(progress, 66.0, 34.0).await?;
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Download missing subtitles
// ---------------------------------------------------------------------------

/// "Download missing subtitles" — searches the internet for missing subtitles
/// based on metadata configuration. Port of `SubtitleScheduledTask` +
/// `SubtitleDownloader`: for every library with configured
/// `SubtitleDownloadLanguages`, finds movies/episodes lacking each language
/// (honoring the skip-if-embedded / skip-if-audio-matches options) and
/// downloads the best match through the subtitle-manager provider fan-out.
pub struct SubtitleDownloadTask {
    library: Arc<dyn LibraryManager>,
    folders: Arc<dyn VirtualFolderManager>,
    downloader: Arc<crate::subtitle_downloader::SubtitleDownloader>,
}

impl SubtitleDownloadTask {
    /// Builds the task with the same downloader used by library scans.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        downloader: Arc<crate::subtitle_downloader::SubtitleDownloader>,
    ) -> Self {
        Self {
            library,
            folders,
            downloader,
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for SubtitleDownloadTask {
    fn key(&self) -> &str {
        "DownloadSubtitles"
    }
    fn name(&self) -> &str {
        "Download missing subtitles"
    }
    fn description(&self) -> &str {
        "Searches the internet for missing subtitles based on metadata configuration."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let folders = self.folders.get_virtual_folders().await?;
        if !folders.iter().any(|folder| {
            folder
                .library_options
                .as_ref()
                .and_then(|options| options.subtitle_download_languages.as_ref())
                .is_some_and(|languages| !languages.is_empty())
        }) {
            progress.report(100.0);
            return Ok(());
        }
        let videos = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Movie],
                is_virtual_item: Some(false),
                source_types: vec![SourceType::Library],
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;
        let total = videos.len().max(1);
        for (index, video) in videos.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            progress.report(100.0 * (index as f64) / total as f64);
            let Some(options) = ferrofin_model::entities_media::owning_library(
                &folders,
                video.top_parent_id.as_deref(),
                video.path.as_deref(),
            )
            .and_then(|folder| folder.library_options.as_ref()) else {
                continue;
            };
            if !self.downloader.is_task_candidate(video, options).await? {
                continue;
            }
            self.downloader
                .download_missing(video, options, &crate::ScanCancel::new())
                .await;
        }
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Download missing lyrics
// ---------------------------------------------------------------------------

/// "Download missing lyrics" — downloads lyrics for songs. Port of
/// `LyricScheduledTask`: every audio item without lyrics is searched through
/// the lyric-manager provider fan-out and the first result is downloaded.
pub struct LyricDownloadTask {
    library: Arc<dyn LibraryManager>,
    lyrics: Arc<dyn LyricManager>,
}

impl LyricDownloadTask {
    /// Builds the task over the library and lyric-manager seams.
    #[must_use]
    pub fn new(library: Arc<dyn LibraryManager>, lyrics: Arc<dyn LyricManager>) -> Self {
        Self { library, lyrics }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for LyricDownloadTask {
    fn key(&self) -> &str {
        "DownloadLyrics"
    }
    fn name(&self) -> &str {
        "Download missing lyrics"
    }
    fn description(&self) -> &str {
        "Downloads lyrics for songs"
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            is_virtual_item: Some(false),
            recursive: true,
            ..InternalItemsQuery::default()
        };
        let total = self.library.get_count(&query).await?.max(0);
        let mut done = 0i32;
        let mut start_index = 0i32;
        while start_index < total {
            let items = page(&self.library, &query, start_index).await?;
            if items.is_empty() {
                break;
            }
            for item in &items {
                done += 1;
                progress.report(100.0 * f64::from(done) / f64::from(total.max(1)));
                let Ok(item_id) = Uuid::parse_str(&item.id) else {
                    continue;
                };
                // Only items with no lyrics yet (stream or sidecar).
                match self.lyrics.get_lyrics(item_id).await {
                    Ok(Some(_)) => continue,
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(item = %item.id, error = %e, "lyric lookup failed");
                        continue;
                    }
                }
                match self.lyrics.search_lyrics_automatically(item_id).await {
                    Ok(results) => {
                        if let Some(first) = results.first()
                            && let Err(e) = self.lyrics.download_lyrics(item_id, &first.id).await
                        {
                            tracing::warn!(item = %item.id, error = %e, "lyric download failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(item = %item.id, error = %e, "lyric search failed");
                    }
                }
            }
            start_index += PAGE_SIZE;
        }
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Generate Trickplay Images
// ---------------------------------------------------------------------------

/// "Generate Trickplay Images" — creates trickplay previews for videos in
/// enabled libraries. Port of `TrickplayImagesTask`: every non-virtual library
/// video is refreshed through the trickplay manager (which owns the per-width
/// generation and the already-generated skip).
pub struct TrickplayImagesTask {
    library: Arc<dyn LibraryManager>,
    folders: Arc<dyn VirtualFolderManager>,
    trickplay: Arc<dyn TrickplayManager>,
}

impl TrickplayImagesTask {
    /// Builds the task over the library, virtual-folder (per-library options)
    /// and trickplay-manager seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        trickplay: Arc<dyn TrickplayManager>,
    ) -> Self {
        Self {
            library,
            folders,
            trickplay,
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for TrickplayImagesTask {
    fn key(&self) -> &str {
        "RefreshTrickplayImages"
    }
    fn name(&self) -> &str {
        "Generate Trickplay Images"
    }
    fn description(&self) -> &str {
        "Creates trickplay previews for videos in enabled libraries."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::DailyTrigger,
            time_of_day_ticks: Some(3 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        }]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        let query = InternalItemsQuery {
            media_types: vec![MediaType::Video],
            source_types: vec![SourceType::Library],
            include_owned_items: true,
            is_virtual_item: Some(false),
            is_folder: Some(false),
            recursive: true,
            ..InternalItemsQuery::default()
        };
        let total = self.library.get_count(&query).await?.max(0);
        let mut done = 0i32;
        let mut start_index = 0i32;
        while start_index < total {
            let items = page(&self.library, &query, start_index).await?;
            if items.is_empty() {
                break;
            }
            for item in &items {
                done += 1;
                // The pin declares SourceTypes but never consumes it in query
                // translation. Retain physical Video kinds, including channel
                // VOD rows; the manager checks live-stream completion.
                if !crate::trickplay_manager::is_library_video(item) {
                    continue;
                }
                let folders = self.folders.get_virtual_folders().await?;
                let options = ferrofin_model::entities_media::owning_library(
                    &folders,
                    item.top_parent_id.as_deref(),
                    item.path.as_deref(),
                )
                .and_then(|folder| folder.library_options.clone())
                .unwrap_or_default();
                if let Ok(item_id) = Uuid::parse_str(&item.id)
                    && let Err(e) = self
                        .trickplay
                        .refresh_trickplay_data(item_id, false, &options)
                        .await
                {
                    tracing::warn!(item = %item.id, error = %e, "trickplay generation failed");
                }
                progress.report(100.0 * f64::from(done) / f64::from(total.max(1)));
            }
            start_index += PAGE_SIZE;
        }
        progress.report(100.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Media Segment Scan
// ---------------------------------------------------------------------------

/// "Media Segment Scan" — runs the media-segment providers over every eligible
/// library item. Port of `MediaSegmentExtractionTask`.
///
/// The item gating is upstream's, verbatim: video/audio media types, the
/// `Episode`/`Movie`/`Audio`/`AudioBook` kinds, non-virtual, library-sourced,
/// recursive, walked a page at a time, and only items whose file actually
/// exists on disk ("only local files supported"). Each such item is handed to
/// [`MediaSegmentManager::run_segment_providers`] — the port of upstream's
/// `RunSegmentPluginProviders`. (`source_types` is set for parity and is inert
/// on both sides: upstream's repository never filters on it either.)
///
/// Provider extraction failures are contained by the manager. A backend failure
/// for one item is logged by this task so the remaining library can be processed.
pub struct MediaSegmentExtractionTask {
    library: Arc<dyn LibraryManager>,
    media_segments: Arc<dyn MediaSegmentManager>,
}

impl MediaSegmentExtractionTask {
    /// Builds the task over the library and media-segment seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        media_segments: Arc<dyn MediaSegmentManager>,
    ) -> Self {
        Self {
            library,
            media_segments,
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for MediaSegmentExtractionTask {
    fn key(&self) -> &str {
        "TaskExtractMediaSegments"
    }
    fn name(&self) -> &str {
        "Media Segment Scan"
    }
    fn description(&self) -> &str {
        "Extracts or obtains media segments from MediaSegment enabled plugins."
    }
    fn category(&self) -> &str {
        LIBRARY
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(12)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        progress.report(0.0);
        let query = InternalItemsQuery {
            media_types: vec![MediaType::Video, MediaType::Audio],
            include_item_types: vec![
                BaseItemKind::Episode,
                BaseItemKind::Movie,
                BaseItemKind::Audio,
                BaseItemKind::AudioBook,
            ],
            is_virtual_item: Some(false),
            source_types: vec![SourceType::Library],
            recursive: true,
            ..InternalItemsQuery::default()
        };
        let total = self.library.get_count(&query).await?.max(0);
        let mut done = 0i32;
        let mut start_index = 0i32;
        let mut providers_run = 0usize;
        while start_index < total {
            let items = page(&self.library, &query, start_index).await?;
            if items.is_empty() {
                break;
            }
            for item in &items {
                done += 1;
                // Only local files are supported (upstream's `IsFileProtocol
                // && File.Exists`); a missing file is skipped, not an error.
                // `tokio::fs` because this stats every movie/episode/track in
                // the library — on a network mount a blocking stat per item
                // would stall a runtime worker for the whole pass.
                if let Some(path) = item.path.as_deref()
                    && tokio::fs::try_exists(path).await.unwrap_or(false)
                {
                    match Uuid::parse_str(&item.id) {
                        Ok(item_id) => match self
                            .media_segments
                            .run_segment_providers(item_id, false)
                            .await
                        {
                            Ok(ran) => providers_run += ran,
                            Err(e) => {
                                tracing::warn!(item = %item.id, path, error = %e, "media segment providers failed");
                            }
                        },
                        // A row whose id is not a GUID cannot be addressed by
                        // any provider; saying so keeps "4000 items, 0
                        // providers ran" answerable.
                        Err(e) => {
                            tracing::debug!(item = %item.id, error = %e, "skipping item with an unparseable id");
                        }
                    }
                }
                progress.report(100.0 * f64::from(done) / f64::from(total.max(1)));
            }
            start_index += PAGE_SIZE;
        }
        tracing::info!(items = done, providers_run, "media segment scan finished");
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_model::configuration::MediaPathInfo;
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::entities::MediaStreamType as StreamTypeEnum;
    use ferrofin_model::entities_media::ChapterInfo;
    use ferrofin_model::lyrics::{LyricDto, RemoteLyricInfoDto};
    use ferrofin_model::providers::LyricProviderInfo;
    use ferrofin_model::providers::RemoteSubtitleInfo;
    use ferrofin_model::providers::SubtitleProviderInfo;
    use ferrofin_traits::subtitles::SubtitleResponse;

    use super::*;
    use crate::db_error::media_stream_type_disc;
    use crate::test_support::{library_manager_over, seed_item, seed_named_item, test_db};

    // -- pure helpers -------------------------------------------------------

    #[test]
    fn parse_lufs_finds_the_integrated_summary_line() {
        let stderr = concat!(
            "[Parsed_ebur128_0 @ 0x1] Summary:\n",
            "\n",
            "  Integrated loudness:\n",
            "    I:         -23.1 LUFS\n",
            "    Threshold: -33.6 LUFS\n",
        );
        assert_eq!(parse_lufs(stderr), Some(-23.1));
        assert_eq!(parse_lufs("no summary here"), None);
        assert_eq!(parse_lufs("    I: not-a-number LUFS"), None);
        assert_eq!(parse_lufs("I: -21.0 LUFS"), None);
    }

    // -- fakes --------------------------------------------------------------

    /// A [`VirtualFolderManager`] fake serving a canned folder list.
    struct FakeFolders(Vec<VirtualFolderInfo>);

    #[async_trait]
    impl VirtualFolderManager for FakeFolders {
        async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
            Ok(self.0.clone())
        }
        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<CollectionTypeOptions>,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn rename_virtual_folder(
            &self,
            _name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn add_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn update_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn remove_media_path(
            &self,
            _virtual_folder_name: &str,
            _path: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn update_library_options(
            &self,
            _virtual_folder_name: &str,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    /// An [`FfmpegRunner`] fake returning canned stderr, recording each call.
    struct FakeRunner {
        stderr: String,
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl FfmpegRunner for FakeRunner {
        async fn run_stderr(
            &self,
            _program: &str,
            args: &[String],
        ) -> Result<String, ServiceError> {
            self.calls.lock().expect("lock").push(args.to_vec());
            Ok(self.stderr.clone())
        }
    }

    /// A [`MediaEncoder`] fake: fixed tool paths; `extract_video_image` writes
    /// a marker file next to the input (or fails when `fail` is set).
    struct FakeEncoder {
        fail_extract: bool,
    }

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "/bin/false".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _request: &ferrofin_traits::media_encoding::MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            unimplemented!("fake")
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unimplemented!("fake")
        }
        async fn extract_video_image(
            &self,
            input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            if self.fail_extract {
                return Err(ServiceError::backend("extract failed"));
            }
            let out = format!("{input_file}.image.jpg");
            std::fs::write(&out, b"jpg").map_err(|e| ServiceError::backend(e.to_string()))?;
            Ok(out)
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            input_file.to_owned()
        }
        fn get_time_parameter(&self, ticks: i64) -> String {
            ticks.to_string()
        }
        async fn convert_image(
            &self,
            _input_path: &str,
            _output_path: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    /// A [`MediaStreamRepository`] fake serving canned stream rows.
    struct FakeStreams(Vec<MediaStreamInfoEntity>);

    #[async_trait]
    impl MediaStreamRepository for FakeStreams {
        async fn get_media_streams(
            &self,
            filter: &MediaStreamQuery,
        ) -> Result<Vec<MediaStreamInfoEntity>, ServiceError> {
            Ok(self
                .0
                .iter()
                .filter(|s| {
                    filter
                        .stream_type
                        .is_none_or(|t| s.stream_type == media_stream_type_disc(t))
                })
                .cloned()
                .collect())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: StreamTypeEnum,
        ) -> Result<Vec<String>, ServiceError> {
            unimplemented!("fake")
        }
        async fn save_media_streams(
            &self,
            _item_id: Uuid,
            _streams: &[MediaStreamInfoEntity],
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    /// A [`SubtitleManager`] fake recording searches/downloads.
    #[derive(Default)]
    struct FakeSubtitles {
        searches: Mutex<Vec<SubtitleSearchRequest>>,
        downloads: Mutex<Vec<(Uuid, String)>>,
    }

    #[async_trait]
    impl SubtitleManager for FakeSubtitles {
        async fn search_subtitles(
            &self,
            request: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            self.searches.lock().expect("lock").push(request.clone());
            Ok(vec![RemoteSubtitleInfo {
                id: Some("sub-1".to_owned()),
                is_hash_match: Some(true),
                ..RemoteSubtitleInfo::default()
            }])
        }
        async fn download_subtitles(
            &self,
            item_id: Uuid,
            subtitle_id: &str,
        ) -> Result<(), ServiceError> {
            self.downloads
                .lock()
                .expect("lock")
                .push((item_id, subtitle_id.to_owned()));
            Ok(())
        }
        async fn upload_subtitle(
            &self,
            item_id: Uuid,
            response: &SubtitleResponse,
        ) -> Result<(), ServiceError> {
            self.downloads.lock().expect("lock").push((
                item_id,
                String::from_utf8(response.content.clone()).unwrap(),
            ));
            Ok(())
        }
        async fn get_remote_subtitles(&self, id: &str) -> Result<SubtitleResponse, ServiceError> {
            Ok(SubtitleResponse {
                content: id.as_bytes().to_vec(),
                ..Default::default()
            })
        }
        async fn delete_subtitles(&self, _item_id: Uuid, _index: i32) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<SubtitleProviderInfo>, ServiceError> {
            unimplemented!("fake")
        }
    }

    /// A [`LyricManager`] fake: `existing` items already have lyrics.
    #[derive(Default)]
    struct FakeLyrics {
        existing: Vec<Uuid>,
        downloads: Mutex<Vec<(Uuid, String)>>,
        automated_searches: Mutex<Vec<Uuid>>,
    }

    #[async_trait]
    impl LyricManager for FakeLyrics {
        async fn get_lyrics(&self, item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
            Ok(self.existing.contains(&item_id).then(LyricDto::default))
        }
        async fn search_lyrics(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
            Ok(vec![RemoteLyricInfoDto {
                id: "lrclib_42".to_owned(),
                provider_name: "LrcLib".to_owned(),
                lyrics: LyricDto::default(),
            }])
        }
        async fn search_lyrics_automatically(
            &self,
            item_id: Uuid,
        ) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
            self.automated_searches.lock().unwrap().push(item_id);
            self.search_lyrics(item_id).await
        }
        async fn download_lyrics(
            &self,
            item_id: Uuid,
            lyric_id: &str,
        ) -> Result<Option<LyricDto>, ServiceError> {
            self.downloads
                .lock()
                .expect("lock")
                .push((item_id, lyric_id.to_owned()));
            Ok(Some(LyricDto::default()))
        }
        async fn get_remote_lyrics(
            &self,
            _lyric_id: &str,
        ) -> Result<Option<LyricDto>, ServiceError> {
            unimplemented!("fake")
        }
        async fn save_lyric(
            &self,
            _item_id: Uuid,
            _format: &str,
            _lyrics: &str,
        ) -> Result<Option<LyricDto>, ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_lyrics(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
            unimplemented!("fake")
        }
    }

    /// A [`TrickplayManager`] fake recording refreshes.
    #[derive(Default)]
    struct FakeTrickplay {
        /// `(item, library option "extraction enabled")` per refresh call.
        refreshed: Mutex<Vec<(Uuid, bool)>>,
        after_refresh: Option<(Arc<dyn VirtualFolderManager>, String, LibraryOptions)>,
    }

    #[async_trait]
    impl TrickplayManager for FakeTrickplay {
        async fn refresh_trickplay_data(
            &self,
            item_id: Uuid,
            _replace: bool,
            library_options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            self.refreshed
                .lock()
                .expect("lock")
                .push((item_id, library_options.enable_trickplay_image_extraction));
            if let Some((folders, name, options)) = &self.after_refresh {
                folders.update_library_options(name, options).await?;
            }
            Ok(())
        }
        async fn get_trickplay_resolutions(
            &self,
            _item_id: Uuid,
        ) -> Result<
            std::collections::HashMap<i32, ferrofin_db::entities::playback::TrickplayInfoEntity>,
            ServiceError,
        > {
            unimplemented!("fake")
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<ferrofin_db::entities::playback::TrickplayInfoEntity>, ServiceError>
        {
            unimplemented!("fake")
        }
        async fn save_trickplay_info(
            &self,
            _info: &ferrofin_db::entities::playback::TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_trickplay_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_trickplay_manifest(
            &self,
            _item_id: Uuid,
        ) -> Result<
            std::collections::HashMap<
                String,
                std::collections::HashMap<
                    i32,
                    ferrofin_db::entities::playback::TrickplayInfoEntity,
                >,
            >,
            ServiceError,
        > {
            unimplemented!("fake")
        }
        async fn get_hls_playlist(
            &self,
            _item_id: Uuid,
            _width: i32,
            _api_key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_trickplay_tile_path(
            &self,
            _item_id: Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            unimplemented!("fake")
        }
    }

    /// A [`ChapterManager`] fake holding one item's chapters in memory.
    struct FakeChapters {
        chapters: Mutex<Vec<ChapterInfo>>,
        fail_read: std::sync::atomic::AtomicBool,
        fail_save: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl ChapterManager for FakeChapters {
        async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn save_chapters(
            &self,
            _item_id: Uuid,
            chapters: &[ChapterInfo],
        ) -> Result<(), ServiceError> {
            if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ServiceError::backend("temporary chapter save failure"));
            }
            *self.chapters.lock().expect("lock") = chapters.to_vec();
            Ok(())
        }
        async fn get_chapter(
            &self,
            _item_id: Uuid,
            _index: i32,
        ) -> Result<Option<ChapterInfo>, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
            if self.fail_read.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ServiceError::backend("temporary chapter read failure"));
            }
            Ok(self.chapters.lock().expect("lock").clone())
        }
        async fn delete_chapter_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    // -- db seed helpers ----------------------------------------------------

    async fn set_path(db: &ferrofin_db::Database, id: Uuid, path: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?1 WHERE "Id" = ?2"#)
            .bind(path)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("set path");
    }

    async fn set_media_type(db: &ferrofin_db::Database, id: Uuid, media_type: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "MediaType" = ?1 WHERE "Id" = ?2"#)
            .bind(media_type)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("set media type");
    }

    async fn add_ancestor(db: &ferrofin_db::Database, item: Uuid, ancestor: Uuid) {
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(item))
            .bind(guid_to_db(ancestor))
            .execute(db.writer())
            .await
            .expect("ancestor");
    }

    fn folder_with(options: LibraryOptions, item_id: Option<Uuid>, location: &str) -> FakeFolders {
        FakeFolders(vec![VirtualFolderInfo {
            name: Some("Lib".into()),
            locations: vec![location.to_owned()],
            library_options: Some(options),
            item_id: item_id.map(|i| i.to_string()),
            ..VirtualFolderInfo::default()
        }])
    }

    // -- Audio Normalization ------------------------------------------------

    #[tokio::test]
    async fn audio_normalization_measures_albums_and_tracks() {
        let db = test_db().await;
        let media = tempfile::tempdir().expect("tempdir");
        let library = library_manager_over(db.clone());

        let folder = Uuid::from_u128(0xF0);
        let album = Uuid::from_u128(0xA0);
        let (t1, t2) = (Uuid::from_u128(0xA1), Uuid::from_u128(0xA2));
        seed_item(&db, folder, BaseItemKind::Folder).await;
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
        seed_item(&db, t1, BaseItemKind::Audio).await;
        seed_item(&db, t2, BaseItemKind::Audio).await;
        for (track, name) in [(t1, "t1.flac"), (t2, "t2.flac")] {
            let path = media.path().join(name);
            std::fs::write(&path, b"x").expect("write");
            set_path(&db, track, &path.to_string_lossy()).await;
            add_ancestor(&db, track, album).await;
            add_ancestor(&db, track, folder).await;
        }
        add_ancestor(&db, album, folder).await;

        let runner = Arc::new(FakeRunner {
            stderr: "    I:         -21.5 LUFS\n".to_owned(),
            calls: Mutex::new(Vec::new()),
        });
        let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            media.path().join("data"),
            media.path().join("logs"),
            media.path().join("config"),
            media.path().join("cache"),
            media.path().join("web"),
        ));
        let task = AudioNormalizationTask::new(
            db.clone(),
            library,
            Arc::new(folder_with(
                LibraryOptions {
                    enable_lufs_scan: true,
                    ..LibraryOptions::default()
                },
                Some(folder),
                &media.path().to_string_lossy(),
            )),
            Arc::new(FakeEncoder {
                fail_extract: false,
            }),
            runner.clone(),
            paths,
        );
        assert_eq!(task.key(), "AudioNormalization");
        task.execute(&TaskProgress::default()).await.expect("run");

        let lufs: Vec<(String, Option<f64>)> =
            sqlx::query_as(r#"SELECT "Id", "LUFS" FROM "BaseItems" WHERE "LUFS" IS NOT NULL"#)
                .fetch_all(db.pool())
                .await
                .expect("query");
        let with_lufs: Vec<&str> = lufs.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            with_lufs.contains(&guid_to_db(album).as_str()),
            "album measured"
        );
        assert!(
            with_lufs.contains(&guid_to_db(t1).as_str()),
            "track 1 measured"
        );
        assert!(
            with_lufs.contains(&guid_to_db(t2).as_str()),
            "track 2 measured"
        );
        assert!(lufs.iter().all(|(_, v)| *v == Some(-21.5)));

        // One concat (album) + two per-track runs.
        let calls = runner.calls.lock().expect("lock");
        assert_eq!(calls.len(), 3);
        assert!(calls[0].iter().any(|a| a == "concat"));
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn normalization_analysis_follows_saved_flags_and_preserves_embedded_gain() {
        use ferrofin_model::configuration::MediaPathInfo;
        use ferrofin_traits::persistence::ItemPersistenceService;
        struct LocalConcatRunner(Arc<FakeRunner>);
        #[async_trait]
        impl FfmpegRunner for LocalConcatRunner {
            async fn run_stderr(
                &self,
                program: &str,
                args: &[String],
            ) -> Result<String, ServiceError> {
                if args.iter().any(|argument| argument == "concat") {
                    let input = args.iter().position(|argument| argument == "-i").unwrap() + 1;
                    let list = std::fs::read_to_string(&args[input]).unwrap();
                    assert_eq!(list.lines().count(), 2);
                    assert!(
                        !list.contains("https://"),
                        "remote tracks must not enter album analysis"
                    );
                }
                self.0.run_stderr(program, args).await
            }
        }
        let db = test_db().await;
        let temp = tempfile::tempdir().unwrap();
        let media = temp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let folders = Arc::new(
            crate::FerrofinVirtualFolderManager::new(temp.path().join("libraries"))
                .with_item_store(Arc::new(crate::FerrofinItemPersistenceService::new(
                    db.clone(),
                ))),
        );
        let mut options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            enable_lufs_scan: false,
            ..Default::default()
        };
        folders
            .add_virtual_folder("Music", Some(CollectionTypeOptions::music), &options)
            .await
            .unwrap();
        let folder = Uuid::parse_str(
            folders.get_virtual_folders().await.unwrap()[0]
                .item_id
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let album_id = Uuid::new_v4();
        let gain_track = Uuid::new_v4();
        let fresh_track = Uuid::new_v4();
        let remote_track = Uuid::new_v4();
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        let mut album = BaseItemEntity {
            id: guid_to_db(album_id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::MusicAlbum)
                .unwrap()
                .to_owned(),
            name: Some("Album".to_owned()),
            normalization_gain: Some(-3.0),
            ..Default::default()
        };
        persistence.save_items(&[album.clone()]).await.unwrap();
        add_ancestor(&db, album_id, folder).await;
        for (id, file, gain) in [
            (gain_track, "gain.flac", Some(-2.0)),
            (fresh_track, "fresh.flac", None),
            (remote_track, "remote.flac", None),
        ] {
            let path = if id == remote_track {
                "https://example.invalid/remote.flac".to_owned()
            } else {
                let path = media.join(file);
                std::fs::write(&path, b"audio").unwrap();
                path.to_string_lossy().into_owned()
            };
            persistence
                .save_items(&[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Audio)
                        .unwrap()
                        .to_owned(),
                    path: Some(path),
                    normalization_gain: gain,
                    ..Default::default()
                }])
                .await
                .unwrap();
            add_ancestor(&db, id, album_id).await;
            add_ancestor(&db, id, folder).await;
        }
        let runner = Arc::new(FakeRunner {
            stderr: "    I: -21.5 LUFS\n".to_owned(),
            calls: Mutex::new(Vec::new()),
        });
        let task = AudioNormalizationTask::new(
            db.clone(),
            library_manager_over(db.clone()),
            folders.clone(),
            Arc::new(FakeEncoder {
                fail_extract: false,
            }),
            Arc::new(LocalConcatRunner(runner.clone())),
            Arc::new(crate::FerrofinServerApplicationPaths::new(
                temp.path().join("data"),
                temp.path().join("logs"),
                temp.path().join("config"),
                temp.path().join("cache"),
                temp.path().join("web"),
            )),
        );
        task.execute(&TaskProgress::default()).await.unwrap();
        assert!(runner.calls.lock().unwrap().is_empty());
        options.enable_lufs_scan = true;
        folders
            .update_library_options("Music", &options)
            .await
            .unwrap();
        task.execute(&TaskProgress::default()).await.unwrap();
        let rows = library_manager_over(db.clone());
        assert_eq!(
            rows.get_item_by_id(album_id)
                .await
                .unwrap()
                .unwrap()
                .normalization_gain,
            Some(-3.0)
        );
        assert_eq!(
            rows.get_item_by_id(album_id).await.unwrap().unwrap().lufs,
            None
        );
        assert_eq!(
            rows.get_item_by_id(gain_track).await.unwrap().unwrap().lufs,
            None
        );
        assert_eq!(
            rows.get_item_by_id(fresh_track)
                .await
                .unwrap()
                .unwrap()
                .lufs,
            Some(-21.5)
        );
        assert_eq!(
            rows.get_item_by_id(remote_track)
                .await
                .unwrap()
                .unwrap()
                .lufs,
            None
        );
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            1,
            "only the unmeasured local track is eligible"
        );
        // Remove the embedded album gain: two local tracks now make album gain
        // useful, while the remote URL must never enter its concat list.
        album.normalization_gain = None;
        persistence.save_items(&[album]).await.unwrap();
        options.enable_lufs_scan = false;
        folders
            .update_library_options("Music", &options)
            .await
            .unwrap();
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            1,
            "saved disable prevents later album work"
        );
        options.enable_lufs_scan = true;
        folders
            .update_library_options("Music", &options)
            .await
            .unwrap();
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(
            rows.get_item_by_id(album_id).await.unwrap().unwrap().lufs,
            Some(-21.5)
        );
        assert_eq!(runner.calls.lock().unwrap().len(), 2);
        let concat = runner.calls.lock().unwrap()[1].clone();
        assert!(concat.iter().any(|arg| arg == "concat"));
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            2,
            "stored LUFS prevents repeat measurement"
        );
    }

    #[tokio::test]
    async fn normalization_spawn_failure_does_not_abort_later_tracks() {
        struct FailOnce(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl FfmpegRunner for FailOnce {
            async fn run_stderr(&self, _: &str, _: &[String]) -> Result<String, ServiceError> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Err(ServiceError::backend("fixture failed to start ffmpeg"))
                } else {
                    Ok("    I: -24.0 LUFS\n".to_owned())
                }
            }
        }
        let db = test_db().await;
        let temp = tempfile::tempdir().unwrap();
        let folder = Uuid::new_v4();
        seed_item(&db, folder, BaseItemKind::Folder).await;
        let tracks = [Uuid::new_v4(), Uuid::new_v4()];
        for track in tracks {
            seed_item(&db, track, BaseItemKind::Audio).await;
            let path = temp.path().join(format!("{track}.flac"));
            std::fs::write(&path, b"audio").unwrap();
            set_path(&db, track, &path.to_string_lossy()).await;
            add_ancestor(&db, track, folder).await;
        }
        let runner = Arc::new(FailOnce(std::sync::atomic::AtomicUsize::new(0)));
        let library = library_manager_over(db.clone());
        let task = AudioNormalizationTask::new(
            db,
            library.clone(),
            Arc::new(folder_with(
                LibraryOptions {
                    enable_lufs_scan: true,
                    ..Default::default()
                },
                Some(folder),
                &temp.path().to_string_lossy(),
            )),
            Arc::new(FakeEncoder {
                fail_extract: false,
            }),
            runner.clone(),
            Arc::new(crate::FerrofinServerApplicationPaths::new(
                temp.path().join("data"),
                temp.path().join("logs"),
                temp.path().join("config"),
                temp.path().join("cache"),
                temp.path().join("web"),
            )),
        );
        let progress = TaskProgress::default();
        task.execute(&progress).await.unwrap();
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        let mut measured = 0;
        for track in tracks {
            measured += usize::from(
                library.get_item_by_id(track).await.unwrap().unwrap().lufs == Some(-24.0),
            );
        }
        assert_eq!(
            measured, 1,
            "the later track is measured after the failed spawn"
        );
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn audio_normalization_without_enabled_libraries_is_a_noop() {
        let db = test_db().await;
        let library = library_manager_over(db.clone());
        let runner = Arc::new(FakeRunner {
            stderr: String::new(),
            calls: Mutex::new(Vec::new()),
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            dir.path().join("data"),
            dir.path().join("logs"),
            dir.path().join("config"),
            dir.path().join("cache"),
            dir.path().join("web"),
        ));
        let task = AudioNormalizationTask::new(
            db,
            library,
            Arc::new(FakeFolders(Vec::new())),
            Arc::new(FakeEncoder {
                fail_extract: false,
            }),
            runner.clone(),
            paths,
        );
        task.execute(&TaskProgress::default()).await.expect("run");
        assert!(runner.calls.lock().expect("lock").is_empty());
    }

    // -- Download missing subtitles ------------------------------------------

    #[tokio::test]
    async fn subtitle_download_skips_satisfied_languages() {
        let db = test_db().await;
        let media = tempfile::tempdir().expect("tempdir");
        let library = library_manager_over(db.clone());

        let movie = Uuid::from_u128(0x51);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        let path = media.path().join("film.mkv");
        std::fs::write(&path, b"x").expect("write");
        set_path(&db, movie, &path.to_string_lossy()).await;

        // A German audio track satisfies "ger" via skip-if-audio-matches;
        // "eng" has nothing and must be searched + downloaded.
        let streams = FakeStreams(vec![MediaStreamInfoEntity {
            item_id: movie.to_string(),
            stream_index: 0,
            stream_type: media_stream_type_disc(StreamTypeEnum::Audio),
            language: Some("ger".to_owned()),
            ..MediaStreamInfoEntity::default()
        }]);
        let subtitles = Arc::new(FakeSubtitles::default());
        let task = SubtitleDownloadTask::new(
            library,
            Arc::new(folder_with(
                LibraryOptions {
                    subtitle_download_languages: Some(vec!["eng".to_owned(), "ger".to_owned()]),
                    skip_subtitles_if_audio_track_matches: true,
                    ..LibraryOptions::default()
                },
                None,
                &media.path().to_string_lossy(),
            )),
            Arc::new(crate::subtitle_downloader::SubtitleDownloader::new(
                &(subtitles.clone() as Arc<dyn SubtitleManager>),
                Arc::new(streams),
            )),
        );
        assert_eq!(task.key(), "DownloadSubtitles");
        task.execute(&TaskProgress::default()).await.expect("run");

        let searches = subtitles.searches.lock().expect("lock");
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0].language, "eng");
        assert!(searches[0].is_automated);
        let downloads = subtitles.downloads.lock().expect("lock");
        assert_eq!(downloads.as_slice(), &[(movie, "sub-1".to_owned())]);
    }

    // -- Download missing lyrics ---------------------------------------------

    #[tokio::test]
    async fn lyric_download_targets_only_items_without_lyrics() {
        let db = test_db().await;
        let library = library_manager_over(db.clone());
        let (has, missing) = (Uuid::from_u128(0x61), Uuid::from_u128(0x62));
        seed_item(&db, has, BaseItemKind::Audio).await;
        seed_item(&db, missing, BaseItemKind::Audio).await;

        let lyrics = Arc::new(FakeLyrics {
            existing: vec![has],
            downloads: Mutex::new(Vec::new()),
            automated_searches: Mutex::new(Vec::new()),
        });
        let task = LyricDownloadTask::new(library, lyrics.clone());
        assert_eq!(task.key(), "DownloadLyrics");
        task.execute(&TaskProgress::default()).await.expect("run");

        let downloads = lyrics.downloads.lock().expect("lock");
        assert_eq!(downloads.as_slice(), &[(missing, "lrclib_42".to_owned())]);
        assert_eq!(
            lyrics.automated_searches.lock().unwrap().as_slice(),
            &[missing]
        );
    }

    // -- Generate Trickplay Images -------------------------------------------

    #[tokio::test]
    async fn trickplay_task_refreshes_every_video() {
        let db = test_db().await;
        let library = library_manager_over(db.clone());
        let (v1, v2) = (Uuid::from_u128(0x71), Uuid::from_u128(0x72));
        for v in [v1, v2] {
            seed_item(&db, v, BaseItemKind::Movie).await;
            set_media_type(&db, v, "Video").await;
        }

        // v1 lives in a library with extraction on; v2 outside any library
        // (C# `GetLibraryOptions` → default options, extraction off).
        set_path(&db, v1, "/media/movies/a.mkv").await;
        set_path(&db, v2, "/elsewhere/b.mkv").await;

        let trickplay = Arc::new(FakeTrickplay::default());
        let task = TrickplayImagesTask::new(
            library,
            Arc::new(folder_with(
                LibraryOptions {
                    enable_trickplay_image_extraction: true,
                    ..LibraryOptions::default()
                },
                None,
                "/media/movies",
            )),
            trickplay.clone(),
        );
        assert_eq!(task.key(), "RefreshTrickplayImages");
        assert_eq!(
            task.default_triggers()[0].type_,
            TaskTriggerInfoType::DailyTrigger
        );
        task.execute(&TaskProgress::default()).await.expect("run");

        let mut refreshed = trickplay.refreshed.lock().expect("lock").clone();
        refreshed.sort();
        assert_eq!(refreshed, vec![(v1, true), (v2, false)]);
    }

    async fn trickplay_nested_libraries(
        db: &Database,
        directory: &Path,
    ) -> (
        Arc<crate::FerrofinVirtualFolderManager>,
        LibraryOptions,
        Uuid,
        Uuid,
    ) {
        use ferrofin_model::configuration::MediaPathInfo;
        let folders = Arc::new(
            crate::FerrofinVirtualFolderManager::new(directory.join("libraries")).with_item_store(
                Arc::new(crate::FerrofinItemPersistenceService::new(db.clone())),
            ),
        );
        let broad = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: directory.join("media").to_string_lossy().into_owned(),
            }],
            ..Default::default()
        };
        let deep = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: directory.join("media/deep").to_string_lossy().into_owned(),
            }],
            enable_trickplay_image_extraction: true,
            ..Default::default()
        };
        std::fs::create_dir_all(directory.join("media/deep")).unwrap();
        folders
            .add_virtual_folder("Broad", Some(CollectionTypeOptions::movies), &broad)
            .await
            .unwrap();
        folders
            .add_virtual_folder("Deep", Some(CollectionTypeOptions::movies), &deep)
            .await
            .unwrap();
        let configured = folders.get_virtual_folders().await.unwrap();
        let id = |name| {
            configured
                .iter()
                .find(|folder| folder.name.as_deref() == Some(name))
                .and_then(|folder| folder.item_id.as_deref())
                .and_then(|id| Uuid::parse_str(id).ok())
                .unwrap()
        };
        (folders, deep, id("Broad"), id("Deep"))
    }

    #[tokio::test]
    async fn trickplay_task_uses_physical_owner_and_deepest_path_and_includes_owned_videos() {
        let db = test_db().await;
        let temp = tempfile::tempdir().unwrap();
        let (folders, _, broad, _) = trickplay_nested_libraries(&db, temp.path()).await;
        let parent_video = Uuid::from_u128(0xC300);
        let extra_video = Uuid::from_u128(0xC301);
        let physical = Uuid::from_u128(0xC302);
        let channel = Uuid::from_u128(0xC303);
        for (id, name) in [
            (parent_video, "Owner"),
            (extra_video, "Owned"),
            (physical, "Physical"),
            (channel, "Channel"),
        ] {
            seed_named_item(&db, id, BaseItemKind::Movie, name).await;
            let mut entity = crate::test_support::fetch_item(&db, id).await;
            entity.media_type = Some("Video".to_owned());
            entity.path = Some(
                temp.path()
                    .join(format!("media/deep/{name}.mkv"))
                    .to_string_lossy()
                    .into_owned(),
            );
            if id == extra_video {
                entity.owner_id = Some(guid_to_db(parent_video));
            }
            if id == physical {
                entity.top_parent_id = Some(guid_to_db(broad));
            }
            if id == channel {
                entity.channel_id = Some(Uuid::new_v4().to_string());
            }
            crate::test_support::save_item(&db, &entity).await;
        }
        let manager = Arc::new(FakeTrickplay::default());
        let task = TrickplayImagesTask::new(library_manager_over(db), folders, manager.clone());
        task.execute(&TaskProgress::default()).await.unwrap();
        let mut calls = manager.refreshed.lock().unwrap().clone();
        calls.sort();
        let mut expected = vec![
            (parent_video, true),
            (extra_video, true),
            (physical, false),
            (channel, true),
        ];
        expected.sort();
        assert_eq!(
            calls, expected,
            "source library videos use physical ownership before deepest paths and include extra_video extras"
        );
    }

    #[tokio::test]
    async fn trickplay_task_rereads_saved_options_between_items() {
        let db = test_db().await;
        let temp = tempfile::tempdir().unwrap();
        let (folders, mut options, _, deep) = trickplay_nested_libraries(&db, temp.path()).await;
        for (id, name) in [
            (Uuid::from_u128(0xC310), "A"),
            (Uuid::from_u128(0xC311), "B"),
        ] {
            seed_named_item(&db, id, BaseItemKind::Movie, name).await;
            let mut entity = crate::test_support::fetch_item(&db, id).await;
            entity.media_type = Some("Video".to_owned());
            entity.top_parent_id = Some(guid_to_db(deep));
            entity.path = Some(
                temp.path()
                    .join(format!("media/deep/{name}.mkv"))
                    .to_string_lossy()
                    .into_owned(),
            );
            crate::test_support::save_item(&db, &entity).await;
        }
        options.enable_trickplay_image_extraction = false;
        let manager = Arc::new(FakeTrickplay {
            after_refresh: Some((folders.clone(), "Deep".to_owned(), options)),
            ..Default::default()
        });
        let task = TrickplayImagesTask::new(library_manager_over(db), folders, manager.clone());
        task.execute(&TaskProgress::default()).await.unwrap();
        let calls = manager.refreshed.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1);
        assert!(
            !calls[1].1,
            "a saved disable reaches the next item in the same task"
        );
    }

    // -- Media Segment Scan --------------------------------------------------

    /// A media-segment manager recording which items the providers were run
    /// over. Everything else is unreachable from this task.
    #[derive(Default)]
    struct FakeSegments {
        ran: Mutex<Vec<Uuid>>,
    }

    #[async_trait]
    impl MediaSegmentManager for FakeSegments {
        async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn create_segment(
            &self,
            _segment: &ferrofin_model::media_segments::MediaSegmentDto,
            _segment_provider_id: &str,
        ) -> Result<ferrofin_model::media_segments::MediaSegmentDto, ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_segment(&self, _segment_id: Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn delete_provider_segments(
            &self,
            _item_id: Uuid,
            _provider_id: &str,
            _type_filter: Option<ferrofin_model::media_segments::MediaSegmentType>,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_segments(
            &self,
            _item_id: Uuid,
            _type_filter: Option<&[ferrofin_model::media_segments::MediaSegmentType]>,
            _filter_by_provider: bool,
        ) -> Result<Vec<ferrofin_model::media_segments::MediaSegmentDto>, ServiceError> {
            unimplemented!("fake")
        }
        async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_traits::media_segments::MediaSegmentProviderInfo>, ServiceError>
        {
            Ok(Vec::new())
        }
        async fn run_segment_providers(
            &self,
            item_id: Uuid,
            overwrite: bool,
        ) -> Result<usize, ServiceError> {
            assert!(!overwrite, "upstream passes overwrite: false");
            self.ran.lock().expect("lock").push(item_id);
            Ok(1)
        }
    }

    #[tokio::test]
    async fn media_segment_scan_runs_providers_over_local_files_only() {
        let db = test_db().await;
        let library = library_manager_over(db.clone());
        let dir = tempfile::tempdir().expect("tempdir");

        // Two eligible items with a real file, one whose file is missing, and
        // one of an ineligible kind.
        let (present_a, present_b) = (Uuid::from_u128(0xA1), Uuid::from_u128(0xA2));
        for (id, kind, name) in [
            (present_a, BaseItemKind::Movie, "a.mkv"),
            (present_b, BaseItemKind::Episode, "b.mkv"),
        ] {
            seed_item(&db, id, kind).await;
            set_media_type(&db, id, "Video").await;
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").expect("write");
            set_path(&db, id, &path.to_string_lossy()).await;
        }
        let missing = Uuid::from_u128(0xA3);
        seed_item(&db, missing, BaseItemKind::Movie).await;
        set_media_type(&db, missing, "Video").await;
        set_path(&db, missing, &dir.path().join("gone.mkv").to_string_lossy()).await;
        let series = Uuid::from_u128(0xA4);
        seed_item(&db, series, BaseItemKind::Series).await;

        let segments = Arc::new(FakeSegments::default());
        let task = MediaSegmentExtractionTask::new(library, segments.clone());

        assert_eq!(task.key(), "TaskExtractMediaSegments");
        assert_eq!(task.name(), "Media Segment Scan");
        assert_eq!(task.category(), "Library");
        assert_eq!(
            task.description(),
            "Extracts or obtains media segments from MediaSegment enabled plugins."
        );
        let triggers = task.default_triggers();
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].type_, TaskTriggerInfoType::IntervalTrigger);
        // 12 hours, matching the oracle's `IntervalTicks`.
        assert_eq!(triggers[0].interval_ticks, Some(432_000_000_000));

        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");

        let mut ran = segments.ran.lock().expect("lock").clone();
        ran.sort();
        let mut expected = vec![present_a, present_b];
        expected.sort();
        assert_eq!(
            ran, expected,
            "only items with a local file are handed over"
        );
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn media_segment_scan_pages_past_the_first_page() {
        // The page size is 100; a fixture smaller than that never exercises the
        // `start_index` walk, which is exactly where an off-by-one hides.
        let db = test_db().await;
        let library = library_manager_over(db.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.mkv");
        std::fs::write(&path, b"x").expect("write");

        let count = usize::try_from(PAGE_SIZE).expect("page size") + 37;
        for n in 0..count {
            let id = Uuid::from_u128(0x1000 + n as u128);
            seed_item(&db, id, BaseItemKind::Movie).await;
            set_media_type(&db, id, "Video").await;
            set_path(&db, id, &path.to_string_lossy()).await;
        }
        // A virtual item (an episode the series has but the disk does not) must
        // not be handed over even though it matches on kind and media type.
        let virtual_id = Uuid::from_u128(0x2000);
        crate::test_support::seed_episode(&db, virtual_id, "series-key", 1, 1, true, None).await;
        set_media_type(&db, virtual_id, "Video").await;
        set_path(&db, virtual_id, &path.to_string_lossy()).await;

        let segments = Arc::new(FakeSegments::default());
        let task = MediaSegmentExtractionTask::new(library, segments.clone());
        task.execute(&TaskProgress::default()).await.expect("run");

        let ran = segments.ran.lock().expect("lock").clone();
        assert_eq!(ran.len(), count, "every page was walked");
        assert!(!ran.contains(&virtual_id), "virtual items are skipped");
    }

    // -- Refresh People ------------------------------------------------------

    #[tokio::test]
    async fn people_validation_dedupes_and_removes_orphans() {
        use ferrofin_traits::providers::MetadataRefreshMode::FullRefresh;
        let db = test_db().await;
        let item = Uuid::from_u128(0x81);
        seed_item(&db, item, BaseItemKind::Movie).await;

        let insert_person = |id: Uuid, name: &str| {
            let db = db.clone();
            let name = name.to_owned();
            async move {
                sqlx::query(
                    r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType") VALUES (?1, ?2, 'Actor')"#,
                )
                .bind(guid_to_db(id))
                .bind(name)
                .execute(db.writer())
                .await
                .expect("person");
            }
        };
        let (keep, dup, orphan) = (
            Uuid::from_u128(0x91),
            Uuid::from_u128(0x92),
            Uuid::from_u128(0x93),
        );
        insert_person(keep, "John Smith").await;
        insert_person(dup, "John Smith").await;
        insert_person(orphan, "Ghost").await;
        // The duplicate person carries the only item link.
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role") VALUES (?1, ?2, 'Hero')"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(dup))
        .execute(db.writer())
        .await
        .expect("map");

        // A person item backing "John Smith" (kept + refreshed, no overview)
        // and one for a name with no people row (deleted).
        let (person_item, dead_item) = (Uuid::from_u128(0xA1), Uuid::from_u128(0xA2));
        seed_named_item(&db, person_item, BaseItemKind::Person, "John Smith").await;
        seed_named_item(&db, dead_item, BaseItemKind::Person, "Gone").await;

        // A person already refreshed with everything it needs: left alone.
        let complete = Uuid::from_u128(0xA3);
        seed_named_item(&db, complete, BaseItemKind::Person, "John Smith").await;
        crate::item_persistence_service::seed_refreshed_overview(
            &db,
            complete,
            "2026-09-01 00:00:00.0000000",
            "Bio.",
        )
        .await;
        crate::item_persistence_service::seed_primary_image(
            &db,
            Uuid::from_u128(0xA4),
            complete,
            "/p.jpg",
        )
        .await;

        let providers = Arc::new(RecordingProviders::default());
        let task = PeopleValidationTask::new(db.clone(), providers.clone());
        assert_eq!(task.key(), "RefreshPeople");
        task.execute(&TaskProgress::default()).await.expect("run");

        let people: Vec<(String, String)> = sqlx::query_as(r#"SELECT "Id", "Name" FROM "Peoples""#)
            .fetch_all(db.pool())
            .await
            .expect("people");
        assert_eq!(people.len(), 1, "dup merged, orphan removed");
        assert_eq!(people[0].0, guid_to_db(keep), "first id survives");

        let mapped: Vec<String> =
            sqlx::query_scalar(r#"SELECT "PeopleId" FROM "PeopleBaseItemMap""#)
                .fetch_all(db.pool())
                .await
                .expect("map");
        assert_eq!(mapped, vec![guid_to_db(keep)], "link re-pointed");

        let person_items: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" WHERE "Type" = 'MediaBrowser.Controller.Entities.Person'"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("items");
        assert_eq!(
            person_items,
            vec![guid_to_db(person_item), guid_to_db(complete)],
            "dead item removed"
        );

        // `PeopleValidator` refreshes the never-refreshed person with the
        // default options (its first refresh runs the providers); the
        // image/overview pass then asks a full refresh of both halves it
        // still lacks (the fake stamps nothing). The complete, recently
        // refreshed person is refreshed by neither.
        let refreshed = providers.refreshed.lock().expect("lock").clone();
        let modes: Vec<(Uuid, _, _)> = refreshed
            .iter()
            .map(|(id, o)| (*id, o.metadata_refresh_mode, o.image_refresh_mode))
            .collect();
        assert_eq!(
            modes,
            vec![
                (
                    person_item,
                    ferrofin_traits::providers::MetadataRefreshMode::Default,
                    ferrofin_traits::providers::MetadataRefreshMode::Default
                ),
                (person_item, FullRefresh, FullRefresh),
            ],
        );
    }

    /// The image/overview pass fetches only the half a person lacks: a
    /// person with an overview but no image asks a `FullRefresh` of its
    /// images and `ValidationOnly` of its metadata.
    #[tokio::test]
    async fn the_people_image_pass_fetches_only_the_missing_half() {
        use ferrofin_traits::providers::MetadataRefreshMode::{FullRefresh, ValidationOnly};
        let db = test_db().await;
        let person = Uuid::from_u128(0xB1);
        seed_named_item(&db, person, BaseItemKind::Person, "Jane Doe").await;
        crate::people_repository::seed_people_row(&db, Uuid::from_u128(0xB2), "Jane Doe", "Actor")
            .await;
        crate::item_persistence_service::seed_refreshed_overview(
            &db,
            person,
            "2020-01-01 00:00:00.0000000",
            "Bio.",
        )
        .await;
        let providers = Arc::new(RecordingProviders::default());
        let task = PeopleValidationTask::new(db.clone(), providers.clone());
        task.refresh_new_person_items(&TaskProgress::default(), 0.0, 1.0)
            .await
            .expect("new people");
        assert!(
            providers.refreshed.lock().expect("lock").is_empty(),
            "an already-refreshed person is not new"
        );
        task.refresh_person_items(&TaskProgress::default(), 0.0, 1.0)
            .await
            .expect("image pass");
        let refreshed = providers.refreshed.lock().expect("lock").clone();
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].0, person);
        assert_eq!(refreshed[0].1.metadata_refresh_mode, ValidationOnly);
        assert_eq!(refreshed[0].1.image_refresh_mode, FullRefresh);
    }

    /// A [`ProviderManager`] fake recording `refresh_single_item` calls.
    #[derive(Default)]
    struct RecordingProviders {
        refreshed: Mutex<Vec<(Uuid, MetadataRefreshOptions)>>,
    }

    #[async_trait]
    impl ProviderManager for RecordingProviders {
        async fn queue_refresh(
            &self,
            _item_id: Uuid,
            _options: &MetadataRefreshOptions,
            _priority: ferrofin_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn refresh_single_item(
            &self,
            item_id: Uuid,
            options: &MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
            self.refreshed
                .lock()
                .expect("lock")
                .push((item_id, options.clone()));
            Ok(ferrofin_traits::providers::ItemUpdateType::default())
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &ferrofin_model::providers::RemoteImageQuery,
        ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
            unimplemented!("fake")
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: ferrofin_traits::providers::ItemUpdateType,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError>
        {
            unimplemented!("fake")
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
            unimplemented!("fake")
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            unimplemented!("fake")
        }
    }

    // -- Keyframe Extractor --------------------------------------------------

    #[tokio::test]
    async fn keyframe_task_skips_extracted_items_and_survives_probe_failure() {
        let db = test_db().await;
        let media = tempfile::tempdir().expect("tempdir");
        let library = library_manager_over(db.clone());
        let keyframes: Arc<dyn KeyframeRepository> =
            Arc::new(crate::FerrofinKeyframeRepository::new(db.clone()));

        let (done, fresh) = (Uuid::from_u128(0xB1), Uuid::from_u128(0xB2));
        for (id, name) in [(done, "done.mkv"), (fresh, "fresh.mkv")] {
            seed_item(&db, id, BaseItemKind::Movie).await;
            let path = media.path().join(name);
            std::fs::write(&path, b"x").expect("write");
            set_path(&db, id, &path.to_string_lossy()).await;
        }
        let existing = KeyframeDataEntity {
            item_id: guid_to_db(done),
            keyframe_ticks: Some("[1,2,3]".to_owned()),
            total_duration: 42,
        };
        keyframes
            .save_keyframe_data(done, &existing)
            .await
            .expect("seed keyframes");

        // The fake probe path is /bin/false: extraction "runs" and yields
        // empty data, which must not fail the task.
        let task = KeyframeExtractionTask::new(
            library,
            Arc::clone(&keyframes),
            Arc::new(FakeEncoder {
                fail_extract: false,
            }),
        );
        assert_eq!(task.key(), "KeyframeExtraction");
        assert!(task.default_triggers().is_empty());
        task.execute(&TaskProgress::default()).await.expect("run");

        // The pre-extracted item is untouched.
        let kept = keyframes.get_keyframe_data(done).await.expect("kept");
        assert_eq!(kept[0].total_duration, 42);
        assert_eq!(kept[0].keyframe_ticks.as_deref(), Some("[1,2,3]"));
    }

    // -- Extract Chapter Images ----------------------------------------------

    fn chapter_at(ticks: i64) -> ChapterInfo {
        ChapterInfo {
            start_position_ticks: ticks,
            name: Some("Chapter".to_owned()),
            ..ChapterInfo::default()
        }
    }

    async fn chapter_task_fixture(
        fail_extract: bool,
    ) -> (
        tempfile::TempDir,
        Uuid,
        Arc<FakeChapters>,
        ChapterImagesTask,
    ) {
        let db = test_db().await;
        let media = tempfile::tempdir().expect("tempdir");
        let library = library_manager_over(db.clone());

        let movie = Uuid::from_u128(0xC1);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        set_media_type(&db, movie, "Video").await;
        let path = media.path().join("film.mkv");
        std::fs::write(&path, b"x").expect("write");
        set_path(&db, movie, &path.to_string_lossy()).await;
        let mut video = crate::test_support::fetch_item(&db, movie).await;
        video.run_time_ticks = Some(1_800_000_000);
        video.data = Some(r#"{"DefaultVideoStreamIndex":0}"#.to_owned());
        crate::test_support::save_item(&db, &video).await;

        let chapters = Arc::new(FakeChapters {
            chapters: Mutex::new(vec![chapter_at(0), chapter_at(600_000_000)]),
            fail_read: std::sync::atomic::AtomicBool::new(false),
            fail_save: std::sync::atomic::AtomicBool::new(false),
        });
        let streams = FakeStreams(vec![MediaStreamInfoEntity {
            item_id: movie.to_string(),
            stream_index: 0,
            stream_type: media_stream_type_disc(StreamTypeEnum::Video),
            codec: Some("h264".to_owned()),
            ..MediaStreamInfoEntity::default()
        }]);
        let app_paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            media.path().join("data"),
            media.path().join("logs"),
            media.path().join("config"),
            media.path().join("cache"),
            media.path().join("web"),
        ));
        let path_manager: Arc<dyn PathManager> =
            Arc::new(crate::FerrofinPathManager::new(Arc::clone(&app_paths)));
        let folders: Arc<dyn VirtualFolderManager> = Arc::new(folder_with(
            LibraryOptions {
                enable_chapter_image_extraction: true,
                ..LibraryOptions::default()
            },
            None,
            &media.path().to_string_lossy(),
        ));
        let extractor = Arc::new(crate::chapter_image_extractor::ChapterImageExtractor::new(
            Arc::clone(&folders),
            Arc::new(streams),
            Arc::new(FakeEncoder { fail_extract }),
            path_manager,
        ));
        let task = ChapterImagesTask::new(
            library,
            folders,
            Arc::clone(&chapters) as Arc<dyn ChapterManager>,
            extractor,
            app_paths,
        );
        (media, movie, chapters, task)
    }

    #[tokio::test]
    async fn chapter_images_are_extracted_and_stored() {
        let (media, _movie, chapters, task) = chapter_task_fixture(false).await;
        assert_eq!(task.key(), "RefreshChapterImages");
        task.execute(&TaskProgress::default()).await.expect("run");

        let saved = chapters.chapters.lock().expect("lock").clone();
        assert_eq!(saved.len(), 2);
        for chapter in &saved {
            let image = chapter.image_path.as_deref().expect("image path set");
            assert!(Path::new(image).exists(), "extracted image exists");
        }
        drop(media);
    }

    #[tokio::test]
    async fn chapter_image_failure_lands_in_the_failure_history() {
        let (media, _movie, chapters, task) = chapter_task_fixture(true).await;
        task.execute(&TaskProgress::default()).await.expect("run");

        // No image was stored, and the video landed in the failure history.
        assert!(
            chapters
                .chapters
                .lock()
                .expect("lock")
                .iter()
                .all(|c| c.image_path.is_none())
        );
        let history =
            std::fs::read_to_string(media.path().join("cache").join("chapter-failures.txt"))
                .expect("history written");
        assert!(history.contains("film.mkv"));
        drop(media);
    }

    #[tokio::test]
    async fn chapter_task_includes_owned_video_extras_like_the_source_query() {
        use ferrofin_traits::persistence::ChapterRepository as _;
        let fixture = crate::chapter_image_extractor::tests::Fixture::new(
            crate::chapter_image_extractor::tests::RecordingEncoder::default(),
        )
        .await;
        let original = Uuid::parse_str(&fixture.video.id).unwrap();
        let owned_id = Uuid::from_u128(0xC2800);
        let mut owned = fixture.video.clone();
        owned.id = guid_to_db(owned_id);
        owned.type_ = crate::item_type_lookup::stored_type_name(BaseItemKind::Video)
            .unwrap()
            .to_owned();
        owned.owner_id = Some(guid_to_db(original));
        owned.path = Some(format!("{}.owned.mkv", owned.path.as_deref().unwrap()));
        std::fs::write(owned.path.as_deref().unwrap(), b"owned video").unwrap();
        crate::test_support::save_item(&fixture.db, &owned).await;
        fixture
            .streams
            .save_media_streams(
                owned_id,
                &[MediaStreamInfoEntity {
                    item_id: owned.id.clone(),
                    stream_index: 2,
                    stream_type: media_stream_type_disc(StreamTypeEnum::Video),
                    codec: Some("h264".to_owned()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let repo = Arc::new(crate::FerrofinChapterRepository::new(fixture.db.clone()));
        repo.save_chapters(
            owned_id,
            &[ferrofin_db::entities::base_items::ChapterEntity {
                item_id: owned.id.clone(),
                chapter_index: 0,
                image_date_modified: None,
                image_path: None,
                start_position_ticks: 0,
                name: Some("Owned chapter".to_owned()),
            }],
        )
        .await
        .unwrap();
        let library = library_manager_over(fixture.db.clone());
        let chapters = Arc::new(crate::FerrofinChapterManager::new(
            repo.clone(),
            library.clone(),
        ));
        let temp = tempfile::tempdir().unwrap();
        let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            temp.path().join("data"),
            temp.path().join("log"),
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("web"),
        ));
        let task = ChapterImagesTask::new(
            library,
            fixture.folders.clone(),
            chapters,
            fixture.extractor.clone(),
            paths,
        );
        task.execute(&TaskProgress::default()).await.unwrap();
        let saved = repo.get_chapters(owned_id).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert!(
            saved[0]
                .image_path
                .as_deref()
                .is_some_and(|image| Path::new(image).exists()),
            "owned extras participate in the actual scheduled query"
        );
        assert_eq!(fixture.encoder.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn temporary_chapter_service_errors_fail_without_poisoning_extraction_history() {
        for read_failure in [true, false] {
            let (media, _, chapters, task) = chapter_task_fixture(false).await;
            let flag = if read_failure {
                &chapters.fail_read
            } else {
                &chapters.fail_save
            };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let error = task
                .execute(&TaskProgress::default())
                .await
                .expect_err("service errors fail the task");
            assert!(error.to_string().contains(if read_failure {
                "read failure"
            } else {
                "save failure"
            }));
            let history = media.path().join("cache/chapter-failures.txt");
            assert!(
                !history.exists(),
                "a transient repository error must not blocklist extraction"
            );
            assert!(
                chapters
                    .chapters
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|chapter| chapter.image_path.is_none())
            );
            flag.store(false, std::sync::atomic::Ordering::SeqCst);
            task.execute(&TaskProgress::default()).await.unwrap();
            assert!(
                chapters
                    .chapters
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|chapter| chapter
                        .image_path
                        .as_deref()
                        .is_some_and(|path| Path::new(path).exists())),
                "retry must extract or adopt frames after service recovery"
            );
            assert!(!history.exists());
        }
    }

    #[test]
    fn failure_history_write_errors_are_reported_and_clean_runs_need_no_history_write() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("chapter-failures.txt");
        std::fs::create_dir(&history).unwrap();
        let failures = std::collections::BTreeSet::from(["film.mkv123".to_owned()]);
        assert!(write_failure_history(&history, &failures, false).is_ok());
        let error = write_failure_history(&history, &failures, true)
            .expect_err("history I/O failure fails execution");
        assert!(error.to_string().contains("chapter-failures.txt"));
        let nested = temp.path().join("new-cache/chapter-failures.txt");
        write_failure_history(&nested, &failures, true).unwrap();
        assert_eq!(std::fs::read_to_string(nested).unwrap(), "film.mkv123");
    }

    // The failure mode that cost a real library every chapter image: the
    // extraction temp directory was owned by another user, so ffmpeg produced
    // nothing for every chapter of every video, and the run recorded ~3000
    // videos as permanently failed. A directory the run cannot write is a
    // server misconfiguration — fail the task and leave the history alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_temp_directory_fails_the_run_without_blocklisting_anything() {
        use std::os::unix::fs::PermissionsExt as _;

        let (media, _movie, chapters, task) = chapter_task_fixture(false).await;
        let temp = std::path::PathBuf::from(task.paths.temp_path());
        std::fs::create_dir_all(&temp).expect("create temp");
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let outcome = task.execute(&TaskProgress::default()).await;

        // Running as root ignores the mode bits; only assert when the probe is
        // meaningful for this uid.
        if std::fs::File::create(temp.join("probe")).is_err() {
            let err = outcome.expect_err("an unwritable temp directory must fail the task");
            assert!(
                err.to_string().contains("temp"),
                "the error must name the directory: {err}"
            );
            assert!(
                chapters
                    .chapters
                    .lock()
                    .expect("lock")
                    .iter()
                    .all(|c| c.image_path.is_none())
            );
            assert!(
                !media
                    .path()
                    .join("cache")
                    .join("chapter-failures.txt")
                    .exists(),
                "a misconfigured server must not blocklist the library"
            );
        }

        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).expect("restore");
        drop(media);
    }
}
