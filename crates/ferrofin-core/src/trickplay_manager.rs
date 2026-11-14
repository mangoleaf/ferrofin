//! [`FerrofinTrickplayManager`] — the [`TrickplayManager`] over the
//! `TrickplayInfos` table **plus** real tile generation.
//!
//! Port of `Jellyfin.Server.Implementations.Trickplay.TrickplayManager`. The
//! metadata rows ([`TrickplayInfoEntity`])
//! are stored/listed/deleted directly over `ferrofin-db` (there is no separate
//! repository trait for this leaf table); `Self::refresh_trickplay_data` is
//! the real `RefreshTrickplayDataAsync` port: extract thumbnails through the
//! [`TrickplayFrameExtractor`] seam (ffmpeg in production), pack them into tile
//! grids through the [`ImageEncoder`] seam (`CreateTrickplayTile`), write them
//! into the [`PathManager`] trickplay layout, and persist the info row.
//!
//! Departures from the C# (documented per the port rules):
//! - Per-library extraction and storage settings govern generation, tile
//!   lookup and migration. Disabled extraction preserves media-adjacent files
//!   as user-managed unless an explicit replacement was requested.
//! - Eligibility reads the resolved/stored video type, placeholder and shortcut
//!   facts, live DVR captures/channel livestreams, runtime and any media streams
//!   before discovery, pruning or generation (`CanGenerateTrickplay`).
//! - Existing user-managed grids are discovered in the selected location,
//!   including when extraction is disabled. Invalid or overflowing grid
//!   dimensions are ignored before division or database insertion.
//! - The hardware-acceleration / keyframe-only extraction options select the
//!   software ffmpeg path (see [`TrickplayFrameExtractor`]).
//! - A Replace does not prune first: each width is generated in the work
//!   directory and swapped in, so the old images serve until then and a
//!   failed Replace keeps them. Widths no longer selected go afterwards.
//! - After a change of the selected widths or the chroma, the next Generate
//!   Trickplay Images run replaces the sets that no longer match the same way
//!   (upstream never replaces images unasked). The settings of the last
//!   finished pass are kept in `FerrofinMeta`, so passes with nothing changed
//!   read no tiles.
//! - A video's widths are each checked first (skipped, imported, or to
//!   generate), and the widths to generate are then extracted by one
//!   [`TrickplayFrameExtractor::extract_trickplay_frame_sets`] call, which the
//!   ffmpeg extractor makes from one decode, in one generation slot; upstream
//!   checks and extracts each width in turn, decoding the video once per
//!   width (gl-1227, gl-1228).
//!
//! Trickplay rows are keyed by *media-source id*, as in C#: a media source is
//! an item of the video's version group, and its id is that item's id (the
//! dashless `ToString("N")` form `MediaSources[].Id` carries). The DTO service
//! builds `BaseItemDto.Trickplay` over the group from
//! [`TrickplayManager::get_trickplay_resolutions_batch`].

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::playback::TrickplayInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, TrickplayOptions};
use ferrofin_model::entities::{MediaStreamType, VideoType};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_util::keyed_locks::KeyedLocks;
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{
    TrickplayExtraction, TrickplayFrameExtractor, TrickplayOutput,
};
use ferrofin_traits::options::ImageCollageOptions;
use ferrofin_traits::persistence::{ItemRepository, MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::system::{MediaLocation, PathManager};
use ferrofin_traits::trickplay::{TrickplayCheck, TrickplayManager};

use crate::db_error::db_err;
use crate::library_scan::off_runtime;
use crate::media_source_manager::stream_to_dto;

/// The number of 100-nanosecond ticks per millisecond
/// (`TimeSpan.TicksPerMillisecond`).
const TICKS_PER_MS: i64 = 10_000;

/// The minimum valid trickplay interval, in milliseconds — smaller configured
/// values are reset to this, mirroring the C# guard in
/// `RefreshTrickplayDataAsync`.
const MIN_INTERVAL_MS: i32 = 1000;

/// The `FerrofinMeta` key holding the [`PassSettings`] the last finished
/// Generate Trickplay Images pass ran under.
const PASS_SETTINGS_KEY: &str = "trickplay_pass_settings";

/// What a set of trickplay images is checked against after a settings change
/// (D5): the selected widths and the chroma. `Qscale` and JPEG quality are not
/// readable back from the files, so a hand edit of them needs a Replace, as in
/// Jellyfin.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PassSettings {
    widths: Vec<i32>,
    full_chroma: bool,
    /// Present while sets may be on disk that the recorded settings do not
    /// describe: the widths of checking passes started since that have not
    /// finished, and of sets made under other settings outside a checking
    /// pass (a library scan, an item refresh). The next run then checks
    /// again, even after a change back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unfinished: Option<Vec<i32>>,
}

/// A checking pass's [`TrickplayCheck::settings`]: the settings it records
/// when it finishes, and the `unfinished` widths it wrote when it began, so a
/// width recorded as made while it ran stays recorded.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PassCheck {
    settings: PassSettings,
    begun: Vec<i32>,
}

impl PassSettings {
    fn of(options: &TrickplayOptions) -> Self {
        let mut widths: Vec<i32> = options
            .width_resolutions
            .iter()
            .copied()
            .filter(|width| *width > 0)
            .collect();
        widths.sort_unstable();
        widths.dedup();
        Self {
            widths,
            full_chroma: options.full_chroma,
            unfinished: None,
        }
    }

    fn encode(&self) -> Result<String, ServiceError> {
        serde_json::to_string(self).map_err(ServiceError::backend_source)
    }
}

/// The concrete trickplay manager.
#[derive(Clone)]
pub struct FerrofinTrickplayManager {
    db: Database,
    path_manager: Arc<dyn PathManager>,
    config: Arc<dyn ServerConfigurationManager>,
    items: Arc<dyn ItemRepository>,
    streams: Arc<dyn MediaStreamRepository>,
    frame_extractor: Arc<dyn TrickplayFrameExtractor>,
    image_encoder: Arc<dyn ImageEncoder>,
    live_tv: Option<std::sync::Weak<dyn ferrofin_traits::stubs::LiveTvManager>>,
    virtual_folders: Arc<std::sync::OnceLock<Arc<dyn VirtualFolderManager>>>,
    /// Bounds how many items extract and tile at once, across scans and the
    /// scheduled task. An item holds one slot across the run that extracts
    /// all of its widths that generate and their tiling, so its sets are made
    /// together (gl-1227, gl-1228).
    /// Upstream holds a lock of 1 (`TrickplayManager._resourcePool`), taken
    /// per width; Ferrofin sizes it from `FERROFIN_TRICKPLAY_CONCURRENCY`
    /// instead.
    generation_slots: Arc<tokio::sync::Semaphore>,
    /// One refresh or move of an item's trickplay at a time, keyed by item id.
    /// Upstream's lock of 1 also gave this: a second refresh of the item waits,
    /// then finds the first one's tiles and row and skips them, and one
    /// refresh's prune never removes a folder the other has just moved in.
    item_locks: Arc<KeyedLocks>,
    /// One read-and-write of the recorded pass settings at a time, so a set
    /// recorded as made while a pass begins or finishes is not lost.
    pass_settings: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for FerrofinTrickplayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinTrickplayManager")
            .finish_non_exhaustive()
    }
}

/// Task-owned temporary directories must also be removed when a scan/task
/// drops its future during encoding, the Rust counterpart of the source finally.
struct TemporaryTrickplayDirectory(PathBuf);

impl Drop for TemporaryTrickplayDirectory {
    fn drop(&mut self) {
        if let Err(error) = off_runtime(|| std::fs::remove_dir_all(&self.0))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(directory = %self.0.display(), %error, "cannot remove trickplay temporary directory");
        }
    }
}

/// The per-width refresh context shared by [`FerrofinTrickplayManager`] helpers:
/// the item identity, its media path, its stored video width, and the selected
/// trickplay directory.
struct WidthRefreshContext<'a> {
    /// The item being refreshed.
    item_id: Uuid,
    /// The item's on-disk media path.
    media_path: &'a str,
    /// The item's video stream, when it has one recorded.
    ///
    /// Upstream reads `mediaSource.VideoStream`. It is what the hardware
    /// trickplay path needs and the software one does not: picking a decoder
    /// needs the codec, and a hardware scaler takes fixed pixel dimensions, so
    /// an anamorphic source has to be un-stretched from its aspect ratio.
    video_stream: Option<MediaStream>,
    /// Every stream of the item in stream order (`mediaSource.MediaStreams`),
    /// which the extractor needs to find `video_stream`'s ffmpeg index for
    /// `-map 0:N` (`EncodingHelper.FindIndex`).
    media_streams: Vec<MediaStream>,
    /// The selected trickplay directory for the item.
    trickplay_dir: &'a str,
    /// Whether `trickplay_dir` is beside the media (`SaveTrickplayWithMedia`),
    /// so generation probes it for write access first.
    save_with_media: bool,
}

/// A width of a video that needs its set generated, as
/// [`FerrofinTrickplayManager::prepare_width`] found it.
struct PendingSet {
    /// The selected trickplay width.
    width: i32,
    /// The width the set is made at: `width`, or the video's when narrower.
    actual_width: i32,
    /// The video's width, when it narrowed `width`.
    narrower: Option<i64>,
    /// The folder the set's tiles are moved into.
    output_dir: PathBuf,
}

impl FerrofinTrickplayManager {
    /// Reads all streams because `CanGenerateTrickplay` tests their count,
    /// while the encoder only consumes the first video stream.
    async fn media_streams(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ferrofin_db::entities::base_items::MediaStreamInfoEntity>, ServiceError> {
        self.streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                ..MediaStreamQuery::default()
            })
            .await
    }

    /// Connects the live DVR capture state without a strong library/DVR cycle.
    #[must_use]
    pub fn with_live_tv(
        mut self,
        live_tv: &Arc<dyn ferrofin_traits::stubs::LiveTvManager>,
    ) -> Self {
        self.live_tv = Some(Arc::downgrade(live_tv));
        self
    }

    async fn is_active_recording(&self, entity: &BaseItemEntity) -> Result<bool, ServiceError> {
        let Some(live_tv) = self.live_tv.as_ref().and_then(std::sync::Weak::upgrade) else {
            return Ok(false);
        };
        Ok(live_tv
            .active_recording_paths()
            .await?
            .iter()
            .any(|path| entity.path.as_ref() == Some(path)))
    }

    /// Creates a trickplay manager over the given database, path manager,
    /// server configuration (for [`TrickplayOptions`]), item repository (media
    /// path/runtime lookup), frame-extraction seam (ffmpeg), and image encoder
    /// (tile packing).
    #[must_use]
    pub fn new(
        db: Database,
        path_manager: Arc<dyn PathManager>,
        config: Arc<dyn ServerConfigurationManager>,
        items: Arc<dyn ItemRepository>,
        streams: Arc<dyn MediaStreamRepository>,
        frame_extractor: Arc<dyn TrickplayFrameExtractor>,
        image_encoder: Arc<dyn ImageEncoder>,
    ) -> Self {
        Self {
            db,
            path_manager,
            config,
            items,
            streams,
            frame_extractor,
            image_encoder,
            live_tv: None,
            virtual_folders: Arc::new(std::sync::OnceLock::new()),
            generation_slots: Arc::new(tokio::sync::Semaphore::new(
                tokio::sync::Semaphore::MAX_PERMITS,
            )),
            item_locks: Arc::new(KeyedLocks::new()),
            pass_settings: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Caps how many items generate trickplay at once (each one's widths in
    /// turn, in one slot). Unset, only the frame extractor's shared
    /// image-encoding pool bounds them.
    #[must_use]
    pub fn with_generation_concurrency(mut self, slots: usize) -> Self {
        self.generation_slots = Arc::new(tokio::sync::Semaphore::new(slots.max(1)));
        self
    }

    /// The server's scratch directory (`ApplicationPaths.TempDirectory`), where
    /// frames and tiles are made before the tiles move into place.
    fn temp_directory(&self) -> PathBuf {
        PathBuf::from(self.config.application_paths().temp_path())
    }

    /// Connects the live library-options reader before the server starts.
    pub fn attach_virtual_folders(&self, folders: Arc<dyn VirtualFolderManager>) {
        let _ = self.virtual_folders.set(folders);
    }

    async fn library_options(
        &self,
        entity: &BaseItemEntity,
    ) -> Result<Option<LibraryOptions>, ServiceError> {
        let Some(folders) = self.virtual_folders.get() else {
            return Ok(None);
        };
        let folders = folders.library_folders().await?;
        Ok(ferrofin_model::entities_media::owning_library(
            &folders,
            entity.top_parent_id.as_deref(),
            entity.path.as_deref(),
        )
        .and_then(|library| library.library_options.clone()))
    }

    /// Fetches all stored trickplay rows for an item, keyed by tile width.
    async fn resolutions_for(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, TrickplayInfoEntity>(
            r#"SELECT * FROM "TrickplayInfos" WHERE "ItemId" = ?1 ORDER BY "Width""#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| (r.width, r)).collect())
    }

    /// Deletes the stored trickplay row for one (item, width) pair.
    async fn delete_trickplay_row(&self, item_id: Uuid, width: i32) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "TrickplayInfos" WHERE "ItemId" = ?1 AND "Width" = ?2"#)
            .bind(guid_to_db(item_id))
            .bind(width)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Reads raw options. Eligibility and discovery precede the generation-only
    /// minimum-interval guard in the pinned manager.
    async fn trickplay_options(&self) -> Result<TrickplayOptions, ServiceError> {
        Ok(self.config.configuration().await?.trickplay_options.clone())
    }

    fn clamp_generation_interval(options: &mut TrickplayOptions) {
        if options.interval < MIN_INTERVAL_MS {
            tracing::warn!(
                interval = options.interval,
                "trickplay image interval is too small, reset to the minimum valid value of {MIN_INTERVAL_MS}"
            );
            options.interval = MIN_INTERVAL_MS;
        }
    }

    /// `CanGenerateTrickplay` uses any recorded stream, including an audio-only
    /// probe. The later extraction step tolerates an absent video stream.
    fn can_generate_trickplay(
        entity: &BaseItemEntity,
        interval_ms: i32,
        has_streams: bool,
        active_recording: bool,
    ) -> Option<String> {
        if !has_streams || active_recording {
            return None;
        }
        let data = crate::item_data::parse_data(entity.data.as_deref());
        let video_type = data
            .get("VideoType")
            .and_then(|v| serde_json::from_value::<VideoType>(v.clone()).ok())
            .unwrap_or(VideoType::VideoFile);
        if matches!(
            video_type,
            VideoType::Iso | VideoType::Dvd | VideoType::BluRay
        ) || data
            .get("IsPlaceHolder")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            || data.get("IsShortcut").and_then(serde_json::Value::as_bool) == Some(true)
        {
            return None;
        }
        let path = entity.path.clone().unwrap_or_default();
        if Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("strm"))
        {
            return None;
        }
        let channel = entity
            .channel_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_some_and(|id| !id.is_nil());
        if channel
            && entity
                .tags
                .as_deref()
                .into_iter()
                .flat_map(|tags| tags.split('|'))
                .any(|tag| tag.eq_ignore_ascii_case("livestream"))
        {
            return None;
        }
        let runtime = entity.run_time_ticks?;
        if runtime < i64::from(interval_ms) * TICKS_PER_MS {
            return None;
        }
        Some(path)
    }

    /// The checks of `RefreshTrickplayDataInternal` for one configured width,
    /// before its frames are extracted: skip or import existing tiles, and
    /// otherwise return the set to generate, once the folder beside the media
    /// is known to be writable and the set is recorded as made. `rows` are the
    /// item's stored rows, which an import adds to.
    async fn prepare_width(
        &self,
        ctx: &WidthRefreshContext<'_>,
        replace: bool,
        width: i32,
        options: &TrickplayOptions,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Result<Option<PendingSet>, ServiceError> {
        // Source checks these only inside per-width work: they do not bypass
        // discovery or the outer disabled/replace cleanup.
        if !off_runtime(|| Path::new(ctx.media_path).is_file()) {
            tracing::warn!(item = %ctx.item_id, path = ctx.media_path, "trickplay media file not found");
            return Ok(None);
        }
        if Path::new(ctx.media_path)
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.eq_ignore_ascii_case("backdrops"))
        {
            tracing::debug!(item = %ctx.item_id, path = ctx.media_path, "ignoring backdrop media");
            return Ok(None);
        }
        // CanGenerateTrickplay counts all streams, but the pinned per-width
        // body dereferences mediaSource.VideoStream.Width. An audio-only source
        // therefore stops inside that body's caught error after outer cleanup.
        let Some(video_stream) = ctx.video_stream.as_ref() else {
            return Err(ServiceError::backend(
                "trickplay media source has no video stream",
            ));
        };
        let (actual_width, narrower) = generated_width(width, video_stream);
        if actual_width <= 0 {
            return Ok(None);
        }
        let options = &options_at_width(options, actual_width);

        let output_dir = Path::new(ctx.trickplay_dir).join(format!(
            "{actual_width} - {}x{}",
            options.tile_width, options.tile_height
        ));

        // Import existing trickplay tiles.
        if !replace {
            // A stored width only needs a tile there; importing needs them all.
            match rows.entry(actual_width) {
                Entry::Occupied(_) => {
                    if off_runtime(|| has_tiles(&output_dir)) {
                        tracing::debug!(item = %ctx.item_id, "found existing trickplay files");
                        return Ok(None);
                    }
                }
                Entry::Vacant(slot) => {
                    let existing_files = off_runtime(|| jpg_files_sorted(&output_dir));
                    if !existing_files.is_empty() {
                        let info = self
                            .import_tiles(ctx.item_id, actual_width, options, &existing_files)
                            .await?;
                        slot.insert(info);
                        return Ok(None);
                    }
                }
            }
        }

        // Check for write permission before generating images
        // (`TrickplayManager.cs:482-488`), so an unwritable media folder fails
        // before any ffmpeg work rather than when the packed tiles are moved.
        if ctx.save_with_media {
            off_runtime(|| probe_write_access(&output_dir))?;
        }
        // A set the recorded settings cannot be told about is not made: the
        // width fails and the next run tries again.
        self.record_set_made(width, options).await?;
        Ok(Some(PendingSet {
            width,
            actual_width,
            narrower,
            output_dir,
        }))
    }

    /// Generates `sets`, the widths of one video that [`Self::prepare_width`]
    /// found need it: one [`TrickplayFrameExtractor::extract_trickplay_frame_sets`]
    /// call extracts every width's frames, so the ffmpeg extractor decodes
    /// the video once for all of them (gl-1228; upstream extracts each width
    /// with a run of its own), then each width's tiles are packed and saved.
    /// Returns each set's outcome in order; one width's failure costs only
    /// that width.
    ///
    /// The video's generation slot is taken first and held until its last
    /// set is saved (gl-1227). Each saved set's row goes into `rows`.
    async fn generate_sets(
        &self,
        ctx: &WidthRefreshContext<'_>,
        sets: &[PendingSet],
        options: &TrickplayOptions,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Vec<Result<(), ServiceError>> {
        if sets.is_empty() {
            return Vec::new();
        }
        let _slot = match Arc::clone(&self.generation_slots).acquire_owned().await {
            Ok(slot) => slot,
            Err(e) => {
                return sets
                    .iter()
                    .map(|set| {
                        drop_probed_folder(ctx, set);
                        Err(ServiceError::backend(format!("no generation slot: {e}")))
                    })
                    .collect();
            }
        };
        for set in sets {
            // Upstream warns "video width is smaller than trickplay setting"
            // before its existing-tiles check, so once per pass over every
            // narrow video. Using the video's width is the intended outcome,
            // so it is a field of the one line a generation logs
            // (`video_width` present when the setting was narrowed).
            tracing::info!(
                width = set.actual_width,
                trickplay_width = set.width,
                video_width = set.narrower,
                path = ctx.media_path,
                item = %ctx.item_id,
                "creating trickplay files"
            );
        }
        // `MediaEncoder.ExtractVideoImagesOnIntervalAccelerated`: under the
        // server's temp directory, which "Clean Cache Directory" sweeps, one
        // directory per width.
        let frames_dirs: Vec<String> = sets
            .iter()
            .map(|_| {
                self.temp_directory()
                    .join(Uuid::new_v4().simple().to_string())
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        // Each width's frames go once its tiles are packed, and every width's
        // if the future is dropped.
        let mut frames_cleanup: Vec<Option<TemporaryTrickplayDirectory>> = frames_dirs
            .iter()
            .map(|dir| Some(TemporaryTrickplayDirectory(PathBuf::from(dir))))
            .collect();
        let outputs: Vec<TrickplayOutput<'_>> = sets
            .iter()
            .zip(&frames_dirs)
            .map(|(set, dir)| TrickplayOutput {
                max_width: set.actual_width,
                output_dir: dir,
            })
            .collect();
        let first = outputs[0];
        let request = TrickplayExtraction {
            input_path: ctx.media_path,
            video_stream: ctx.video_stream.as_ref(),
            media_streams: &ctx.media_streams,
            interval_ms: options.interval,
            max_width: first.max_width,
            qscale: options.qscale,
            threads: options.process_threads,
            priority: options.process_priority,
            output_dir: first.output_dir,
            allow_hw_accel: options.enable_hw_acceleration,
            enable_hw_encoding: options.enable_hw_encoding,
            keyframe_only: options.enable_key_frame_only_extraction,
            full_chroma: options.full_chroma,
        };
        let mut extracted = self
            .frame_extractor
            .extract_trickplay_frame_sets(&request, &outputs)
            .await
            .into_iter();

        let mut results = Vec::with_capacity(sets.len());
        for (set, frames) in sets.iter().zip(&mut frames_cleanup) {
            let images = extracted.next().unwrap_or_else(|| {
                Err(ServiceError::backend(
                    "the frame extractor returned fewer sets than asked for",
                ))
            });
            let result = match images {
                Ok(images) => self.tile_and_save(ctx, set, options, &images, rows).await,
                Err(e) => Err(e),
            };
            drop(frames.take());
            if result.is_err() {
                drop_probed_folder(ctx, set);
            }
            results.push(result);
        }
        results
    }

    /// Packs a width's extracted `images` with [`Self::create_tiles`] and saves
    /// the resulting info row; on a save failure the freshly written tiles are
    /// removed so no files stay behind without a row (mirroring the C#),
    /// unless they replaced a set whose row in `rows` names their folder: that
    /// set is already gone, and the row still points at the new tiles, whose
    /// timing it may describe until the next save. A saved row goes into
    /// `rows`.
    async fn tile_and_save(
        &self,
        ctx: &WidthRefreshContext<'_>,
        set: &PendingSet,
        options: &TrickplayOptions,
        images: &[String],
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Result<(), ServiceError> {
        let options = &options_at_width(options, set.actual_width);
        let mut info = self
            .create_tiles(images, set.actual_width, options, &set.output_dir)
            .await?;
        info.item_id = guid_to_db(ctx.item_id);

        if let Err(e) = self.save_trickplay_info(&info).await {
            // Make sure no files stay in metadata folders when the info row
            // wasn't saved.
            if !row_names_folder(rows, set.actual_width, options) {
                let _ = off_runtime(|| std::fs::remove_dir_all(&set.output_dir));
            }
            return Err(e);
        }
        rows.insert(info.width, info);
        tracing::info!(
            path = ctx.media_path,
            width = set.actual_width,
            "finished creation of trickplay files"
        );
        Ok(())
    }

    /// Port of `CreateTiles`: packs `images` (each `width` px wide) into
    /// `tile_width × tile_height` JPEG grids in a temp work dir, then moves the
    /// grids into `output_dir`, returning the computed info row (with an empty
    /// `item_id` for the caller to fill).
    async fn create_tiles(
        &self,
        images: &[String],
        width: i32,
        options: &TrickplayOptions,
        output_dir: &Path,
    ) -> Result<TrickplayInfoEntity, ServiceError> {
        if images.is_empty() {
            return Err(ServiceError::invalid_input(
                "Can't create trickplay from 0 images.",
            ));
        }

        let work_dir = self
            .temp_directory()
            .join(format!("trickplay_{}", Uuid::new_v4().simple()));
        off_runtime(|| std::fs::create_dir_all(&work_dir))
            .map_err(|e| ServiceError::backend(format!("cannot create tile work dir: {e}")))?;

        let _tiles_cleanup = TemporaryTrickplayDirectory(work_dir.clone());
        self.create_tiles_in(images, width, options, &work_dir, output_dir)
            .await
    }

    /// The fallible body of [`Self::create_tiles`], separated so the caller can
    /// clean the work dir on error.
    async fn create_tiles_in(
        &self,
        images: &[String],
        width: i32,
        options: &TrickplayOptions,
        work_dir: &Path,
        output_dir: &Path,
    ) -> Result<TrickplayInfoEntity, ServiceError> {
        let mut info = TrickplayInfoEntity {
            item_id: String::new(),
            width,
            interval: options.interval,
            tile_width: options.tile_width,
            tile_height: options.tile_height,
            thumbnail_count: i32::try_from(images.len()).unwrap_or(i32::MAX),
            // Set during tile generation.
            height: 0,
            bandwidth: 0,
        };

        let thumbnails_per_tile = usize::try_from(options.tile_width.max(0)).unwrap_or(0)
            * usize::try_from(options.tile_height.max(0)).unwrap_or(0);
        if thumbnails_per_tile == 0 {
            return Err(ServiceError::invalid_input(
                "tile grid dimensions must be positive",
            ));
        }

        for (i, chunk) in images.chunks(thumbnails_per_tile).enumerate() {
            let tile_path = work_dir.join(format!("{i}.jpg"));
            let collage = ImageCollageOptions {
                input_paths: chunk.to_vec(),
                output_path: tile_path.to_string_lossy().into_owned(),
                width: options.tile_width,
                height: options.tile_height,
            };
            let height = self
                .image_encoder
                .create_trickplay_tile(
                    &collage,
                    options.jpeg_quality,
                    info.width,
                    (info.height != 0).then_some(info.height),
                    options.full_chroma,
                )
                .await?;
            if info.height == 0 {
                info.height = height;
            }

            let bytes = off_runtime(|| std::fs::metadata(&tile_path))
                .map_err(|e| ServiceError::backend(format!("cannot stat tile: {e}")))?
                .len();
            let bitrate = tile_bitrate(bytes, info.tile_width, info.tile_height, info.interval);
            info.bandwidth = info.bandwidth.max(bitrate);
        }

        // Move trickplay tiles to the output directory (replacing any old ones).
        off_runtime(|| move_dir(work_dir, output_dir))?;
        Ok(info)
    }

    /// Port of the "Import tiles" branch of `RefreshTrickplayDataInternal`:
    /// adopts on-disk tile files without a DB row, computing height/bandwidth
    /// from the files themselves. The row stores `width`, the width the tiles'
    /// folder was made at (the selected width, or the video's own when it is
    /// narrower, rounded down to even), the configured interval, and the tile
    /// grid of `options`, the grid at that width ([`options_at_width`]), and
    /// returns the saved row. The C# stores the selected width, which, when
    /// the two differ, names a folder that does not exist (gl-1246).
    async fn import_tiles(
        &self,
        item_id: Uuid,
        width: i32,
        options: &TrickplayOptions,
        existing_files: &[String],
    ) -> Result<TrickplayInfoEntity, ServiceError> {
        let mut info = TrickplayInfoEntity {
            item_id: guid_to_db(item_id),
            width,
            interval: options.interval,
            tile_width: options.tile_width,
            tile_height: options.tile_height,
            thumbnail_count: i32::try_from(existing_files.len()).unwrap_or(i32::MAX),
            height: 0,
            bandwidth: 0,
        };

        for tile in existing_files {
            let size = self.image_encoder.get_image_size(tile).await?;
            info.height = info.height.max(ceil_div(size.height, info.tile_height));
            let bytes = off_runtime(|| std::fs::metadata(tile))
                .map_err(|e| ServiceError::backend(format!("cannot stat tile: {e}")))?
                .len();
            let bitrate = tile_bitrate(bytes, info.tile_width, info.tile_height, info.interval);
            info.bandwidth = info.bandwidth.max(bitrate);
        }

        self.save_trickplay_info(&info).await?;
        tracing::debug!(item = %item_id, "imported existing trickplay files");
        Ok(info)
    }

    /// The orphaned-row prune of `DiscoverExistingTrickplayAsync`: drops the
    /// rows (from the database and from `rows`) whose tile folder no longer
    /// exists in either possible location (internal or media-adjacent).
    async fn prune_orphaned_rows(
        &self,
        item_id: Uuid,
        media_path: &str,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Result<(), ServiceError> {
        if rows.is_empty() {
            return Ok(());
        }
        let local_root = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(media_path),
            false,
        );
        let media_root = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(media_path),
            true,
        );
        let orphaned: Vec<i32> = off_runtime(|| {
            rows.values()
                .filter(|info| {
                    let sub = format!("{} - {}x{}", info.width, info.tile_width, info.tile_height);
                    !has_tiles(&Path::new(&local_root).join(&sub))
                        && (media_root.is_empty() || !has_tiles(&Path::new(&media_root).join(&sub)))
                })
                .map(|info| info.width)
                .collect()
        });
        for width in orphaned {
            tracing::info!(
                width,
                path = media_path,
                "removed orphaned trickplay DB entry"
            );
            self.delete_trickplay_row(item_id, width).await?;
            rows.remove(&width);
        }
        Ok(())
    }

    /// Discovers the selected location before applying the extraction flag.
    /// User-managed grids are catalogued even when extraction is disabled.
    /// `rows` are the item's stored rows, kept up to date as it prunes and
    /// adds.
    async fn discover_existing_trickplay(
        &self,
        entity: &BaseItemEntity,
        item_id: Uuid,
        media_path: &str,
        save_with_media: bool,
        options: &TrickplayOptions,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Result<(), ServiceError> {
        self.prune_orphaned_rows(item_id, media_path, rows).await?;
        let root = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(media_path),
            save_with_media,
        );
        // Listed off the runtime, each entry with whether it is a folder; an
        // entry that cannot be read fails discovery once the loop reaches it,
        // after the grids listed before it.
        let Some(entries) = off_runtime(|| {
            Path::new(&root).is_dir().then(|| {
                std::fs::read_dir(&root).map(|entries| {
                    entries
                        .map(|entry| {
                            entry
                                .map(|entry| entry.path())
                                .map(|path| (path.is_dir(), path))
                        })
                        .collect::<Vec<_>>()
                })
            })
        }) else {
            return Ok(());
        };
        let entries = entries
            .map_err(|error| ServiceError::backend(format!("cannot list trickplay: {error}")))?;
        for entry in entries {
            let (is_dir, path) = entry.map_err(|error| {
                ServiceError::backend(format!("cannot list trickplay: {error}"))
            })?;
            if !is_dir {
                continue;
            }
            let Some((width, tile_width, tile_height)) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(parse_grid_name)
            else {
                continue;
            };
            if rows.contains_key(&width) {
                continue;
            }
            let mut tiles = off_runtime(|| grid_tiles(&path))?;
            tiles.sort();
            let Some(first) = tiles.first() else {
                continue;
            };
            let Some((interval, thumbnail_count)) = discovered_timing(
                entity.run_time_ticks,
                tiles.len(),
                tile_width,
                tile_height,
                options.interval,
            ) else {
                // No valid row can represent an overflowing/unusable grid.
                tracing::warn!(directory = %path.display(), "invalid trickplay tile capacity");
                continue;
            };
            let size = self.image_encoder.get_image_size(first).await?;
            let info = TrickplayInfoEntity {
                item_id: guid_to_db(item_id),
                width,
                tile_width,
                tile_height,
                interval,
                thumbnail_count,
                height: ceil_div(size.height, tile_height).max(1),
                bandwidth: off_runtime(|| peak_bitrate(&tiles, tile_width, tile_height, interval))?,
            };
            self.save_trickplay_info(&info).await?;
            rows.insert(width, info);
            tracing::debug!(item_id = %item_id, width, interval, thumbnail_count, "discovered existing trickplay grid");
        }
        Ok(())
    }

    /// The trailing cleanup of `RefreshTrickplayDataAsync`: removes tile
    /// folders in the trickplay directory that none of the item's stored
    /// `rows` accounts for.
    fn prune_unexpected_folders(trickplay_dir: &str, rows: &HashMap<i32, TrickplayInfoEntity>) {
        let root = Path::new(trickplay_dir);
        let expected: HashSet<PathBuf> = rows
            .values()
            .map(|i| root.join(format!("{} - {}x{}", i.width, i.tile_width, i.tile_height)))
            .collect();
        off_runtime(|| {
            if !root.is_dir() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    if path.is_dir() && !expected.contains(&path) {
                        tracing::warn!(folder = %path.display(), "pruning trickplay files");
                        if let Err(e) = std::fs::remove_dir_all(&path) {
                            tracing::warn!(
                                folder = %path.display(),
                                "unable to remove trickplay directory: {e}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Moves this item's generated trickplay tiles to the location the
    /// configuration dictates.
    ///
    /// Port of `MoveGeneratedTrickplayDataAsync`: reads the owning library's
    /// current extraction flag and destination on every task invocation.
    /// Moves either way and removes the emptied source container.
    ///
    /// This is an inherent method (not on the [`TrickplayManager`] trait),
    /// consumed by the "Migrate Trickplay Image Location" scheduled task. That
    /// task's source query selects physical Video kinds, including owned extras
    /// and complete channel VOD (SourceTypes is declared but not consumed by
    /// the pinned query), so any other item is skipped here.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the item lookup, configuration read, or a
    /// filesystem move fails.
    pub(crate) async fn move_generated_library_trickplay_data(
        &self,
        item_id: Uuid,
    ) -> Result<(), ServiceError> {
        let lock = self.item_locks.get(&item_id.to_string());
        let _item = lock.lock().await;
        let Some(entity) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
        if !is_library_video(&entity) {
            return Ok(());
        }
        let Some(library) = self.library_options(&entity).await? else {
            return Ok(());
        };
        if !library.enable_trickplay_image_extraction {
            return Ok(());
        }
        let options = self.trickplay_options().await?;
        let streams = self.media_streams(item_id).await?;
        let Some(media_path) = Self::can_generate_trickplay(
            &entity,
            options.interval,
            !streams.is_empty(),
            self.is_active_recording(&entity).await?,
        ) else {
            return Ok(());
        };
        let local_root = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(&media_path),
            false,
        );
        // `MediaLocation::of_file` is the location a delete uses
        // (`containing_folder_path_at`) for every path that reaches here:
        // `can_generate_trickplay` admits only a file, never a disc rip's
        // folder. When rips gain trickplay, read their location as the delete
        // does.
        let media_root = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(&media_path),
            true,
        );
        // No folder beside the media (a path at the root): nothing to move.
        if media_root.is_empty() {
            return Ok(());
        }
        let (source_root, destination_root) = if library.save_trickplay_with_media {
            (&local_root, &media_root)
        } else {
            (&media_root, &local_root)
        };
        for info in self.resolutions_for(item_id).await?.values() {
            let sub = format!("{} - {}x{}", info.width, info.tile_width, info.tile_height);
            let source = Path::new(source_root).join(&sub);
            let destination = Path::new(destination_root).join(&sub);
            // Match the source's enumerated-file guard, including a populated
            // destination that receives matching-file overwrites, while an
            // existing empty destination remains untouched.
            if source.is_dir()
                && has_files(&source)?
                && (!destination.is_dir() || has_files(&destination)?)
            {
                move_content(&source, &destination)?;
                tracing::info!(item = %item_id, to = %destination.display(), "moved trickplay images");
            }
        }
        Ok(())
    }
    /// The scan and Generate Trickplay Images hand in the row and streams
    /// they hold; the by-id forms read them from the repository. All use one
    /// extraction and storage implementation.
    ///
    /// `check` is a checking pass ([`TrickplayManager::check_trickplay_data`]),
    /// for which this returns whether the item's sets now match
    /// ([`Self::sets_match`]); it is `true` otherwise.
    async fn refresh_media(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        media_streams: Vec<MediaStream>,
        replace: bool,
        check: Option<&TrickplayCheck>,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        // Held across discovery, the existing-tiles checks, generation and
        // the prune, as upstream's `_resourcePool` is held across the first
        // three; a generation slot bounds the ffmpeg work, held per video
        // across the one run that extracts its widths and their tiling.
        let lock = self.item_locks.get(&item_id.to_string());
        let _item = lock.lock().await;
        let has_streams = !media_streams.is_empty();
        let video_stream = media_streams
            .iter()
            .find(|stream| stream.stream_type == MediaStreamType::Video)
            .cloned();
        let mut options = self.trickplay_options().await?;
        let Some(media_path) = Self::can_generate_trickplay(
            entity,
            options.interval,
            has_streams,
            self.is_active_recording(entity).await?,
        ) else {
            return Ok(true);
        };
        // The item's rows, read once: every later step reads this map, and
        // each write below updates it. The item lock keeps other refreshes
        // and moves out; an item deletion or rekey meanwhile is not seen, as
        // the reads between this refresh's steps could miss it too.
        let mut rows = self.resolutions_for(item_id).await?;
        // Catalog what is already on disk before pruning or generating.
        if !replace {
            self.discover_existing_trickplay(
                entity,
                item_id,
                &media_path,
                library_options.save_trickplay_with_media,
                &options,
                &mut rows,
            )
            .await?;
        }

        let trickplay_dir = self.path_manager.trickplay_directory(
            item_id,
            MediaLocation::of_file(&media_path),
            library_options.save_trickplay_with_media,
        );

        let enabled = library_options.enable_trickplay_image_extraction;
        // When extraction is disabled and files live next to media, treat them
        // as user-managed: discovery above already catalogued what is on disk.
        if !enabled && !replace && library_options.save_trickplay_with_media {
            return Ok(true);
        }

        // Upstream also prunes first on a Replace. Ferrofin's Replace keeps
        // each old set until its new one is swapped in (D7), so a video never
        // goes without images and a failed Replace loses nothing.
        if !enabled {
            // Prune existing data.
            if let Err(e) = off_runtime(|| {
                if Path::new(&trickplay_dir).exists() {
                    std::fs::remove_dir_all(&trickplay_dir)
                } else {
                    Ok(())
                }
            }) {
                tracing::warn!(
                    directory = trickplay_dir,
                    "unable to clear trickplay directory: {e}"
                );
            }
            self.delete_trickplay_data(item_id).await?;
            rows.clear();
            if !replace {
                return Ok(true);
            }
        }

        Self::clamp_generation_interval(&mut options);
        tracing::debug!(item_id = %item_id, replace, check = check.is_some(), "trickplay refresh");
        let ctx = WidthRefreshContext {
            item_id,
            media_path: &media_path,
            video_stream,
            media_streams,
            trickplay_dir: &trickplay_dir,
            save_with_media: library_options.save_trickplay_with_media,
        };
        // On a checking pass, the widths Ferrofin made sets at (earlier
        // passes' and those a scan or refresh recorded), as this video narrows
        // them.
        let previous: Option<HashSet<i32>> =
            check.zip(ctx.video_stream.as_ref()).map(|(check, stream)| {
                check
                    .previous_widths
                    .iter()
                    .map(|width| generated_width(*width, stream).0)
                    .collect()
            });
        let generated = self
            .refresh_widths(&ctx, replace, previous.is_some(), &mut rows, &options)
            .await;

        if replace || previous.is_some() {
            self.remove_dropped_widths(item_id, &generated, previous.as_ref(), &mut rows)
                .await?;
        }
        Self::prune_unexpected_folders(&trickplay_dir, &rows);
        Ok(previous.is_none_or(|previous| {
            Self::sets_match(
                item_id,
                &trickplay_dir,
                &generated,
                &previous,
                options.full_chroma,
                &rows,
            )
        }))
    }

    /// Refreshes each selected width, returning the widths generated for this
    /// video. `rows` are the item's stored rows, which imports and generated
    /// sets add to; on a checking pass (`check_chroma`), a stored set at
    /// another chroma is regenerated and swapped in.
    ///
    /// Each width is checked first ([`Self::prepare_width`]: skipped,
    /// imported, or to generate), then the widths to generate are made
    /// together by [`Self::generate_sets`], from one decode and in one
    /// generation slot, so a scan's backlog of videos waiting for a slot gets
    /// each video's sets together. Upstream checks and generates one width at
    /// a time, taking its `_resourcePool` per width (gl-1227, gl-1228).
    async fn refresh_widths(
        &self,
        ctx: &WidthRefreshContext<'_>,
        replace: bool,
        check_chroma: bool,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
        options: &TrickplayOptions,
    ) -> HashSet<i32> {
        let mut generated = HashSet::new();
        let mut pending = Vec::new();
        for width in &options.width_resolutions {
            if *width <= 0 {
                continue;
            }
            let target = ctx
                .video_stream
                .as_ref()
                .map(|stream| generated_width(*width, stream).0)
                .filter(|target| *target > 0);
            // A video narrower than two settings gives both one set.
            if let Some(target) = target
                && !generated.insert(target)
            {
                continue;
            }
            let replace_width = replace
                || (check_chroma
                    && target
                        .and_then(|target| rows.get(&target))
                        .is_some_and(|row| {
                            chroma_mismatch(ctx.trickplay_dir, row, options.full_chroma)
                        }));
            // Per-width failures are logged and do not abort the remaining
            // resolutions (the C# catch-and-log).
            match self
                .prepare_width(ctx, replace_width, *width, options, rows)
                .await
            {
                Ok(Some(set)) => pending.push(set),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(item_id = %ctx.item_id, width, "error creating trickplay images: {e}");
                }
            }
        }
        if !pending.is_empty() {
            let results = self.generate_sets(ctx, &pending, options, rows).await;
            for (set, result) in pending.iter().zip(results) {
                if let Err(e) = result {
                    tracing::error!(item_id = %ctx.item_id, width = set.width, "error creating trickplay images: {e}");
                }
            }
        }
        generated
    }

    /// Whether an item's sets match the settings after a checking pass: each
    /// selected width's set, where it has one, at the chroma asked for, and no
    /// width of `previous` (those Ferrofin made sets at) left behind. A set
    /// that could not be replaced (a failed generation, a missing file) keeps
    /// the pass from being recorded, so the next run checks it again. A width
    /// with no set at all does not: every run generates a missing width anyway.
    /// `rows` are the item's stored rows after the pass.
    fn sets_match(
        item_id: Uuid,
        trickplay_dir: &str,
        generated: &HashSet<i32>,
        previous: &HashSet<i32>,
        full_chroma: bool,
        rows: &HashMap<i32, TrickplayInfoEntity>,
    ) -> bool {
        // No width selected for this video: nothing is removed (a video keeps
        // its images), so there is nothing to wait for either.
        if generated.is_empty() {
            return true;
        }
        let left_behind = rows
            .keys()
            .find(|width| previous.contains(width) && !generated.contains(width));
        let mismatched = generated
            .iter()
            .filter_map(|width| rows.get(width))
            .find(|row| chroma_mismatch(trickplay_dir, row, full_chroma));
        if left_behind.is_some() || mismatched.is_some() {
            tracing::debug!(
                item_id = %item_id,
                left_behind,
                mismatched = mismatched.map(|row| row.width),
                "trickplay images still differ from the settings"
            );
            return false;
        }
        true
    }

    /// After a Replace or a checking pass, removes the rows of widths no
    /// longer selected once every selected width (`generated`) has a row, so a
    /// video is never left without images; [`Self::prune_unexpected_folders`]
    /// then removes their folders. A Replace (`previous` is `None`) removes
    /// every other width, as upstream's prune-first Replace leaves none. A
    /// checking pass removes only the widths Ferrofin made sets at (the
    /// passes' widths, and those [`Self::record_set_made`] recorded), so a
    /// user-placed grid stays. `rows` are the item's stored rows, which the
    /// removed widths leave.
    async fn remove_dropped_widths(
        &self,
        item_id: Uuid,
        generated: &HashSet<i32>,
        previous: Option<&HashSet<i32>>,
        rows: &mut HashMap<i32, TrickplayInfoEntity>,
    ) -> Result<(), ServiceError> {
        if generated.is_empty() || !generated.iter().all(|width| rows.contains_key(width)) {
            return Ok(());
        }
        let dropped: Vec<i32> = rows
            .keys()
            .copied()
            .filter(|width| {
                !generated.contains(width) && previous.is_none_or(|p| p.contains(width))
            })
            .collect();
        for width in dropped {
            tracing::debug!(item_id = %item_id, width, "removing trickplay images of a width no longer selected");
            self.delete_trickplay_row(item_id, width).await?;
            rows.remove(&width);
        }
        Ok(())
    }

    /// Records the current trickplay widths and chroma as those of the last
    /// finished Generate Trickplay Images pass when none are recorded, as on
    /// the first boot after upgrading: the images on disk are taken to match
    /// them, so an upgrade never starts a library-wide job, and a change made
    /// before the task's first run is still caught.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the configuration or `FerrofinMeta`
    /// cannot be read or written.
    pub async fn record_trickplay_settings_once(&self) -> Result<(), ServiceError> {
        let current = PassSettings::of(&self.trickplay_options().await?);
        let _settings = self.pass_settings.lock().await;
        self.recorded_pass_settings(&current).await.map(drop)
    }

    /// Records, before a set of `width` is made under `options`, that a set
    /// may be on disk the recorded settings do not describe: `width` joins the
    /// `unfinished` widths unless the recorded settings already cover it (it
    /// is among their widths, at their chroma) or it is there already. A
    /// library scan or an item refresh after a settings change thus leaves a
    /// set the next checking pass removes once its width is dropped, or
    /// replaces at the chroma asked for, even after a change back. Settings
    /// this version cannot read already check everything once and remove no
    /// width, so they are left as they are.
    async fn record_set_made(
        &self,
        width: i32,
        options: &TrickplayOptions,
    ) -> Result<(), ServiceError> {
        let current = PassSettings::of(options);
        let _settings = self.pass_settings.lock().await;
        let Some(stored) = self.recorded_pass_settings(&current).await? else {
            return Ok(());
        };
        let Ok(mut last) = serde_json::from_str::<PassSettings>(&stored) else {
            return Ok(());
        };
        let covered = last.widths.contains(&width) && last.full_chroma == current.full_chroma;
        if covered || last.unfinished.as_ref().is_some_and(|w| w.contains(&width)) {
            return Ok(());
        }
        tracing::debug!(
            width,
            "trickplay images made under settings no pass has recorded"
        );
        let unfinished = last.unfinished.get_or_insert_default();
        unfinished.push(width);
        unfinished.sort_unstable();
        self.db.meta_set(PASS_SETTINGS_KEY, &last.encode()?).await?;
        Ok(())
    }

    /// The recorded settings of the last finished pass, recording `current`
    /// (and returning `None`) when there are none.
    async fn recorded_pass_settings(
        &self,
        current: &PassSettings,
    ) -> Result<Option<String>, ServiceError> {
        let stored = self.db.meta_get(PASS_SETTINGS_KEY).await?;
        if stored.is_none() {
            self.db
                .meta_set(PASS_SETTINGS_KEY, &current.encode()?)
                .await?;
        }
        Ok(stored)
    }

    /// Refreshes an item from its stored row and streams.
    async fn refresh_stored(
        &self,
        item_id: Uuid,
        replace: bool,
        check: Option<&TrickplayCheck>,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        let Some(entity) = self.items.retrieve_item(item_id).await? else {
            return Ok(true);
        };
        let streams = self.media_streams(item_id).await?;
        self.refresh_media(
            item_id,
            &entity,
            streams.into_iter().map(stream_to_dto).collect(),
            replace,
            check,
            library_options,
        )
        .await
    }
}

#[async_trait]
impl TrickplayManager for FerrofinTrickplayManager {
    async fn refresh_trickplay_data(
        &self,
        item_id: Uuid,
        replace: bool,
        library_options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.refresh_stored(item_id, replace, None, library_options)
            .await
            .map(drop)
    }

    async fn refresh_trickplay_for_media(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        streams: Option<&[ferrofin_db::entities::base_items::MediaStreamInfoEntity]>,
        replace: bool,
        library_options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let stored_streams;
        let streams = if let Some(streams) = streams {
            streams
        } else {
            stored_streams = self.media_streams(item_id).await?;
            &stored_streams
        };
        self.refresh_media(
            item_id,
            entity,
            streams.iter().cloned().map(stream_to_dto).collect(),
            replace,
            None,
            library_options,
        )
        .await
        .map(drop)
    }

    async fn begin_trickplay_pass(&self) -> Result<Option<TrickplayCheck>, ServiceError> {
        let current = PassSettings::of(&self.trickplay_options().await?);
        let _settings = self.pass_settings.lock().await;
        let Some(stored) = self.recorded_pass_settings(&current).await? else {
            return Ok(None);
        };
        // With no width selected a pass makes no set and removes none (a video
        // keeps its images), so it checks nothing and records nothing: the
        // next change still removes the widths recorded now.
        if current.widths.is_empty() {
            return Ok(None);
        }
        // A value this version cannot read checks everything once and
        // removes no width.
        let mut last = serde_json::from_str::<PassSettings>(&stored).unwrap_or(PassSettings {
            widths: Vec::new(),
            full_chroma: current.full_chroma,
            unfinished: Some(Vec::new()),
        });
        if last == current {
            return Ok(None);
        }
        tracing::debug!(previous = stored, "trickplay settings changed");
        let mut previous_widths: Vec<i32> = last
            .widths
            .iter()
            .chain(last.unfinished.iter().flatten())
            .copied()
            .collect();
        previous_widths.sort_unstable();
        previous_widths.dedup();
        // Recorded before any set is replaced, so an interrupted pass is
        // checked again whatever the settings are next.
        let unfinished = last.unfinished.get_or_insert_default();
        unfinished.extend(&current.widths);
        unfinished.sort_unstable();
        unfinished.dedup();
        let begun = unfinished.clone();
        self.db.meta_set(PASS_SETTINGS_KEY, &last.encode()?).await?;
        let check = PassCheck {
            settings: current,
            begun,
        };
        Ok(Some(TrickplayCheck {
            previous_widths,
            settings: serde_json::to_string(&check).map_err(ServiceError::backend_source)?,
        }))
    }

    async fn check_trickplay_data(
        &self,
        item_id: Uuid,
        check: &TrickplayCheck,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        self.refresh_stored(item_id, false, Some(check), library_options)
            .await
    }

    async fn check_trickplay_for_media(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        streams: &[ferrofin_db::entities::base_items::MediaStreamInfoEntity],
        check: &TrickplayCheck,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        self.refresh_media(
            item_id,
            entity,
            streams.iter().cloned().map(stream_to_dto).collect(),
            false,
            Some(check),
            library_options,
        )
        .await
    }

    async fn finish_trickplay_pass(&self, check: &TrickplayCheck) -> Result<(), ServiceError> {
        let PassCheck {
            settings: mut finished,
            begun,
        } = serde_json::from_str(&check.settings).map_err(ServiceError::backend_source)?;
        let _settings = self.pass_settings.lock().await;
        // Widths recorded as made since the pass began (a scan under settings
        // changed again meanwhile) stay recorded, so the next change still
        // removes their sets. A width the pass began with is not told apart:
        // a set a scan made at it mid-pass at the other chroma waits for the
        // next settings change to be checked.
        let made: Vec<i32> = self
            .db
            .meta_get(PASS_SETTINGS_KEY)
            .await?
            .and_then(|stored| serde_json::from_str::<PassSettings>(&stored).ok())
            .and_then(|now| now.unfinished)
            .into_iter()
            .flatten()
            .filter(|width| !begun.contains(width))
            .collect();
        if !made.is_empty() {
            finished.unfinished = Some(made);
        }
        self.db
            .meta_set(PASS_SETTINGS_KEY, &finished.encode()?)
            .await?;
        Ok(())
    }

    async fn get_trickplay_resolutions(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
        self.resolutions_for(item_id).await
    }

    async fn get_trickplay_items(
        &self,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, TrickplayInfoEntity>(
            r#"SELECT * FROM "TrickplayInfos"
               ORDER BY "ItemId", "Width" LIMIT ?1 OFFSET ?2"#,
        )
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn save_trickplay_info(&self, info: &TrickplayInfoEntity) -> Result<(), ServiceError> {
        // Upsert on the (ItemId, Width) natural key so a re-scan replaces the row.
        sqlx::query(
            r#"INSERT INTO "TrickplayInfos"
               ("ItemId", "Width", "Bandwidth", "Height", "Interval",
                "ThumbnailCount", "TileHeight", "TileWidth")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT ("ItemId", "Width") DO UPDATE SET
                 "Bandwidth" = excluded."Bandwidth",
                 "Height" = excluded."Height",
                 "Interval" = excluded."Interval",
                 "ThumbnailCount" = excluded."ThumbnailCount",
                 "TileHeight" = excluded."TileHeight",
                 "TileWidth" = excluded."TileWidth""#,
        )
        .bind(&info.item_id)
        .bind(info.width)
        .bind(info.bandwidth)
        .bind(info.height)
        .bind(info.interval)
        .bind(info.thumbnail_count)
        .bind(info.tile_height)
        .bind(info.tile_width)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn delete_trickplay_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "TrickplayInfos" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_trickplay_resolutions_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
        let mut out: HashMap<Uuid, HashMap<i32, TrickplayInfoEntity>> =
            HashMap::with_capacity(item_ids.len());
        if item_ids.is_empty() {
            return Ok(out);
        }
        // One query for every media source the page lists, grouped by item.
        for chunk in ferrofin_db::padded_in_chunks(item_ids) {
            let ph = ferrofin_db::numbered_placeholders(1..=chunk.len());
            let sql = format!(
                r#"SELECT * FROM "TrickplayInfos" WHERE "ItemId" IN ({ph})
                   ORDER BY "ItemId", "Width""#,
            );
            let mut query = sqlx::query_as::<_, TrickplayInfoEntity>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&row.item_id) {
                    out.entry(id).or_default().insert(row.width, row);
                }
            }
        }
        Ok(out)
    }

    async fn get_hls_playlist(
        &self,
        item_id: Uuid,
        width: i32,
        api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        // Port of C# `TrickplayManager.GetHlsPlaylist`: the playlist is derived
        // wholly from the stored `TrickplayInfo` row, so no on-disk tiles are
        // needed to emit it.
        let resolutions = self.resolutions_for(item_id).await?;
        let Some(info) = resolutions.get(&width) else {
            return Ok(None);
        };
        if info.thumbnail_count <= 0 {
            return Ok(None);
        }

        let resolution = format!("{}x{}", info.width, info.height);
        let layout = format!("{}x{}", info.tile_width, info.tile_height);
        let thumbnails_per_tile = info.tile_width * info.tile_height;
        // A malformed row with a zero tile grid would divide by zero; treat it as
        // "no playlist" rather than panicking.
        if thumbnails_per_tile <= 0 {
            return Ok(None);
        }
        let thumbnail_duration = f64::from(info.interval) / 1000.0;
        let tile_count = (info.thumbnail_count + thumbnails_per_tile - 1) / thumbnails_per_tile;
        let api_key = api_key.unwrap_or("");
        let item_id_dashless = item_id.simple().to_string();

        let mut out = String::with_capacity(128);
        out.push_str("#EXTM3U\n");
        let _ = writeln!(out, "#EXT-X-TARGETDURATION:{tile_count}");
        out.push_str("#EXT-X-VERSION:7\n");
        out.push_str("#EXT-X-MEDIA-SEQUENCE:1\n");
        out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        out.push_str("#EXT-X-IMAGES-ONLY\n");

        for i in 0..tile_count {
            // Every tile but the last carries a full grid of thumbnails.
            let per_tile = if i == tile_count - 1 {
                info.thumbnail_count - (i * thumbnails_per_tile)
            } else {
                thumbnails_per_tile
            };
            let inf_duration = thumbnail_duration * f64::from(per_tile);

            let _ = writeln!(out, "#EXTINF:{},", format_decimal(inf_duration));
            let _ = writeln!(
                out,
                "#EXT-X-TILES:RESOLUTION={resolution},LAYOUT={layout},DURATION={}",
                format_decimal(thumbnail_duration)
            );
            let _ = writeln!(
                out,
                "{i}.jpg?MediaSourceId={item_id_dashless}&ApiKey={api_key}"
            );
        }

        out.push_str("#EXT-X-ENDLIST\n");
        Ok(Some(out))
    }

    async fn get_trickplay_tile_path(
        &self,
        item_id: Uuid,
        width: i32,
        index: i32,
    ) -> Result<Option<String>, ServiceError> {
        // Resolve the library on every read, from the snapshot each options
        // save refreshes, so toggling the option affects HTTP tile reads
        // immediately. Existing data is moved by the migration task.
        let resolutions = self.resolutions_for(item_id).await?;
        let Some(info) = resolutions.get(&width) else {
            return Ok(None);
        };
        let entity = self.items.retrieve_item(item_id).await?;
        let library = if let Some(entity) = entity.as_ref() {
            self.library_options(entity).await?
        } else {
            None
        };
        let base = self.path_manager.trickplay_directory(
            item_id,
            entity
                .as_ref()
                .and_then(|row| row.path.as_deref())
                .map(MediaLocation::of_file)
                .unwrap_or_default(),
            library.is_some_and(|options| options.save_trickplay_with_media),
        );
        let subdir = format!("{} - {}x{}", width, info.tile_width, info.tile_height);
        let path = std::path::Path::new(&base)
            .join(subdir)
            .join(format!("{index}.jpg"));
        Ok(Some(path.to_string_lossy().into_owned()))
    }
}

/// Physical Video kinds accepted by the scheduled tasks, including owned extras.
/// The pinned query never consumes SourceTypes, so complete channel VOD remains eligible.
pub(crate) fn is_library_video(entity: &BaseItemEntity) -> bool {
    !entity.is_folder
        && !entity.is_virtual_item
        && entity
            .media_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("Video"))
        && matches!(
            crate::item_type_lookup::kind_from_type_name(&entity.type_),
            Some(
                ferrofin_model::data::BaseItemKind::Movie
                    | ferrofin_model::data::BaseItemKind::Episode
                    | ferrofin_model::data::BaseItemKind::MusicVideo
                    | ferrofin_model::data::BaseItemKind::Trailer
                    | ferrofin_model::data::BaseItemKind::Video
            )
        )
}

/// The pinned `^(\d+) - (\d+)x(\d+)$` grid shape. All dimensions must be
/// positive and fit the stored i32 row; malformed user folders are ignored.
fn parse_grid_name(name: &str) -> Option<(i32, i32, i32)> {
    let (width, grid) = name.split_once(" - ")?;
    let (columns, rows) = grid.split_once('x')?;
    let number = |value: &str| {
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse::<i32>().ok())
            .flatten()
            .filter(|value| *value > 0)
    };
    Some((number(width)?, number(columns)?, number(rows)?))
}

/// Infers padded tile count/interval exactly as DiscoverExistingTrickplayAsync:
/// retain the configured interval if consistent, otherwise snap the midpoint
/// to seconds with .NET midpoint-to-even rounding and clamp to the bounds.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // Bounds are checked before conversion; video runtimes fit exact f64 integers.
fn discovered_timing(
    runtime_ticks: Option<i64>,
    files: usize,
    tile_width: i32,
    tile_height: i32,
    configured_interval: i32,
) -> Option<(i32, i32)> {
    let capacity = tile_width
        .checked_mul(tile_height)
        .filter(|capacity| *capacity > 0)?;
    let files = i32::try_from(files).ok().filter(|files| *files > 0)?;
    let maximum = files.checked_mul(capacity)?;
    maximum.checked_add(capacity - 1)?;
    let minimum = (files - 1).checked_mul(capacity)?.checked_add(1)?;
    let Some(runtime_ticks) = runtime_ticks else {
        return Some((configured_interval.max(MIN_INTERVAL_MS), maximum));
    };
    let runtime_ms = (runtime_ticks / TICKS_PER_MS).max(0) as f64;
    let min_interval = (runtime_ms / f64::from(maximum)).ceil().max(1000.0);
    let max_interval = (runtime_ms / f64::from(minimum)).floor().max(min_interval);
    let interval = if f64::from(configured_interval) >= min_interval
        && f64::from(configured_interval) <= max_interval
    {
        f64::from(configured_interval)
    } else {
        ((min_interval.midpoint(max_interval) / 1000.0).round_ties_even() * 1000.0)
            .clamp(min_interval, max_interval)
    };
    if interval > f64::from(i32::MAX) {
        return None;
    }
    let count = (runtime_ms / interval)
        .round_ties_even()
        .clamp(f64::from(minimum), f64::from(maximum)) as i32;
    Some((interval as i32, count))
}

/// Formats a floating-point duration like C#'s `{0:0.###}` — up to three
/// fractional digits, trailing zeros (and a bare decimal point) trimmed.
fn format_decimal(value: f64) -> String {
    let mut s = format!("{value:.3}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Integer ceiling division (`ceil(a / b)`) for non-negative values; a
/// non-positive divisor yields `0`.
fn ceil_div(a: i32, b: i32) -> i32 {
    if b <= 0 { 0 } else { (a + b - 1) / b }
}

/// The per-tile peak bitrate in bits per second: `ceil(bytes·8 / tileW / tileH
/// / interval-seconds)` — the C# bandwidth formula from `CreateTiles`.
fn tile_bitrate(bytes: u64, tile_width: i32, tile_height: i32, interval_ms: i32) -> i32 {
    let denominator =
        f64::from(tile_width) * f64::from(tile_height) * (f64::from(interval_ms) / 1000.0);
    if denominator <= 0.0 {
        return 0;
    }
    // Tile files are far below 2^52 bytes, so the u64→f64 conversion is exact
    // in practice; the result saturates into i32.
    #[allow(clippy::cast_precision_loss)]
    ceil_to_i32(bytes as f64 * 8.0 / denominator)
}

/// Ceils a `f64` into an `i32`, saturating at the `i32` range.
fn ceil_to_i32(value: f64) -> i32 {
    let v = value.ceil();
    if v >= f64::from(i32::MAX) {
        i32::MAX
    } else if v <= 0.0 {
        0
    } else {
        // In (0, i32::MAX) after the guards above, so the cast is lossless.
        #[allow(clippy::cast_possible_truncation)]
        {
            v as i32
        }
    }
}

/// The width a `width` setting generates at for a video, and the video's
/// width when that capped it. The width has to be even, otherwise a lot of
/// filters cannot sample it; a video narrower than the setting caps the width
/// at the video's — its video STREAM's width (`mediaSource.VideoStream.Width`,
/// `TrickplayManager.cs:427-431`), and only when it has one: the item row's
/// `Width` is 0 for a probe that found no dimensions, which is "unknown", not
/// a zero-pixel video.
fn generated_width(width: i32, video_stream: &MediaStream) -> (i32, Option<i64>) {
    let narrower = video_stream
        .width
        .map(i64::from)
        .filter(|video_width| *video_width < i64::from(width));
    let even = 2 * (width / 2);
    let generated = narrower.map_or(even, |video_width| {
        i32::try_from(2 * (video_width / 2)).unwrap_or(even)
    });
    (generated, narrower)
}

/// Whether a JPEG samples chroma as often as luma (4:4:4), from the sampling
/// factors in its frame header, read without decoding the image. Components
/// are compared with each other: ffmpeg writes 4:4:4 as `(1, 2)` on every
/// component and `jpeg-encoder` as `(1, 1)`. `None` when the file is not a
/// readable JPEG or is grayscale.
fn jpeg_full_chroma(path: &Path) -> Option<bool> {
    use std::io::{BufReader, Read as _};
    let mut file = BufReader::new(std::fs::File::open(path).ok()?);
    let mut soi = [0u8; 2];
    file.read_exact(&mut soi).ok()?;
    if soi != [0xFF, 0xD8] {
        return None;
    }
    loop {
        let mut marker = [0u8; 4];
        file.read_exact(&mut marker).ok()?;
        if marker[0] != 0xFF {
            return None;
        }
        let length = u16::from_be_bytes([marker[2], marker[3]]);
        match marker[1] {
            // Frame headers; C4 (DHT), C8 (JPG) and CC (DAC) are not.
            0xC0..=0xCF if !matches!(marker[1], 0xC4 | 0xC8 | 0xCC) => {
                let mut header = vec![0u8; usize::from(length.checked_sub(2)?)];
                file.read_exact(&mut header).ok()?;
                // precision, height (2), width (2), component count, then
                // (id, sampling, table) per component.
                let count = usize::from(*header.get(5)?);
                let factors = (0..count)
                    .map(|k| header.get(7 + 3 * k).copied())
                    .collect::<Option<Vec<u8>>>()?;
                return (count > 1).then(|| factors.iter().all(|f| *f == factors[0]));
            }
            // Start of scan or end of image before any frame header.
            0xDA | 0xD9 => return None,
            _ => file.seek_relative(i64::from(length) - 2).ok()?,
        }
    }
}

/// Jellyfin's default trickplay width, the thumbnail width its default 10 × 10
/// tile grid is sized for.
const GRID_REFERENCE_WIDTH: i32 = 320;

/// The options a set `width` px wide is made at: the configured tile grid at
/// [`GRID_REFERENCE_WIDTH`] or narrower, and above it each axis scaled by
/// `GRID_REFERENCE_WIDTH / width`, rounded down, at least 1 and never more than
/// configured. So a tile holds no more pixels than a 320 tile at the configured
/// grid, until an axis is down to 1.
///
/// Departure from Jellyfin, which packs every width into the configured grid
/// (owner decision 2026-11-05, gl-1400): clients decode a whole tile to show
/// one thumbnail, and a 640 tile at 10 × 10 is 6400 × 3600 (about 92 MB
/// decoded), which Wholphin was seen decoding again on every scrub.
fn options_at_width(options: &TrickplayOptions, width: i32) -> TrickplayOptions {
    let scale = |configured: i32| {
        if width <= GRID_REFERENCE_WIDTH {
            return configured;
        }
        let scaled = i64::from(configured) * i64::from(GRID_REFERENCE_WIDTH) / i64::from(width);
        i32::try_from(scaled.max(1)).map_or(configured, |scaled| scaled.min(configured))
    };
    TrickplayOptions {
        tile_width: scale(options.tile_width),
        tile_height: scale(options.tile_height),
        ..options.clone()
    }
}

/// Whether the row stored for `width` names the folder a generation at
/// `options` writes: then that folder replaced a set the row still serves.
fn row_names_folder(
    rows: &HashMap<i32, TrickplayInfoEntity>,
    width: i32,
    options: &TrickplayOptions,
) -> bool {
    rows.get(&width).is_some_and(|row| {
        (row.tile_width, row.tile_height) == (options.tile_width, options.tile_height)
    })
}

/// On a checking pass, whether a stored set was written at another chroma
/// than `full_chroma`, read from its first tile's frame header (D5). A tile
/// that cannot be read counts as matching.
fn chroma_mismatch(trickplay_dir: &str, row: &TrickplayInfoEntity, full_chroma: bool) -> bool {
    let tile = Path::new(trickplay_dir)
        .join(format!(
            "{} - {}x{}",
            row.width, row.tile_width, row.tile_height
        ))
        .join("0.jpg");
    let Some(found) = off_runtime(|| jpeg_full_chroma(&tile)) else {
        tracing::debug!(path = %tile.display(), "cannot read the chroma of a trickplay tile; keeping it");
        return false;
    };
    found != full_chroma
}

/// After `set` failed, drops the width folder
/// [`FerrofinTrickplayManager::prepare_width`] probed beside the media when
/// nothing was written into it; a non-empty or already removed folder makes
/// this a no-op, and `prune_unexpected_folders` sweeps any row-less folder
/// afterwards.
fn drop_probed_folder(ctx: &WidthRefreshContext<'_>, set: &PendingSet) {
    if ctx.save_with_media {
        let _ = off_runtime(|| std::fs::remove_dir(&set.output_dir));
    }
}

/// Whether `dir` exists and directly contains at least one `.jpg` file
/// (C# `HasTrickplayTiles`), stopping at the first one.
fn has_tiles(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| is_jpg_file(&entry.path()))
    })
}

/// Whether `path` is a file with a `.jpg` extension (any case).
fn is_jpg_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
}

/// The `.jpg` tiles of a grid folder discovery adopts, unsorted; a folder or
/// an entry that cannot be read fails discovery.
fn grid_tiles(path: &Path) -> Result<Vec<String>, ServiceError> {
    let mut tiles = Vec::new();
    for tile in std::fs::read_dir(path)
        .map_err(|error| ServiceError::backend(format!("cannot read trickplay grid: {error}")))?
    {
        let tile = tile
            .map_err(|error| ServiceError::backend(format!("cannot list trickplay grid: {error}")))?
            .path();
        if is_jpg_file(&tile) {
            tiles.push(tile.to_string_lossy().into_owned());
        }
    }
    Ok(tiles)
}

/// The largest [`tile_bitrate`] of `tiles`, the bandwidth of a discovered
/// grid; a tile that cannot be read fails discovery.
fn peak_bitrate(
    tiles: &[String],
    tile_width: i32,
    tile_height: i32,
    interval: i32,
) -> Result<i32, ServiceError> {
    let mut bandwidth = 0;
    for tile in tiles {
        let bytes = std::fs::metadata(tile)
            .map_err(|error| ServiceError::backend(format!("cannot stat tile: {error}")))?
            .len();
        bandwidth = bandwidth.max(tile_bitrate(bytes, tile_width, tile_height, interval));
    }
    Ok(bandwidth)
}

/// The `.jpg` files directly inside `dir`, sorted by file name; a missing or
/// unreadable directory yields an empty list.
fn jpg_files_sorted(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_jpg_file(p))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files
}

/// The write-permission check `RefreshTrickplayDataInternal` runs before
/// extracting into a media folder: creates `dir` and an empty probe file in
/// it. Unlike upstream, the probe is removed again so it never sits beside
/// the tiles; a concurrent refresh of the same width may remove it first.
fn probe_write_access(dir: &Path) -> Result<(), ServiceError> {
    tracing::debug!(directory = %dir.display(), "verifying write permission in media directory");
    std::fs::create_dir_all(dir)
        .map_err(|e| ServiceError::backend(format!("cannot create {}: {e}", dir.display())))?;
    let probe = dir.join(".jellyfin-trickplay");
    std::fs::File::create(&probe)
        .map_err(|e| ServiceError::backend(format!("cannot write {}: {e}", probe.display())))?;
    match std::fs::remove_file(&probe) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ServiceError::backend(format!(
            "cannot remove {}: {e}",
            probe.display()
        ))),
        _ => Ok(()),
    }
}

/// Moves the flat directory `src` to `dst`, replacing `dst` if present; falls
/// back to copy-and-delete when a rename crosses filesystems.
///
/// The tiles are first brought beside `dst` (renamed, or copied across
/// filesystems), then the old `dst` is renamed aside and the new one renamed
/// in, so the old set serves until the gap between those two renames. A
/// staging or old folder left by a crash has no row, so the item's next
/// refresh prunes it.
fn move_dir(src: &Path, dst: &Path) -> Result<(), ServiceError> {
    let staged = dst.with_extension("partial");
    let old = dst.with_extension("old");
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ServiceError::backend(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    for leftover in [&staged, &old] {
        if leftover.exists() {
            std::fs::remove_dir_all(leftover).map_err(|e| {
                ServiceError::backend(format!("cannot remove {}: {e}", leftover.display()))
            })?;
        }
    }
    if std::fs::rename(src, &staged).is_err() {
        // Cross-device fallback: copy the (flat) tile files, then drop the source.
        std::fs::create_dir_all(&staged).map_err(|e| {
            ServiceError::backend(format!("cannot create {}: {e}", staged.display()))
        })?;
        let entries = std::fs::read_dir(src)
            .map_err(|e| ServiceError::backend(format!("cannot read {}: {e}", src.display())))?;
        for entry in entries.filter_map(Result::ok) {
            let from = entry.path();
            if from.is_file()
                && let Some(name) = from.file_name()
            {
                std::fs::copy(&from, staged.join(name))
                    .map_err(|e| ServiceError::backend(format!("cannot copy tile: {e}")))?;
            }
        }
        std::fs::remove_dir_all(src)
            .map_err(|e| ServiceError::backend(format!("cannot remove {}: {e}", src.display())))?;
    }
    let replacing = dst.exists();
    if replacing {
        std::fs::rename(dst, &old)
            .map_err(|e| ServiceError::backend(format!("cannot replace {}: {e}", dst.display())))?;
    }
    if let Err(e) = std::fs::rename(&staged, dst) {
        // Put the old set back rather than leave none.
        let restored = if replacing {
            std::fs::rename(&old, dst)
                .err()
                .map_or_else(String::new, |restore| {
                    format!(
                        "; the old images could not be put back from {}: {restore}",
                        old.display()
                    )
                })
        } else {
            String::new()
        };
        return Err(ServiceError::backend(format!(
            "cannot move to {}: {e}{restored}",
            dst.display()
        )));
    }
    if replacing && let Err(e) = std::fs::remove_dir_all(&old) {
        tracing::warn!(path = %old.display(), "cannot remove replaced trickplay images: {e}");
    }
    Ok(())
}

/// ManagedFileSystem.MoveDirectory: rename when possible; otherwise copy flat
/// source files with overwrite, preserving other populated destination files.
fn move_migration_directory(src: &Path, dst: &Path) -> Result<(), ServiceError> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ServiceError::backend(format!("cannot create migration parent: {error}"))
        })?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(dst).map_err(|error| {
        ServiceError::backend(format!("cannot create migration destination: {error}"))
    })?;
    for entry in std::fs::read_dir(src)
        .map_err(|error| ServiceError::backend(format!("cannot read migration source: {error}")))?
    {
        let entry = entry.map_err(|error| {
            ServiceError::backend(format!("cannot list migration source: {error}"))
        })?;
        if entry.path().is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name())).map_err(|error| {
                ServiceError::backend(format!("cannot copy migrated tile: {error}"))
            })?;
        }
    }
    std::fs::remove_dir_all(src).map_err(|error| {
        ServiceError::backend(format!("cannot remove migration source: {error}"))
    })?;
    Ok(())
}

/// Migration checks any file, whereas discovery checks specifically JPEGs.
fn has_files(directory: &Path) -> Result<bool, ServiceError> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        ServiceError::backend(format!("cannot read migration directory: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            ServiceError::backend(format!("cannot list migration directory: {error}"))
        })?;
        if entry.path().is_file() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Port of the C# `MoveContent`: moves `src` into `dst`, then removes `src`'s
/// now-empty parent folder (the `.trickplay` container) when nothing is left in
/// it.
fn move_content(src: &Path, dst: &Path) -> Result<(), ServiceError> {
    move_migration_directory(src, dst)?;
    if let Some(parent) = src.parent() {
        let entries = std::fs::read_dir(parent).map_err(|error| {
            ServiceError::backend(format!("cannot read migration source parent: {error}"))
        })?;
        let mut has_directories = false;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ServiceError::backend(format!("cannot list migration source parent: {error}"))
            })?;
            has_directories |= entry.path().is_dir();
        }
        // The source enumerates directories, then attempts a nonrecursive
        // deletion. Remaining ordinary files cause its reported cleanup error.
        if !has_directories {
            std::fs::remove_dir(parent).map_err(|error| {
                ServiceError::backend(format!("cannot remove migration source parent: {error}"))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrofin_model::entities_media::MediaStream;
    use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayOutput};

    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, ServerConfiguration, TrickplayOptions};
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::system::MediaLocation;
    use uuid::Uuid;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ferrofin_db::Database;
    use ferrofin_drawing::ImageCrateEncoder;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::drawing::ImageEncoder;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::TrickplayFrameExtractor;
    use ferrofin_traits::options::ImageCollageOptions;
    use ferrofin_traits::persistence::MediaStreamRepository as _;
    use ferrofin_traits::system::PathManager;
    use ferrofin_traits::trickplay::TrickplayManager;

    use crate::app_paths::test_paths;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::path_manager::FerrofinPathManager;
    use crate::test_support::{seed_item, test_db};

    use super::FerrofinTrickplayManager;

    /// One hour, as runtime ticks.
    const HOUR_TICKS: i64 = 3_600_000 * 10_000;

    /// A config manager returning the [`TrickplayOptions`] a test sets.
    struct FixedConfig {
        options: Mutex<TrickplayOptions>,
        paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths>,
    }

    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            Arc::clone(&self.paths)
        }

        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(ServerConfiguration {
                trickplay_options: self.options.lock().expect("lock").clone(),
                ..ServerConfiguration::default()
            }))
        }

        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }

        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// The real `image`-crate encoder, recording where each trickplay tile is
    /// written.
    struct RecordingEncoder {
        inner: ImageCrateEncoder,
        tiles: Mutex<Vec<String>>,
        /// The `full_chroma` of the last tile.
        last_full_chroma: Mutex<Option<bool>>,
    }

    #[async_trait]
    impl ImageEncoder for RecordingEncoder {
        fn supported_input_formats(&self) -> Vec<String> {
            self.inner.supported_input_formats()
        }
        fn supported_output_formats(&self) -> Vec<ferrofin_model::drawing::ImageFormat> {
            self.inner.supported_output_formats()
        }
        fn name(&self) -> String {
            self.inner.name()
        }
        fn supports_image_collage_creation(&self) -> bool {
            self.inner.supports_image_collage_creation()
        }
        fn supports_image_encoding(&self) -> bool {
            self.inner.supports_image_encoding()
        }
        async fn get_image_size(
            &self,
            path: &str,
        ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
            self.inner.get_image_size(path).await
        }
        async fn get_image_blur_hash(
            &self,
            x_comp: i32,
            y_comp: i32,
            path: &str,
        ) -> Result<String, ServiceError> {
            self.inner.get_image_blur_hash(x_comp, y_comp, path).await
        }
        async fn encode_image(
            &self,
            input_path: &str,
            date_modified: chrono::DateTime<chrono::Utc>,
            output_path: &str,
            auto_orient: bool,
            orientation: Option<ferrofin_model::drawing::ImageOrientation>,
            quality: i32,
            options: &ferrofin_traits::options::ImageProcessingOptions,
            output_format: ferrofin_model::drawing::ImageFormat,
        ) -> Result<String, ServiceError> {
            self.inner
                .encode_image(
                    input_path,
                    date_modified,
                    output_path,
                    auto_orient,
                    orientation,
                    quality,
                    options,
                    output_format,
                )
                .await
        }
        async fn create_image_collage(
            &self,
            options: &ImageCollageOptions,
            library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            self.inner.create_image_collage(options, library_name).await
        }
        async fn create_splashscreen(
            &self,
            posters: &[String],
            backdrops: &[String],
        ) -> Result<(), ServiceError> {
            self.inner.create_splashscreen(posters, backdrops).await
        }
        async fn create_trickplay_tile(
            &self,
            options: &ImageCollageOptions,
            quality: i32,
            img_width: i32,
            img_height: Option<i32>,
            full_chroma: bool,
        ) -> Result<i32, ServiceError> {
            self.tiles
                .lock()
                .expect("lock")
                .push(options.output_path.clone());
            *self.last_full_chroma.lock().expect("lock") = Some(full_chroma);
            self.inner
                .create_trickplay_tile(options, quality, img_width, img_height, full_chroma)
                .await
        }
    }

    /// A frame-extraction fake writing real (decodable) JPEG frames of the
    /// requested width, recording its invocations.
    struct FakeExtractor {
        /// How many frames each run writes.
        frames: usize,
        /// The pixel height of each written frame.
        frame_height: u32,
        calls: AtomicUsize,
        last_request: Mutex<Option<(i32, i32)>>,
        /// The priority the last request carried.
        last_priority: Mutex<Option<ferrofin_model::configuration::ProcessPriorityClass>>,
        /// The `full_chroma` the last request carried.
        last_full_chroma: Mutex<Option<bool>>,
        /// The video stream the last request carried, which is what decides
        /// whether the extractor can take the hardware path at all.
        last_stream: Mutex<Option<MediaStream>>,
        /// Every stream the last request carried, in order.
        last_streams: Mutex<Vec<MediaStream>>,
        last_output: Mutex<Option<String>>,
        hold: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        /// Fails every extraction while set, as a broken file or ffmpeg would.
        fail: std::sync::atomic::AtomicBool,
        /// Every extraction's `(input_path, max_width)`, in the order they ran.
        order: Mutex<Vec<(String, i32)>>,
        /// The widths of each `extract_trickplay_frame_sets` call, in order:
        /// one entry per decode the ffmpeg extractor would make.
        sets: Mutex<Vec<Vec<i32>>>,
        /// Widths whose own extraction fails, as one width of a shared run
        /// that fell back to a run per width might.
        fail_widths: Mutex<Vec<i32>>,
        /// Returns one result fewer than asked for, as a broken extractor
        /// might.
        short_sets: std::sync::atomic::AtomicBool,
    }

    impl FakeExtractor {
        fn new(frames: usize, frame_height: u32) -> Self {
            Self {
                frames,
                frame_height,
                calls: AtomicUsize::new(0),
                last_request: Mutex::new(None),
                last_priority: Mutex::new(None),
                last_full_chroma: Mutex::new(None),
                last_stream: Mutex::new(None),
                last_streams: Mutex::new(Vec::new()),
                last_output: Mutex::new(None),
                hold: None,
                fail: std::sync::atomic::AtomicBool::new(false),
                order: Mutex::new(Vec::new()),
                sets: Mutex::new(Vec::new()),
                fail_widths: Mutex::new(Vec::new()),
                short_sets: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// The widths of each `extract_trickplay_frame_sets` call so far.
        fn sets(&self) -> Vec<Vec<i32>> {
            self.sets.lock().expect("lock").clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The `(interval_ms, max_width)` of the last extraction request.
        fn last_request(&self) -> Option<(i32, i32)> {
            *self.last_request.lock().expect("lock")
        }

        /// The video stream the last extraction request carried.
        fn last_stream(&self) -> Option<MediaStream> {
            self.last_stream.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl TrickplayFrameExtractor for FakeExtractor {
        async fn extract_trickplay_frames(
            &self,
            request: &TrickplayExtraction<'_>,
        ) -> Result<Vec<String>, ServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.order
                .lock()
                .expect("lock")
                .push((request.input_path.to_owned(), request.max_width));
            *self.last_request.lock().expect("lock") =
                Some((request.interval_ms, request.max_width));
            *self.last_priority.lock().expect("lock") = Some(request.priority);
            *self.last_full_chroma.lock().expect("lock") = Some(request.full_chroma);
            *self.last_stream.lock().expect("lock") = request.video_stream.cloned();
            *self.last_streams.lock().expect("lock") = request.media_streams.to_vec();
            if self.fail.load(Ordering::SeqCst)
                || self
                    .fail_widths
                    .lock()
                    .expect("lock")
                    .contains(&request.max_width)
            {
                return Err(ServiceError::backend("extraction failed"));
            }
            let output_dir = request.output_dir;
            *self.last_output.lock().unwrap() = Some(output_dir.to_owned());
            let max_width = request.max_width;
            std::fs::create_dir_all(output_dir).expect("create frames dir");
            let width = u32::try_from(max_width).expect("positive width");
            let mut paths = Vec::new();
            for i in 1..=self.frames {
                let path = Path::new(output_dir).join(format!("{i:08}.jpg"));
                image::RgbImage::new(width, self.frame_height)
                    .save(&path)
                    .expect("write frame jpg");
                paths.push(path.to_string_lossy().into_owned());
            }
            if let Some((entered, release)) = &self.hold {
                entered.notify_one();
                release.notified().await;
            }
            Ok(paths)
        }

        /// Records the call's widths, then makes each width on its own, as
        /// the trait's default does.
        async fn extract_trickplay_frame_sets(
            &self,
            request: &TrickplayExtraction<'_>,
            outputs: &[TrickplayOutput<'_>],
        ) -> Vec<Result<Vec<String>, ServiceError>> {
            self.sets
                .lock()
                .expect("lock")
                .push(outputs.iter().map(|output| output.max_width).collect());
            // One decode writes every width's frames as it goes, so each
            // width's directory holds frames before any width is done.
            for output in outputs {
                std::fs::create_dir_all(output.output_dir).expect("frames dir");
                std::fs::write(Path::new(output.output_dir).join("partial.jpg"), b"")
                    .expect("partial frame");
            }
            let mut sets = Vec::with_capacity(outputs.len());
            for output in outputs {
                sets.push(self.extract_trickplay_frames(&output.of(request)).await);
            }
            if self.short_sets.load(Ordering::SeqCst) {
                sets.pop();
            }
            sets
        }
    }

    /// The full manager fixture: temp app paths, real path manager / item
    /// repository / `image`-crate encoder, fake config + extractor.
    struct Rig {
        temp_dir: tempfile::TempDir,
        mgr: FerrofinTrickplayManager,
        extractor: Arc<FakeExtractor>,
        pm: Arc<dyn PathManager>,
        /// A directory for test media files inside the temp root.
        media_dir: PathBuf,
        /// The application paths the manager's configuration reports.
        paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths>,
        encoder: Arc<RecordingEncoder>,
        /// The configuration the manager reads, for a test to change.
        config: Arc<FixedConfig>,
    }

    impl Rig {
        /// Changes the trickplay settings, as an admin saving them would.
        fn set_options(&self, options: TrickplayOptions) {
            *self.config.options.lock().expect("lock") = options;
        }
    }

    fn rig(db: &Database, options: TrickplayOptions, extractor: FakeExtractor) -> Rig {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media_dir = tmp.path().join("media");
        std::fs::create_dir_all(&media_dir).expect("media dir");
        let app_paths = test_paths(tmp.path());
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> = app_paths.clone();
        let pm: Arc<dyn PathManager> = Arc::new(FerrofinPathManager::new(app_paths));
        let encoder = Arc::new(RecordingEncoder {
            inner: ImageCrateEncoder::new(),
            tiles: Mutex::new(Vec::new()),
            last_full_chroma: Mutex::new(None),
        });
        let extractor = Arc::new(extractor);
        let items = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let config = Arc::new(FixedConfig {
            options: Mutex::new(options),
            paths: Arc::clone(&paths),
        });
        let mgr = FerrofinTrickplayManager::new(
            db.clone(),
            Arc::clone(&pm),
            Arc::clone(&config) as Arc<dyn ServerConfigurationManager>,
            items,
            Arc::new(crate::FerrofinMediaStreamRepository::new(db.clone())),
            Arc::clone(&extractor) as Arc<dyn TrickplayFrameExtractor>,
            Arc::clone(&encoder) as Arc<dyn ImageEncoder>,
        );
        Rig {
            temp_dir: tmp,
            mgr,
            extractor,
            pm,
            media_dir,
            paths,
            encoder,
            config,
        }
    }

    /// Library options with trickplay extraction enabled (the C# task only
    /// generates for libraries that opt in).
    fn extraction_on() -> LibraryOptions {
        LibraryOptions {
            enable_trickplay_image_extraction: true,
            ..LibraryOptions::default()
        }
    }

    /// A 2×2-tile options fixture at the given widths.
    fn options_2x2(widths: &[i32]) -> TrickplayOptions {
        TrickplayOptions {
            width_resolutions: widths.to_vec(),
            tile_width: 2,
            tile_height: 2,
            interval: 10_000,
            ..TrickplayOptions::default()
        }
    }

    /// Seeds a video item with an existing on-disk media file, a runtime, and
    /// an optional probed video width.
    async fn seed_video(
        db: &Database,
        rig: &Rig,
        id: Uuid,
        runtime_ticks: Option<i64>,
        video_width: Option<i64>,
    ) -> String {
        crate::test_support::seed_video_item(db, id, BaseItemKind::Movie).await;
        let media_path = rig.media_dir.join(format!("{id}.mkv"));
        std::fs::write(&media_path, b"not really a video").expect("media file");
        let path_str = media_path.to_string_lossy().into_owned();
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Path" = ?2, "RunTimeTicks" = ?3, "Width" = ?4
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .bind(&path_str)
        .bind(runtime_ticks)
        .bind(video_width)
        .execute(db.writer())
        .await
        .expect("update item media fields");
        let repo = crate::FerrofinMediaStreamRepository::new(db.clone());
        repo.save_media_streams(
            id,
            &[ferrofin_db::entities::base_items::MediaStreamInfoEntity {
                item_id: guid_to_db(id),
                stream_index: 1,
                stream_type: crate::db_error::media_stream_type_to_disc(MediaStreamType::Video),
                codec: Some("h264".to_owned()),
                width: video_width,
                ..Default::default()
            }],
        )
        .await
        .expect("seed eligible media stream");
        path_str
    }

    fn info(item: Uuid, width: i32) -> TrickplayInfoEntity {
        TrickplayInfoEntity {
            item_id: guid_to_db(item),
            width,
            bandwidth: 500_000,
            height: width * 9 / 16,
            interval: 10_000,
            thumbnail_count: 240,
            tile_height: 10,
            tile_width: 10,
        }
    }

    #[tokio::test]
    async fn save_upsert_query_and_delete() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let r = rig(&db, TrickplayOptions::default(), FakeExtractor::new(0, 180));

        r.mgr
            .save_trickplay_info(&info(item, 320))
            .await
            .expect("save");
        r.mgr
            .save_trickplay_info(&info(item, 640))
            .await
            .expect("save2");

        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        assert_eq!(res.len(), 2);
        assert!(res.contains_key(&320));

        // Re-saving the same width upserts rather than duplicating.
        let mut bumped = info(item, 320);
        bumped.bandwidth = 999_999;
        r.mgr.save_trickplay_info(&bumped).await.expect("upsert");
        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res2");
        assert_eq!(res.len(), 2);
        assert_eq!(res[&320].bandwidth, 999_999);

        // The batch form reads the same rows, keyed by item, and leaves out
        // an item that has none.
        let other = Uuid::new_v4();
        let batch = r
            .mgr
            .get_trickplay_resolutions_batch(&[item, other])
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[&item].len(), 2);
        assert_eq!(batch[&item][&320].bandwidth, 999_999);

        let page = r.mgr.get_trickplay_items(10, 0).await.expect("items");
        assert_eq!(page.len(), 2);

        r.mgr.delete_trickplay_data(item).await.expect("delete");
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .expect("res3")
                .is_empty()
        );
    }

    /// Writes a video stream row for `item`, the way a real scan would.
    async fn seed_video_stream(db: &Database, item: Uuid, codec: &str, dar: Option<&str>) {
        seed_sized_video_stream(db, item, codec, dar, Some(1920)).await;
    }

    /// [`seed_video_stream`] with the stream's width (`None`: ffprobe gave
    /// none).
    async fn seed_sized_video_stream(
        db: &Database,
        item: Uuid,
        codec: &str,
        dar: Option<&str>,
        width: Option<i64>,
    ) {
        let repo = crate::FerrofinMediaStreamRepository::new(db.clone());
        let row = ferrofin_db::entities::base_items::MediaStreamInfoEntity {
            item_id: guid_to_db(item),
            stream_index: 0,
            stream_type: crate::db_error::media_stream_type_to_disc(
                ferrofin_model::entities::MediaStreamType::Video,
            ),
            codec: Some(codec.to_owned()),
            width,
            height: width.map(|_| 1080),
            aspect_ratio: dar.map(str::to_owned),
            ..Default::default()
        };
        repo.save_media_streams(item, std::slice::from_ref(&row))
            .await
            .expect("save video stream");
    }

    /// `ExtractVideoImagesOnIntervalAccelerated` maps the video stream by its
    /// index among the source's streams (`FindIndex`, `MediaEncoder.cs:925-932`),
    /// so the extractor needs all of them, in stream order: with an audio
    /// stream stored ahead of the video, the video is ffmpeg's `0:1`.
    #[tokio::test]
    async fn refresh_hands_the_extractor_every_stream_in_order() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let repo = crate::FerrofinMediaStreamRepository::new(db.clone());
        let rows: Vec<_> = [
            (0, ferrofin_model::entities::MediaStreamType::Audio, "aac"),
            (1, ferrofin_model::entities::MediaStreamType::Video, "h264"),
        ]
        .into_iter()
        .map(
            |(index, kind, codec)| ferrofin_db::entities::base_items::MediaStreamInfoEntity {
                item_id: guid_to_db(item),
                stream_index: index,
                stream_type: crate::db_error::media_stream_type_to_disc(kind),
                codec: Some(codec.to_owned()),
                width: (kind == ferrofin_model::entities::MediaStreamType::Video).then_some(1920),
                height: (kind == ferrofin_model::entities::MediaStreamType::Video).then_some(1080),
                ..Default::default()
            },
        )
        .collect();
        repo.save_media_streams(item, &rows)
            .await
            .expect("save streams");

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let streams = r.extractor.last_streams.lock().expect("lock").clone();
        let video = r.extractor.last_stream().expect("the video stream arrived");
        assert_eq!(
            streams.iter().map(|s| s.stream_type).collect::<Vec<_>>(),
            [
                ferrofin_model::entities::MediaStreamType::Audio,
                ferrofin_model::entities::MediaStreamType::Video,
            ]
        );
        assert_eq!(streams[1], video, "the video is the second stream: 0:1");
    }

    /// Upstream writes both scratch directories under `TempDirectory`
    /// (`MediaEncoder.ExtractVideoImagesOnIntervalAccelerated`,
    /// `TrickplayManager.CreateTiles`), which "Clean Cache Directory" sweeps.
    #[tokio::test]
    async fn frames_and_tiles_are_scratched_under_the_configured_temp_path() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let temp = PathBuf::from(r.paths.temp_path());
        let frames = PathBuf::from(r.extractor.last_output.lock().unwrap().clone().unwrap());
        assert_eq!(
            frames.parent(),
            Some(temp.as_path()),
            "frames: {}",
            frames.display()
        );
        let tiles = r.encoder.tiles.lock().expect("lock").clone();
        assert!(!tiles.is_empty(), "tiles were written");
        for tile in &tiles {
            let work_dir = Path::new(tile).parent().expect("tile directory");
            assert_eq!(work_dir.parent(), Some(temp.as_path()), "tile: {tile}");
            assert!(
                work_dir
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("trickplay_")),
                "tile: {tile}"
            );
        }
        // Both scratch directories are gone once the generation is done.
        assert!(!frames.exists());
        assert!(tiles.iter().all(|tile| !Path::new(tile).exists()));
    }

    #[tokio::test]
    async fn refresh_hands_the_extractor_the_video_stream() {
        // The hardware trickplay path cannot exist without this: choosing a
        // decoder needs the codec, and a hardware scaler takes fixed pixel
        // sizes, so an anamorphic source has to be un-stretched from its
        // aspect ratio. Upstream reads `mediaSource.VideoStream`; if this
        // stops arriving, every job silently falls back to software and the
        // only symptom is that trickplay is slow.
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        seed_video_stream(&db, item, "hevc", Some("16:9")).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let stream = r.extractor.last_stream().expect("the video stream arrived");
        assert_eq!(stream.codec.as_deref(), Some("hevc"));
        assert_eq!(stream.width, Some(1920));
        assert_eq!(stream.aspect_ratio.as_deref(), Some("16:9"));
    }

    #[tokio::test]
    async fn audio_only_streams_allow_outer_pruning_but_do_not_encode() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let repo = crate::FerrofinMediaStreamRepository::new(db.clone());
        repo.save_media_streams(
            item,
            &[ferrofin_db::entities::base_items::MediaStreamInfoEntity {
                item_id: guid_to_db(item),
                stream_index: 0,
                stream_type: crate::db_error::media_stream_type_to_disc(MediaStreamType::Audio),
                codec: Some("aac".to_owned()),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
        let root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        let grid = root.join("320 - 10x10");
        std::fs::create_dir_all(&grid).unwrap();
        image::RgbImage::new(3200, 1800)
            .save(grid.join("0.jpg"))
            .unwrap();
        r.mgr.save_trickplay_info(&info(item, 320)).await.unwrap();
        r.mgr
            .refresh_trickplay_data(item, false, &LibraryOptions::default())
            .await
            .unwrap();
        assert!(
            !root.exists(),
            "all-stream eligibility permits disabled internal pruning"
        );
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            0,
            "the caught per-width missing-video error never encodes"
        );
        assert!(r.extractor.last_stream().is_none());
    }

    /// The dashboard's "FFmpeg process priority" (`TrickplayOptions.ProcessPriority`)
    /// reaches the extraction.
    #[tokio::test]
    async fn the_configured_ffmpeg_priority_reaches_the_extraction() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let options = TrickplayOptions {
            process_priority: ferrofin_model::configuration::ProcessPriorityClass::Idle,
            ..options_2x2(&[320])
        };
        let r = rig(&db, options, FakeExtractor::new(6, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(
            *r.extractor.last_priority.lock().expect("lock"),
            Some(ferrofin_model::configuration::ProcessPriorityClass::Idle)
        );
    }

    #[tokio::test]
    async fn refresh_generates_tiles_and_info_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        // Six 320×180 frames on a 2×2 grid → ceil(6/4) = 2 tile files.
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(6, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(r.extractor.calls(), 1);
        assert_eq!(r.extractor.last_request(), Some((10_000, 320)));

        let tile_dir = Path::new(&r.pm.trickplay_directory(
            item,
            MediaLocation::of_file(&media),
            false,
        ))
        .join("320 - 2x2");
        assert!(tile_dir.join("0.jpg").is_file(), "first tile written");
        assert!(tile_dir.join("1.jpg").is_file(), "second tile written");
        assert!(!tile_dir.join("2.jpg").exists(), "only two tiles");

        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        assert_eq!(res.len(), 1);
        let row = &res[&320];
        assert_eq!(row.width, 320);
        assert_eq!(row.height, 180);
        assert_eq!(row.thumbnail_count, 6);
        assert_eq!(row.interval, 10_000);
        assert_eq!((row.tile_width, row.tile_height), (2, 2));
        assert!(row.bandwidth > 0, "bandwidth computed from tile bytes");

        // The tile path resolver agrees with where the tiles were written.
        let tile = r
            .mgr
            .get_trickplay_tile_path(item, 320, 0)
            .await
            .expect("tile")
            .expect("some path");
        assert!(Path::new(&tile).is_file(), "resolved tile exists: {tile}");
    }

    /// `TrickplayOptions::full_chroma` reaches both stages, and is off unless
    /// an admin turns it on.
    #[rstest::rstest]
    #[case::by_default(None, false)]
    #[case::on(Some(true), true)]
    #[tokio::test]
    async fn the_chroma_option_reaches_the_frames_and_the_tiles(
        #[case] setting: Option<bool>,
        #[case] expected: bool,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let mut options = options_2x2(&[320]);
        if let Some(setting) = setting {
            options.full_chroma = setting;
        }
        let r = rig(&db, options, FakeExtractor::new(1, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(
            *r.extractor.last_full_chroma.lock().expect("lock"),
            Some(expected)
        );
        assert_eq!(
            *r.encoder.last_full_chroma.lock().expect("lock"),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn refresh_skips_existing_and_replace_regenerates() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        assert_eq!(r.extractor.calls(), 1);

        // Tiles + row exist → a non-replace refresh does nothing.
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("second");
        assert_eq!(r.extractor.calls(), 1, "existing data is skipped");

        // Replace re-extracts (the swap is covered below).
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(r.extractor.calls(), 2, "replace regenerates");
        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        assert_eq!(res[&320].thumbnail_count, 4);
    }

    /// A stored width whose tiles are only in the other location keeps its
    /// row (they are found there) and is made in the selected folder.
    #[tokio::test]
    async fn a_stored_width_with_no_tiles_in_the_selected_folder_is_made_there() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let beside =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("320 - 2x2");
        std::fs::create_dir_all(&beside).expect("set beside the media");
        std::fs::write(beside.join("0.jpg"), b"tile").expect("tile");
        r.mgr
            .save_trickplay_info(&TrickplayInfoEntity {
                tile_width: 2,
                tile_height: 2,
                ..info(item, 320)
            })
            .await
            .expect("row");

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(r.extractor.sets(), [vec![320]], "the set is made");
        assert!(set_dir(&r, item, &media, 320).join("0.jpg").is_file());
    }

    /// The folder of an item's internal set at `width` under `options_2x2`,
    /// at that width's grid.
    fn set_dir(r: &Rig, item: Uuid, media: &str, width: i32) -> PathBuf {
        let grid = super::options_at_width(&options_2x2(&[]), width);
        PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(media), false)).join(
            format!("{width} - {}x{}", grid.tile_width, grid.tile_height),
        )
    }

    /// `options_2x2` with chroma sampled as often as luma.
    fn full_chroma_2x2(widths: &[i32]) -> TrickplayOptions {
        TrickplayOptions {
            full_chroma: true,
            ..options_2x2(widths)
        }
    }

    /// The chroma is read from the frame header, comparing components with
    /// each other: ffmpeg's mjpeg declares 4:4:4 as `(1, 2)` on every
    /// component, `jpeg-encoder` as `(1, 1)`.
    #[rstest::rstest]
    #[case::ffmpeg_full(&[0x12, 0x12, 0x12], Some(true))]
    #[case::encoder_full(&[0x11, 0x11, 0x11], Some(true))]
    #[case::subsampled(&[0x22, 0x11, 0x11], Some(false))]
    #[case::grayscale(&[0x11], None)]
    fn the_chroma_is_read_from_the_frame_header(
        #[case] factors: &[u8],
        #[case] expected: Option<bool>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("0.jpg");
        // SOI, an APP0 to skip, then SOF0: length, precision, height, width,
        // component count and (id, sampling, table) per component.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        let count = u8::try_from(factors.len()).expect("count");
        bytes.extend([0xFF, 0xC0, 0x00, 8 + 3 * count, 8, 0, 16, 0, 16, count]);
        for (id, sampling) in (1u8..).zip(factors) {
            bytes.extend([id, *sampling, 0]);
        }
        bytes.extend([0xFF, 0xD9]);
        std::fs::write(&path, bytes).expect("write");
        assert_eq!(super::jpeg_full_chroma(&path), expected);
    }

    /// The new tiles replace the old set whole, and a staging folder a crash
    /// left behind is not mixed into them.
    #[test]
    fn move_dir_replaces_the_old_set_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (src, dst) = (dir.path().join("work"), dir.path().join("320 - 2x2"));
        let staged = dir.path().join("320 - 2x2.partial");
        let old = dir.path().join("320 - 2x2.old");
        for (folder, files) in [
            (&src, &["0.jpg"][..]),
            (&dst, &["0.jpg", "1.jpg"]),
            (&staged, &["stale.jpg"]),
            (&old, &["stale.jpg"]),
        ] {
            std::fs::create_dir_all(folder).expect("folder");
            for file in files {
                std::fs::write(folder.join(file), folder.to_string_lossy().as_bytes())
                    .expect("file");
            }
        }
        super::move_dir(&src, &dst).expect("move");
        assert_eq!(
            std::fs::read(dst.join("0.jpg")).expect("tile"),
            src.to_string_lossy().as_bytes()
        );
        assert!(!dst.join("1.jpg").exists() && !dst.join("stale.jpg").exists());
        assert!(!src.exists() && !staged.exists() && !old.exists());
    }

    /// A row save that fails after the swap keeps the new tiles only when the
    /// width's stored row names their folder: the set it replaced is gone.
    #[test]
    fn a_failed_save_keeps_only_a_folder_its_stored_row_names() {
        let rows: std::collections::HashMap<i32, TrickplayInfoEntity> =
            [(320, info(Uuid::nil(), 320))].into();
        let grid_10x10 = TrickplayOptions {
            tile_width: 10,
            tile_height: 10,
            ..TrickplayOptions::default()
        };
        assert!(super::row_names_folder(&rows, 320, &grid_10x10));
        assert!(!super::row_names_folder(&rows, 640, &grid_10x10), "no row");
        assert!(
            !super::row_names_folder(&rows, 320, &options_2x2(&[320])),
            "a row of another grid"
        );
    }

    /// One Generate Trickplay Images run over `items`, as the task makes it:
    /// a checking pass after a settings change, recorded once every item
    /// matches, and a plain refresh otherwise.
    async fn run_pass(r: &Rig, items: &[Uuid]) {
        let check = r.mgr.begin_trickplay_pass().await.expect("begin");
        let mut complete = true;
        for item in items {
            match &check {
                Some(check) => {
                    complete &= r
                        .mgr
                        .check_trickplay_data(*item, check, &extraction_on())
                        .await
                        .expect("check");
                }
                None => r
                    .mgr
                    .refresh_trickplay_data(*item, false, &extraction_on())
                    .await
                    .expect("refresh"),
            }
        }
        if let Some(check) = check
            && complete
        {
            r.mgr.finish_trickplay_pass(&check).await.expect("finish");
        }
    }

    /// The stored widths of an item, in order.
    async fn stored_widths(r: &Rig, item: Uuid) -> Vec<i32> {
        let mut widths: Vec<i32> = r
            .mgr
            .get_trickplay_resolutions(item)
            .await
            .expect("rows")
            .into_keys()
            .collect();
        widths.sort_unstable();
        widths
    }

    /// A run with no width selected keeps every set and is no checking pass,
    /// so the widths of the last pass that made sets stay recorded and the
    /// next change still removes them.
    #[tokio::test]
    async fn a_run_with_no_width_selected_keeps_the_widths_the_next_change_removes() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        run_pass(&r, &[item]).await;

        r.set_options(options_2x2(&[]));
        assert_eq!(r.mgr.begin_trickplay_pass().await.expect("begin"), None);
        run_pass(&r, &[item]).await;
        assert_eq!(stored_widths(&r, item).await, vec![320], "the images stay");

        r.set_options(options_2x2(&[640]));
        run_pass(&r, &[item]).await;
        assert_eq!(stored_widths(&r, item).await, vec![640]);
    }

    /// A set made outside a checking pass (a library scan, an item refresh)
    /// under settings other than the recorded ones is recorded as made, so a
    /// later change removes it; under the recorded settings nothing is
    /// written.
    #[tokio::test]
    async fn a_width_made_outside_a_pass_after_a_change_goes_on_the_next_change() {
        let db = test_db().await;
        let (item, other) = (Uuid::new_v4(), Uuid::new_v4());
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        run_pass(&r, &[item]).await;
        seed_video(&db, &r, other, Some(HOUR_TICKS), None).await;
        r.mgr
            .refresh_trickplay_data(other, false, &extraction_on())
            .await
            .expect("scan under the recorded settings");
        assert_eq!(
            db.meta_get(super::PASS_SETTINGS_KEY)
                .await
                .expect("meta")
                .as_deref(),
            Some(r#"{"widths":[320],"full_chroma":false}"#)
        );

        r.set_options(options_2x2(&[640]));
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("scan");
        assert_eq!(stored_widths(&r, item).await, vec![320, 640]);
        r.set_options(options_2x2(&[480]));
        run_pass(&r, &[item, other]).await;
        assert_eq!(stored_widths(&r, item).await, vec![480]);
        assert_eq!(stored_widths(&r, other).await, vec![480]);
        assert_eq!(r.mgr.begin_trickplay_pass().await.expect("begin"), None);
    }

    /// A set a scan makes while a checking pass runs, under settings changed
    /// again since the pass began, stays recorded as made when the pass
    /// finishes.
    #[tokio::test]
    async fn a_width_made_while_a_pass_runs_stays_recorded_after_it() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        run_pass(&r, &[item]).await;

        r.set_options(options_2x2(&[640]));
        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("check");
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check")
        );
        r.set_options(options_2x2(&[480]));
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("scan");
        assert_eq!(stored_widths(&r, item).await, vec![480, 640]);
        r.set_options(options_2x2(&[640]));
        r.mgr.finish_trickplay_pass(&check).await.expect("finish");

        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("the scan's width is checked");
        assert_eq!(check.previous_widths, vec![480, 640]);
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check")
        );
        r.mgr.finish_trickplay_pass(&check).await.expect("finish");
        assert_eq!(stored_widths(&r, item).await, vec![640]);
        assert_eq!(r.mgr.begin_trickplay_pass().await.expect("begin"), None);
    }

    /// A set a scan makes at another chroma is checked once the settings
    /// change back, and replaced at the chroma asked for.
    #[tokio::test]
    async fn a_set_made_at_another_chroma_is_checked_after_a_change_back() {
        let db = test_db().await;
        let (item, other) = (Uuid::new_v4(), Uuid::new_v4());
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        run_pass(&r, &[item]).await;

        r.set_options(full_chroma_2x2(&[320]));
        let media = seed_video(&db, &r, other, Some(HOUR_TICKS), None).await;
        r.mgr
            .refresh_trickplay_data(other, false, &extraction_on())
            .await
            .expect("scan");
        let tile = set_dir(&r, other, &media, 320).join("0.jpg");
        assert_eq!(super::jpeg_full_chroma(&tile), Some(true));

        r.set_options(options_2x2(&[320]));
        run_pass(&r, &[item, other]).await;
        assert_eq!(super::jpeg_full_chroma(&tile), Some(false));
        assert_eq!(r.mgr.begin_trickplay_pass().await.expect("begin"), None);
    }

    #[test]
    fn a_file_without_a_frame_header_has_no_chroma() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases: [(&str, &[u8]); 3] = [
            ("not-a-jpeg", b"not a jpeg"),
            ("scan-first", &[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]),
            ("truncated", &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 8]),
        ];
        for (name, bytes) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).expect("write");
            assert_eq!(super::jpeg_full_chroma(&path), None, "{name}");
        }
        assert_eq!(super::jpeg_full_chroma(&dir.path().join("missing")), None);
    }

    /// The first boot after upgrading records the settings and checks
    /// nothing; only a change checks, and changing it back cancels out until
    /// a pass has started. A pass that did not finish is checked again, even
    /// after a change back, and its widths count as made.
    #[tokio::test]
    async fn only_a_settings_change_since_the_last_finished_pass_checks() {
        let db = test_db().await;
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let begin = || async { r.mgr.begin_trickplay_pass().await.expect("begin") };
        r.mgr.record_trickplay_settings_once().await.expect("boot");
        assert_eq!(
            db.meta_get(super::PASS_SETTINGS_KEY)
                .await
                .expect("meta")
                .as_deref(),
            Some(r#"{"widths":[320],"full_chroma":false}"#)
        );
        assert_eq!(begin().await, None);

        r.set_options(full_chroma_2x2(&[640, 320, 640]));
        r.set_options(options_2x2(&[320]));
        assert_eq!(begin().await, None, "changed back before any pass");

        r.set_options(full_chroma_2x2(&[640, 320, 640]));
        let check = begin().await.expect("a change checks");
        assert_eq!(check.previous_widths, vec![320]);
        r.set_options(options_2x2(&[320]));
        let again = begin().await.expect("an unfinished pass is checked again");
        assert_eq!(again.previous_widths, vec![320, 640]);
        r.mgr.finish_trickplay_pass(&again).await.expect("finish");
        assert_eq!(begin().await, None);

        // Order and repeats of the widths do not count as a change.
        r.set_options(full_chroma_2x2(&[640, 320]));
        let check = begin().await.expect("check");
        r.mgr.finish_trickplay_pass(&check).await.expect("finish");
        r.set_options(full_chroma_2x2(&[640, 320, 640]));
        assert_eq!(begin().await, None);

        // A value this version cannot read checks once and removes nothing.
        db.meta_set(super::PASS_SETTINGS_KEY, "not json")
            .await
            .expect("meta");
        assert_eq!(
            begin().await.expect("check").previous_widths,
            Vec::<i32>::new()
        );
    }

    /// B7/B10: a checking pass replaces a set written at the old chroma, and
    /// the old set serves until the new one is swapped in; outside a checking
    /// pass no tile is read and nothing is regenerated.
    #[tokio::test]
    async fn a_checking_pass_swaps_in_a_set_at_the_new_chroma_while_the_old_one_serves() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(4, 180);
        extractor.hold = Some((entered.clone(), release.clone()));
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), extractor);
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        release.notify_one();
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        entered.notified().await;
        r.mgr.begin_trickplay_pass().await.expect("record");
        let tile = set_dir(&r, item, &media, 320).join("0.jpg");
        assert_eq!(super::jpeg_full_chroma(&tile), Some(false));

        r.set_options(full_chroma_2x2(&[320]));
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("plain refresh");
        assert_eq!(r.extractor.calls(), 1, "a plain refresh never checks");

        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("check");
        let pass = {
            let mgr = r.mgr.clone();
            let check = check.clone();
            tokio::spawn(async move {
                mgr.check_trickplay_data(item, &check, &extraction_on())
                    .await
            })
        };
        entered.notified().await;
        let served = r
            .mgr
            .get_trickplay_tile_path(item, 320, 0)
            .await
            .expect("path")
            .expect("row");
        assert_eq!(
            super::jpeg_full_chroma(Path::new(&served)),
            Some(false),
            "the old set serves while the new one is made"
        );
        release.notify_one();
        assert!(pass.await.expect("join").expect("check"), "item matches");
        assert_eq!(r.extractor.calls(), 2);
        assert_eq!(super::jpeg_full_chroma(&tile), Some(true));

        // Checked again, the set matches: nothing is generated.
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("again")
        );
        assert_eq!(r.extractor.calls(), 2);
    }

    /// B8/B14: a failed replacement, on a checking pass or an explicit
    /// Replace, keeps the old set and its row, reports the item unfinished so
    /// the pass is not recorded, and the next run retries.
    #[tokio::test]
    async fn a_failed_replacement_keeps_the_old_set() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        r.mgr.begin_trickplay_pass().await.expect("record");
        let tile = set_dir(&r, item, &media, 320).join("0.jpg");
        let old = std::fs::read(&tile).expect("old tile");

        r.set_options(full_chroma_2x2(&[320]));
        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("check");
        // An offline file cannot be replaced either.
        let away = format!("{media}.away");
        std::fs::rename(&media, &away).expect("take the file away");
        assert!(
            !r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check"),
            "a set at the old chroma is left"
        );
        std::fs::rename(&away, &media).expect("bring it back");
        r.extractor.fail.store(true, Ordering::SeqCst);
        assert!(
            !r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check"),
            "the item is unfinished"
        );
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(r.extractor.calls(), 3);
        assert_eq!(std::fs::read(&tile).expect("tile"), old, "old tiles stay");
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .expect("rows")
                .contains_key(&320),
            "and their row"
        );

        r.extractor.fail.store(false, Ordering::SeqCst);
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("retry")
        );
        assert_eq!(super::jpeg_full_chroma(&tile), Some(true));
    }

    /// B9: a width no longer selected goes only once every selected width has
    /// its set, folder included; a user-placed grid stays through a checking
    /// pass, and a Replace leaves only the selected widths, as upstream's.
    #[tokio::test]
    async fn a_dropped_width_goes_only_once_the_selected_widths_exist() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        r.mgr.begin_trickplay_pass().await.expect("record");
        let user = set_dir(&r, item, &media, 480);
        std::fs::create_dir_all(&user).expect("user grid");
        image::RgbImage::new(960, 540)
            .save(user.join("0.jpg"))
            .expect("user tile");
        let widths = |rows: std::collections::HashMap<i32, TrickplayInfoEntity>| {
            let mut widths: Vec<i32> = rows.into_keys().collect();
            widths.sort_unstable();
            widths
        };

        r.set_options(options_2x2(&[640]));
        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("check");
        r.extractor.fail.store(true, Ordering::SeqCst);
        assert!(
            !r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("failing check")
        );
        assert!(set_dir(&r, item, &media, 320).join("0.jpg").is_file());
        assert_eq!(
            widths(r.mgr.get_trickplay_resolutions(item).await.expect("rows")),
            vec![320, 480]
        );

        r.extractor.fail.store(false, Ordering::SeqCst);
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check")
        );
        assert_eq!(
            widths(r.mgr.get_trickplay_resolutions(item).await.expect("rows")),
            vec![480, 640]
        );
        assert!(!set_dir(&r, item, &media, 320).exists(), "disk freed");
        assert!(user.join("0.jpg").is_file(), "a user-placed grid stays");

        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(
            widths(r.mgr.get_trickplay_resolutions(item).await.expect("rows")),
            vec![640]
        );
        assert!(!user.exists());
    }

    /// A video narrower than both widths gives them one set: it is made once,
    /// checked once and never removed as a dropped width.
    #[tokio::test]
    async fn a_video_narrower_than_both_widths_makes_one_set_and_keeps_it() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), Some(300)).await;
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        r.mgr.begin_trickplay_pass().await.expect("record");

        r.set_options(full_chroma_2x2(&[320, 640]));
        let check = r
            .mgr
            .begin_trickplay_pass()
            .await
            .expect("begin")
            .expect("check");
        assert!(
            r.mgr
                .check_trickplay_data(item, &check, &extraction_on())
                .await
                .expect("check")
        );
        assert_eq!(r.extractor.calls(), 2, "one regeneration for both widths");
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(r.extractor.calls(), 3, "one Replace for both widths");
        assert_eq!(
            r.extractor.sets(),
            vec![vec![300]; 3],
            "and one width a run"
        );
        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), vec![300]);
        assert_eq!(
            super::jpeg_full_chroma(&set_dir(&r, item, &media, 300).join("0.jpg")),
            Some(true)
        );
    }

    /// The widths a refresh generates go to the extractor in one call, so
    /// the ffmpeg extractor decodes the video once for all of them (gl-1228):
    /// a width with its set and row is skipped and one with tiles but no row
    /// is imported, as before, and neither is in the call.
    #[tokio::test]
    async fn one_run_makes_only_the_widths_that_need_generating() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("first");
        let imported = set_dir(&r, item, &media, 480);
        std::fs::create_dir_all(&imported).expect("tiles without a row");
        image::RgbImage::new(960, 540)
            .save(imported.join("0.jpg"))
            .expect("tile");
        let placed = std::fs::read(imported.join("0.jpg")).expect("placed tile");

        r.set_options(options_2x2(&[320, 480, 640, 1280]));
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(
            r.extractor.sets(),
            vec![vec![320], vec![640, 1280]],
            "the missing widths, together"
        );
        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        let mut widths: Vec<i32> = rows.keys().copied().collect();
        widths.sort_unstable();
        assert_eq!(widths, vec![320, 480, 640, 1280]);
        assert_eq!(
            std::fs::read(imported.join("0.jpg")).expect("tile"),
            placed,
            "480 was taken as it is"
        );
        assert_eq!(rows[&1280].thumbnail_count, 4, "1280 was generated");
        for width in [640, 1280] {
            assert!(set_dir(&r, item, &media, width).join("0.jpg").is_file());
        }

        // A Replace makes every selected width again, in one run.
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(r.extractor.sets().last(), Some(&vec![320, 480, 640, 1280]));
    }

    /// One width of the run failing (the extractor's per-width fallback after
    /// a failed shared run) costs only that width: the others are tiled and
    /// saved, the failed width's folder probed beside the media goes, and the
    /// next refresh asks again for the failed width alone.
    #[rstest::rstest]
    #[case::internal(false)]
    #[case::beside_media(true)]
    #[tokio::test]
    async fn a_width_that_fails_in_the_run_keeps_the_others(#[case] save_with_media: bool) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320, 640]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let library = LibraryOptions {
            save_trickplay_with_media: save_with_media,
            ..extraction_on()
        };
        let dir = |width: i32| {
            PathBuf::from(r.pm.trickplay_directory(
                item,
                MediaLocation::of_file(&media),
                save_with_media,
            ))
            .join(format!("{width} - 2x2"))
        };
        r.extractor.fail_widths.lock().expect("lock").push(640);

        r.mgr
            .refresh_trickplay_data(item, false, &library)
            .await
            .expect("a failed width is logged, not returned");

        assert_eq!(r.extractor.sets(), vec![vec![320, 640]]);
        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), vec![320]);
        assert!(dir(320).join("0.jpg").is_file());
        // The failed width's probed folder goes with it, or with the prune
        // that follows every refresh.
        assert!(!dir(640).exists(), "no folder is left for the failed width");
        let temp = r.mgr.temp_directory();
        assert!(
            !temp.exists() || std::fs::read_dir(&temp).expect("temp").next().is_none(),
            "every width's frames are cleaned up"
        );

        r.extractor.fail_widths.lock().expect("lock").clear();
        r.mgr
            .refresh_trickplay_data(item, false, &library)
            .await
            .expect("retry");
        assert_eq!(r.extractor.sets(), vec![vec![320, 640], vec![640]]);
        assert_eq!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .expect("rows")
                .len(),
            2
        );
    }

    /// A width whose media folder cannot be written fails before the run and
    /// is left out of it; the other widths are still made together.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_width_folder_is_left_out_of_the_run() {
        use std::os::unix::fs::PermissionsExt as _;

        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(
            &db,
            options_2x2(&[320, 640, 1280]),
            FakeExtractor::new(4, 180),
        );
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let library = LibraryOptions {
            save_trickplay_with_media: true,
            ..extraction_on()
        };
        let beside =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        // Wider sets get a smaller grid (gl-1400): 2x2 at 320 is 1x1 at 640.
        let locked = beside.join("640 - 1x1");
        std::fs::create_dir_all(&locked).expect("width folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))
            .expect("read-only");
        if std::fs::File::create(locked.join(".probe")).is_ok() {
            // Running as root: nothing is unwritable, so nothing to check.
            return;
        }

        let refreshed = r.mgr.refresh_trickplay_data(item, false, &library).await;
        // The prune may have removed the empty folder already.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        refreshed.expect("refresh");
        assert_eq!(r.extractor.sets(), vec![vec![320, 1280]]);
        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        let mut widths: Vec<i32> = rows.keys().copied().collect();
        widths.sort_unstable();
        assert_eq!(widths, vec![320, 1280], "the others are made, 640 is not");
        for grid in ["320 - 2x2", "1280 - 1x1"] {
            assert!(beside.join(grid).join("0.jpg").is_file());
        }
    }

    /// An extractor that answers fewer widths than it was asked for fails the
    /// widths it left out, and only those.
    #[tokio::test]
    async fn a_width_the_extractor_did_not_answer_fails_alone() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320, 640]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        r.extractor.short_sets.store(true, Ordering::SeqCst);

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), vec![320]);
        assert!(!set_dir(&r, item, &media, 640).exists());
        let temp = r.mgr.temp_directory();
        assert!(
            !temp.exists() || std::fs::read_dir(&temp).expect("temp").next().is_none(),
            "the unanswered width's frames are cleaned up too"
        );
    }

    /// A video narrower than the selected width has its tiles at its own
    /// width, so importing them saves the row at that width, the folder's:
    /// stored at the selected 640, the row would name a `640 - 1x1` folder
    /// that does not exist and give 480-wide tiles a 640 × 270 aspect
    /// (gl-1246). Discovery catalogues such a folder before this check runs,
    /// so the check is called directly, as when discovery skips the folder.
    #[tokio::test]
    async fn importing_a_narrow_videos_tiles_saves_the_folders_width() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let options = options_2x2(&[640]);
        let r = rig(&db, options.clone(), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), Some(480)).await;
        // 480's grid under options_2x2 is 1x1 (gl-1400): one 480x270 preview.
        let tiles = set_dir(&r, item, &media, 480);
        std::fs::create_dir_all(&tiles).expect("tiles without a row");
        image::RgbImage::new(480, 270)
            .save(tiles.join("0.jpg"))
            .expect("tile");
        let trickplay_dir =
            r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false);
        let ctx = super::WidthRefreshContext {
            item_id: item,
            media_path: &media,
            video_stream: Some(MediaStream {
                width: Some(480),
                ..MediaStream::default()
            }),
            media_streams: Vec::new(),
            trickplay_dir: &trickplay_dir,
            save_with_media: false,
        };
        let mut rows = std::collections::HashMap::new();

        let pending = r
            .mgr
            .prepare_width(&ctx, false, 640, &options, &mut rows)
            .await
            .expect("import");

        assert!(pending.is_none(), "imported, not generated");
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), vec![480]);
        let stored = r.mgr.get_trickplay_resolutions(item).await.expect("rows");
        assert_eq!(stored.keys().copied().collect::<Vec<_>>(), vec![480]);
        assert_eq!(
            (stored[&480].width, stored[&480].height),
            (480, 270),
            "the folder's width and the tiles' height"
        );
        assert_eq!(
            (stored[&480].tile_width, stored[&480].tile_height),
            (1, 1),
            "the grid at the folder's width"
        );
        assert_eq!(r.extractor.calls(), 0, "import path never extracts");
    }

    #[tokio::test]
    async fn refresh_imports_on_disk_tiles_without_a_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        // A user-placed 640×360 tile grid (2×2 of 320×180) with no DB row.
        let tile_dir = Path::new(&r.pm.trickplay_directory(
            item,
            MediaLocation::of_file(&media),
            false,
        ))
        .join("320 - 2x2");
        std::fs::create_dir_all(&tile_dir).expect("tile dir");
        image::RgbImage::new(640, 360)
            .save(tile_dir.join("0.jpg"))
            .expect("tile jpg");

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(r.extractor.calls(), 0, "import path never extracts");
        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        let row = &res[&320];
        assert_eq!(
            row.thumbnail_count, 2,
            "padded thumbnail count is inferred from runtime"
        );
        assert_eq!(
            row.interval, 2_250_000,
            "incompatible configured interval is inferred from the grid bounds"
        );
        assert_eq!(row.height, 180, "tile height / grid rows");
        assert!(row.bandwidth > 0);
    }

    /// gl-1400: a set wider than 320 gets the grid that keeps its tile no
    /// larger than a 320 tile at the configured grid, never a larger one.
    #[rstest::rstest]
    #[case::reference(320, (10, 10), (10, 10))]
    #[case::narrower_keeps_configured(160, (10, 10), (10, 10))]
    #[case::just_over(321, (10, 10), (9, 9))]
    #[case::w480(480, (10, 10), (6, 6))]
    #[case::w640(640, (10, 10), (5, 5))]
    #[case::w1280(1280, (10, 10), (2, 2))]
    #[case::non_square(640, (8, 3), (4, 1))]
    #[case::never_below_one(4000, (10, 10), (1, 1))]
    #[case::one_by_one(640, (1, 1), (1, 1))]
    fn grid_scales_down_with_width(
        #[case] width: i32,
        #[case] configured: (i32, i32),
        #[case] expected: (i32, i32),
    ) {
        let options = TrickplayOptions {
            tile_width: configured.0,
            tile_height: configured.1,
            ..TrickplayOptions::default()
        };
        let scaled = super::options_at_width(&options, width);
        assert_eq!((scaled.tile_width, scaled.tile_height), expected);
    }

    /// gl-1400: a 640 set is made at half the configured grid, and a refresh
    /// without Replace swaps a set made at the full grid for it and removes
    /// the old folder.
    #[tokio::test]
    async fn a_wider_set_is_made_at_a_smaller_grid_and_replaces_one_at_the_full_grid() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let options = TrickplayOptions {
            width_resolutions: vec![640],
            tile_width: 4,
            tile_height: 4,
            interval: 10_000,
            ..TrickplayOptions::default()
        };
        let r = rig(&db, options, FakeExtractor::new(4, 360));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        let old = root.join("640 - 4x4");
        std::fs::create_dir_all(&old).expect("old grid");
        image::RgbImage::new(2560, 1440)
            .save(old.join("0.jpg"))
            .expect("old tile");
        r.mgr
            .save_trickplay_info(&TrickplayInfoEntity {
                tile_width: 4,
                tile_height: 4,
                height: 360,
                ..info(item, 640)
            })
            .await
            .expect("old row");

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let row = &r.mgr.get_trickplay_resolutions(item).await.expect("res")[&640];
        assert_eq!((row.tile_width, row.tile_height), (2, 2));
        assert!(root.join("640 - 2x2/0.jpg").is_file(), "new grid's tiles");
        assert!(!old.exists(), "the full-grid set is removed");
    }

    /// gl-1400: a video narrower than the selected width gets the grid of the
    /// width its set is made at, not the selected one: 1280 selected on a
    /// 640-wide video is a 640 set at 640's grid.
    #[tokio::test]
    async fn a_narrow_video_gets_the_grid_of_its_own_width() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let options = TrickplayOptions {
            width_resolutions: vec![1280],
            tile_width: 4,
            tile_height: 4,
            interval: 10_000,
            ..TrickplayOptions::default()
        };
        let r = rig(&db, options, FakeExtractor::new(4, 360));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), Some(640)).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        let rows = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        let row = &rows[&640];
        assert_eq!(
            (row.tile_width, row.tile_height),
            (2, 2),
            "640's grid, not 1280's 1x1"
        );
        let root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        assert!(root.join("640 - 2x2/0.jpg").is_file());
    }

    #[tokio::test]
    async fn refresh_prunes_when_library_extraction_is_disabled() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(6, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        // Saved defaults keep extraction off; a normal refresh does not start
        // ffmpeg or insert a manifest row before the library opts in.
        r.mgr
            .refresh_trickplay_data(item, false, &LibraryOptions::default())
            .await
            .expect("initial disabled refresh");
        assert_eq!(r.extractor.calls(), 0);
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );
        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("generate");
        let tile_dir = Path::new(&r.pm.trickplay_directory(
            item,
            MediaLocation::of_file(&media),
            false,
        ))
        .join("320 - 2x2");
        assert!(tile_dir.join("0.jpg").is_file());

        // The library turns extraction off: tiles and rows go (C# prune branch).
        r.mgr
            .refresh_trickplay_data(item, false, &LibraryOptions::default())
            .await
            .expect("prune");
        assert_eq!(r.extractor.calls(), 1, "no regeneration while disabled");
        assert!(!tile_dir.exists(), "tile directory pruned");
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .expect("res")
                .is_empty()
        );

        // RegenerateTrickplay in a full refresh sets replace even when the
        // enable flag is off: the pinned manager deliberately regenerates.
        r.mgr
            .refresh_trickplay_data(item, true, &LibraryOptions::default())
            .await
            .expect("forced generation despite extraction being disabled");
        assert_eq!(r.extractor.calls(), 2);
        assert!(tile_dir.join("0.jpg").is_file());
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .contains_key(&320)
        );
        r.mgr
            .refresh_trickplay_data(item, false, &LibraryOptions::default())
            .await
            .expect("normal refresh resumes disabled pruning");
        assert_eq!(r.extractor.calls(), 2);
        assert!(!tile_dir.exists());

        // Save-with-media + disabled: user-managed, catalogued and left alone.
        let tile_dir = Path::new(&r.pm.trickplay_directory(
            item,
            MediaLocation::of_file(&media),
            true,
        ))
        .join("320 - 2x2");
        std::fs::create_dir_all(&tile_dir).expect("tile dir");
        image::RgbImage::new(640, 360)
            .save(tile_dir.join("0.jpg"))
            .expect("valid tile");
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..LibraryOptions::default()
                },
            )
            .await
            .expect("leave alone");
        assert!(tile_dir.join("0.jpg").is_file(), "user-managed tiles kept");
    }

    #[tokio::test]
    async fn missing_and_backdrop_files_do_not_bypass_disabled_outer_cleanup() {
        for backdrop in [false, true] {
            let db = test_db().await;
            let item = Uuid::new_v4();
            let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
            let mut media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
            if backdrop {
                let directory = r.media_dir.join("backdrops");
                std::fs::create_dir_all(&directory).unwrap();
                let path = directory.join("background.mkv");
                std::fs::rename(&media, &path).unwrap();
                media = path.to_string_lossy().into_owned();
                let mut entity = crate::test_support::fetch_item(&db, item).await;
                entity.path = Some(media.clone());
                crate::test_support::save_item(&db, &entity).await;
            } else {
                std::fs::remove_file(&media).unwrap();
            }
            let root = PathBuf::from(r.pm.trickplay_directory(
                item,
                MediaLocation::of_file(&media),
                false,
            ));
            let grid = root.join("320 - 2x2");
            std::fs::create_dir_all(&grid).unwrap();
            image::RgbImage::new(640, 360)
                .save(grid.join("0.jpg"))
                .unwrap();
            r.mgr.save_trickplay_info(&info(item, 320)).await.unwrap();
            r.mgr
                .refresh_trickplay_data(item, false, &LibraryOptions::default())
                .await
                .unwrap();
            assert!(
                !root.exists(),
                "source disabled cleanup precedes missing-file/backdrop checks"
            );
            assert!(
                r.mgr
                    .get_trickplay_resolutions(item)
                    .await
                    .unwrap()
                    .is_empty()
            );
            r.mgr
                .refresh_trickplay_data(item, true, &extraction_on())
                .await
                .unwrap();
            assert_eq!(
                r.extractor.calls(),
                0,
                "missing/backdrop media still skips actual encoding"
            );
        }
    }

    #[tokio::test]
    async fn refresh_is_a_noop_for_ineligible_items() {
        let db = test_db().await;
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));

        // Unknown item.
        let missing = Uuid::new_v4();
        r.mgr
            .refresh_trickplay_data(missing, true, &extraction_on())
            .await
            .expect("missing ok");

        // No media path.
        let no_path = Uuid::new_v4();
        seed_item(&db, no_path, BaseItemKind::Movie).await;
        r.mgr
            .refresh_trickplay_data(no_path, true, &extraction_on())
            .await
            .expect("no path ok");

        // Runtime shorter than one interval.
        let short = Uuid::new_v4();
        seed_video(&db, &r, short, Some(5_000 * 10_000), None).await;
        r.mgr
            .refresh_trickplay_data(short, true, &extraction_on())
            .await
            .expect("short ok");

        // No runtime at all.
        let no_runtime = Uuid::new_v4();
        seed_video(&db, &r, no_runtime, None, None).await;
        r.mgr
            .refresh_trickplay_data(no_runtime, true, &extraction_on())
            .await
            .expect("no runtime ok");

        assert_eq!(r.extractor.calls(), 0);
        let trickplay_items = r.mgr.get_trickplay_items(10, 0).await.expect("items");
        assert!(trickplay_items.is_empty(), "{trickplay_items:?}");
    }

    #[tokio::test]
    async fn refresh_clamps_interval_and_caps_width_at_video_width() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        // Interval below the 1000 ms floor; video narrower than the setting.
        let mut options = options_2x2(&[320]);
        options.interval = 100;
        let r = rig(&db, options, FakeExtractor::new(2, 112));
        // The cap is the video STREAM's width, not the item row's.
        seed_video(&db, &r, item, Some(HOUR_TICKS), Some(1920)).await;
        seed_sized_video_stream(&db, item, "h264", None, Some(201)).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        // Clamped interval, and 201 px video → 200 px (even) thumbnails.
        assert_eq!(r.extractor.last_request(), Some((1000, 200)));
        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        let row = &res[&200];
        assert_eq!(row.interval, 1000);
        assert_eq!(row.width, 200);
    }

    /// `if (mediaSource.VideoStream.Width is not null && … < width)`
    /// (`TrickplayManager.cs:427-431`): a video stream with no width caps
    /// nothing, even when the item row has the zero width from a dimensionless
    /// probe. Missing video streams are covered by the audio-only regression.
    #[tokio::test]
    async fn an_unknown_video_width_caps_nothing() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(2, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), Some(0)).await;
        seed_sized_video_stream(&db, item, "h264", None, None).await;

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert_eq!(r.extractor.calls(), 1);
        assert_eq!(r.extractor.last_request(), Some((10_000, 320)));
    }

    #[tokio::test]
    async fn refresh_prunes_orphaned_rows_and_unexpected_folders() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        // No configured widths: refresh only reconciles rows and folders.
        let r = rig(&db, options_2x2(&[]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        // A row whose tile folder exists nowhere on disk → orphaned.
        r.mgr
            .save_trickplay_info(&info(item, 320))
            .await
            .expect("save");
        // A folder no row accounts for → unexpected.
        let root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        let stray = root.join("unexpected-folder");
        std::fs::create_dir_all(&stray).expect("stray dir");
        std::fs::write(stray.join("0.jpg"), b"x").expect("stray tile");

        r.mgr
            .refresh_trickplay_data(item, false, &extraction_on())
            .await
            .expect("refresh");

        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .expect("res")
                .is_empty(),
            "orphaned row pruned"
        );
        assert!(!stray.exists(), "unexpected folder pruned");
        assert_eq!(r.extractor.calls(), 0);
    }

    #[tokio::test]
    async fn move_generated_library_trickplay_data_consolidates_media_adjacent_tiles() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, _) = configured_library(&r, false).await;
        r.mgr.attach_virtual_folders(folders);

        // A stored row plus tiles living media-adjacent (`{stem}.trickplay`).
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.expect("save");
        let media_root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        let media_tiles = media_root.join("320 - 2x2");
        std::fs::create_dir_all(&media_tiles).expect("media tiles dir");
        std::fs::write(media_tiles.join("0.jpg"), b"tile").expect("tile");

        r.mgr
            .move_generated_library_trickplay_data(item)
            .await
            .expect("move");

        let local_tiles =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false))
                .join("320 - 2x2");
        assert!(local_tiles.join("0.jpg").is_file(), "tiles moved inward");
        assert!(!media_tiles.exists(), "media-adjacent tiles gone");
        assert!(!media_root.exists(), "emptied .trickplay folder removed");
    }

    #[tokio::test]
    async fn move_generated_library_trickplay_data_is_a_noop_without_media_tiles() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, _) = configured_library(&r, false).await;
        r.mgr.attach_virtual_folders(folders);

        // Row + internal tiles only: nothing to move, nothing destroyed.
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.expect("save");
        let local_tiles =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false))
                .join("320 - 2x2");
        std::fs::create_dir_all(&local_tiles).expect("local tiles dir");
        std::fs::write(local_tiles.join("0.jpg"), b"tile").expect("tile");

        r.mgr
            .move_generated_library_trickplay_data(item)
            .await
            .expect("move");
        assert!(local_tiles.join("0.jpg").is_file(), "internal tiles kept");

        // An unknown item is also fine.
        r.mgr
            .move_generated_library_trickplay_data(Uuid::new_v4())
            .await
            .expect("missing item ok");
    }

    #[tokio::test]
    async fn hls_playlist_and_tile_path_from_stored_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let r = rig(&db, TrickplayOptions::default(), FakeExtractor::new(0, 180));

        // A 2x2 tile grid with 6 thumbnails → 2 tiles (4 + 2).
        let stored = TrickplayInfoEntity {
            item_id: guid_to_db(item),
            width: 320,
            bandwidth: 500_000,
            height: 180,
            interval: 10_000,
            thumbnail_count: 6,
            tile_height: 2,
            tile_width: 2,
        };
        r.mgr.save_trickplay_info(&stored).await.expect("save");

        let playlist = r
            .mgr
            .get_hls_playlist(item, 320, Some("KEY"))
            .await
            .expect("hls")
            .expect("some playlist");
        assert!(playlist.starts_with("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-IMAGES-ONLY"));
        assert!(playlist.contains("RESOLUTION=320x180"));
        assert!(playlist.contains("LAYOUT=2x2"));
        // Two tiles, addressed 0.jpg / 1.jpg with the dashless id + api key.
        assert!(playlist.contains(&format!("0.jpg?MediaSourceId={}&ApiKey=KEY", item.simple())));
        assert!(playlist.contains("1.jpg?MediaSourceId="));
        assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));

        // Tile path is `{trickplay}/{id}/{width} - {tw}x{th}/{index}.jpg`.
        let tile = r
            .mgr
            .get_trickplay_tile_path(item, 320, 1)
            .await
            .expect("tile")
            .expect("some path");
        assert!(
            tile.ends_with("320 - 2x2/1.jpg"),
            "unexpected tile path {tile}"
        );

        // Unknown resolution → no playlist / no tile path.
        assert!(
            r.mgr
                .get_hls_playlist(item, 999, None)
                .await
                .expect("hls")
                .is_none()
        );
        assert!(
            r.mgr
                .get_trickplay_tile_path(item, 999, 0)
                .await
                .expect("tile")
                .is_none()
        );
    }
    #[test]
    fn user_grid_names_and_padded_thumbnail_timing_are_source_compatible() {
        assert_eq!(super::parse_grid_name("640 - 3x2"), Some((640, 3, 2)));
        for name in [
            "640-3x2",
            "640 - 3X2",
            "640 - 3x2x1",
            "0 - 3x2",
            "640 - 0x2",
            "640 - 3x0",
            "999999999999999999 - 3x2",
            " 640 - 3x2",
        ] {
            assert!(super::parse_grid_name(name).is_none(), "{name}");
        }
        assert_eq!(
            super::discovered_timing(Some(60_000 * 10_000), 2, 3, 2, 10_000),
            Some((7_000, 9)),
            "configured 10s cannot fit two partially padded grids"
        );
        assert_eq!(
            super::discovered_timing(Some(60_000 * 10_000), 2, 3, 2, 6_000),
            Some((6_000, 10)),
            "compatible configured interval is retained"
        );
        assert_eq!(
            super::discovered_timing(Some(5_000 * 10_000), 1, 5, 1, 2_000),
            Some((2_000, 2)),
            ".NET rounds 2.5 to even"
        );
        assert_eq!(
            super::discovered_timing(None, 2, 3, 2, 100),
            Some((1000, 12))
        );
        assert!(super::discovered_timing(Some(HOUR_TICKS), 2, i32::MAX, 2, 10_000).is_none());
        assert!(super::discovered_timing(Some(HOUR_TICKS), usize::MAX, 2, 2, 10_000).is_none());
    }

    #[tokio::test]
    async fn disabled_sidecars_are_catalogued_at_their_own_grid_without_extracting_or_deleting() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(60_000 * 10_000), None).await;
        let root =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        let tiles = root.join("640 - 3x2");
        std::fs::create_dir_all(&tiles).unwrap();
        for index in 0..2 {
            image::RgbImage::new(1920, 360)
                .save(tiles.join(format!("{index}.jpg")))
                .unwrap();
        }
        // Malformed/empty grids do not produce database entries. A zero grid
        // is rejected before dividing, and overflow never reaches row storage.
        for name in [
            "800 - 0x2",
            "900 - 2x0",
            "0 - 2x2",
            "800 - 9999999999x2",
            "unrelated",
            "800 - 2x2",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let bytes = std::fs::read(tiles.join("0.jpg")).unwrap();
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(r.extractor.calls(), 0);
        assert_eq!(std::fs::read(tiles.join("0.jpg")).unwrap(), bytes);
        let resolutions = r.mgr.get_trickplay_resolutions(item).await.unwrap();
        assert_eq!(resolutions.len(), 1);
        let info = &resolutions[&640];
        assert_eq!(
            (info.tile_width, info.tile_height, info.height),
            (3, 2, 180)
        );
        assert_eq!((info.interval, info.thumbnail_count), (7_000, 9));
        assert!(info.bandwidth > 0);
        assert!(
            r.mgr
                .get_trickplay_resolutions_batch(&[item])
                .await
                .unwrap()[&item]
                .contains_key(&640)
        );
        assert!(
            r.mgr
                .get_hls_playlist(item, 640, None)
                .await
                .unwrap()
                .unwrap()
                .contains("LAYOUT=3x2")
        );
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            0,
            "a second disabled refresh reuses catalogued data"
        );
        // Pruning checks both locations before dropping rows; deleting the
        // selected files makes the next discovery remove the orphaned row.
        std::fs::remove_dir_all(&tiles).unwrap();
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn scan_without_a_new_probe_uses_stored_streams_and_adopts_disabled_user_grid() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(2, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        seed_sized_video_stream(&db, item, "h264", None, Some(200)).await;
        let mut entity = r.mgr.items.retrieve_item(item).await.unwrap().unwrap();
        entity.data = Some(r#"{"DefaultVideoStreamIndex":0}"#.to_owned());
        let grid =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("640 - 3x2");
        std::fs::create_dir_all(&grid).unwrap();
        image::RgbImage::new(1920, 360)
            .save(grid.join("0.jpg"))
            .unwrap();
        let before = std::fs::read(grid.join("0.jpg")).unwrap();
        r.mgr
            .refresh_trickplay_for_media(
                item,
                &entity,
                None,
                false,
                &LibraryOptions {
                    extract_trickplay_images_during_library_scan: true,
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .contains_key(&640)
        );
        assert_eq!(std::fs::read(grid.join("0.jpg")).unwrap(), before);
        assert_eq!(
            r.extractor.calls(),
            0,
            "disabled extraction adopts cached sidecar without encoding"
        );
        r.mgr
            .refresh_trickplay_for_media(item, &entity, None, true, &extraction_on())
            .await
            .unwrap();
        assert_eq!(
            r.extractor.last_request(),
            Some((10_000, 200)),
            "no fresh probe still carries persisted stream dimensions"
        );
    }

    #[tokio::test]
    async fn generation_concurrency_holds_a_second_item_until_a_slot_frees() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release.clone()));
        let db = test_db().await;
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let r = rig(&db, options_2x2(&[320]), extractor);
        seed_video(&db, &r, first, Some(HOUR_TICKS), None).await;
        seed_video(&db, &r, second, Some(HOUR_TICKS), None).await;
        let manager = r.mgr.clone().with_generation_concurrency(1);
        let refresh = |item| {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .refresh_trickplay_data(item, false, &extraction_on())
                    .await
            })
        };
        let (a, b) = (refresh(first), refresh(second));

        entered.notified().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), entered.notified())
                .await
                .is_err(),
            "the second extraction waits for the single slot"
        );
        assert_eq!(r.extractor.calls(), 1);

        release.notify_one();
        entered.notified().await;
        release.notify_one();
        a.await.unwrap().expect("first refresh");
        b.await.unwrap().expect("second refresh");
        assert_eq!(r.extractor.calls(), 2);
    }

    /// With several widths selected, a video makes all of its sets in one
    /// generation slot before the next queued video starts (gl-1227): a slot
    /// per width let every queued video's first width go before any second
    /// width, so a scan backlog made every 320 set before any 640 set.
    #[tokio::test]
    async fn a_video_makes_every_width_before_the_next_video_takes_the_slot() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release.clone()));
        let db = test_db().await;
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        let r = rig(&db, options_2x2(&[320, 640]), extractor);
        let one_path = seed_video(&db, &r, one, Some(HOUR_TICKS), None).await;
        let two_path = seed_video(&db, &r, two, Some(HOUR_TICKS), None).await;
        let manager = r.mgr.clone().with_generation_concurrency(1);
        let refresh = |item| {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .refresh_trickplay_data(item, false, &extraction_on())
                    .await
            })
        };
        let first = refresh(one);
        entered.notified().await;
        let second = refresh(two);
        // Let the second video reach the slot and queue for it while the
        // first video's 320 set is being made.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), entered.notified())
                .await
                .is_err(),
            "the second video waits for the single slot"
        );
        for _ in 0..3 {
            release.notify_one();
            entered.notified().await;
        }
        release.notify_one();
        first.await.unwrap().expect("first video");
        second.await.unwrap().expect("second video");

        let order = r.extractor.order.lock().expect("lock").clone();
        assert_eq!(
            order,
            vec![
                (one_path.clone(), 320),
                (one_path, 640),
                (two_path.clone(), 320),
                (two_path, 640),
            ],
            "each video's widths are made together, in the slot it took"
        );
    }

    /// Two refreshes of one item at once (a scan's spawned trickplay job and
    /// the daily task, or a manual refresh) generate its tiles once: upstream
    /// takes `_resourcePool` before the existing-tiles check, so the second
    /// waits, then finds the first one's tiles and row and skips. Ferrofin's
    /// generation slots are not that lock (unbounded by default), so an item
    /// lock is.
    #[tokio::test]
    async fn concurrent_refreshes_of_one_item_generate_its_tiles_once() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release.clone()));
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), extractor);
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let refresh = || {
            let manager = r.mgr.clone();
            tokio::spawn(async move {
                manager
                    .refresh_trickplay_data(item, false, &extraction_on())
                    .await
            })
        };
        let first = refresh();
        entered.notified().await;
        let second = refresh();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), entered.notified())
                .await
                .is_err(),
            "the second refresh of the item waits for the first"
        );

        release.notify_one();
        first.await.unwrap().expect("first refresh");
        second.await.unwrap().expect("second refresh");
        assert_eq!(r.extractor.calls(), 1, "the second refresh found the tiles");
        assert_eq!(
            r.mgr.get_trickplay_resolutions(item).await.unwrap().len(),
            1
        );
    }

    /// The item lock serialises one item only: two items still generate side
    /// by side when the generation slots allow it.
    #[tokio::test]
    async fn refreshes_of_two_items_still_run_side_by_side() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release.clone()));
        let db = test_db().await;
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        let r = rig(&db, options_2x2(&[320]), extractor);
        seed_video(&db, &r, one, Some(HOUR_TICKS), None).await;
        seed_video(&db, &r, two, Some(HOUR_TICKS), None).await;
        let refresh = |item| {
            let manager = r.mgr.clone();
            tokio::spawn(async move {
                manager
                    .refresh_trickplay_data(item, false, &extraction_on())
                    .await
            })
        };
        let (a, b) = (refresh(one), refresh(two));
        // Neither extraction returns until released, so two calls means both
        // are extracting at once.
        let both_started = async {
            while r.extractor.calls() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), both_started)
            .await
            .expect("both items are extracting");

        let mut both = Box::pin(async { (a.await, b.await) });
        let (a, b) = loop {
            release.notify_waiters();
            if let Ok(done) =
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut both).await
            {
                break done;
            }
        };
        a.unwrap().expect("first item");
        b.unwrap().expect("second item");
    }

    /// With two widths, the run is held after the first width's frames, while
    /// the second width's directory already holds frames of its own: both
    /// go.
    #[rstest::rstest]
    #[case::one_width(&[320])]
    #[case::two_widths(&[320, 640])]
    #[tokio::test]
    async fn cancelling_a_running_scan_removes_its_partially_written_frame_directory(
        #[case] widths: &[i32],
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release));
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(widths), extractor);
        seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let manager = r.mgr.clone();
        let work = tokio::spawn(async move {
            manager
                .refresh_trickplay_data(item, false, &extraction_on())
                .await
        });
        entered.notified().await;
        let directory = PathBuf::from(r.extractor.last_output.lock().unwrap().clone().unwrap());
        assert!(
            directory.join("00000001.jpg").exists(),
            "encoder wrote a partial frame before suspension"
        );
        work.abort();
        assert!(work.await.unwrap_err().is_cancelled());
        assert!(
            !directory.exists(),
            "source finally cleanup runs when the future is dropped"
        );
        let temp = r.mgr.temp_directory();
        assert!(
            !temp.exists() || std::fs::read_dir(&temp).expect("temp").next().is_none(),
            "no width's frames are left in the temp directory"
        );
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );
    }
    async fn configured_library(
        rig: &Rig,
        save_with_media: bool,
    ) -> (Arc<crate::FerrofinVirtualFolderManager>, LibraryOptions) {
        use ferrofin_traits::library::VirtualFolderManager as _;
        let folders = Arc::new(crate::FerrofinVirtualFolderManager::new(
            rig.temp_dir.path().join("views"),
        ));
        let options = LibraryOptions {
            path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
                path: rig.media_dir.to_string_lossy().into_owned(),
            }],
            enable_trickplay_image_extraction: true,
            save_trickplay_with_media: save_with_media,
            ..LibraryOptions::default()
        };
        folders
            .add_virtual_folder(
                "Movies",
                Some(ferrofin_model::entities::CollectionTypeOptions::movies),
                &options,
            )
            .await
            .unwrap();
        (folders, options)
    }

    #[tokio::test]
    async fn sidecar_generation_and_http_lookup_honor_the_library_destination_without_fallback() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(6, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, mut options) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        let internal =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        r.mgr
            .refresh_trickplay_data(item, false, &options)
            .await
            .unwrap();
        assert!(sidecar.join("320 - 2x2/0.jpg").is_file());
        assert!(!internal.exists());
        assert_eq!(
            r.mgr.get_trickplay_tile_path(item, 320, 0).await.unwrap(),
            Some(
                sidecar
                    .join("320 - 2x2/0.jpg")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        let original = std::fs::read(sidecar.join("320 - 2x2/0.jpg")).unwrap();
        options.enable_trickplay_image_extraction = false;
        r.mgr
            .refresh_trickplay_data(item, false, &options)
            .await
            .unwrap();
        assert_eq!(r.extractor.calls(), 1, "disabled sidecars are user-managed");
        assert_eq!(
            std::fs::read(sidecar.join("320 - 2x2/0.jpg")).unwrap(),
            original
        );
        r.mgr
            .refresh_trickplay_data(item, true, &options)
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            2,
            "forced replacement still uses the selected sidecar"
        );
        std::fs::remove_dir_all(&sidecar).unwrap();
        std::fs::write(&sidecar, b"blocked output destination").unwrap();
        r.mgr
            .refresh_trickplay_data(item, true, &options)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"blocked output destination"
        );
        assert!(
            !internal.exists(),
            "a failed sidecar write does not choose internal storage"
        );
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty(),
            "failed writing cannot leave a manifest pointing at absent tiles"
        );
    }

    /// A regular file where the `.trickplay` folder belongs blocks the
    /// destination the way a read-only media folder does, and still blocks it
    /// when the tests run as root.
    #[tokio::test]
    async fn unwritable_sidecar_destination_fails_before_any_frame_extraction() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320, 640]), FakeExtractor::new(6, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, options) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        std::fs::write(&sidecar, b"blocked output destination").unwrap();
        r.mgr
            .refresh_trickplay_data(item, false, &options)
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            0,
            "the write probe stops every width before ffmpeg runs"
        );
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );

        std::fs::remove_file(&sidecar).unwrap();
        r.mgr
            .refresh_trickplay_data(item, false, &options)
            .await
            .unwrap();
        assert_eq!(r.extractor.calls(), 2);
        let mut names: Vec<String> = std::fs::read_dir(sidecar.join("320 - 2x2"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["0.jpg", "1.jpg"],
            "the probe file is not left behind"
        );
    }

    #[tokio::test]
    async fn live_destination_changes_move_both_ways_and_disabled_extraction_prevents_migration() {
        use ferrofin_traits::library::VirtualFolderManager as _;
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(6, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, mut options) = configured_library(&r, false).await;
        r.mgr.attach_virtual_folders(folders.clone());
        r.mgr
            .refresh_trickplay_data(item, false, &options)
            .await
            .unwrap();
        let internal =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        let original = std::fs::read(internal.join("320 - 2x2/0.jpg")).unwrap();
        for save in [true, false] {
            options.save_trickplay_with_media = save;
            folders
                .update_library_options("Movies", &options)
                .await
                .unwrap();
            let selected = if save { &sidecar } else { &internal };
            let old = if save { &internal } else { &sidecar };
            let tile = selected.join("320 - 2x2/0.jpg");
            assert_eq!(
                r.mgr.get_trickplay_tile_path(item, 320, 0).await.unwrap(),
                Some(tile.to_string_lossy().into_owned()),
                "HTTP lookup reads the new option live"
            );
            assert!(
                !tile.exists(),
                "changing the checkbox alone does not relocate data"
            );
            r.mgr
                .move_generated_library_trickplay_data(item)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&tile).unwrap(), original);
            assert!(
                !old.exists(),
                "empty source container is cleaned after migration"
            );
            assert_eq!(
                r.extractor.calls(),
                1,
                "migration reuses tiles instead of re-encoding"
            );
        }
        options.enable_trickplay_image_extraction = false;
        options.save_trickplay_with_media = true;
        folders
            .update_library_options("Movies", &options)
            .await
            .unwrap();
        r.mgr
            .move_generated_library_trickplay_data(item)
            .await
            .unwrap();
        assert!(internal.join("320 - 2x2/0.jpg").is_file());
        assert!(
            !sidecar.exists(),
            "disabled extraction leaves generated data in place during migration"
        );
    }

    #[tokio::test]
    async fn migration_overwrites_matching_tiles_preserves_other_files_and_skips_empty_destination()
    {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, _) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.unwrap();
        let internal =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false))
                .join("320 - 2x2");
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("320 - 2x2");
        std::fs::create_dir_all(&internal).unwrap();
        std::fs::write(internal.join("0.jpg"), b"source").unwrap();
        std::fs::create_dir_all(&sidecar).unwrap();
        r.mgr
            .move_generated_library_trickplay_data(item)
            .await
            .unwrap();
        assert!(
            internal.join("0.jpg").exists(),
            "an existing empty destination is skipped by the pinned guard"
        );
        std::fs::write(sidecar.join("0.jpg"), b"old collision").unwrap();
        std::fs::write(sidecar.join("extra.jpg"), b"preserved destination file").unwrap();
        r.mgr
            .move_generated_library_trickplay_data(item)
            .await
            .unwrap();
        assert_eq!(std::fs::read(sidecar.join("0.jpg")).unwrap(), b"source");
        assert_eq!(
            std::fs::read(sidecar.join("extra.jpg")).unwrap(),
            b"preserved destination file"
        );
        assert!(!internal.exists());
        assert_eq!(r.extractor.calls(), 0);
    }

    #[tokio::test]
    async fn migration_reports_a_file_blocking_the_selected_destination_and_retains_the_source() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, _) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.unwrap();
        let internal =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false))
                .join("320 - 2x2");
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("320 - 2x2");
        std::fs::create_dir_all(&internal).unwrap();
        std::fs::write(internal.join("0.jpg"), b"source tiles").unwrap();
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"destination blocker").unwrap();
        assert!(
            r.mgr
                .move_generated_library_trickplay_data(item)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(internal.join("0.jpg")).unwrap(),
            b"source tiles"
        );
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"destination blocker");
        assert_eq!(r.extractor.calls(), 0);
        assert_eq!(
            r.mgr.get_trickplay_resolutions(item).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn migration_reports_source_container_cleanup_errors_after_moving_tiles() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let (folders, _) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.unwrap();
        let internal =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), false));
        let sidecar =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true));
        std::fs::create_dir_all(internal.join("320 - 2x2")).unwrap();
        std::fs::write(internal.join("320 - 2x2/0.jpg"), b"source tiles").unwrap();
        std::fs::write(internal.join("retained.txt"), b"ordinary source file").unwrap();
        assert!(
            r.mgr
                .move_generated_library_trickplay_data(item)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(sidecar.join("320 - 2x2/0.jpg")).unwrap(),
            b"source tiles"
        );
        assert!(!internal.join("320 - 2x2").exists());
        assert_eq!(
            std::fs::read(internal.join("retained.txt")).unwrap(),
            b"ordinary source file"
        );
        assert_eq!(r.extractor.calls(), 0);
    }

    #[tokio::test]
    async fn migration_task_selects_physical_videos_and_preserves_owned_extras_and_channel_vod() {
        use crate::scheduled_tasks::{ScheduledTask as _, TaskProgress};
        let db = test_db().await;
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let (folders, _) = configured_library(&r, true).await;
        r.mgr.attach_virtual_folders(folders);
        let owner = Uuid::new_v4();
        seed_item(&db, owner, BaseItemKind::Movie).await;
        let mut cases = Vec::new();
        for (label, eligible) in [
            ("owned", true),
            ("missing_media", true),
            ("channel", true),
            ("virtual", false),
            ("folder", false),
            ("audio", false),
        ] {
            let item = Uuid::new_v4();
            let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
            let mut entity = crate::test_support::fetch_item(&db, item).await;
            match label {
                "owned" => entity.owner_id = Some(guid_to_db(owner)),
                "missing_media" => std::fs::remove_file(&media).unwrap(),
                "channel" => entity.channel_id = Some(Uuid::new_v4().to_string()),
                "virtual" => entity.is_virtual_item = true,
                "folder" => entity.is_folder = true,
                "audio" => entity.media_type = Some("Audio".to_owned()),
                _ => unreachable!(),
            }
            crate::test_support::save_item(&db, &entity).await;
            let mut row = info(item, 320);
            row.tile_width = 2;
            row.tile_height = 2;
            r.mgr.save_trickplay_info(&row).await.unwrap();
            let internal = PathBuf::from(r.pm.trickplay_directory(
                item,
                MediaLocation::of_file(&media),
                false,
            ))
            .join("320 - 2x2");
            let sidecar =
                PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                    .join("320 - 2x2");
            std::fs::create_dir_all(&internal).unwrap();
            std::fs::write(internal.join("0.jpg"), label.as_bytes()).unwrap();
            cases.push((label, internal, sidecar, eligible));
        }
        let task = crate::scheduled_tasks::maintenance::MoveTrickplayImagesTask::new(Arc::new(
            r.mgr.clone(),
        ));
        task.execute(&TaskProgress::default()).await.unwrap();
        for (label, internal, sidecar, eligible) in cases {
            assert_eq!(sidecar.join("0.jpg").exists(), eligible, "{label}");
            assert_eq!(internal.join("0.jpg").exists(), !eligible, "{label}");
            let selected = if eligible { sidecar } else { internal };
            assert_eq!(
                std::fs::read(selected.join("0.jpg")).unwrap(),
                label.as_bytes()
            );
        }
        assert_eq!(r.extractor.calls(), 0);
    }

    #[tokio::test]
    async fn scan_context_uses_fresh_runtime_and_video_dimensions_before_final_row_save() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(2, 180));
        seed_video(&db, &r, item, Some(HOUR_TICKS), Some(1920)).await;
        let mut fresh = r.mgr.items.retrieve_item(item).await.unwrap().unwrap();
        fresh.run_time_ticks = Some(5_000 * 10_000);
        r.mgr
            .refresh_trickplay_for_media(item, &fresh, Some(&[]), true, &extraction_on())
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            0,
            "fresh runtime below the interval is ineligible despite the stored hour"
        );
        fresh.run_time_ticks = Some(60_000 * 10_000);
        r.mgr
            .refresh_trickplay_for_media(item, &fresh, Some(&[]), true, &extraction_on())
            .await
            .unwrap();
        assert_eq!(
            r.extractor.calls(),
            0,
            "fresh empty streams are ineligible even when stored probe rows exist"
        );
        let stream = ferrofin_db::entities::base_items::MediaStreamInfoEntity {
            item_id: fresh.id.clone(),
            stream_index: 0,
            stream_type: crate::db_error::media_stream_type_to_disc(
                ferrofin_model::entities::MediaStreamType::Video,
            ),
            codec: Some("h264".to_owned()),
            width: Some(200),
            height: Some(180),
            ..Default::default()
        };
        r.mgr
            .refresh_trickplay_for_media(item, &fresh, Some(&[stream]), true, &extraction_on())
            .await
            .unwrap();
        assert_eq!(
            r.extractor.last_request(),
            Some((10_000, 200)),
            "fresh stream width caps extraction"
        );
        assert_eq!(
            r.mgr
                .items
                .retrieve_item(item)
                .await
                .unwrap()
                .unwrap()
                .run_time_ticks,
            Some(HOUR_TICKS),
            "the old metadata row remains untouched until the scanner's final save"
        );
    }
    #[tokio::test]
    async fn unreadable_user_tile_is_catalogued_with_source_minimum_height_without_encoding() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let grid =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("640 - 3x2");
        std::fs::create_dir_all(&grid).unwrap();
        std::fs::write(grid.join("0.jpg"), b"invalid JPEG bytes").unwrap();
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(grid.join("0.jpg")).unwrap(),
            b"invalid JPEG bytes"
        );
        assert_eq!(r.extractor.calls(), 0);
        // Pinned SkiaEncoder.GetImageSize returns default dimensions for
        // undecodable bytes; discovery clamps the resulting height to one.
        let rows = r.mgr.get_trickplay_resolutions(item).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&640].height, 1);
    }

    #[tokio::test]
    async fn ineligible_media_is_untouched_before_discovery_pruning_or_forced_generation() {
        let db = test_db().await;
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        for data in [
            serde_json::json!({"VideoType":"Iso"}),
            serde_json::json!({"VideoType":2}),
            serde_json::json!({"VideoType":"BluRay"}),
            serde_json::json!({"IsPlaceHolder":true}),
            serde_json::json!({"IsShortcut":true}),
            serde_json::json!({}),
        ] {
            let item = Uuid::new_v4();
            let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
            let mut entity = crate::test_support::fetch_item(&db, item).await;
            entity.data = Some(data.to_string());
            crate::test_support::save_item(&db, &entity).await;
            if data.as_object().unwrap().is_empty() {
                crate::FerrofinMediaStreamRepository::new(db.clone())
                    .save_media_streams(item, &[])
                    .await
                    .unwrap();
            }
            let grid = PathBuf::from(r.pm.trickplay_directory(
                item,
                MediaLocation::of_file(&media),
                false,
            ))
            .join("320 - 2x2");
            std::fs::create_dir_all(&grid).unwrap();
            std::fs::write(grid.join("0.jpg"), b"existing data must remain untouched").unwrap();
            r.mgr.save_trickplay_info(&info(item, 320)).await.unwrap();
            for (replace, options) in [(false, LibraryOptions::default()), (true, extraction_on())]
            {
                r.mgr
                    .refresh_trickplay_data(item, replace, &options)
                    .await
                    .unwrap();
                assert_eq!(
                    std::fs::read(grid.join("0.jpg")).unwrap(),
                    b"existing data must remain untouched"
                );
                assert!(
                    r.mgr
                        .get_trickplay_resolutions(item)
                        .await
                        .unwrap()
                        .contains_key(&320)
                );
            }
        }
        assert_eq!(r.extractor.calls(), 0);
    }

    #[tokio::test]
    async fn complete_media_gate_uses_shortcut_channel_and_current_recording_facts() {
        let db = test_db().await;
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let item = Uuid::new_v4();
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;
        let mut entity = r.mgr.items.retrieve_item(item).await.unwrap().unwrap();
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, false)
                .is_some()
        );
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, true).is_none()
        );
        entity.tags = Some("LiVeStReAm".to_owned());
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, false)
                .is_some(),
            "a library tag alone is complete"
        );
        entity.channel_id = Some(guid_to_db(Uuid::new_v4()));
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, false)
                .is_none()
        );
        entity.tags = None;
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, false)
                .is_some(),
            "complete channel files remain eligible"
        );
        let shortcut = Path::new(&media).with_extension("STRM");
        std::fs::write(&shortcut, b"file:///video.mkv").unwrap();
        entity.path = Some(shortcut.to_string_lossy().into_owned());
        assert!(
            FerrofinTrickplayManager::can_generate_trickplay(&entity, 10_000, true, false)
                .is_none()
        );
    }

    #[tokio::test]
    async fn disabled_user_grid_discovery_uses_raw_interval_before_generation_clamps_it() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let mut options = options_2x2(&[320]);
        options.interval = 100;
        let r = rig(&db, options, FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(5_000 * 10_000), None).await;
        let grid =
            PathBuf::from(r.pm.trickplay_directory(item, MediaLocation::of_file(&media), true))
                .join("320 - 5x1");
        std::fs::create_dir_all(&grid).unwrap();
        image::RgbImage::new(1600, 180)
            .save(grid.join("0.jpg"))
            .unwrap();
        r.mgr
            .refresh_trickplay_data(
                item,
                false,
                &LibraryOptions {
                    save_trickplay_with_media: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let row = &r.mgr.get_trickplay_resolutions(item).await.unwrap()[&320];
        assert_eq!(
            (row.interval, row.thumbnail_count),
            (3000, 2),
            "raw 100ms cannot fit this grid; a premature 1000ms clamp would incorrectly infer five thumbnails"
        );
        assert_eq!(r.extractor.calls(), 0);
    }
}
