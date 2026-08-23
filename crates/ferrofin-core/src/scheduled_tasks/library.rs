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
//!
//! The "Media Segment Scan" task (`TaskExtractMediaSegments`) lives in
//! `ferrofin-extensions`, next to the segment providers it runs.
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
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::{MediaStream, VirtualFolderInfo};
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::chapters::ChapterManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{KeyframeRepository, MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshOptions, ProviderManager};
use ferrofin_traits::stubs::LyricManager;
use ferrofin_traits::subtitles::{SubtitleManager, SubtitleMediaType, SubtitleSearchRequest};
use ferrofin_traits::system::{PathManager, ServerApplicationPaths};
use ferrofin_traits::trickplay::TrickplayManager;
use uuid::Uuid;

use crate::db_error::{db_err, media_stream_type_from_disc};

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

/// The library options that apply to an item path: the first virtual folder
/// with a location the path lives under. Mirrors the C#
/// `ILibraryManager.GetLibraryOptions(item)` resolution.
fn options_for_path<'a>(
    folders: &'a [VirtualFolderInfo],
    path: &str,
) -> Option<&'a LibraryOptions> {
    folders
        .iter()
        .find(|f| {
            f.locations
                .iter()
                .any(|loc| Path::new(path).starts_with(loc))
        })
        .and_then(|f| f.library_options.as_ref())
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
        let stderr = self
            .runner
            .run_stderr(&self.encoder.encoder_path(), &args)
            .await?;
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
                let track_paths: Vec<String> =
                    tracks.iter().filter_map(|t| t.path.clone()).collect();
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

/// Chapters starting closer to the end of the file than this margin get no
/// image: there is not enough remaining video for ffmpeg to decode a frame.
const CHAPTER_IMAGE_EOF_MARGIN_TICKS: i64 = 10 * TICKS_PER_SECOND;

/// "Extract Chapter Images" — creates thumbnails for videos that have
/// chapters. Port of `ChapterImagesTask` + the image half of the upstream
/// `ChapterManager.RefreshChapterImages`: for every non-virtual library video
/// whose library enables chapter image extraction, a frame is extracted at
/// each chapter's position into the path manager's chapter-image layout and
/// the image path is stored on the chapter row. Videos whose extraction failed
/// before are remembered in `chapter-failures.txt` under the cache directory
/// and skipped until the file changes.
pub struct ChapterImagesTask {
    library: Arc<dyn LibraryManager>,
    folders: Arc<dyn VirtualFolderManager>,
    chapters: Arc<dyn ChapterManager>,
    streams: Arc<dyn MediaStreamRepository>,
    encoder: Arc<dyn MediaEncoder>,
    path_manager: Arc<dyn PathManager>,
    paths: Arc<dyn ServerApplicationPaths>,
}

impl ChapterImagesTask {
    /// Builds the task over the library, chapter, stream, encoder and path
    /// seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        chapters: Arc<dyn ChapterManager>,
        streams: Arc<dyn MediaStreamRepository>,
        encoder: Arc<dyn MediaEncoder>,
        path_manager: Arc<dyn PathManager>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            library,
            folders,
            chapters,
            streams,
            encoder,
            path_manager,
            paths,
        }
    }

    /// Extracts any missing chapter images for one video, returning `false`
    /// when an extraction failed (the video joins the failure history).
    ///
    /// Port of `ChapterManager.RefreshChapterImages`'s return, which is the
    /// same bool: a video with nothing to do and a video that extracted are
    /// both "success", because the only question the caller asks is whether to
    /// record this video as failed.
    async fn refresh_video(
        &self,
        item_id: Uuid,
        media_path: &str,
        run_time_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        let mut chapters = self.chapters.get_chapters(item_id).await?;
        if chapters.is_empty() {
            return Ok(true);
        }
        // The stored video stream drives the extraction arguments.
        let video_stream = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: Some(MediaStreamType::Video),
                index: None,
            })
            .await?
            .into_iter()
            .next();
        let Some(video_stream) = video_stream else {
            return Ok(true);
        };
        let stream = MediaStream {
            stream_type: MediaStreamType::Video,
            index: i32::try_from(video_stream.stream_index).unwrap_or(0),
            codec: video_stream.codec.clone(),
            ..MediaStream::default()
        };
        let source = MediaSourceInfo {
            path: Some(media_path.to_owned()),
            ..MediaSourceInfo::default()
        };

        let mut changed = false;
        let mut ok = true;
        for chapter in &mut chapters {
            // A chapter in the last seconds of the file (common for a final
            // "credits" marker) has too little video left to decode a frame
            // from; ffmpeg exits without writing anything. Skip it.
            if run_time_ticks.is_some_and(|runtime| {
                chapter.start_position_ticks > runtime - CHAPTER_IMAGE_EOF_MARGIN_TICKS
            }) {
                continue;
            }
            let target = self.path_manager.chapter_image_path(
                item_id,
                media_path,
                chapter.start_position_ticks,
            );
            if Path::new(&target).exists() {
                if chapter.image_path.as_deref() != Some(target.as_str()) {
                    chapter.image_path = Some(target);
                    chapter.image_date_modified = Utc::now();
                    changed = true;
                }
                continue;
            }
            // Extract, then move into the chapter-image layout. A per-chapter
            // failure (including ffmpeg silently producing no frame) marks the
            // video for the failure history rather than failing the task.
            let moved = match self
                .encoder
                .extract_video_image(
                    media_path,
                    "",
                    &source,
                    &stream,
                    None,
                    Some(chapter.start_position_ticks),
                )
                .await
            {
                Ok(frame) => Path::new(&target)
                    .parent()
                    // A directory the server cannot create is a server problem,
                    // like the extraction failures around it — fail this video
                    // the same way rather than aborting the whole method, which
                    // would discard the chapters already resolved above AND
                    // blocklist the video for something that is not its fault.
                    .map_or(Ok(()), |parent| {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| ServiceError::backend(e.to_string()))
                    })
                    .and_then(|()| move_file(&frame, &target)),
                Err(e) => Err(e),
            };
            match moved {
                Ok(()) => {
                    chapter.image_path = Some(target);
                    chapter.image_date_modified = Utc::now();
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(
                        item = %item_id,
                        error = %e,
                        "chapter image could not be extracted or stored"
                    );
                    ok = false;
                    break;
                }
            }
        }
        if changed {
            self.chapters.save_chapters(item_id, &chapters).await?;
        }
        Ok(ok)
    }
}

/// Moves a file, falling back to copy+delete across filesystems.
fn move_file(from: &str, to: &str) -> Result<(), ServiceError> {
    std::fs::rename(from, to)
        .or_else(|_| {
            std::fs::copy(from, to)
                .map(|_| ())
                .and_then(|()| std::fs::remove_file(from))
        })
        .map_err(|e| ServiceError::backend(e.to_string()))
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
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;

        // Failure history: videos whose extraction failed before are skipped
        // until their file changes (the key embeds the mtime).
        let fail_history_path = Path::new(&self.paths.cache_path()).join("chapter-failures.txt");

        self.preflight_writable_dirs(&fail_history_path)?;

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
            let Ok(item_id) = Uuid::parse_str(&video.id) else {
                continue;
            };
            let Some(path) = video.path.clone().filter(|p| Path::new(p).exists()) else {
                continue;
            };
            if !options_for_path(&folders, &path).is_some_and(|o| o.enable_chapter_image_extraction)
            {
                continue;
            }
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let history_key = format!("{path}{mtime}").to_lowercase();
            if failed.contains(&history_key) {
                continue;
            }
            let outcome = self
                .refresh_video(item_id, &path, video.run_time_ticks)
                .await;
            if let Err(e) = &outcome {
                tracing::warn!(item = %video.id, error = %e, "chapter image refresh failed");
            }
            // A failed video (extraction failure or refresh error) joins the
            // failure history so it is skipped until its file changes.
            // A failed video joins the failure history so it is skipped until
            // its file changes — upstream's behaviour exactly. Nothing here can
            // tell a per-file failure from a systemic one; that is what the
            // pre-flight above and the skip-count log are for.
            if !matches!(outcome, Ok(true)) {
                failed.insert(history_key);
                history_dirty = true;
            }
        }
        write_failure_history(&fail_history_path, &failed, history_dirty);
        progress.report(100.0);
        Ok(())
    }
}

/// Persists the chapter-image failure history, if the run added to it.
///
/// Written once per run rather than per failure: rewriting the whole file each
/// time made a systemic failure quadratic in the history's own size.
fn write_failure_history(path: &Path, failed: &std::collections::BTreeSet<String>, dirty: bool) {
    if dirty
        && let Err(e) = std::fs::write(path, failed.iter().cloned().collect::<Vec<_>>().join("|"))
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "cannot write the chapter failure history; failures will be retried \
             on every run until this is fixed"
        );
    }
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
/// 2. removes `Person` items whose people row is gone (the
///    `ValidatePeopleAsync` sweep);
/// 3. refreshes `Person` items missing a primary image or overview through the
///    provider manager (30-day backoff via `DateLastRefreshed`).
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
        let removed = sqlx::query(
            r#"DELETE FROM "BaseItems"
               WHERE "Type" = ?1
                 AND ("Name" IS NULL OR "Name" NOT IN (SELECT "Name" FROM "Peoples"))"#,
        )
        .bind(PERSON_TYPE)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        tracing::info!(
            removed = removed.rows_affected(),
            "removed dead person items"
        );
        Ok(())
    }

    /// Phase 3: refresh person items missing a primary image or overview.
    async fn refresh_person_items(
        &self,
        progress: &TaskProgress,
        base: f64,
        span: f64,
    ) -> Result<(), ServiceError> {
        let cutoff = datetime_to_db(Utc::now() - chrono::Duration::days(PEOPLE_REFRESH_DAYS));
        let ids: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" b
               WHERE "Type" = ?1
                 AND ("DateLastRefreshed" IS NULL OR "DateLastRefreshed" < ?2)
                 AND (("Overview" IS NULL OR "Overview" = '')
                      OR NOT EXISTS (SELECT 1 FROM "BaseItemImageInfos" i
                                     WHERE i."ItemId" = b."Id" AND i."ImageType" = 0))
               ORDER BY "Id""#,
        )
        .bind(PERSON_TYPE)
        .bind(cutoff)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        tracing::info!(count = ids.len(), "people needing image/overview refresh");
        let total = ids.len().max(1);
        let mut refreshed = 0usize;
        for (index, id) in ids.iter().enumerate() {
            if let Ok(item_id) = Uuid::parse_str(id) {
                match self
                    .providers
                    .refresh_single_item(item_id, &MetadataRefreshOptions::default())
                    .await
                {
                    Ok(_) => refreshed += 1,
                    Err(e) => {
                        // One representative warning, then stop the pass: a
                        // provider-manager failure (no network, no image store)
                        // repeats identically for every person. The next
                        // scheduled run retries.
                        tracing::warn!(person = id, error = %e, "person refresh failed");
                        break;
                    }
                }
            }
            #[allow(clippy::cast_precision_loss)]
            progress.report(base + span * ((index + 1) as f64 / total as f64));
        }
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
    subtitles: Arc<dyn SubtitleManager>,
    streams: Arc<dyn MediaStreamRepository>,
}

impl SubtitleDownloadTask {
    /// Builds the task over the library, subtitle-manager and stream seams.
    #[must_use]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        folders: Arc<dyn VirtualFolderManager>,
        subtitles: Arc<dyn SubtitleManager>,
        streams: Arc<dyn MediaStreamRepository>,
    ) -> Self {
        Self {
            library,
            folders,
            subtitles,
            streams,
        }
    }

    /// Whether `item_id` still needs subtitles in `lang`, per the library
    /// options flags.
    async fn needs_language(
        &self,
        item_id: Uuid,
        lang: &str,
        options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        let streams = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        let lang_matches =
            |l: &Option<String>| l.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(lang));
        for stream in &streams {
            match media_stream_type_from_disc(stream.stream_type) {
                MediaStreamType::Audio
                    if options.skip_subtitles_if_audio_track_matches
                        && lang_matches(&stream.language) =>
                {
                    return Ok(false);
                }
                // An external subtitle always satisfies the language; an
                // embedded one only when the library says so.
                MediaStreamType::Subtitle
                    if lang_matches(&stream.language)
                        && (options.skip_subtitles_if_embedded_subtitles_present
                            || stream.is_external) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Searches and downloads the best subtitle for one video + language.
    async fn download_for(
        &self,
        video: &BaseItemEntity,
        item_id: Uuid,
        lang: &str,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let is_episode = video.season_id.is_some() || video.series_name.is_some();
        let request = SubtitleSearchRequest {
            item_id,
            language: lang.to_owned(),
            is_perfect_match: options.require_perfect_subtitle_match.then_some(true),
            is_automated: true,
            content_type: if is_episode {
                SubtitleMediaType::Episode
            } else {
                SubtitleMediaType::Movie
            },
            name: video.name.clone(),
            series_name: video.series_name.clone(),
            production_year: video.production_year.and_then(|y| i32::try_from(y).ok()),
            parent_index_number: video
                .parent_index_number
                .and_then(|n| i32::try_from(n).ok()),
            index_number: video.index_number.and_then(|n| i32::try_from(n).ok()),
            runtime_ticks: video.run_time_ticks,
            media_path: video.path.clone(),
            ..SubtitleSearchRequest::default()
        };
        let results = self.subtitles.search_subtitles(&request).await?;
        let Some(first) = results.first().and_then(|r| r.id.clone()) else {
            return Ok(());
        };
        self.subtitles.download_subtitles(item_id, &first).await
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
        let folders: Vec<VirtualFolderInfo> = self
            .folders
            .get_virtual_folders()
            .await?
            .into_iter()
            .filter(|f| {
                f.library_options
                    .as_ref()
                    .and_then(|o| o.subtitle_download_languages.as_ref())
                    .is_some_and(|langs| !langs.is_empty())
            })
            .collect();
        if folders.is_empty() {
            progress.report(100.0);
            return Ok(());
        }
        let videos = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Movie],
                is_virtual_item: Some(false),
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;
        let total = videos.len().max(1);
        for (index, video) in videos.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            progress.report(100.0 * (index as f64) / total as f64);
            let Ok(item_id) = Uuid::parse_str(&video.id) else {
                continue;
            };
            let Some(path) = video.path.as_deref() else {
                continue;
            };
            let Some(options) = options_for_path(&folders, path) else {
                continue;
            };
            let Some(langs) = options.subtitle_download_languages.clone() else {
                continue;
            };
            for lang in &langs {
                match self.needs_language(item_id, lang, options).await {
                    Ok(true) => {
                        if let Err(e) = self.download_for(video, item_id, lang, options).await {
                            tracing::warn!(item = %video.id, lang, error = %e, "subtitle download failed");
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(item = %video.id, error = %e, "subtitle check failed");
                    }
                }
            }
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
                match self.lyrics.search_lyrics(item_id).await {
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
            is_virtual_item: Some(false),
            is_folder: Some(false),
            recursive: true,
            ..InternalItemsQuery::default()
        };
        let total = self.library.get_count(&query).await?.max(0);
        let folders = self.folders.get_virtual_folders().await?;
        let mut done = 0i32;
        let mut start_index = 0i32;
        while start_index < total {
            let items = page(&self.library, &query, start_index).await?;
            if items.is_empty() {
                break;
            }
            for item in &items {
                done += 1;
                // C# `GetLibraryOptions(video)`: the containing library's
                // options, or a default (extraction off) when none contains it.
                let options = item
                    .path
                    .as_deref()
                    .and_then(|path| options_for_path(&folders, path))
                    .cloned()
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
        let stderr = "\
[Parsed_ebur128_0 @ 0x1] Summary:\n\
\n\
  Integrated loudness:\n\
    I:         -23.1 LUFS\n\
    Threshold: -33.6 LUFS\n";
        assert_eq!(parse_lufs(stderr), Some(-23.1));
        assert_eq!(parse_lufs("no summary here"), None);
        assert_eq!(parse_lufs("    I: not-a-number LUFS"), None);
    }

    #[test]
    fn options_for_path_matches_the_containing_location() {
        let folders = vec![VirtualFolderInfo {
            name: Some("Music".into()),
            locations: vec!["/media/music".into()],
            library_options: Some(LibraryOptions {
                enable_lufs_scan: true,
                ..LibraryOptions::default()
            }),
            ..VirtualFolderInfo::default()
        }];
        assert!(
            options_for_path(&folders, "/media/music/a/b.flac").is_some_and(|o| o.enable_lufs_scan)
        );
        assert!(options_for_path(&folders, "/media/movies/x.mkv").is_none());
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
            _item_id: Uuid,
            _response: &SubtitleResponse,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn get_remote_subtitles(&self, _id: &str) -> Result<SubtitleResponse, ServiceError> {
            unimplemented!("fake")
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
            subtitles.clone(),
            Arc::new(streams),
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
        });
        let task = LyricDownloadTask::new(library, lyrics.clone());
        assert_eq!(task.key(), "DownloadLyrics");
        task.execute(&TaskProgress::default()).await.expect("run");

        let downloads = lyrics.downloads.lock().expect("lock");
        assert_eq!(downloads.as_slice(), &[(missing, "lrclib_42".to_owned())]);
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

    // -- Refresh People ------------------------------------------------------

    #[tokio::test]
    async fn people_validation_dedupes_and_removes_orphans() {
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
            vec![guid_to_db(person_item)],
            "dead item removed"
        );

        assert_eq!(
            providers.refreshed.lock().expect("lock").as_slice(),
            &[person_item],
            "surviving person refreshed"
        );
    }

    /// A [`ProviderManager`] fake recording `refresh_single_item` calls.
    #[derive(Default)]
    struct RecordingProviders {
        refreshed: Mutex<Vec<Uuid>>,
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
            _options: &MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
            self.refreshed.lock().expect("lock").push(item_id);
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

        let chapters = Arc::new(FakeChapters {
            chapters: Mutex::new(vec![chapter_at(0), chapter_at(600_000_000)]),
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
        let task = ChapterImagesTask::new(
            library,
            Arc::new(folder_with(
                LibraryOptions {
                    enable_chapter_image_extraction: true,
                    ..LibraryOptions::default()
                },
                None,
                &media.path().to_string_lossy(),
            )),
            Arc::clone(&chapters) as Arc<dyn ChapterManager>,
            Arc::new(streams),
            Arc::new(FakeEncoder { fail_extract }),
            path_manager,
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
