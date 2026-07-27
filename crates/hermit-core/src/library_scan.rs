//! [`LibraryScanner`] — the filesystem → item-store scan.
//!
//! Walks every virtual folder's media paths and materializes typed
//! [`BaseItemEntity`] rows under the library's `CollectionFolder`, linking each
//! into the `AncestorIds` closure so the library listing
//! (`GET /Items?ParentId=<library>&Recursive=true`) is populated and items
//! direct-play.
//!
//! Dispatches by collection type: `movies`/`homevideos`/`musicvideos`/`mixed`
//! (and untyped) libraries flatten every video file to a `Movie`; `tvshows` build
//! the Series→Season→Episode hierarchy; `music` builds MusicAlbum→Audio. Pruning
//! of deleted files and remote-metadata refresh are follow-ups (see
//! `brain/plans/PLAN_HERMIT_LIBRARY_SCAN.md`).
//!
//! Two passes: a **synchronous plan** (walk + filename resolution — this is where
//! the `!Sync` [`NamingOptions`] lazy-regex cells live, so they never cross an
//! `.await`), then an **async persist**. The filesystem seam is synchronous, so
//! the whole walk fits the sync pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use hermit_db::entities::base_items::{BaseItemEntity, MediaStreamInfoEntity, PeopleEntity};
use hermit_model::data::BaseItemKind;
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities::{CollectionTypeOptions, ImageType};
use hermit_model::entities_media::VirtualFolderInfo;
use hermit_model::io::FileSystemEntryType;
use hermit_naming::audio::is_audio_file;
use hermit_naming::common::NamingOptions;
use hermit_naming::tv::{EpisodeResolver, season_path_parser, series_resolver};
use hermit_naming::video::video_resolver;
use hermit_providers::{
    EpisodeLocalImageProvider, FsDirectoryService, ImageItem, ImageItemKind, LocalImageProvider,
    RemoteImage, TmdbClient, TmdbDetails, TmdbKind,
};
use hermit_traits::error::ServiceError;
use hermit_traits::filesystem::FileSystem;
use hermit_traits::library::VirtualFolderManager;
use hermit_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use hermit_traits::options::ItemImageInfo;
use hermit_traits::persistence::{ItemPersistenceService, MediaStreamRepository};
use uuid::Uuid;

use crate::item_type_lookup;
use crate::media_source_manager::stream_dto_to_entity;

/// Per-scan artwork lookup state, so a series is matched against TMDB once and
/// its seasons/episodes reuse that match (and its per-season episode stills).
#[derive(Default)]
struct ArtworkCache {
    /// Series item id → matched TMDB series id.
    series_tmdb: std::collections::HashMap<String, i64>,
    /// (series item id, season number) → {episode number → still image URL}.
    season_stills: std::collections::HashMap<(String, i32), std::collections::HashMap<i32, String>>,
}

/// Downloads each [`RemoteImage`] into `item_dir` (as `{type}.jpg`) and returns
/// the persisted-image rows.
///
/// Idempotent per file: an image already on disk is referenced without a
/// re-download. Individual download/write failures are skipped (best-effort); a
/// wholly-failed set yields an empty vec.
async fn download_images(
    tmdb: &TmdbClient,
    item_dir: &Path,
    item_id: &str,
    images: Vec<RemoteImage>,
) -> Vec<ItemImageInfo> {
    let mut infos = Vec::new();
    for image in images {
        let dest = item_dir.join(format!("{}.jpg", image_type_file_stem(image.image_type)));
        if !dest.exists() {
            let Some(bytes) = tmdb.download(&image.url).await else {
                continue;
            };
            if let Err(err) =
                std::fs::create_dir_all(item_dir).and_then(|()| std::fs::write(&dest, &bytes))
            {
                tracing::warn!(%err, item = %item_id, "failed to write downloaded artwork");
                continue;
            }
        }
        infos.push(ItemImageInfo {
            path: dest.to_string_lossy().into_owned(),
            image_type: image.image_type,
            date_modified: Utc::now(),
            width: 0,
            height: 0,
            blur_hash: None,
        });
    }
    infos
}

/// One item the plan pass resolved, ready to persist.
struct Planned {
    /// The item id (also `entity.id`, kept typed for `set_ancestors`).
    id: Uuid,
    entity: BaseItemEntity,
    /// The ancestor closure (`ParentId` chain up to the collection folder).
    ancestors: Vec<Uuid>,
}

/// Walks configured libraries and persists their contents as item rows.
pub struct LibraryScanner {
    virtual_folders: Arc<dyn VirtualFolderManager>,
    file_system: Arc<dyn FileSystem>,
    persistence: Arc<dyn ItemPersistenceService>,
    /// Optional ffprobe seam. When present, each leaf media file is probed during
    /// the scan so its duration/size and per-stream codec info are persisted —
    /// which is what lets the web client choose direct play (and the transcoder
    /// build its arguments). Absent in unit tests, which don't need ffmpeg.
    media_encoder: Option<Arc<dyn MediaEncoder>>,
    /// Where probed streams are stored (paired with `media_encoder`).
    media_streams: Option<Arc<dyn MediaStreamRepository>>,
    /// Optional TMDB client for fetching remote artwork (posters/backdrops) for
    /// items without local images. Paired with [`metadata_dir`](Self::metadata_dir).
    tmdb: Option<Arc<TmdbClient>>,
    /// The directory downloaded artwork is stored under (`{meta}/library/{id}`).
    metadata_dir: Option<PathBuf>,
    /// Where cast/crew credits are persisted (paired with [`tmdb`](Self::tmdb) so a
    /// movie/series with no overview gets its TMDB cast during the scan).
    people: Option<Arc<dyn hermit_traits::persistence::PeopleRepository>>,
}

impl std::fmt::Debug for LibraryScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryScanner").finish_non_exhaustive()
    }
}

impl LibraryScanner {
    /// Builds a scanner over the library + filesystem + item-store seams.
    #[must_use]
    pub fn new(
        virtual_folders: Arc<dyn VirtualFolderManager>,
        file_system: Arc<dyn FileSystem>,
        persistence: Arc<dyn ItemPersistenceService>,
    ) -> Self {
        Self {
            virtual_folders,
            file_system,
            persistence,
            media_encoder: None,
            media_streams: None,
            tmdb: None,
            metadata_dir: None,
            people: None,
        }
    }

    /// Attaches the people repository so TMDB cast/crew credits are persisted
    /// during the scan. Paired with [`with_metadata`](Self::with_metadata).
    #[must_use]
    pub fn with_people(
        mut self,
        people: Arc<dyn hermit_traits::persistence::PeopleRepository>,
    ) -> Self {
        self.people = Some(people);
        self
    }

    /// Attaches the TMDB artwork client + the directory downloaded images are
    /// stored under, so items with no local artwork get remote posters/backdrops
    /// during the scan (Jellyfin's automatic-artwork behaviour). Omitted in unit
    /// tests, which don't hit the network.
    #[must_use]
    pub fn with_metadata(mut self, tmdb: Arc<TmdbClient>, metadata_dir: PathBuf) -> Self {
        self.tmdb = Some(tmdb);
        self.metadata_dir = Some(metadata_dir);
        self
    }

    /// Attaches the ffprobe seam so leaf media files are probed during the scan
    /// (persisting duration/size + per-stream codec info). Wired by the composition
    /// root; omitted in unit tests that don't exercise playback metadata.
    #[must_use]
    pub fn with_probe(
        mut self,
        media_encoder: Arc<dyn MediaEncoder>,
        media_streams: Arc<dyn MediaStreamRepository>,
    ) -> Self {
        self.media_encoder = Some(media_encoder);
        self.media_streams = Some(media_streams);
        self
    }

    /// Scans every configured library; returns the number of items created.
    ///
    /// Idempotent: item ids are deterministic
    /// ([`derive_item_id`](item_type_lookup::derive_item_id)), so re-scanning
    /// upserts rather than duplicates.
    ///
    /// # Errors
    /// Propagates the item-store failure if listing libraries, saving an item,
    /// or writing its ancestor closure fails.
    pub async fn scan_all(&self) -> Result<usize, ServiceError> {
        let folders = self.virtual_folders.get_virtual_folders().await?;
        let planned = self.plan(&folders); // sync: NamingOptions never crosses an await
        // Carries matched series' TMDB ids + their episode-still URLs across the
        // scan so seasons/episodes resolve against the same series lookup.
        let mut art_cache = ArtworkCache::default();
        for item in &planned {
            // Probe first so the item row is saved already carrying its duration and
            // size (the streams themselves are saved after, since they FK the row).
            let mut entity = item.entity.clone();
            let streams = self.probe(&mut entity).await;
            // Enrich the row from TMDB (overview/tagline/genres/studios/ratings +
            // cast/crew) before it's saved, so a bare file with no NFO shows the
            // same detail page Jellyfin does. Best-effort: failures don't abort.
            let people = self.fetch_remote_metadata(&mut entity).await;
            self.persistence
                .save_items(std::slice::from_ref(&entity))
                .await?;
            self.persistence
                .set_ancestors(item.id, &item.ancestors)
                .await?;
            if !people.is_empty()
                && let Some(repo) = &self.people
                && let Err(err) = repo.update_people(item.id, &people).await
            {
                tracing::warn!(%err, item = %item.id, "failed to persist cast/crew");
            }
            if let (false, Some(repo)) = (streams.is_empty(), &self.media_streams) {
                repo.save_media_streams(item.id, &streams).await?;
            }
            // Artwork: local files first (poster/backdrop/logo/… next to the
            // media), then a TMDB fallback for movies/series with none — matching
            // Jellyfin, which fetches remote artwork automatically. Best-effort: a
            // failure here must not abort the rest of the scan.
            let mut images = discover_local_images(&entity);
            if images.is_empty() {
                images = self.fetch_remote_images(&entity, &mut art_cache).await;
            }
            if !images.is_empty()
                && let Err(err) = self.persistence.save_item_images(item.id, &images).await
            {
                tracing::warn!(%err, item = %item.id, "failed to persist discovered artwork");
            }
        }
        Ok(planned.len())
    }

    /// Best-effort ffprobe of a leaf media item: enriches `entity` with the probed
    /// `run_time_ticks`/`size` and returns its media streams (ready to persist).
    ///
    /// Returns an empty vec — leaving the item unprobed but still browsable — when
    /// no encoder is wired, the item is a folder or non-media, it has no path, or
    /// the probe fails (missing ffmpeg, unreadable file). Probe failures are
    /// swallowed so one bad file never aborts a whole library scan.
    async fn probe(&self, entity: &mut BaseItemEntity) -> Vec<MediaStreamInfoEntity> {
        let Some(encoder) = &self.media_encoder else {
            return Vec::new();
        };
        let is_audio = entity.media_type.as_deref() == Some("Audio");
        let is_media = is_audio || entity.media_type.as_deref() == Some("Video");
        if entity.is_folder || !is_media {
            return Vec::new();
        }
        let Some(path) = entity.path.clone() else {
            return Vec::new();
        };
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(path),
                ..Default::default()
            },
            extract_chapters: false,
            media_is_audio: is_audio,
        };
        let probed = match encoder.get_media_info(&request).await {
            Ok(probed) => probed,
            Err(e) => {
                tracing::warn!(error = %e, path = ?entity.path, "media probe failed; item left unprobed");
                return Vec::new();
            }
        };
        entity.run_time_ticks = probed.run_time_ticks.or(entity.run_time_ticks);
        entity.size = probed.size.or(entity.size);
        probed
            .media_streams
            .iter()
            .map(|s| stream_dto_to_entity(&entity.id, s))
            .collect()
    }

    /// Fetches remote artwork (TMDB) for an item that has no local images,
    /// downloading each image into `{metadata}/library/{id}` and returning the
    /// rows to persist.
    ///
    /// Covers movies + the whole TV tree: a movie/series poster+backdrop, a
    /// season poster, and an episode still. `cache` carries the matched series'
    /// TMDB id and its seasons' episode-still URLs across the scan so a season is
    /// looked up once (`/tv/{id}/season/{n}`) rather than per episode.
    ///
    /// Idempotent: an item whose artwork was already downloaded (its folder holds
    /// `primary.*`) is reused from disk with no network call. Returns empty when
    /// Enriches a movie/series row from TMDB (overview, tagline, genres, studios,
    /// community rating, US certification, premiere date) and returns its cast +
    /// key crew to persist. No-op when TMDB is unconfigured, the item already has
    /// an overview (a local NFO or a prior scan), or the item isn't a movie/series.
    /// Best-effort — a network/parse failure returns no people and leaves the row.
    async fn fetch_remote_metadata(&self, entity: &mut BaseItemEntity) -> Vec<PeopleEntity> {
        let Some(tmdb) = &self.tmdb else {
            return Vec::new();
        };
        if entity.overview.as_deref().is_some_and(|o| !o.is_empty()) {
            return Vec::new();
        }
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        let kind = match short {
            "Movie" => TmdbKind::Movie,
            "Series" => TmdbKind::Series,
            _ => return Vec::new(),
        };
        let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
            return Vec::new();
        };
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
        let Some(tmdb_id) = tmdb
            .search(kind, name, year)
            .await
            .into_iter()
            .next()
            .map(|h| h.tmdb_id)
        else {
            return Vec::new();
        };
        let Some(details) = tmdb.details(kind, tmdb_id).await else {
            return Vec::new();
        };
        apply_details(entity, &details);
        details
            .people
            .iter()
            .map(|p| PeopleEntity {
                id: Uuid::new_v4().to_string(),
                name: p.name.clone(),
                person_type: Some(p.person_type.clone()),
            })
            .collect()
    }

    /// metadata is not configured or nothing matched — best-effort, never fatal.
    async fn fetch_remote_images(
        &self,
        entity: &BaseItemEntity,
        cache: &mut ArtworkCache,
    ) -> Vec<ItemImageInfo> {
        let (Some(tmdb), Some(meta_root)) = (&self.tmdb, &self.metadata_dir) else {
            return Vec::new();
        };
        let item_dir = meta_root.join(&entity.id);
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());

        // Each branch resolves its TMDB match/fetch (cheap search) and downloads
        // via `download_images`, which skips any image already on disk — so a
        // re-scan re-uses files without re-downloading, while still populating the
        // per-series id/still cache the seasons and episodes depend on.
        match short {
            "Movie" => {
                let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                    return Vec::new();
                };
                let images = tmdb.images_for(TmdbKind::Movie, name, year).await;
                download_images(tmdb, &item_dir, &entity.id, images).await
            }
            "Series" => {
                let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                    return Vec::new();
                };
                let Some(matched) = tmdb.series_match(name, year).await else {
                    return Vec::new();
                };
                // Remember the TMDB id so this series' seasons/episodes resolve.
                cache.series_tmdb.insert(entity.id.clone(), matched.tmdb_id);
                download_images(tmdb, &item_dir, &entity.id, matched.images).await
            }
            "Season" => {
                let (Some(series_id), Some(season_num)) = (
                    entity.series_id.as_deref(),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) else {
                    return Vec::new();
                };
                let Some(&tmdb_id) = cache.series_tmdb.get(series_id) else {
                    return Vec::new(); // series didn't match TMDB
                };
                // One request yields the season poster + every episode still.
                let season = tmdb.season_images(tmdb_id, season_num).await;
                cache
                    .season_stills
                    .insert((series_id.to_owned(), season_num), season.episode_stills);
                let images = season
                    .poster
                    .map(|url| {
                        vec![RemoteImage {
                            image_type: ImageType::Primary,
                            url,
                        }]
                    })
                    .unwrap_or_default();
                download_images(tmdb, &item_dir, &entity.id, images).await
            }
            "Episode" => {
                let (Some(series_id), Some(season_num), Some(ep_num)) = (
                    entity.series_id.as_deref(),
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) else {
                    return Vec::new();
                };
                let Some(url) = cache
                    .season_stills
                    .get(&(series_id.to_owned(), season_num))
                    .and_then(|stills| stills.get(&ep_num))
                    .cloned()
                else {
                    return Vec::new();
                };
                let images = vec![RemoteImage {
                    image_type: ImageType::Primary,
                    url,
                }];
                download_images(tmdb, &item_dir, &entity.id, images).await
            }
            _ => Vec::new(),
        }
    }

    /// The synchronous plan pass: resolve every library's files into [`Planned`]
    /// items. Owns the `NamingOptions` so its `!Sync` cells stay off the async path.
    ///
    /// Dispatches by the library's collection type: `tvshows` builds the
    /// Series→Season→Episode hierarchy, `music` builds MusicAlbum→Audio, and the
    /// video types (`movies`/`homevideos`/`musicvideos`/`mixed`) plus an untyped
    /// library flatten every video file to a `Movie`. Other types (books, photos,
    /// …) are not scanned yet.
    fn plan(&self, folders: &[VirtualFolderInfo]) -> Vec<Planned> {
        let naming = NamingOptions::new();
        let mut out = Vec::new();
        for folder in folders {
            // `item_id` is the library's CollectionFolder id (projected by the
            // virtual-folder manager); items hang beneath it.
            let Some(cf) = folder
                .item_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            for location in &folder.locations {
                match folder.collection_type {
                    Some(CollectionTypeOptions::tvshows) => {
                        self.plan_tv(location, cf, &naming, &mut out);
                    }
                    Some(CollectionTypeOptions::music) => {
                        self.plan_music(location, cf, &naming, &mut out);
                    }
                    None
                    | Some(
                        CollectionTypeOptions::movies
                        | CollectionTypeOptions::homevideos
                        | CollectionTypeOptions::musicvideos
                        | CollectionTypeOptions::mixed,
                    ) => self.plan_movies(location, cf, &naming, &mut out),
                    // books / photos / boxsets / … aren't scanned in v1.
                    Some(_) => {}
                }
            }
        }
        out
    }

    /// Builds a typed item row under collection folder `cf` with direct parent
    /// `parent`, returning its deterministic id and the row (the caller sets any
    /// type-specific fields and pushes it with its ancestor closure). `None` when
    /// the id cannot be derived.
    fn base_item(
        kind: BaseItemKind,
        cf: Uuid,
        parent: Uuid,
        name: String,
        path: &str,
        is_folder: bool,
    ) -> Option<(Uuid, BaseItemEntity)> {
        let id = item_type_lookup::derive_item_id(kind, path)?;
        let entity = BaseItemEntity {
            id: id.to_string(),
            type_: item_type_lookup::stored_type_name(kind)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name),
            path: Some(path.to_owned()),
            parent_id: Some(parent.to_string()),
            top_parent_id: Some(cf.to_string()),
            is_folder,
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        };
        Some((id, entity))
    }

    /// Flat video library: every video file (recursing per-title folders) becomes a
    /// `Movie` directly under the collection folder.
    fn plan_movies(&self, dir: &str, cf: Uuid, naming: &NamingOptions, out: &mut Vec<Planned>) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_movies(&entry.path, cf, naming, out);
                continue;
            }
            if !video_resolver::is_video_file(&entry.path, naming) {
                continue;
            }
            let (name, year) = video_resolver::resolve_file(Some(&entry.path), naming, None)
                .map_or_else(|| (entry.name.clone(), None), |info| (info.name, info.year));
            let Some((id, mut entity)) =
                Self::base_item(BaseItemKind::Movie, cf, cf, name, &entry.path, false)
            else {
                continue;
            };
            entity.is_movie = true;
            entity.media_type = Some("Video".to_owned());
            entity.production_year = year.map(i64::from);
            out.push(Planned {
                id,
                entity,
                ancestors: vec![cf],
            });
        }
    }

    /// TV library: each top-level folder is a `Series`; its `Season NN` subfolders
    /// are `Season`s and the videos beneath them `Episode`s.
    fn plan_tv(&self, location: &str, cf: Uuid, naming: &NamingOptions, out: &mut Vec<Planned>) {
        for entry in self.file_system.get_file_system_entries(location) {
            if entry.type_ != FileSystemEntryType::Directory {
                continue; // loose files directly under a tvshows root are skipped in v1
            }
            let info = series_resolver::resolve(naming, &entry.path);
            let name = info.name.unwrap_or_else(|| entry.name.clone());
            let Some((series_id, mut series)) =
                Self::base_item(BaseItemKind::Series, cf, cf, name, &entry.path, true)
            else {
                continue;
            };
            series.production_year = info.year.map(i64::from);
            // The series' presentation key groups its seasons/episodes: the
            // `/Shows/{id}/{Seasons,Episodes}` queries filter on
            // `SeriesPresentationUniqueKey`, and `series_presentation_key` falls
            // back to this. Use the series id so children can match it.
            series.presentation_unique_key = Some(series_id.to_string());
            out.push(Planned {
                id: series_id,
                entity: series,
                ancestors: vec![cf],
            });
            self.plan_series(&entry.path, cf, series_id, naming, out);
        }
    }

    /// Plans one series folder: `Season NN` subfolders → a `Season` plus its
    /// episodes; a video directly in the series folder → an `Episode` with no
    /// season parent.
    fn plan_series(
        &self,
        series_dir: &str,
        cf: Uuid,
        series_id: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        // Episodes not under a `Season NN` folder (a flat series folder, or loose
        // videos in a non-season subfolder). Grouped into *virtual* seasons below
        // by their filename-detected season number, so a show without season
        // folders still gets the Series→Season→Episode hierarchy the clients
        // navigate (Series→Seasons→Episodes) — without seasons, a show renders
        // with no episodes.
        let mut loose: Vec<String> = Vec::new();
        for entry in self.file_system.get_file_system_entries(series_dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                let season = season_path_parser::parse(&entry.path, Some(series_dir), true, true);
                if season.season_number.is_some() || season.is_season_folder {
                    let num = season.season_number;
                    let name = num.map_or_else(|| entry.name.clone(), season_display_name);
                    let Some((season_id, mut e)) = Self::base_item(
                        BaseItemKind::Season,
                        cf,
                        series_id,
                        name,
                        &entry.path,
                        true,
                    ) else {
                        continue;
                    };
                    e.index_number = num.map(i64::from);
                    e.series_id = Some(series_id.to_string());
                    e.series_presentation_unique_key = Some(series_id.to_string());
                    out.push(Planned {
                        id: season_id,
                        entity: e,
                        ancestors: vec![cf, series_id],
                    });
                    self.plan_episodes(
                        &entry.path,
                        cf,
                        series_id,
                        Some((season_id, num)),
                        naming,
                        out,
                    );
                } else {
                    // A non-season subfolder (extras, etc.): collect its videos as
                    // loose episodes (grouped into virtual seasons below).
                    self.collect_videos(&entry.path, naming, &mut loose);
                }
            } else if video_resolver::is_video_file(&entry.path, naming) {
                loose.push(entry.path);
            }
        }
        Self::plan_loose_episodes(&loose, cf, series_id, series_dir, naming, out);
    }

    /// Collects every video file under `dir` (recursively) into `out_paths`.
    fn collect_videos(&self, dir: &str, naming: &NamingOptions, out_paths: &mut Vec<String>) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.collect_videos(&entry.path, naming, out_paths);
            } else if video_resolver::is_video_file(&entry.path, naming) {
                out_paths.push(entry.path);
            }
        }
    }

    /// Groups season-folder-less episodes into **virtual** `Season`s by their
    /// filename-detected season number, emitting one `Season` per distinct number
    /// and each episode parented to it. A file with no detectable season number
    /// falls into a single "Season Unknown" grouping.
    fn plan_loose_episodes(
        paths: &[String],
        cf: Uuid,
        series_id: Uuid,
        series_dir: &str,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        use std::collections::BTreeMap;
        let resolved: Vec<(&String, Option<i32>)> = paths
            .iter()
            .map(|p| {
                let num = EpisodeResolver::new(naming)
                    .resolve_simple(p, false)
                    .and_then(|i| i.season_number);
                (p, num)
            })
            .collect();

        // One virtual season per distinct number (BTreeMap → deterministic order).
        let mut season_ids: BTreeMap<Option<i32>, Uuid> = BTreeMap::new();
        for &(_, num) in &resolved {
            if season_ids.contains_key(&num) {
                continue;
            }
            let name = num.map_or_else(|| "Season Unknown".to_owned(), season_display_name);
            // A flat series has no season folder, so derive a stable id from a
            // synthetic path (unique per series+season) and leave the season's own
            // path unset (it is a virtual grouping, not an on-disk folder).
            let synthetic = format!("{series_dir}/#virtual-season-{}", num.unwrap_or(-1));
            let Some((season_id, mut e)) =
                Self::base_item(BaseItemKind::Season, cf, series_id, name, &synthetic, true)
            else {
                continue;
            };
            e.path = None;
            e.index_number = num.map(i64::from);
            e.series_id = Some(series_id.to_string());
            e.series_presentation_unique_key = Some(series_id.to_string());
            out.push(Planned {
                id: season_id,
                entity: e,
                ancestors: vec![cf, series_id],
            });
            season_ids.insert(num, season_id);
        }

        for (path, num) in resolved {
            let season = season_ids.get(&num).map(|&sid| (sid, num));
            Self::emit_episode(path, cf, series_id, season, naming, out);
        }
    }

    /// Plans every video under `dir` (recursively) as an `Episode`. `season` is the
    /// `(season_id, season_number)` when the files live in a season folder.
    fn plan_episodes(
        &self,
        dir: &str,
        cf: Uuid,
        series_id: Uuid,
        season: Option<(Uuid, Option<i32>)>,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_episodes(&entry.path, cf, series_id, season, naming, out);
            } else if video_resolver::is_video_file(&entry.path, naming) {
                Self::emit_episode(&entry.path, cf, series_id, season, naming, out);
            }
        }
    }

    /// Emits one `Episode` row, parented to its season (or the series when there is
    /// no season folder), carrying `IndexNumber`/`ParentIndexNumber` from the
    /// filename's episode/season numbers.
    fn emit_episode(
        path: &str,
        cf: Uuid,
        series_id: Uuid,
        season: Option<(Uuid, Option<i32>)>,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let info = EpisodeResolver::new(naming).resolve_simple(path, false);
        let (parent, ancestors) = match season {
            Some((season_id, _)) => (season_id, vec![cf, series_id, season_id]),
            None => (series_id, vec![cf, series_id]),
        };
        let Some((id, mut entity)) = Self::base_item(
            BaseItemKind::Episode,
            cf,
            parent,
            file_stem(path),
            path,
            false,
        ) else {
            return;
        };
        entity.media_type = Some("Video".to_owned());
        entity.index_number = info.as_ref().and_then(|i| i.episode_number).map(i64::from);
        entity.parent_index_number = season
            .and_then(|(_, n)| n)
            .or_else(|| info.as_ref().and_then(|i| i.season_number))
            .map(i64::from);
        entity.series_name = info.and_then(|i| i.series_name);
        // Link the episode to its series/season so the `/Shows/{id}/Episodes`
        // query (which filters on `SeriesPresentationUniqueKey`) returns it.
        entity.series_id = Some(series_id.to_string());
        entity.series_presentation_unique_key = Some(series_id.to_string());
        entity.season_id = season.map(|(sid, _)| sid.to_string());
        out.push(Planned {
            id,
            entity,
            ancestors,
        });
    }

    /// Music library: any folder that directly contains audio files is a
    /// `MusicAlbum` (its audio files become `Audio` tracks); subfolders are walked
    /// so an `Artist/Album/` layout still yields the albums.
    fn plan_music(&self, dir: &str, cf: Uuid, naming: &NamingOptions, out: &mut Vec<Planned>) {
        let entries = self.file_system.get_file_system_entries(dir);
        let audio: Vec<_> = entries
            .iter()
            .filter(|e| e.type_ != FileSystemEntryType::Directory && is_audio_file(&e.path, naming))
            .collect();
        if !audio.is_empty() {
            let album_name = file_stem(dir);
            if let Some((album_id, album)) = Self::base_item(
                BaseItemKind::MusicAlbum,
                cf,
                cf,
                album_name.clone(),
                dir,
                true,
            ) {
                out.push(Planned {
                    id: album_id,
                    entity: album,
                    ancestors: vec![cf],
                });
                for track in &audio {
                    let Some((id, mut entity)) = Self::base_item(
                        BaseItemKind::Audio,
                        cf,
                        album_id,
                        file_stem(&track.name),
                        &track.path,
                        false,
                    ) else {
                        continue;
                    };
                    entity.media_type = Some("Audio".to_owned());
                    entity.album = Some(album_name.clone());
                    out.push(Planned {
                        id,
                        entity,
                        ancestors: vec![cf, album_id],
                    });
                }
            }
        }
        for entry in &entries {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_music(&entry.path, cf, naming, out);
            }
        }
    }
}

/// The file name without its extension — a lightweight display name until real
/// metadata (titles, track names) lands in Part B.
fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| path.to_owned(), ToOwned::to_owned)
}

/// Maps a stored `BaseItems.Type` string to the local-image-provider item kind.
/// The on-disk filename stem a downloaded image of `image_type` is stored under.
fn image_type_file_stem(image_type: ImageType) -> &'static str {
    match image_type {
        ImageType::Backdrop => "backdrop",
        ImageType::Logo => "logo",
        ImageType::Thumb => "thumb",
        ImageType::Banner => "banner",
        // Primary + anything else lands on the primary poster name.
        _ => "primary",
    }
}

/// The display name for a season number — `0` is "Specials" (Jellyfin's
/// convention for extras/specials), every other number is "Season N".
/// Applies fetched TMDB [`TmdbDetails`] onto a row, filling only fields the scan
/// hasn't already set (so a probed runtime, a local NFO, or a prior scan wins).
/// Genres/studios are stored pipe-delimited, matching the `BaseItems` columns the
/// DTO service reads.
fn apply_details(entity: &mut BaseItemEntity, d: &TmdbDetails) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&d.overview);
    }
    if entity.tagline.is_none() {
        entity.tagline.clone_from(&d.tagline);
    }
    if entity.community_rating.is_none() {
        entity.community_rating = d.community_rating;
    }
    if entity.official_rating.is_none() {
        entity.official_rating.clone_from(&d.official_rating);
    }
    if entity.genres.as_deref().unwrap_or_default().is_empty() && !d.genres.is_empty() {
        entity.genres = Some(d.genres.join("|"));
    }
    if entity.studios.as_deref().unwrap_or_default().is_empty() && !d.studios.is_empty() {
        entity.studios = Some(d.studios.join("|"));
    }
    if entity.production_year.is_none() {
        entity.production_year = d.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = d.premiere_date.as_deref().and_then(parse_ymd);
    }
}

/// Parses a TMDB `YYYY-MM-DD` date to a UTC timestamp at midnight.
fn parse_ymd(s: &str) -> Option<chrono::DateTime<Utc>> {
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(chrono::DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

fn season_display_name(number: i32) -> String {
    if number == 0 {
        "Specials".to_owned()
    } else {
        format!("Season {number}")
    }
}

fn image_item_kind(type_: &str) -> ImageItemKind {
    match type_.rsplit('.').next().unwrap_or(type_) {
        "Movie" => ImageItemKind::Movie,
        "Series" => ImageItemKind::Series,
        "Season" => ImageItemKind::Season,
        "Episode" => ImageItemKind::Episode,
        "MusicVideo" => ImageItemKind::MusicVideo,
        "Video" => ImageItemKind::Video,
        _ => ImageItemKind::Generic,
    }
}

/// Discovers an item's local artwork (poster/backdrop/logo/…) by scanning its
/// folder with the local-image providers, returning rows ready to persist.
///
/// Dimensions are left `0` (unknown) here — the image files are decoded lazily on
/// serve, not during the scan. Episodes use the episode provider; everything the
/// generic provider supports uses it; unsupported kinds yield nothing.
fn discover_local_images(entity: &BaseItemEntity) -> Vec<ItemImageInfo> {
    let Some(path) = entity.path.as_deref() else {
        return Vec::new();
    };
    let kind = image_item_kind(&entity.type_);

    let containing = if entity.is_folder {
        Some(path.to_owned())
    } else {
        std::path::Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .map(ToOwned::to_owned)
    };

    let mut item = ImageItem::new(kind);
    item.name = entity.name.clone().unwrap_or_default();
    item.path = Some(path.to_owned());
    item.file_name_without_extension = Some(file_stem(path));
    item.containing_folder_path = containing;

    let dir = FsDirectoryService::new();
    let found = if kind == ImageItemKind::Episode {
        EpisodeLocalImageProvider::get_images(&item, &dir)
    } else if LocalImageProvider::supports(&item) {
        LocalImageProvider::get_images(&item, &dir)
    } else {
        Vec::new()
    };

    found
        .into_iter()
        .map(|local| ItemImageInfo {
            path: local.file_info.full_name,
            image_type: local.type_,
            date_modified: Utc::now(),
            width: 0,
            height: 0,
            blur_hash: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::LibraryScanner;
    use crate::file_system::HermitFileSystem;
    use crate::item_persistence_service::HermitItemPersistenceService;
    use crate::media_stream_repository::HermitMediaStreamRepository;
    use crate::virtual_folder_manager::HermitVirtualFolderManager;
    use async_trait::async_trait;
    use hermit_db::Database;
    use hermit_model::configuration::{LibraryOptions, MediaPathInfo};
    use hermit_model::dto::MediaSourceInfo;
    use hermit_model::entities::{CollectionTypeOptions, MediaStreamType};
    use hermit_model::entities_media::MediaStream;
    use hermit_traits::error::ServiceError;
    use hermit_traits::library::VirtualFolderManager;
    use hermit_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
    use std::sync::Arc;

    /// A fake encoder whose probe returns a fixed 3s h264+aac source — exercises
    /// the scan's probe→persist path without a real ffmpeg.
    struct FakeProbe;

    #[async_trait]
    impl MediaEncoder for FakeProbe {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "ffprobe".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Ok(MediaSourceInfo {
                run_time_ticks: Some(30_000_000),
                size: Some(51753),
                media_streams: vec![
                    MediaStream {
                        index: 0,
                        stream_type: MediaStreamType::Video,
                        codec: Some("h264".to_owned()),
                        width: Some(640),
                        height: Some(480),
                        ..Default::default()
                    },
                    MediaStream {
                        index: 1,
                        stream_type: MediaStreamType::Audio,
                        codec: Some("aac".to_owned()),
                        channels: Some(2),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<hermit_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            input_file.to_owned()
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            String::new()
        }
        async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn scan_probes_media_and_persists_streams_and_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(HermitItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            HermitVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();

        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(HermitFileSystem::new()), persistence)
                .with_probe(
                    Arc::new(FakeProbe),
                    Arc::new(HermitMediaStreamRepository::new(db.clone())),
                );
        scanner.scan_all().await.unwrap();

        // The probed duration + size land on the item row.
        let (ticks, size): (Option<i64>, Option<i64>) = sqlx::query_as(
            r#"SELECT "RunTimeTicks","Size" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(ticks, Some(30_000_000));
        assert_eq!(size, Some(51753));

        // Both probed streams are persisted (a video + an audio row).
        let streams: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "MediaStreamInfos""#)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(streams, 2);
        let video_codec: Option<String> =
            sqlx::query_scalar(r#"SELECT "Codec" FROM "MediaStreamInfos" WHERE "StreamType" = 1"#)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(video_codec.as_deref(), Some("h264"));
    }

    #[tokio::test]
    async fn scan_discovers_and_persists_local_artwork() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();
        // A filename-matched poster next to the movie → its Primary image.
        std::fs::write(media.join("The Matrix (1999)-poster.jpg"), b"jpg").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(HermitItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            HermitVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();

        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(HermitFileSystem::new()), persistence);
        scanner.scan_all().await.unwrap();

        // A Primary image row (ImageType = 0) pointing at the poster was persisted.
        let (count, path): (i64, Option<String>) = sqlx::query_as(
            r#"SELECT COUNT(*), MAX("Path") FROM "BaseItemImageInfos" WHERE "ImageType" = 0"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "one primary image should be persisted");
        assert!(
            path.as_deref().unwrap_or_default().ends_with("poster.jpg"),
            "primary image path points at the poster: {path:?}"
        );
    }

    #[tokio::test]
    async fn scan_creates_movie_rows_with_parent_and_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();
        // A nested per-title folder — still flattens under the library.
        std::fs::create_dir_all(media.join("Dune (2021)")).unwrap();
        std::fs::write(media.join("Dune (2021)/Dune (2021).mkv"), b"").unwrap();
        std::fs::write(media.join("poster.jpg"), b"").unwrap(); // non-video: ignored

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(HermitItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            HermitVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();

        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(HermitFileSystem::new()), persistence);
        assert_eq!(
            scanner.scan_all().await.unwrap(),
            2,
            "two movies (flat + nested), poster ignored"
        );

        let cf = vf.get_virtual_folders().await.unwrap()[0]
            .item_id
            .clone()
            .unwrap();
        // Both movies parent to the collection folder and carry an ancestor row.
        let movie_rows: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems"
               WHERE "Type" = 'MediaBrowser.Controller.Entities.Movies.Movie'
                 AND "ParentId" = ?1"#,
        )
        .bind(&cf)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(movie_rows, 2);
        let ancestor_rows: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ParentItemId" = ?1"#)
                .bind(&cf)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(ancestor_rows, 2);

        // Deterministic ids → re-scan upserts, does not duplicate.
        assert_eq!(scanner.scan_all().await.unwrap(), 2);
        let total: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(total, 2, "re-scan did not duplicate");
    }

    /// Creates a library of `ct` over `media`, scans it, and returns the DB handle
    /// plus the library's projected CollectionFolder id.
    async fn scan_one(
        ct: CollectionTypeOptions,
        name: &str,
        media: &std::path::Path,
    ) -> (Database, String) {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(HermitItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            HermitVirtualFolderManager::new(media.parent().unwrap().join(".views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            name,
            Some(ct),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        LibraryScanner::new(vf.clone(), Arc::new(HermitFileSystem::new()), persistence)
            .scan_all()
            .await
            .unwrap();
        let cf = vf.get_virtual_folders().await.unwrap()[0]
            .item_id
            .clone()
            .unwrap();
        (db, cf)
    }

    #[tokio::test]
    async fn scan_builds_tv_series_season_episode_hierarchy() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("tv");
        let season = media.join("Breaking Bad (2008)").join("Season 1");
        std::fs::create_dir_all(&season).unwrap();
        std::fs::write(season.join("Breaking Bad S01E01.mkv"), b"").unwrap();
        std::fs::write(season.join("Breaking Bad S01E02.mkv"), b"").unwrap();

        let (db, cf) = scan_one(CollectionTypeOptions::tvshows, "Shows", &media).await;

        // Series → parented to the collection folder.
        let series: (String, String) = sqlx::query_as(
            r#"SELECT "Id","ParentId" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Series'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(series.1, cf, "series parents to the collection folder");

        // Season → parented to the series, IndexNumber = 1.
        let season_row: (String, String, Option<i64>) = sqlx::query_as(
            r#"SELECT "Id","ParentId","IndexNumber" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Season'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(season_row.1, series.0, "season parents to the series");
        assert_eq!(season_row.2, Some(1));

        // Two episodes → parented to the season, with Index/ParentIndex numbers.
        let eps: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"SELECT "ParentId","IndexNumber","ParentIndexNumber" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Episode' ORDER BY "IndexNumber""#,
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(eps.len(), 2);
        assert!(
            eps.iter().all(|e| e.0 == season_row.0),
            "episodes parent to the season"
        );
        assert_eq!(eps[0].1, Some(1));
        assert_eq!(eps[1].1, Some(2));
        assert!(
            eps.iter().all(|e| e.2 == Some(1)),
            "ParentIndexNumber = season 1"
        );

        // Each episode's ancestor closure is depth 3 (cf, series, season).
        let anc: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "AncestorIds" a JOIN "BaseItems" b ON b."Id"=a."ItemId"
               WHERE b."Type"='MediaBrowser.Controller.Entities.TV.Episode'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(anc, 6, "2 episodes × 3 ancestors each");
    }

    #[tokio::test]
    async fn tv_episode_without_a_season_folder_gets_a_virtual_season() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("tv");
        // No "Season N" folder — the episode sits directly in the series folder.
        let series = media.join("The Office (US)");
        std::fs::create_dir_all(&series).unwrap();
        std::fs::write(series.join("The Office S02E05.mkv"), b"").unwrap();

        let (db, cf) = scan_one(CollectionTypeOptions::tvshows, "Shows", &media).await;

        let series_id: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Series'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        // A flat series folder still gets a Season: a *virtual* one derived from
        // the episode's filename season number, so the show renders its episodes.
        let season: (String, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT "Id","IndexNumber","Path","SeriesPresentationUniqueKey" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Season'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            season.1,
            Some(2),
            "virtual season carries the season number"
        );
        assert_eq!(season.2, None, "a virtual season has no on-disk path");
        assert_eq!(
            season.3.as_deref(),
            Some(series_id.as_str()),
            "season links to the series by presentation key"
        );

        // The episode parents to the virtual season and carries the linking keys.
        let ep: (
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            r#"SELECT "ParentId","ParentIndexNumber","SeasonId","SeriesId",
                          "SeriesPresentationUniqueKey"
                   FROM "BaseItems"
                   WHERE "Type"='MediaBrowser.Controller.Entities.TV.Episode'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(ep.0, season.0, "episode parents to the virtual season");
        assert_eq!(ep.1, Some(2));
        assert_eq!(ep.2.as_deref(), Some(season.0.as_str()), "SeasonId set");
        assert_eq!(ep.3.as_deref(), Some(series_id.as_str()), "SeriesId set");
        assert_eq!(
            ep.4.as_deref(),
            Some(series_id.as_str()),
            "episode links to the series by presentation key (drives the Episodes query)"
        );
        // Ancestor closure is depth 3 (cf, series, season).
        let anc: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ParentItemId" = ?1"#)
                .bind(&cf)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(
            anc, 3,
            "series + season + episode each have cf as an ancestor"
        );
    }

    #[tokio::test]
    async fn scan_builds_music_album_with_tracks() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("music");
        // Artist/Album layout — the album folder (with audio) is the MusicAlbum.
        let album = media.join("Pink Floyd").join("The Wall");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 In the Flesh.flac"), b"").unwrap();
        std::fs::write(album.join("02 The Thin Ice.flac"), b"").unwrap();

        let (db, cf) = scan_one(CollectionTypeOptions::music, "Music", &media).await;

        let album_row: (String, String, Option<String>) = sqlx::query_as(
            r#"SELECT "Id","ParentId","Name" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.Audio.MusicAlbum'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(album_row.1, cf, "album parents to the collection folder");
        assert_eq!(album_row.2.as_deref(), Some("The Wall"));

        let tracks: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT "ParentId","Album" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.Audio.Audio'"#,
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(
            tracks.iter().all(|t| t.0 == album_row.0),
            "tracks parent to the album"
        );
        assert!(tracks.iter().all(|t| t.1.as_deref() == Some("The Wall")));
    }
}
