//! [`FerrofinTrickplayManager`] — the [`TrickplayManager`] over the
//! `TrickplayInfos` table **plus** real tile generation.
//!
//! Port of `Jellyfin.Server.Implementations.Trickplay.TrickplayManager`. The
//! metadata rows ([`TrickplayInfoEntity`](ferrofin_db::entities::playback::TrickplayInfoEntity))
//! are stored/listed/deleted directly over `ferrofin-db` (there is no separate
//! repository trait for this leaf table); [`Self::refresh_trickplay_data`] is
//! the real `RefreshTrickplayDataAsync` port: extract thumbnails through the
//! [`TrickplayFrameExtractor`] seam (ffmpeg in production), pack them into tile
//! grids through the [`ImageEncoder`] seam (`CreateTrickplayTile`), write them
//! into the [`PathManager`] trickplay layout, and persist the info row.
//!
//! Departures from the C# (documented per the port rules):
//! - The per-library `EnableTrickplayImageExtraction` gate is honoured (an
//!   item in a library with it off has its tiles and rows pruned, as in C#),
//!   but `SaveTrickplayWithMedia` only feeds the "user-managed, leave alone"
//!   guard: tiles are always generated into the **internal**
//!   (non-save-with-media) layout, matching [`Self::get_trickplay_tile_path`].
//! - Eligibility reads the resolved/stored video type, placeholder and shortcut
//!   facts, live DVR captures/channel livestreams, runtime and any media streams
//!   before discovery, pruning or generation (`CanGenerateTrickplay`).
//! - Existing user-managed grids are discovered in the selected location,
//!   including when extraction is disabled. Invalid or overflowing grid
//!   dimensions are ignored before division or database insertion.
//! - The hardware-acceleration / keyframe-only extraction options select the
//!   software ffmpeg path (see [`TrickplayFrameExtractor`]).
//!
//! The manifest key mirrors C#: trickplay is keyed by *media-source id*, and the
//! primary media source of an item is the item's own id in the dashless
//! `ToString("N")` form, so [`Self::get_trickplay_manifest`] nests the
//! resolutions under that string (the same one `MediaSources[].Id` carries).

use std::collections::HashMap;
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
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor};
use ferrofin_traits::options::ImageCollageOptions;
use ferrofin_traits::persistence::{ItemRepository, MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::system::PathManager;
use ferrofin_traits::trickplay::TrickplayManager;

use crate::db_error::db_err;
use crate::media_source_manager::stream_to_dto;

/// The number of 100-nanosecond ticks per millisecond
/// (`TimeSpan.TicksPerMillisecond`).
const TICKS_PER_MS: i64 = 10_000;

/// The minimum valid trickplay interval, in milliseconds — smaller configured
/// values are reset to this, mirroring the C# guard in
/// `RefreshTrickplayDataAsync`.
const MIN_INTERVAL_MS: i32 = 1000;

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
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(directory = %self.0.display(), %error, "cannot remove trickplay temporary directory");
        }
    }
}

/// The per-width refresh context shared by [`FerrofinTrickplayManager`] helpers:
/// the item identity, its media path, its stored video width, and the resolved
/// internal trickplay directory.
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
    /// The internal trickplay directory for the item.
    trickplay_dir: &'a str,
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
        }
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

    /// Port of `RefreshTrickplayDataInternal` for one configured width: skip or
    /// import existing tiles, otherwise extract frames and pack tiles.
    async fn refresh_width(
        &self,
        ctx: &WidthRefreshContext<'_>,
        replace: bool,
        width: i32,
        options: &TrickplayOptions,
    ) -> Result<(), ServiceError> {
        // Source checks these only inside per-width work: they do not bypass
        // discovery or the outer disabled/replace cleanup.
        if !Path::new(ctx.media_path).is_file() {
            tracing::warn!(item = %ctx.item_id, path = ctx.media_path, "trickplay media file not found");
            return Ok(());
        }
        if Path::new(ctx.media_path)
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.eq_ignore_ascii_case("backdrops"))
        {
            tracing::debug!(item = %ctx.item_id, path = ctx.media_path, "ignoring backdrop media");
            return Ok(());
        }
        // CanGenerateTrickplay counts all streams, but the pinned per-width
        // body dereferences mediaSource.VideoStream.Width. An audio-only source
        // therefore stops inside that body's caught error after outer cleanup.
        let Some(video_stream) = ctx.video_stream.as_ref() else {
            return Err(ServiceError::backend(
                "trickplay media source has no video stream",
            ));
        };
        // The width has to be even, otherwise a lot of filters cannot sample it;
        // a video narrower than the setting caps the width at the video's —
        // its video STREAM's width (`mediaSource.VideoStream.Width`,
        // `TrickplayManager.cs:427-431`), and only when it has one: the item
        // row's `Width` is 0 for a probe that found no dimensions, which is
        // "unknown", not a zero-pixel video.
        let mut actual_width = 2 * (width / 2);
        if let Some(video_width) = video_stream.width.map(i64::from)
            && video_width < i64::from(width)
        {
            tracing::warn!(
                video_width,
                trickplay_width = width,
                "video width is smaller than trickplay setting, using video width for thumbnails"
            );
            actual_width = i32::try_from(2 * (video_width / 2)).unwrap_or(actual_width);
        }
        if actual_width <= 0 {
            return Ok(());
        }

        let output_dir = Path::new(ctx.trickplay_dir).join(format!(
            "{actual_width} - {}x{}",
            options.tile_width, options.tile_height
        ));

        // Import existing trickplay tiles.
        if !replace {
            let existing_files = jpg_files_sorted(&output_dir);
            if !existing_files.is_empty() {
                if self
                    .resolutions_for(ctx.item_id)
                    .await?
                    .contains_key(&actual_width)
                {
                    tracing::debug!(item = %ctx.item_id, "found existing trickplay files");
                    return Ok(());
                }
                return self
                    .import_tiles(ctx.item_id, width, options, &existing_files)
                    .await;
            }
        }

        // Generate: extract thumbnails into a temp dir, pack them into tiles.
        tracing::info!(
            width = actual_width,
            path = ctx.media_path,
            item = %ctx.item_id,
            "creating trickplay files"
        );
        let frames_dir =
            std::env::temp_dir().join(format!("ferrofin_trickplay_{}", Uuid::new_v4().simple()));
        let _frames_cleanup = TemporaryTrickplayDirectory(frames_dir.clone());
        self.extract_and_tile(ctx, actual_width, options, &frames_dir, &output_dir)
            .await
    }

    /// Runs the frame extraction plus [`Self::create_tiles`], saving the
    /// resulting info row; on a save failure the freshly written tiles are
    /// removed so no files stay behind without a row (mirroring the C#).
    async fn extract_and_tile(
        &self,
        ctx: &WidthRefreshContext<'_>,
        actual_width: i32,
        options: &TrickplayOptions,
        frames_dir: &Path,
        output_dir: &Path,
    ) -> Result<(), ServiceError> {
        let images = self
            .frame_extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                input_path: ctx.media_path,
                video_stream: ctx.video_stream.as_ref(),
                interval_ms: options.interval,
                max_width: actual_width,
                qscale: options.qscale,
                threads: options.process_threads,
                output_dir: &frames_dir.to_string_lossy(),
                allow_hw_accel: options.enable_hw_acceleration,
                enable_hw_encoding: options.enable_hw_encoding,
                keyframe_only: options.enable_key_frame_only_extraction,
            })
            .await?;

        let mut info = self
            .create_tiles(&images, actual_width, options, output_dir)
            .await?;
        info.item_id = guid_to_db(ctx.item_id);

        if let Err(e) = self.save_trickplay_info(&info).await {
            // Make sure no files stay in metadata folders when the info row
            // wasn't saved.
            let _ = std::fs::remove_dir_all(output_dir);
            return Err(e);
        }
        tracing::info!(
            path = ctx.media_path,
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

        let work_dir = std::env::temp_dir().join(format!(
            "ferrofin_trickplay_tiles_{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&work_dir)
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
                )
                .await?;
            if info.height == 0 {
                info.height = height;
            }

            let bytes = std::fs::metadata(&tile_path)
                .map_err(|e| ServiceError::backend(format!("cannot stat tile: {e}")))?
                .len();
            let bitrate = tile_bitrate(bytes, info.tile_width, info.tile_height, info.interval);
            info.bandwidth = info.bandwidth.max(bitrate);
        }

        // Move trickplay tiles to the output directory (replacing any old ones).
        move_dir(work_dir, output_dir)?;
        Ok(info)
    }

    /// Port of the "Import tiles" branch of `RefreshTrickplayDataInternal`:
    /// adopts on-disk tile files without a DB row, computing height/bandwidth
    /// from the files themselves. As in the C#, the row stores the *requested*
    /// width and the configured interval/tile grid.
    async fn import_tiles(
        &self,
        item_id: Uuid,
        width: i32,
        options: &TrickplayOptions,
        existing_files: &[String],
    ) -> Result<(), ServiceError> {
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
            let bytes = std::fs::metadata(tile)
                .map_err(|e| ServiceError::backend(format!("cannot stat tile: {e}")))?
                .len();
            let bitrate = tile_bitrate(bytes, info.tile_width, info.tile_height, info.interval);
            info.bandwidth = info.bandwidth.max(bitrate);
        }

        self.save_trickplay_info(&info).await?;
        tracing::debug!(item = %item_id, "imported existing trickplay files");
        Ok(())
    }

    /// The orphaned-row prune of `DiscoverExistingTrickplayAsync`: drops DB
    /// rows whose tile folder no longer exists in either possible location
    /// (internal or media-adjacent).
    async fn prune_orphaned_rows(
        &self,
        item_id: Uuid,
        media_path: &str,
    ) -> Result<(), ServiceError> {
        let existing = self.resolutions_for(item_id).await?;
        let local_root = self
            .path_manager
            .trickplay_directory(item_id, media_path, false);
        let media_root = self
            .path_manager
            .trickplay_directory(item_id, media_path, true);
        for info in existing.values() {
            let sub = format!("{} - {}x{}", info.width, info.tile_width, info.tile_height);
            if !has_tiles(&Path::new(&local_root).join(&sub))
                && !has_tiles(&Path::new(&media_root).join(&sub))
            {
                tracing::info!(
                    width = info.width,
                    path = media_path,
                    "removed orphaned trickplay DB entry"
                );
                self.delete_trickplay_row(item_id, info.width).await?;
            }
        }
        Ok(())
    }

    /// Discovers the selected location before applying the extraction flag.
    /// User-managed grids are catalogued even when extraction is disabled.
    async fn discover_existing_trickplay(
        &self,
        entity: &BaseItemEntity,
        item_id: Uuid,
        media_path: &str,
        save_with_media: bool,
        options: &TrickplayOptions,
    ) -> Result<(), ServiceError> {
        self.prune_orphaned_rows(item_id, media_path).await?;
        let mut existing = self.resolutions_for(item_id).await?;
        let root = self
            .path_manager
            .trickplay_directory(item_id, media_path, save_with_media);
        if !Path::new(&root).is_dir() {
            return Ok(());
        }
        let entries = std::fs::read_dir(&root)
            .map_err(|error| ServiceError::backend(format!("cannot list trickplay: {error}")))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ServiceError::backend(format!("cannot list trickplay: {error}"))
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some((width, tile_width, tile_height)) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(parse_grid_name)
            else {
                continue;
            };
            if existing.contains_key(&width) {
                continue;
            }
            let mut tiles = Vec::new();
            for tile in std::fs::read_dir(&path).map_err(|error| {
                ServiceError::backend(format!("cannot read trickplay grid: {error}"))
            })? {
                let tile = tile
                    .map_err(|error| {
                        ServiceError::backend(format!("cannot list trickplay grid: {error}"))
                    })?
                    .path();
                if tile.is_file()
                    && tile
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("jpg"))
                {
                    tiles.push(tile.to_string_lossy().into_owned());
                }
            }
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
            let mut info = TrickplayInfoEntity {
                item_id: guid_to_db(item_id),
                width,
                tile_width,
                tile_height,
                interval,
                thumbnail_count,
                height: ceil_div(size.height, tile_height).max(1),
                bandwidth: 0,
            };
            for tile in &tiles {
                let bytes = std::fs::metadata(tile)
                    .map_err(|error| ServiceError::backend(format!("cannot stat tile: {error}")))?
                    .len();
                info.bandwidth =
                    info.bandwidth
                        .max(tile_bitrate(bytes, tile_width, tile_height, interval));
            }
            self.save_trickplay_info(&info).await?;
            existing.insert(width, info);
            tracing::debug!(item_id = %item_id, width, interval, thumbnail_count, "discovered existing trickplay grid");
        }
        Ok(())
    }

    /// The trailing cleanup of `RefreshTrickplayDataAsync`: removes tile
    /// folders in the trickplay directory that no stored row accounts for.
    async fn prune_unexpected_folders(
        &self,
        item_id: Uuid,
        trickplay_dir: &str,
    ) -> Result<(), ServiceError> {
        let root = Path::new(trickplay_dir);
        if !root.is_dir() {
            return Ok(());
        }
        let rows = self.resolutions_for(item_id).await?;
        let expected: std::collections::HashSet<PathBuf> = rows
            .values()
            .map(|i| root.join(format!("{} - {}x{}", i.width, i.tile_width, i.tile_height)))
            .collect();
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
        Ok(())
    }

    /// Moves this item's generated trickplay tiles to the location the
    /// configuration dictates.
    ///
    /// Port of `MoveGeneratedTrickplayDataAsync`. Ferrofin has no per-library
    /// `SaveTrickplayWithMedia` option (see `ferrofin-providers`'
    /// `LibraryOptions`), so the configured location is always the **internal**
    /// data-dir layout: any tiles found in a media-adjacent `.trickplay` folder
    /// are moved into the internal directory (media wins over an already
    /// populated internal folder, as in the C#), and the emptied `.trickplay`
    /// parent folder is removed.
    ///
    /// This is an inherent method (not on the [`TrickplayManager`] trait),
    /// consumed by the "Migrate Trickplay Image Location" scheduled task.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the item lookup, configuration read, or a
    /// filesystem move fails.
    pub async fn move_generated_trickplay_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(entity) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
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

        let local_root = self
            .path_manager
            .trickplay_directory(item_id, &media_path, false);
        let media_root = self
            .path_manager
            .trickplay_directory(item_id, &media_path, true);

        for info in self.resolutions_for(item_id).await?.values() {
            let sub = format!("{} - {}x{}", info.width, info.tile_width, info.tile_height);
            let media_dir = Path::new(&media_root).join(&sub);
            let local_dir = Path::new(&local_root).join(&sub);
            if !media_dir.is_dir() {
                continue;
            }
            // Mirror the C# guard: move when the media-adjacent folder has
            // files and the internal one is either absent or itself populated
            // (the move replaces it).
            let local_exists = local_dir.exists();
            if has_tiles(&media_dir) && (!local_exists || has_tiles(&local_dir)) {
                move_content(&media_dir, &local_dir)?;
                tracing::info!(
                    item = %item_id,
                    to = %local_dir.display(),
                    "moved trickplay images"
                );
            }
        }
        Ok(())
    }
    /// Scan callers supply the fresh probe context; scheduled tasks read it
    /// from the repository. Both use one extraction and storage implementation.
    async fn refresh_media(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        video_stream: Option<MediaStream>,
        has_streams: bool,
        replace: bool,
        library_options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let mut options = self.trickplay_options().await?;
        let Some(media_path) = Self::can_generate_trickplay(
            entity,
            options.interval,
            has_streams,
            self.is_active_recording(entity).await?,
        ) else {
            return Ok(());
        };
        // Catalog what is already on disk before pruning or generating.
        if !replace {
            self.discover_existing_trickplay(
                entity,
                item_id,
                &media_path,
                library_options.save_trickplay_with_media,
                &options,
            )
            .await?;
        }

        let trickplay_dir = self
            .path_manager
            .trickplay_directory(item_id, &media_path, false);

        let enabled = library_options.enable_trickplay_image_extraction;
        // When extraction is disabled and files live next to media, treat them
        // as user-managed: discovery above already catalogued what is on disk.
        if !enabled && !replace && library_options.save_trickplay_with_media {
            return Ok(());
        }

        if !enabled || replace {
            // Prune existing data.
            if Path::new(&trickplay_dir).exists()
                && let Err(e) = std::fs::remove_dir_all(&trickplay_dir)
            {
                tracing::warn!(
                    directory = trickplay_dir,
                    "unable to clear trickplay directory: {e}"
                );
            }
            self.delete_trickplay_data(item_id).await?;
            if !replace {
                return Ok(());
            }
        }

        Self::clamp_generation_interval(&mut options);
        tracing::debug!(item = %item_id, replace, "trickplay refresh");
        let ctx = WidthRefreshContext {
            item_id,
            media_path: &media_path,
            video_stream,
            trickplay_dir: &trickplay_dir,
        };
        for width in &options.width_resolutions {
            if *width <= 0 {
                continue;
            }
            // Per-width failures are logged and do not abort the remaining
            // resolutions (the C# catch-and-log).
            if let Err(e) = self.refresh_width(&ctx, replace, *width, &options).await {
                tracing::error!(item = %item_id, width, "error creating trickplay images: {e}");
            }
        }

        self.prune_unexpected_folders(item_id, &trickplay_dir).await
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
        let Some(entity) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
        let streams = self.media_streams(item_id).await?;
        let has_streams = !streams.is_empty();
        let stream = streams
            .into_iter()
            .find(|stream| {
                stream.stream_type
                    == crate::db_error::media_stream_type_to_disc(MediaStreamType::Video)
            })
            .map(stream_to_dto);
        self.refresh_media(
            item_id,
            &entity,
            stream,
            has_streams,
            replace,
            library_options,
        )
        .await
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
        let stream = streams
            .iter()
            .find(|stream| {
                stream.stream_type
                    == crate::db_error::media_stream_type_to_disc(MediaStreamType::Video)
            })
            .cloned()
            .map(stream_to_dto);
        self.refresh_media(
            item_id,
            entity,
            stream,
            !streams.is_empty(),
            replace,
            library_options,
        )
        .await
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

    async fn get_trickplay_manifest(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
        let resolutions = self.resolutions_for(item_id).await?;
        let mut manifest = HashMap::new();
        if !resolutions.is_empty() {
            // The primary media source of an item is the item itself, reported
            // by the media-source manager as the dashless id (C# `ToString("N")`).
            manifest.insert(item_id.simple().to_string(), resolutions);
        }
        Ok(manifest)
    }

    async fn get_trickplay_manifest_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, HashMap<String, HashMap<i32, TrickplayInfoEntity>>>, ServiceError>
    {
        let mut out: HashMap<Uuid, HashMap<String, HashMap<i32, TrickplayInfoEntity>>> =
            HashMap::with_capacity(item_ids.len());
        if item_ids.is_empty() {
            return Ok(out);
        }
        // One query for the page's resolutions, grouped into a per-item manifest
        // (media-source id = the item's own id, as the single-item form does).
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let ph = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
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
                    // Key the inner manifest with the same dashless media-source
                    // id the single-item form emits, independent of the stored
                    // (uppercase-hyphenated) column format.
                    out.entry(id)
                        .or_default()
                        .entry(id.simple().to_string())
                        .or_default()
                        .insert(row.width, row);
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
        // Port of C# `GetTrickplayTilePathAsync`: locate the resolution, then
        // build `{trickplay-dir}/{width} - {tw}x{th}/{index}.jpg`. The C#
        // `saveWithMedia` flag (a per-library option) is not modeled at this
        // seam, so the internal (non-save-with-media) directory is used; the
        // media path it ignores is passed empty.
        let resolutions = self.resolutions_for(item_id).await?;
        let Some(info) = resolutions.get(&width) else {
            return Ok(None);
        };
        let base = self.path_manager.trickplay_directory(item_id, "", false);
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

/// Whether `dir` exists and directly contains at least one `.jpg` file
/// (C# `HasTrickplayTiles`).
fn has_tiles(dir: &Path) -> bool {
    !jpg_files_sorted(dir).is_empty()
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
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files
}

/// Moves the flat directory `src` to `dst`, replacing `dst` if present; falls
/// back to copy-and-delete when a rename crosses filesystems.
fn move_dir(src: &Path, dst: &Path) -> Result<(), ServiceError> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ServiceError::backend(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    if dst.exists() {
        std::fs::remove_dir_all(dst)
            .map_err(|e| ServiceError::backend(format!("cannot replace {}: {e}", dst.display())))?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-device fallback: copy the (flat) tile files, then drop the source.
    std::fs::create_dir_all(dst)
        .map_err(|e| ServiceError::backend(format!("cannot create {}: {e}", dst.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| ServiceError::backend(format!("cannot read {}: {e}", src.display())))?;
    for entry in entries.filter_map(Result::ok) {
        let from = entry.path();
        if from.is_file()
            && let Some(name) = from.file_name()
        {
            std::fs::copy(&from, dst.join(name))
                .map_err(|e| ServiceError::backend(format!("cannot copy tile: {e}")))?;
        }
    }
    std::fs::remove_dir_all(src)
        .map_err(|e| ServiceError::backend(format!("cannot remove {}: {e}", src.display())))?;
    Ok(())
}

/// Port of the C# `MoveContent`: moves `src` into `dst`, then removes `src`'s
/// now-empty parent folder (the `.trickplay` container) when nothing is left in
/// it.
fn move_content(src: &Path, dst: &Path) -> Result<(), ServiceError> {
    move_dir(src, dst)?;
    if let Some(parent) = src.parent()
        && std::fs::read_dir(parent).is_ok_and(|mut d| d.next().is_none())
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrofin_model::entities_media::MediaStream;
    use ferrofin_traits::media_encoding::TrickplayExtraction;

    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, ServerConfiguration, TrickplayOptions};
    use ferrofin_model::data::BaseItemKind;
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
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::TrickplayFrameExtractor;
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

    /// A config manager returning a fixed [`TrickplayOptions`].
    struct FixedConfig {
        options: TrickplayOptions,
    }

    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("not used in these tests")
        }

        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(ServerConfiguration {
                trickplay_options: self.options.clone(),
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

    /// A frame-extraction fake writing real (decodable) JPEG frames of the
    /// requested width, recording its invocations.
    struct FakeExtractor {
        /// How many frames each run writes.
        frames: usize,
        /// The pixel height of each written frame.
        frame_height: u32,
        calls: AtomicUsize,
        last_request: Mutex<Option<(i32, i32)>>,
        /// The video stream the last request carried, which is what decides
        /// whether the extractor can take the hardware path at all.
        last_stream: Mutex<Option<MediaStream>>,
        last_output: Mutex<Option<String>>,
        hold: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    }

    impl FakeExtractor {
        fn new(frames: usize, frame_height: u32) -> Self {
            Self {
                frames,
                frame_height,
                calls: AtomicUsize::new(0),
                last_request: Mutex::new(None),
                last_stream: Mutex::new(None),
                last_output: Mutex::new(None),
                hold: None,
            }
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
            *self.last_request.lock().expect("lock") =
                Some((request.interval_ms, request.max_width));
            *self.last_stream.lock().expect("lock") = request.video_stream.cloned();
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
    }

    /// The full manager fixture: temp app paths, real path manager / item
    /// repository / `image`-crate encoder, fake config + extractor.
    struct Rig {
        _tmp: tempfile::TempDir,
        mgr: FerrofinTrickplayManager,
        extractor: Arc<FakeExtractor>,
        pm: Arc<dyn PathManager>,
        /// A directory for test media files inside the temp root.
        media_dir: PathBuf,
    }

    fn rig(db: &Database, options: TrickplayOptions, extractor: FakeExtractor) -> Rig {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media_dir = tmp.path().join("media");
        std::fs::create_dir_all(&media_dir).expect("media dir");
        let paths = test_paths(tmp.path());
        let pm: Arc<dyn PathManager> = Arc::new(FerrofinPathManager::new(paths));
        let extractor = Arc::new(extractor);
        let items = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let mgr = FerrofinTrickplayManager::new(
            db.clone(),
            Arc::clone(&pm),
            Arc::new(FixedConfig { options }),
            items,
            Arc::new(crate::FerrofinMediaStreamRepository::new(db.clone())),
            Arc::clone(&extractor) as Arc<dyn TrickplayFrameExtractor>,
            Arc::new(ImageCrateEncoder::new()),
        );
        Rig {
            _tmp: tmp,
            mgr,
            extractor,
            pm,
            media_dir,
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
        seed_item(db, id, BaseItemKind::Movie).await;
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

        // Manifest nests resolutions under the media-source (item) id.
        let manifest = r.mgr.get_trickplay_manifest(item).await.expect("manifest");
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[&item.simple().to_string()].len(), 2);

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
        let root = PathBuf::from(r.pm.trickplay_directory(item, &media, false));
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

        let tile_dir = Path::new(&r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
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

    #[tokio::test]
    async fn refresh_skips_existing_and_replace_regenerates() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

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

        // Replace wipes the directory (a stale marker vanishes) and re-extracts.
        let root = PathBuf::from(r.pm.trickplay_directory(item, &media, false));
        std::fs::write(root.join("stale.txt"), b"x").expect("marker");
        r.mgr
            .refresh_trickplay_data(item, true, &extraction_on())
            .await
            .expect("replace");
        assert_eq!(r.extractor.calls(), 2, "replace regenerates");
        assert!(
            !root.join("stale.txt").exists(),
            "old directory was cleared"
        );
        let res = r.mgr.get_trickplay_resolutions(item).await.expect("res");
        assert_eq!(res[&320].thumbnail_count, 4);
    }

    #[tokio::test]
    async fn refresh_imports_on_disk_tiles_without_a_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(4, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        // A user-placed 640×360 tile grid (2×2 of 320×180) with no DB row.
        let tile_dir = Path::new(&r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
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
        let tile_dir = Path::new(&r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
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
        let tile_dir = Path::new(&r.pm.trickplay_directory(item, &media, true)).join("320 - 2x2");
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
            let root = PathBuf::from(r.pm.trickplay_directory(item, &media, false));
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
        assert!(
            r.mgr
                .get_trickplay_items(10, 0)
                .await
                .expect("items")
                .is_empty()
        );
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
        let root = PathBuf::from(r.pm.trickplay_directory(item, &media, false));
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
    async fn move_generated_trickplay_data_consolidates_media_adjacent_tiles() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        // A stored row plus tiles living media-adjacent (`{stem}.trickplay`).
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.expect("save");
        let media_root = PathBuf::from(r.pm.trickplay_directory(item, &media, true));
        let media_tiles = media_root.join("320 - 2x2");
        std::fs::create_dir_all(&media_tiles).expect("media tiles dir");
        std::fs::write(media_tiles.join("0.jpg"), b"tile").expect("tile");

        r.mgr
            .move_generated_trickplay_data(item)
            .await
            .expect("move");

        let local_tiles =
            PathBuf::from(r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
        assert!(local_tiles.join("0.jpg").is_file(), "tiles moved inward");
        assert!(!media_tiles.exists(), "media-adjacent tiles gone");
        assert!(!media_root.exists(), "emptied .trickplay folder removed");
    }

    #[tokio::test]
    async fn move_generated_trickplay_data_is_a_noop_without_media_tiles() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), FakeExtractor::new(0, 180));
        let media = seed_video(&db, &r, item, Some(HOUR_TICKS), None).await;

        // Row + internal tiles only: nothing to move, nothing destroyed.
        let mut row = info(item, 320);
        row.tile_width = 2;
        row.tile_height = 2;
        r.mgr.save_trickplay_info(&row).await.expect("save");
        let local_tiles =
            PathBuf::from(r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
        std::fs::create_dir_all(&local_tiles).expect("local tiles dir");
        std::fs::write(local_tiles.join("0.jpg"), b"tile").expect("tile");

        r.mgr
            .move_generated_trickplay_data(item)
            .await
            .expect("move");
        assert!(local_tiles.join("0.jpg").is_file(), "internal tiles kept");

        // An unknown item is also fine.
        r.mgr
            .move_generated_trickplay_data(Uuid::new_v4())
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
        let root = PathBuf::from(r.pm.trickplay_directory(item, &media, true));
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
            r.mgr.get_trickplay_manifest(item).await.unwrap()[&item.simple().to_string()]
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
        let grid = PathBuf::from(r.pm.trickplay_directory(item, &media, true)).join("640 - 3x2");
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
    async fn cancelling_a_running_scan_removes_its_partially_written_frame_directory() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut extractor = FakeExtractor::new(2, 180);
        extractor.hold = Some((entered.clone(), release));
        let db = test_db().await;
        let item = Uuid::new_v4();
        let r = rig(&db, options_2x2(&[320]), extractor);
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
        assert!(
            r.mgr
                .get_trickplay_resolutions(item)
                .await
                .unwrap()
                .is_empty()
        );
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
        let grid = PathBuf::from(r.pm.trickplay_directory(item, &media, true)).join("640 - 3x2");
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
            let grid =
                PathBuf::from(r.pm.trickplay_directory(item, &media, false)).join("320 - 2x2");
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
        let grid = PathBuf::from(r.pm.trickplay_directory(item, &media, true)).join("320 - 5x1");
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
