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
//! the Series→Season→Episode hierarchy; `music` builds MusicAlbum→Audio. After
//! the walk, items whose files vanished from disk are pruned
//! ([`prune_deleted`](LibraryScanner::prune_deleted)), so deleted media stops
//! being served after the next scan.
//!
//! Two passes: a **synchronous plan** (walk + filename resolution — this is where
//! the `!Sync` [`NamingOptions`] lazy-regex cells live, so they never cross an
//! `.await`), then an **async persist**. The filesystem seam is synchronous, so
//! the whole walk fits the sync pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::{
    BaseItemEntity, ChapterEntity, MediaStreamInfoEntity, PeopleEntity,
};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::io::FileSystemEntryType;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_naming::audio::is_audio_file;
use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::{EpisodeResolver, season_path_parser, series_resolver};
use ferrofin_naming::video::video_resolver;
use ferrofin_providers::{
    EpisodeLocalImageProvider, FsDirectoryService, ImageItem, ImageItemKind, LocalImageProvider,
    RemoteImage, TmdbClient, TmdbDetails, TmdbKind,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::FileSystem;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, MediaStreamRepository};
use std::collections::HashMap;
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
    /// Series item id → the matched TVDB series details (when TVDB, the TV
    /// authority, resolved the series). Its `tvdb_id` lets episodes resolve, and
    /// its artwork is reused by the image pass instead of re-fetching.
    series_tvdb: std::collections::HashMap<String, ferrofin_providers::TvdbSeriesDetails>,
    /// Episode item id → its TVDB still URL, cached during the metadata pass so
    /// the image pass downloads it without a second episode fetch.
    episode_tvdb_still: std::collections::HashMap<String, String>,
    /// Item id → the external ids the metadata match yielded, so the image pass
    /// can key fanart off them (movies: `Tmdb`/`Imdb`) within the same scan.
    item_provider_ids: std::collections::HashMap<String, Vec<(String, String)>>,
}

/// What a remote metadata fetch yields for one item: the cast/crew to persist
/// and the external provider ids (`Tmdb`/`Imdb`/`Tvdb`) to write once the row
/// exists. Ids are persisted after `save_items` so id-dependent providers
/// (fanart) can key off them on later passes and re-scans.
#[derive(Default)]
struct RemoteMetadata {
    people: Vec<PeopleEntity>,
    provider_ids: Vec<(String, String)>,
}

impl RemoteMetadata {
    /// People only (no external ids to persist).
    fn just_people(people: Vec<PeopleEntity>) -> Self {
        Self {
            people,
            provider_ids: Vec::new(),
        }
    }
}

/// The image file's mtime as UTC, falling back to [`Utc::now`] when the stat
/// fails. Port of Jellyfin's `IFileSystem.GetLastWriteTimeUtc`, which stamps
/// `ItemImageInfo.DateModified`.
///
/// This MUST be the file's mtime, not the scan time: `date_modified` feeds both
/// the client-facing `ImageTags` (`md5(path + ticks)`) and the resized-image
/// cache key. A rescan of an unchanged file has to reproduce the same timestamp,
/// or every scan changes every poster URL (busting the clients' year-long
/// immutable image caches) and invalidates the entire server-side resize cache.
fn file_date_modified(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from)
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
        let stem = image_type_file_stem(image.image_type);
        // Reuse any on-disk file of this stem regardless of extension — it is
        // either this download from an earlier scan or a user upload (which
        // must win over a re-download).
        let dest = if let Some(existing) = existing_art_file(item_dir, stem) {
            existing
        } else {
            let dest = item_dir.join(format!("{stem}.jpg"));
            let Some(bytes) = tmdb.download(&image.url).await else {
                continue;
            };
            if let Err(err) =
                std::fs::create_dir_all(item_dir).and_then(|()| std::fs::write(&dest, &bytes))
            {
                tracing::warn!(%err, item = %item_id, "failed to write downloaded artwork");
                continue;
            }
            dest
        };
        infos.push(ItemImageInfo {
            path: dest.to_string_lossy().into_owned(),
            image_type: image.image_type,
            date_modified: file_date_modified(&dest),
            width: 0,
            height: 0,
            blur_hash: None,
        });
    }
    infos
}

/// The image file extensions the art-dir helpers recognize.
const ART_FILE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];

/// Finds an existing art file `stem.<ext>` in `dir` for any recognized image
/// extension, preferring the canonical `.jpg` first.
fn existing_art_file(dir: &Path, stem: &str) -> Option<PathBuf> {
    ART_FILE_EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|p| p.exists())
}

/// Parses an art-dir file back to its [`ImageType`] — the inverse of the
/// `image_file_stem` naming both the scan's downloads and the image-upload
/// endpoint write (`primary.jpg`, `backdrop1.jpg`, `logo.png`, …). `None` for
/// unrecognized stems/extensions.
fn parse_art_file_stem(path: &Path) -> Option<ImageType> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !ART_FILE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let base = stem.trim_end_matches(|c: char| c.is_ascii_digit());
    Some(match base {
        "primary" => ImageType::Primary,
        "art" => ImageType::Art,
        "backdrop" => ImageType::Backdrop,
        "banner" => ImageType::Banner,
        "logo" => ImageType::Logo,
        "thumb" => ImageType::Thumb,
        "disc" => ImageType::Disc,
        "box" => ImageType::Box,
        "screenshot" => ImageType::Screenshot,
        "menu" => ImageType::Menu,
        "boxrear" => ImageType::BoxRear,
        "profile" => ImageType::Profile,
        _ => return None,
    })
}

/// One item the plan pass resolved, ready to persist.
struct Planned {
    /// The item id (also `entity.id`, kept typed for `set_ancestors`).
    id: Uuid,
    entity: BaseItemEntity,
    /// The ancestor closure (`ParentId` chain up to the collection folder).
    ancestors: Vec<Uuid>,
}

/// Default scan-progress cadence: emit an `info!` every this-many items so
/// info-level volume stays O(items/N), not O(items). Overridable via the
/// `FERROFIN_SCAN_PROGRESS_EVERY` bootstrap knob.
const DEFAULT_SCAN_PROGRESS_EVERY: usize = 100;

/// Per-library done/total counters driving the `RefreshProgress` pushes.
struct LibraryProgress {
    /// Planned items per collection folder.
    totals: HashMap<Uuid, usize>,
    /// Items processed so far per collection folder.
    done: HashMap<Uuid, usize>,
}

impl LibraryProgress {
    /// Tallies each library's planned item count. An item's library is the
    /// first entry of its ancestor closure (always the collection folder).
    fn new(planned: &[Planned]) -> Self {
        let mut totals: HashMap<Uuid, usize> = HashMap::new();
        for item in planned {
            if let Some(&cf) = item.ancestors.first() {
                *totals.entry(cf).or_default() += 1;
            }
        }
        Self {
            totals,
            done: HashMap::new(),
        }
    }

    /// Counts `item` as processed. Returns the library id and its completion
    /// percentage when a progress push is due — at every `cadence` items
    /// within the library (`0` disables the cadence) and at the library's
    /// completion — or `None` between pushes.
    fn advance(&mut self, item: &Planned, cadence: usize) -> Option<(Uuid, f64)> {
        let cf = *item.ancestors.first()?;
        let done = self.done.entry(cf).or_default();
        *done += 1;
        let total = self.totals.get(&cf).copied().unwrap_or(0).max(1);
        let complete = *done >= total;
        let at_cadence = cadence > 0 && done.is_multiple_of(cadence);
        #[allow(clippy::cast_precision_loss)]
        (complete || at_cadence).then(|| (cf, (*done as f64 / total as f64) * 100.0))
    }
}

/// How many descendant images feed a library tile collage — upstream
/// `CollectionFolderImageProvider.GetItemsWithImages` samples 8.
const LIBRARY_COLLAGE_SOURCES: i32 = 8;

/// Walks configured libraries and persists their contents as item rows.
pub struct LibraryScanner {
    virtual_folders: Arc<dyn VirtualFolderManager>,
    file_system: Arc<dyn FileSystem>,
    persistence: Arc<dyn ItemPersistenceService>,
    /// The per-database item-id derivation mode (see
    /// [`item_type_lookup::IdDerivation`]).
    id_derivation: item_type_lookup::IdDerivation,
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
    /// Optional OMDb client for the Rotten Tomatoes critic rating (keyed by the
    /// title's IMDb id from TMDB). Disabled when no OMDb API key is configured.
    omdb: Option<Arc<ferrofin_providers::OmdbClient>>,
    /// Optional TheTVDB client — the TV authority. When present, series/episode
    /// metadata + artwork come from TVDB (falling back to TMDB when TVDB has no
    /// match). Paired with [`metadata_dir`](Self::metadata_dir).
    tvdb: Option<Arc<ferrofin_providers::TvdbClient>>,
    /// Optional fanart.tv client — appends high-quality artwork for movies (by
    /// Tmdb/Imdb id) and series (by Tvdb id) on top of the primary provider's
    /// images. Keys off the ids persisted during this scan.
    fanart: Option<Arc<ferrofin_providers::FanartClient>>,
    /// Optional MusicBrainz client — resolves `MusicBrainz*` ids for music items
    /// in the post-scan enrichment pass. Paired with [`item_repository`](Self::item_repository).
    musicbrainz: Option<Arc<ferrofin_providers::MusicBrainzClient>>,
    /// Optional AudioDb client — artist bio/genre + album artwork by MusicBrainz
    /// id, in the post-scan music-enrichment pass.
    audiodb: Option<Arc<ferrofin_providers::AudioDbClient>>,
    /// Item repository for the post-scan music-enrichment pass (querying the
    /// MusicAlbum/MusicArtist rows + tracks it created). Absent → no music pass.
    item_repository: Option<Arc<dyn ItemRepository>>,
    /// The directory downloaded artwork is stored under (`{meta}/library/{id}`).
    metadata_dir: Option<PathBuf>,
    /// Where cast/crew credits are persisted (paired with [`tmdb`](Self::tmdb) so a
    /// movie/series with no overview gets its TMDB cast during the scan).
    people: Option<Arc<dyn ferrofin_traits::persistence::PeopleRepository>>,
    /// Where probed chapter markers are persisted (paired with the probe seam).
    chapters: Option<Arc<dyn ferrofin_traits::persistence::ChapterRepository>>,
    /// Optional image processor. When present, each discovered/downloaded artwork file
    /// gets its pixel dimensions and blurhash filled in during the scan (so the DTO layer
    /// can surface Width/Height and ImageBlurHashes). Absent in unit tests.
    image_processor: Option<Arc<dyn ferrofin_traits::drawing::ImageProcessor>>,
    /// Maps `OfficialRating` strings to the numeric parental score persisted
    /// in `InheritedParentalRatingValue` (what the Parental Rating sort and
    /// the max-parental-rating filters read). `None` in minimal unit tests.
    localization: Option<Arc<crate::localization_manager::LocalizationManager>>,
    /// Emit a scan-progress `info!` every N items (bootstrap knob
    /// `FERROFIN_SCAN_PROGRESS_EVERY`; default [`DEFAULT_SCAN_PROGRESS_EVERY`]). `0`
    /// disables progress logging. Keeps info-level volume at O(items/N) per
    /// RULES_LOGGING; per-item detail stays at `debug`.
    progress_every: usize,
    /// Optional domain-event seam. When present the scan publishes
    /// `LibraryChanged` (added/removed items, at scan end) and `RefreshProgress`
    /// (per-library %, at the progress cadence) — the composition root forwards
    /// both to client sessions over the WebSocket, which is how open clients
    /// refresh their views after a scan. Absent in unit tests that don't
    /// exercise events.
    events: Option<Arc<dyn ferrofin_traits::events::EventManager>>,
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
            id_derivation: item_type_lookup::IdDerivation::LegacyLowercase,
            media_encoder: None,
            media_streams: None,
            tmdb: None,
            omdb: None,
            tvdb: None,
            fanart: None,
            musicbrainz: None,
            audiodb: None,
            item_repository: None,
            metadata_dir: None,
            people: None,
            chapters: None,
            image_processor: None,
            localization: None,
            progress_every: DEFAULT_SCAN_PROGRESS_EVERY,
            events: None,
        }
    }

    /// Sets the per-database id-derivation mode. Called once by the
    /// composition root (unit tests keep the legacy default).
    #[must_use]
    pub fn with_id_derivation(mut self, mode: item_type_lookup::IdDerivation) -> Self {
        self.id_derivation = mode;
        self
    }

    /// Attaches the domain-event seam so scans publish `LibraryChanged` and
    /// `RefreshProgress` (forwarded to client sessions by the composition root).
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn ferrofin_traits::events::EventManager>) -> Self {
        self.events = Some(events);
        self
    }

    /// Overrides the scan-progress cadence (items per `info!`); `0` disables
    /// progress logging. Wired from the `FERROFIN_SCAN_PROGRESS_EVERY` bootstrap
    /// knob; unit tests keep the default.
    #[must_use]
    pub fn with_progress_every(mut self, every: usize) -> Self {
        self.progress_every = every;
        self
    }

    /// Stamps the numeric parental score derived from `OfficialRating` — what
    /// the Parental Rating sort and the max-rating filters read (upstream
    /// `BaseItem.OnMetadataChanged` → `GetParentalRatingScore`).
    fn apply_parental_rating_score(&self, entity: &mut BaseItemEntity) {
        if let Some(localization) = &self.localization
            && let Some(rating) = entity.official_rating.as_deref()
            && let Some(score) = localization.get_rating_score(rating, None)
        {
            entity.inherited_parental_rating_value = Some(i64::from(score.score));
            entity.inherited_parental_rating_sub_value = score.sub_score.map(i64::from);
        }
    }

    /// Attaches the localization manager, so scanned items carry the numeric
    /// parental-rating score derived from their `OfficialRating`.
    #[must_use]
    pub fn with_localization(
        mut self,
        localization: Arc<crate::localization_manager::LocalizationManager>,
    ) -> Self {
        self.localization = Some(localization);
        self
    }

    /// Attaches the image processor so discovered artwork gets its pixel dimensions and
    /// blurhash computed during the scan (feeds the DTO's Width/Height + ImageBlurHashes).
    /// Wired by the composition root; omitted in unit tests.
    #[must_use]
    pub fn with_image_processor(
        mut self,
        image_processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor>,
    ) -> Self {
        self.image_processor = Some(image_processor);
        self
    }

    /// Attaches the OMDb client so movies/series get their Rotten Tomatoes critic
    /// rating during the scan (keyed by the IMDb id TMDB returns). A disabled
    /// client (no API key) is a no-op.
    #[must_use]
    pub fn with_omdb(mut self, omdb: Arc<ferrofin_providers::OmdbClient>) -> Self {
        self.omdb = Some(omdb);
        self
    }

    /// Attaches the fanart.tv client so movies/series get fanart artwork
    /// (posters/logos/clear-art/backgrounds/…) appended during the scan, keyed
    /// off the Tmdb/Imdb/Tvdb ids resolved earlier in the same pass.
    #[must_use]
    pub fn with_fanart(mut self, fanart: Arc<ferrofin_providers::FanartClient>) -> Self {
        self.fanart = Some(fanart);
        self
    }

    /// Attaches the MusicBrainz client + the item repository the post-scan
    /// music-enrichment pass needs, so music items get their `MusicBrainz*` ids
    /// resolved (and, once wired, AudioDb/fanart artwork). Both are required for
    /// the pass to run.
    #[must_use]
    pub fn with_music(
        mut self,
        musicbrainz: Arc<ferrofin_providers::MusicBrainzClient>,
        item_repository: Arc<dyn ItemRepository>,
    ) -> Self {
        self.musicbrainz = Some(musicbrainz);
        self.item_repository = Some(item_repository);
        self
    }

    /// Attaches the item repository on its own (without the MusicBrainz music
    /// pass), which the post-scan deleted-item prune needs to list what a
    /// library currently stores. [`with_music`](Self::with_music) also sets it.
    #[must_use]
    pub fn with_items(mut self, item_repository: Arc<dyn ItemRepository>) -> Self {
        self.item_repository = Some(item_repository);
        self
    }

    /// Attaches the AudioDb client so music artists/albums get bio + artwork
    /// during the post-scan music pass (keyed off the resolved MusicBrainz ids).
    #[must_use]
    pub fn with_audiodb(mut self, audiodb: Arc<ferrofin_providers::AudioDbClient>) -> Self {
        self.audiodb = Some(audiodb);
        self
    }

    /// Attaches the TheTVDB client so TV series/episode metadata + artwork come
    /// from TVDB during the scan (the TV authority; TMDB remains the fallback
    /// when TVDB has no match). Paired with [`with_metadata`](Self::with_metadata)
    /// for the download directory.
    #[must_use]
    pub fn with_tvdb(mut self, tvdb: Arc<ferrofin_providers::TvdbClient>) -> Self {
        self.tvdb = Some(tvdb);
        self
    }

    /// Attaches the people repository so TMDB cast/crew credits are persisted
    /// during the scan. Paired with [`with_metadata`](Self::with_metadata).
    #[must_use]
    pub fn with_people(
        mut self,
        people: Arc<dyn ferrofin_traits::persistence::PeopleRepository>,
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

    /// Sets only the metadata art directory (no TMDB client) — the uploaded-art
    /// preservation and library-tile passes work without remote providers.
    #[must_use]
    pub fn with_metadata_dir(mut self, metadata_dir: PathBuf) -> Self {
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
        chapters: Arc<dyn ferrofin_traits::persistence::ChapterRepository>,
    ) -> Self {
        self.media_encoder = Some(media_encoder);
        self.media_streams = Some(media_streams);
        self.chapters = Some(chapters);
        self
    }

    /// Scans every configured library; returns the number of items created.
    ///
    /// # Errors
    /// See [`scan`](Self::scan).
    pub async fn scan_all(&self) -> Result<usize, ServiceError> {
        self.scan(None).await
    }

    /// Scans the configured libraries — restricted to the one whose
    /// CollectionFolder id is `only` (all of them when `None`) — and returns
    /// the number of items created. An `only` matching no library falls back
    /// to a full scan rather than silently scanning nothing (the id may come
    /// from a nested folder or a library removed mid-flight).
    ///
    /// Idempotent: item ids are deterministic
    /// ([`derive_item_id`](item_type_lookup::derive_item_id)), so re-scanning
    /// upserts rather than duplicates.
    ///
    /// # Errors
    /// Propagates the item-store failure if listing libraries, saving an item,
    /// or writing its ancestor closure fails.
    pub async fn scan(&self, only: Option<Uuid>) -> Result<usize, ServiceError> {
        let folders = self.scoped_folders(only).await?;
        let planned = self.plan(&folders); // sync: NamingOptions never crosses an await
        self.run_scan(&folders, planned, None).await
    }

    /// Scans only the items touched by the given `changed` filesystem paths —
    /// the path-scoped ingest behind the library monitor's watcher/webhook
    /// reports, so a single new file is resolved and persisted without
    /// re-walking (or re-enriching) the whole library.
    ///
    /// Per changed path, the plan of its containing library is filtered to the
    /// items **at or under** the path (the new/changed media) plus its
    /// **ancestor** items (series/season/album directories, so a file in a
    /// brand-new season folder brings its hierarchy with it — upserts, cheap
    /// for pre-existing rows). Deleted-item pruning runs restricted to the
    /// changed paths, so a reported deletion removes exactly the vanished
    /// rows. Paths outside every library are ignored.
    ///
    /// # Errors
    /// Propagates the item-store failure exactly as [`scan`](Self::scan) does.
    pub async fn scan_paths(&self, changed: &[String]) -> Result<usize, ServiceError> {
        let folders = self.virtual_folders.get_virtual_folders().await?;
        let affected: Vec<VirtualFolderInfo> = folders
            .into_iter()
            .filter(|f| {
                f.locations
                    .iter()
                    .any(|loc| changed.iter().any(|c| path_is_under(c, loc)))
            })
            .collect();
        if affected.is_empty() {
            tracing::debug!(
                paths = changed.len(),
                "changed paths match no library; nothing to scan"
            );
            return Ok(0);
        }
        let planned = self.plan(&affected);
        let scoped: Vec<Planned> = planned
            .into_iter()
            .filter(|p| {
                p.entity.path.as_deref().is_some_and(|item_path| {
                    changed
                        .iter()
                        .any(|c| path_is_under(item_path, c) || path_is_under(c, item_path))
                })
            })
            .collect();
        tracing::info!(
            changed = changed.len(),
            items = scoped.len(),
            "path-scoped scan planned"
        );
        self.run_scan(&affected, scoped, Some(changed)).await
    }

    /// The shared scan pipeline over an already-planned item set: probe +
    /// metadata + persistence per item, deleted-item pruning (restricted to
    /// `prune_scope` when given), the `LibraryChanged` push, and the music
    /// enrichment pass.
    async fn run_scan(
        &self,
        folders: &[VirtualFolderInfo],
        planned: Vec<Planned>,
        prune_scope: Option<&[String]>,
    ) -> Result<usize, ServiceError> {
        tracing::info!(
            items = planned.len(),
            folders = folders.len(),
            "library scan planned"
        );
        // Per-library progress accounting for the `RefreshProgress` pushes: how
        // many planned items each library has, and how many are done so far.
        let mut library_progress = LibraryProgress::new(&planned);
        // Item ids that did not exist before this scan (→ `ItemsAdded` in the
        // scan-end `LibraryChanged` push). Only tracked when events are wired.
        let mut items_added: Vec<&Planned> = Vec::new();
        // Carries matched series' TMDB ids + their episode-still URLs across the
        // scan so seasons/episodes resolve against the same series lookup.
        let mut art_cache = ArtworkCache::default();
        for (scanned, item) in planned.iter().enumerate() {
            tracing::debug!(item = %item.id, "scanning item");
            // Bounded progress cadence (RULES_LOGGING volume rule); `0` disables it.
            if scanned > 0 && self.progress_every > 0 && scanned.is_multiple_of(self.progress_every)
            {
                tracing::info!(scanned, total = planned.len(), "library scan progress");
            }
            if self.events.is_some() && !self.persistence.item_exists(item.id).await.unwrap_or(true)
            {
                items_added.push(item);
            }
            // A locked item's metadata, cast, and artwork are user-owned: run
            // no NFO or remote providers for it (Jellyfin skips all providers
            // when `IsLocked`), and leave its people/images untouched below.
            // The scan-upsert's `IsLocked` guard backstops the metadata
            // columns; file-derived facts (the probe) still update.
            let locked = self.is_item_locked(item.id).await;
            // Probe first so the item row is saved already carrying its duration and
            // size (the streams themselves are saved after, since they FK the row).
            let mut entity = item.entity.clone();
            let (streams, chapters, tag_provider_ids) = self.probe(&mut entity).await;
            // Local Kodi/XBMC NFO sidecar first — this is Jellyfin's default local
            // metadata reader, which runs before any remote fetch. It fills
            // genres/studios/tags/overview/ratings/year from `movie.nfo` /
            // `tvshow.nfo` / `<episode>.nfo` and yields the credited cast/crew.
            let mut people = if locked {
                Vec::new()
            } else {
                self.fetch_local_nfo(&mut entity).await
            };
            // Then enrich from TMDB (overview/tagline/genres/studios/ratings +
            // cast/crew) to fill any gaps the NFO left, so a bare file with no NFO
            // shows the same detail page Jellyfin does. Best-effort: failures don't
            // abort, and NFO-provided people take precedence.
            let remote = if locked {
                RemoteMetadata::default()
            } else {
                self.fetch_remote_metadata(&mut entity, &mut art_cache)
                    .await
            };
            if people.is_empty() {
                people = remote.people;
            }
            self.apply_parental_rating_score(&mut entity);
            // Scan-variant save: preserves `PrimaryVersionId` (merge-versions
            // links) and the stored `DateCreated` on rows that already exist —
            // this entity is rebuilt from disk and would otherwise reset both
            // on every scan.
            self.persistence
                .save_scanned_items(std::slice::from_ref(&entity))
                .await?;
            // Persist the external provider ids now the item row exists to FK
            // against: the remote match's (Tmdb/Imdb/Tvdb) plus the embedded
            // MusicBrainz ids read from the audio tags. These key the
            // id-dependent providers (fanart, AudioDb, MusicBrainz) and make
            // re-scans/cross-provider lookups stable. Best-effort: a write
            // failure is logged, not fatal.
            let all_provider_ids: Vec<(String, String)> = remote
                .provider_ids
                .iter()
                .cloned()
                .chain(tag_provider_ids)
                .collect();
            for (key, value) in &all_provider_ids {
                if let Err(err) = self.persistence.save_provider_id(item.id, key, value).await {
                    tracing::warn!(%err, item = %item.id, provider = key, "failed to persist provider id");
                }
            }
            // Keep the ids in the scan cache so the image pass can key fanart off
            // them (movies) without a DB round-trip.
            if !all_provider_ids.is_empty() {
                art_cache
                    .item_provider_ids
                    .insert(entity.id.clone(), all_provider_ids);
            }
            // Mirror the item's genres/studios/tags into ItemValues so the
            // genre/studio/tag *filters* (More Like This, genre browse) match.
            let item_values = item_values_of(&entity);
            if !item_values.is_empty() {
                self.persistence
                    .save_item_values(item.id, &item_values)
                    .await?;
            }
            self.persistence
                .set_ancestors(item.id, &item.ancestors)
                .await?;
            if !people.is_empty()
                && let Some(repo) = &self.people
            {
                match repo.update_people(item.id, &people).await {
                    Ok(written) => self.enrich_people(repo.as_ref(), written).await,
                    Err(err) => {
                        tracing::warn!(%err, item = %item.id, "failed to persist cast/crew");
                    }
                }
            }
            if let (false, Some(repo)) = (streams.is_empty(), &self.media_streams) {
                repo.save_media_streams(item.id, &streams).await?;
            }
            if let (false, Some(repo)) = (chapters.is_empty(), &self.chapters) {
                repo.save_chapters(item.id, &chapters).await?;
            }
            // Artwork — locked items skip the rewrite entirely: their image
            // rows are user-owned.
            if !locked {
                self.persist_artwork(item.id, &entity, &mut art_cache).await;
            }
            // Per-library refresh % for open dashboards (`RefreshProgress`),
            // at the same bounded cadence as the progress log plus each
            // library's completion.
            if let Some((cf, pct)) = library_progress.advance(item, self.progress_every) {
                self.publish_refresh_progress(cf, pct).await;
            }
        }
        // Drop rows whose files vanished since the last scan, so deleted media
        // stops being listed and served. Best-effort — a failure must not fail
        // the whole scan.
        let removed = self.prune_deleted(folders, &planned, prune_scope).await;
        // Announce what the scan changed (`LibraryChanged`) so open clients
        // refresh their library views without a manual reload.
        self.publish_library_changed(&items_added, &removed).await;
        // Library tile images: composite each library's Primary from its own
        // content (upstream CollectionFolderImageProvider), so the home
        // screen's "My Media" tiles carry artwork instead of the blue
        // placeholder. Best-effort — a failure must not fail the scan.
        if let Err(err) = self.refresh_library_images(folders).await {
            tracing::warn!(%err, "library image pass failed");
        }
        // Post-scan music enrichment: resolve MusicBrainz ids (and, once wired,
        // AudioDb/fanart artwork) for the MusicAlbum/MusicArtist rows created
        // above. Best-effort — a failure here must not fail the whole scan.
        if let Err(err) = self.enrich_music().await {
            tracing::warn!(%err, "music enrichment pass failed");
        }
        Ok(planned.len())
    }

    /// Resolves the folder set a scan covers: all libraries, or — when `only`
    /// names an existing library's CollectionFolder — just that one. An `only`
    /// matching no library falls back to a full scan rather than silently
    /// scanning nothing.
    async fn scoped_folders(
        &self,
        only: Option<Uuid>,
    ) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        let mut folders = self.virtual_folders.get_virtual_folders().await?;
        if let Some(only) = only {
            if folders
                .iter()
                .any(|f| collection_folder_id(f) == Some(only))
            {
                folders.retain(|f| collection_folder_id(f) == Some(only));
            } else {
                tracing::warn!(library = %only, "scoped scan matched no library; scanning all");
            }
        }
        Ok(folders)
    }

    /// Publishes one library's refresh percentage as a `RefreshProgress` event
    /// (the C# `RefreshProgressMessage` dictionary shape: string values).
    /// No-op without an event seam.
    async fn publish_refresh_progress(&self, library: Uuid, pct: f64) {
        let Some(events) = &self.events else {
            return;
        };
        let payload = serde_json::json!({
            "ItemId": library.to_string(),
            "Progress": format!("{pct:.2}"),
        })
        .to_string();
        let _ = events.publish("RefreshProgress", &payload).await;
    }

    /// Publishes the scan's net changes as a `LibraryChanged` event carrying a
    /// [`LibraryUpdateInfo`] — the payload Jellyfin's `LibraryChangedNotifier`
    /// pushes so clients refresh home rows and library views. No-op when no
    /// event seam is wired or nothing changed (an unchanged rescan is silent,
    /// matching Jellyfin, whose item events only fire on real changes).
    ///
    /// `ItemsUpdated` is deliberately left empty: Ferrofin re-saves every planned
    /// row on every scan, so "saved" is not "changed" — reporting them all
    /// would announce the entire library on each rescan.
    /// ponytail: add dirty tracking if per-item update pushes are ever needed.
    async fn publish_library_changed(&self, added: &[&Planned], removed: &[(Uuid, Vec<Uuid>)]) {
        let Some(events) = &self.events else {
            return;
        };
        let mut folders_added: Vec<Uuid> = added
            .iter()
            .filter_map(|p| p.ancestors.first().copied())
            .collect();
        folders_added.sort_unstable();
        folders_added.dedup();
        let folders_removed: Vec<Uuid> = removed
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(cf, _)| *cf)
            .collect();
        let mut collection_folders: Vec<Uuid> = folders_added
            .iter()
            .chain(&folders_removed)
            .copied()
            .collect();
        collection_folders.sort_unstable();
        collection_folders.dedup();

        let mut update = ferrofin_model::entities_media::LibraryUpdateInfo {
            folders_added_to: folders_added.iter().map(Uuid::to_string).collect(),
            folders_removed_from: folders_removed.iter().map(Uuid::to_string).collect(),
            items_added: added.iter().map(|p| p.id.to_string()).collect(),
            items_removed: removed
                .iter()
                .flat_map(|(_, ids)| ids.iter().map(Uuid::to_string))
                .collect(),
            collection_folders: collection_folders.iter().map(Uuid::to_string).collect(),
            ..ferrofin_model::entities_media::LibraryUpdateInfo::default()
        };
        update.is_empty = update.compute_is_empty();
        if update.is_empty {
            return;
        }
        if let Ok(payload) = serde_json::to_string(&update) {
            let _ = events.publish("LibraryChanged", &payload).await;
        }
    }

    /// Deletes items under the scanned libraries whose backing files no longer
    /// exist. The walk is the source of truth: any stored row (movie, series,
    /// season, episode, album, track) keyed to a scanned library that this
    /// scan did not re-plan is gone from disk — deleted, renamed, or moved —
    /// and is removed. FK cascades clear its streams/chapters/images/user data;
    /// by-name rows (genres, studios, artists, people) carry no `TopParentId`
    /// and are untouched. No-op without an item repository.
    ///
    /// Safety: a library whose location is unreachable (unmounted network
    /// share, detached drive) walks as empty, which is indistinguishable from
    /// "everything was deleted" — such libraries are skipped with a warning
    /// rather than mass-pruned. Collection types the planner doesn't scan
    /// (books/photos/…) are also skipped: their empty plan means "not
    /// managed", not "deleted".
    ///
    /// Returns the deleted item ids per library, feeding the scan-end
    /// `LibraryChanged` push.
    async fn prune_deleted(
        &self,
        folders: &[VirtualFolderInfo],
        planned: &[Planned],
        scope: Option<&[String]>,
    ) -> Vec<(Uuid, Vec<Uuid>)> {
        let mut removed = Vec::new();
        let Some(items) = &self.item_repository else {
            return removed;
        };
        // A path-scoped scan plans only the items under its changed paths, so
        // rows elsewhere in the library are absent from `planned` without being
        // deleted — only rows at/under a changed path may be considered stale.
        let in_scope = |row_path: Option<&str>| match scope {
            None => true,
            Some(paths) => row_path.is_some_and(|rp| paths.iter().any(|c| path_is_under(rp, c))),
        };
        let live: std::collections::HashSet<Uuid> = planned.iter().map(|p| p.id).collect();
        for folder in folders {
            let Some(cf) = collection_folder_id(folder) else {
                continue;
            };
            let planner_scans_type = matches!(
                folder.collection_type,
                None | Some(
                    CollectionTypeOptions::tvshows
                        | CollectionTypeOptions::music
                        | CollectionTypeOptions::movies
                        | CollectionTypeOptions::homevideos
                        | CollectionTypeOptions::musicvideos
                        | CollectionTypeOptions::mixed
                )
            );
            if !planner_scans_type {
                continue;
            }
            if let Some(gone) = folder
                .locations
                .iter()
                .find(|loc| !self.file_system.directory_exists(loc))
            {
                tracing::warn!(
                    library = %cf,
                    location = %gone,
                    "library location unreachable; skipping deleted-item prune"
                );
                continue;
            }
            let existing = match items
                .get_item_list(&InternalItemsQuery {
                    top_parent_ids: vec![cf],
                    recursive: true,
                    ..Default::default()
                })
                .await
            {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::warn!(%err, library = %cf, "failed to list items for deleted-item prune");
                    continue;
                }
            };
            let stale: Vec<Uuid> = existing
                .iter()
                .filter(|row| in_scope(row.path.as_deref()))
                .filter_map(|row| Uuid::parse_str(&row.id).ok())
                .filter(|id| !live.contains(id))
                .collect();
            if stale.is_empty() {
                continue;
            }
            match self.persistence.delete_items(&stale).await {
                Ok(()) => {
                    tracing::info!(library = %cf, removed = stale.len(), "pruned items deleted from disk");
                    removed.push((cf, stale));
                }
                Err(err) => {
                    tracing::warn!(%err, library = %cf, "failed to prune deleted items");
                }
            }
        }
        removed
    }

    /// The post-scan music-enrichment pass. Resolves each `MusicAlbum`'s and
    /// `MusicArtist`'s `MusicBrainz*` ids: preferring the ids embedded in the
    /// tracks' tags (persisted during the main loop), else querying MusicBrainz
    /// by name. Also aggregates album-artist/year from an album's tracks onto the
    /// album row. No-op unless both the item repository and MusicBrainz client
    /// are wired.
    async fn enrich_music(&self) -> Result<(), ServiceError> {
        let (Some(items), Some(mb)) = (&self.item_repository, &self.musicbrainz) else {
            return Ok(());
        };

        // Pre-fetch the embedded MusicBrainz ids by track (persisted from tags),
        // so each album/artist can adopt its tracks' ids without a per-item read.
        let by_provider = |key: &'static str| async move {
            items
                .get_items_with_provider_id(key)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect::<HashMap<Uuid, String>>()
        };
        let track_album = by_provider("MusicBrainzAlbum").await;
        let track_rg = by_provider("MusicBrainzReleaseGroup").await;
        let track_albumartist = by_provider("MusicBrainzAlbumArtist").await;

        // Map album-artist name → its embedded MusicBrainzAlbumArtist id, and
        // gather each album's aggregate from its tracks, in one pass over Audio.
        let audio = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Audio],
                recursive: true,
                ..Default::default()
            })
            .await?;
        let mut artist_mbid: HashMap<String, String> = HashMap::new();
        for track in &audio {
            let Ok(tid) = Uuid::parse_str(&track.id) else {
                continue;
            };
            if let Some(mbid) = track_albumartist.get(&tid) {
                for name in split_pipe(track.album_artists.as_deref()) {
                    artist_mbid.entry(name).or_insert_with(|| mbid.clone());
                }
            }
        }

        self.enrich_albums(
            items.as_ref(),
            mb.as_ref(),
            &track_album,
            &track_rg,
            &artist_mbid,
        )
        .await?;
        self.enrich_artists(items.as_ref(), mb.as_ref(), &artist_mbid)
            .await?;
        Ok(())
    }

    /// Downloads music artwork (AudioDb/fanart) into `{meta}/library/{id}` and
    /// persists the rows, deduped one-file-per-type. No-op without a download
    /// client + metadata dir. Best-effort.
    async fn persist_music_images(
        &self,
        item_id: Uuid,
        images: Vec<ferrofin_providers::TmdbImage>,
    ) {
        if images.is_empty() {
            return;
        }
        let (Some(tmdb), Some(meta_root)) = (&self.tmdb, &self.metadata_dir) else {
            return;
        };
        let id = item_id.to_string();
        let mut remote: Vec<RemoteImage> = Vec::new();
        append_fanart(&mut remote, images);
        let mut infos = download_images(
            tmdb,
            &meta_root.join(&id),
            &id,
            dedup_images_by_type(remote),
        )
        .await;
        self.fill_image_metadata(&mut infos).await;
        if !infos.is_empty()
            && let Err(err) = self.persistence.save_item_images(item_id, &infos).await
        {
            tracing::warn!(%err, item = %id, "failed to persist music artwork");
        }
    }

    /// Resolves and persists each `MusicAlbum`'s `MusicBrainzAlbum` +
    /// `MusicBrainzReleaseGroup` ids, aggregating album-artist/year from its
    /// tracks first (so a folder-named album gains its artist + release ids).
    async fn enrich_albums(
        &self,
        items: &dyn ItemRepository,
        mb: &ferrofin_providers::MusicBrainzClient,
        track_album: &HashMap<Uuid, String>,
        track_rg: &HashMap<Uuid, String>,
        artist_mbid: &HashMap<String, String>,
    ) -> Result<(), ServiceError> {
        let albums = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicAlbum],
                recursive: true,
                ..Default::default()
            })
            .await?;
        for album in albums {
            self.enrich_one_album(&album, items, mb, track_album, track_rg, artist_mbid)
                .await?;
        }
        Ok(())
    }

    /// Enriches one `MusicAlbum`: aggregate album-artist/year from its tracks,
    /// resolve + persist its MusicBrainz ids, then AudioDb metadata + AudioDb/
    /// fanart artwork.
    async fn enrich_one_album(
        &self,
        album: &BaseItemEntity,
        items: &dyn ItemRepository,
        mb: &ferrofin_providers::MusicBrainzClient,
        track_album: &HashMap<Uuid, String>,
        track_rg: &HashMap<Uuid, String>,
        artist_mbid: &HashMap<String, String>,
    ) -> Result<(), ServiceError> {
        let Ok(album_uuid) = Uuid::parse_str(&album.id) else {
            return Ok(());
        };
        {
            let tracks = items
                .get_item_list(&InternalItemsQuery {
                    parent_id: album_uuid,
                    include_item_types: vec![BaseItemKind::Audio],
                    ..Default::default()
                })
                .await?;

            // Aggregate album-artist + year from the tracks onto the album row.
            let mut updated = album.clone();
            let mut changed = false;
            if updated
                .album_artists
                .as_deref()
                .unwrap_or_default()
                .is_empty()
                && let Some(aa) = tracks
                    .iter()
                    .find_map(|t| t.album_artists.clone().filter(|s| !s.is_empty()))
            {
                updated.album_artists = Some(aa);
                changed = true;
            }
            if updated.production_year.is_none()
                && let Some(year) = tracks.iter().filter_map(|t| t.production_year).min()
            {
                updated.production_year = Some(year);
                changed = true;
            }
            if changed {
                self.persistence
                    .save_items(std::slice::from_ref(&updated))
                    .await?;
                let values = item_values_of(&updated);
                if !values.is_empty() {
                    self.persistence
                        .save_item_values(album_uuid, &values)
                        .await?;
                }
            }

            // The embedded ids from any track (they share an album's release).
            let embedded = ferrofin_providers::AlbumIds {
                release_id: tracks
                    .iter()
                    .find_map(|t| track_album.get(&parse_id(&t.id)?).cloned()),
                release_group_id: tracks
                    .iter()
                    .find_map(|t| track_rg.get(&parse_id(&t.id)?).cloned()),
            };
            let album_name = updated.name.clone().unwrap_or_default();
            let album_artist: Option<String> = updated
                .album_artists
                .as_deref()
                .and_then(|s| s.split('|').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let resolved = mb
                .resolve_album(&album_name, embedded, None, album_artist.as_deref())
                .await;
            if let Some(id) = &resolved.release_id {
                let _ = self
                    .persistence
                    .save_provider_id(album_uuid, "MusicBrainzAlbum", id)
                    .await;
            }
            if let Some(id) = &resolved.release_group_id {
                let _ = self
                    .persistence
                    .save_provider_id(album_uuid, "MusicBrainzReleaseGroup", id)
                    .await;
            }

            self.enrich_album_artwork(
                album_uuid,
                &mut updated,
                resolved.release_group_id.as_deref(),
                album_artist.as_deref(),
                artist_mbid,
            )
            .await?;
        }
        Ok(())
    }

    /// AudioDb album metadata (description/year) + AudioDb/fanart album artwork,
    /// keyed by the release-group id (fanart also needs the album-artist's mbid).
    async fn enrich_album_artwork(
        &self,
        album_uuid: Uuid,
        updated: &mut BaseItemEntity,
        release_group_id: Option<&str>,
        album_artist: Option<&str>,
        artist_mbid: &HashMap<String, String>,
    ) -> Result<(), ServiceError> {
        let mut changed = false;
        let mut images: Vec<ferrofin_providers::TmdbImage> = Vec::new();
        if let (Some(adb), Some(rg)) = (&self.audiodb, release_group_id)
            && let Some(a) = adb.album(rg).await
        {
            if updated.overview.is_none() && a.description.is_some() {
                updated.overview = a.description;
                changed = true;
            }
            if updated.production_year.is_none() && a.year.is_some() {
                updated.production_year = a.year.map(i64::from);
                changed = true;
            }
            images.extend(a.images);
        }
        if let (Some(fanart), Some(rg), Some(name)) = (&self.fanart, release_group_id, album_artist)
            && let Some(aa_mbid) = artist_mbid.get(name)
        {
            images.extend(fanart.album_images(aa_mbid, rg).await);
        }
        if changed {
            self.persistence
                .save_items(std::slice::from_ref(updated))
                .await?;
        }
        self.persist_music_images(album_uuid, images).await;
        Ok(())
    }

    /// Resolves and persists each `MusicArtist`'s `MusicBrainzArtist` id — the
    /// embedded album-artist id from its tracks, else a MusicBrainz name search —
    /// then its AudioDb bio/genre + AudioDb/fanart artwork.
    async fn enrich_artists(
        &self,
        items: &dyn ItemRepository,
        mb: &ferrofin_providers::MusicBrainzClient,
        artist_mbid: &HashMap<String, String>,
    ) -> Result<(), ServiceError> {
        let artists = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicArtist],
                recursive: true,
                ..Default::default()
            })
            .await?;
        for artist in artists {
            let Ok(artist_uuid) = Uuid::parse_str(&artist.id) else {
                continue;
            };
            let Some(name) = artist.name.as_deref().filter(|n| !n.is_empty()) else {
                continue;
            };
            let mbid = match artist_mbid.get(name) {
                Some(id) => Some(id.clone()),
                None => mb.search_artist(name).await,
            };
            let Some(id) = mbid else {
                continue;
            };
            let _ = self
                .persistence
                .save_provider_id(artist_uuid, "MusicBrainzArtist", &id)
                .await;

            // AudioDb bio/genre + AudioDb/fanart artist artwork, keyed by the
            // resolved MusicBrainz artist id.
            let mut updated = artist.clone();
            let mut changed = false;
            let mut images: Vec<ferrofin_providers::TmdbImage> = Vec::new();
            if let Some(adb) = &self.audiodb
                && let Some(a) = adb.artist(&id).await
            {
                if updated.overview.is_none() && a.biography.is_some() {
                    updated.overview = a.biography;
                    changed = true;
                }
                if updated.genres.as_deref().unwrap_or_default().is_empty()
                    && let Some(genre) = a.genre.filter(|g| !g.is_empty())
                {
                    updated.genres = Some(genre);
                    changed = true;
                }
                images.extend(a.images);
            }
            if let Some(fanart) = &self.fanart {
                images.extend(fanart.artist_images(&id).await);
            }
            if changed {
                self.persistence
                    .save_items(std::slice::from_ref(&updated))
                    .await?;
            }
            self.persist_music_images(artist_uuid, images).await;
        }
        Ok(())
    }

    /// Fills each image's pixel dimensions + blurhash via the image-processor seam, so the
    /// DTO layer can surface Width/Height and ImageBlurHashes. Dimensions are read once and
    /// reused for the blurhash. Best-effort per image; a no-op when no processor is wired
    /// (unit tests) or a decode fails (the image stays browsable with 0×0 / no hash).
    async fn fill_image_metadata(&self, images: &mut [ItemImageInfo]) {
        let Some(processor) = self.image_processor.as_ref() else {
            return;
        };
        for image in images.iter_mut() {
            let Ok(dims) = processor.get_image_dimensions(&image.path).await else {
                continue;
            };
            image.width = dims.width;
            image.height = dims.height;
            if let Ok(hash) = processor.get_image_blur_hash_sized(&image.path, dims).await {
                image.blur_hash = Some(hash);
            }
        }
    }

    /// Best-effort ffprobe of a leaf media item: enriches `entity` with the probed
    /// `run_time_ticks`/`size` and returns its media streams (ready to persist).
    ///
    /// Returns an empty vec — leaving the item unprobed but still browsable — when
    /// no encoder is wired, the item is a folder or non-media, it has no path, or
    /// the probe fails (missing ffmpeg, unreadable file). Probe failures are
    /// swallowed so one bad file never aborts a whole library scan.
    async fn probe(
        &self,
        entity: &mut BaseItemEntity,
    ) -> (
        Vec<MediaStreamInfoEntity>,
        Vec<ChapterEntity>,
        Vec<(String, String)>,
    ) {
        let empty = (Vec::new(), Vec::new(), Vec::new());
        let Some(encoder) = &self.media_encoder else {
            return empty;
        };
        let is_audio = entity.media_type.as_deref() == Some("Audio");
        let is_media = is_audio || entity.media_type.as_deref() == Some("Video");
        if entity.is_folder || !is_media {
            return empty;
        }
        let Some(path) = entity.path.clone() else {
            return empty;
        };
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(path),
                ..Default::default()
            },
            // Extract embedded chapter markers so they show on the playback
            // timeline (matching Jellyfin's `-show_chapters`).
            extract_chapters: true,
            media_is_audio: is_audio,
        };
        let probed = match encoder.get_media_info_full(&request).await {
            Ok(probed) => probed,
            Err(e) => {
                tracing::warn!(error = %e, path = ?entity.path, "media probe failed; item left unprobed");
                return empty;
            }
        };
        let source = &probed.media_source;
        entity.run_time_ticks = source.run_time_ticks.or(entity.run_time_ticks);
        entity.size = source.size.or(entity.size);
        // Embedded audio tags (album/artists/track/disc/year/genres + the
        // MusicBrainz ids) — the port of `AudioFileProber`. Fill-if-empty so an
        // NFO/prior scan wins; the ids are returned for persistence.
        let provider_ids = if is_audio {
            apply_audio_metadata(entity, &probed)
        } else {
            Vec::new()
        };
        let streams = source
            .media_streams
            .iter()
            .map(|s| stream_dto_to_entity(&entity.id, s))
            .collect();
        let chapters = source
            .chapters
            .iter()
            .enumerate()
            .map(|(index, c)| chapter_to_entity(&entity.id, index, c))
            .collect();
        (streams, chapters, provider_ids)
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
    /// Reads a local Kodi/XBMC `.nfo` sidecar for `entity` (if one exists) and
    /// merges its genres/studios/tags/overview/ratings/year onto the row, returning
    /// its credited people. This is Jellyfin's default local metadata reader — it
    /// runs before any remote fetch, so a library of bare files with NFO sidecars
    /// shows real detail pages, genres, studios and cast entirely offline.
    ///
    /// Best-effort: a missing/unreadable/malformed sidecar leaves the row untouched
    /// and returns no people, so one bad file never aborts the scan.
    async fn fetch_local_nfo(&self, entity: &mut BaseItemEntity) -> Vec<PeopleEntity> {
        use ferrofin_providers::xbmc::{
            self, base_parser::NoDirectoryService, config::NfoConfiguration, item::NfoItemKind,
        };
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        let kind = match short {
            "Movie" => NfoItemKind::Movie,
            "Series" => NfoItemKind::Series,
            "Season" => NfoItemKind::Season,
            "Episode" => NfoItemKind::Episode,
            _ => return Vec::new(),
        };
        let Some(path) = entity.path.as_deref() else {
            return Vec::new();
        };
        let mut xml = None;
        let mut nfo_path = String::new();
        for cand in nfo_candidates(path, entity.is_folder, kind) {
            if let Ok(contents) = tokio::fs::read_to_string(&cand).await {
                nfo_path = cand.to_string_lossy().into_owned();
                xml = Some(contents);
                break;
            }
        }
        let Some(xml) = xml else {
            return Vec::new();
        };
        let config = NfoConfiguration::default();
        let ext_ids = xbmc::StaticExternalIds::new(["Imdb", "Tmdb", "Tvdb"]);
        let ds = NoDirectoryService;
        let mut result = xbmc::new_result(kind);
        let parsed = match kind {
            NfoItemKind::Movie => {
                xbmc::fetch_movie(&mut result, &nfo_path, &xml, &config, &ext_ids, &ds)
            }
            NfoItemKind::Series => {
                xbmc::fetch_series(&mut result, &nfo_path, &xml, &config, &ext_ids, &ds)
            }
            NfoItemKind::Season => {
                xbmc::fetch_season(&mut result, &nfo_path, &xml, &config, &ext_ids, &ds)
            }
            NfoItemKind::Episode => {
                xbmc::fetch_episode(&mut result, &nfo_path, &xml, &config, &ext_ids, &ds)
            }
            _ => return Vec::new(),
        };
        if parsed.is_err() {
            return Vec::new();
        }
        apply_nfo(entity, &result.item);
        result
            .people
            .unwrap_or_default()
            .into_iter()
            .map(person_to_entity)
            .collect()
    }

    /// Enriches a movie/series row from TMDB (overview, tagline, genres, studios,
    /// community rating, US certification, premiere date) and returns its cast +
    /// key crew to persist. No-op when TMDB is unconfigured, the item already has
    /// an overview (a local NFO or a prior scan), or the item isn't a movie/series.
    /// Best-effort — a network/parse failure returns no people and leaves the row.
    async fn fetch_remote_metadata(
        &self,
        entity: &mut BaseItemEntity,
        cache: &mut ArtworkCache,
    ) -> RemoteMetadata {
        let short = entity
            .type_
            .rsplit('.')
            .next()
            .unwrap_or(&entity.type_)
            .to_owned();
        // TheTVDB is the TV authority: when configured, it drives series and
        // episode metadata (TMDB stays the fallback for a series TVDB can't
        // match). Movies are TMDB-only.
        if self.tvdb.is_some() && matches!(short.as_str(), "Series" | "Episode") {
            let result = self.fetch_tvdb_metadata(entity, &short, cache).await;
            // A TVDB hit (series cached, or episode text applied) is authoritative;
            // only fall through to TMDB for a series TVDB could not resolve.
            if short == "Episode" || cache.series_tvdb.contains_key(&entity.id) {
                return result;
            }
        }
        let Some(tmdb) = &self.tmdb else {
            return RemoteMetadata::default();
        };
        let kind = match short.as_str() {
            "Movie" => TmdbKind::Movie,
            "Series" => TmdbKind::Series,
            _ => return RemoteMetadata::default(),
        };
        // Fetch when the row still lacks core metadata OR still lacks a Rotten
        // Tomatoes rating (with OMDb enabled) — the latter backfills the RT score
        // for titles scanned before OMDb was configured. A fully-enriched title is
        // skipped, so re-scans stay cheap.
        let has_overview = entity.overview.as_deref().is_some_and(|o| !o.is_empty());
        let wants_rating =
            self.omdb.as_ref().is_some_and(|o| o.is_enabled()) && entity.critic_rating.is_none();
        if has_overview && !wants_rating {
            return RemoteMetadata::default();
        }
        let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
            return RemoteMetadata::default();
        };
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
        let Some(tmdb_id) = tmdb
            .search(kind, name, year)
            .await
            .into_iter()
            .next()
            .map(|h| h.tmdb_id)
        else {
            return RemoteMetadata::default();
        };
        let Some(details) = tmdb.details(kind, tmdb_id).await else {
            return RemoteMetadata::default();
        };
        apply_details(entity, &details);
        // Rotten Tomatoes critic rating via OMDb, keyed by the IMDb id.
        if wants_rating
            && let (Some(omdb), Some(imdb_id)) = (&self.omdb, details.imdb_id.as_deref())
            && let Some(rating) = omdb.critic_rating(imdb_id).await
        {
            entity.critic_rating = Some(f64::from(rating));
        }
        // The external ids to persist: the matched TMDB id, plus the IMDb id TMDB
        // carries (keys OMDb + fanart's IMDb fallback).
        let mut provider_ids = vec![("Tmdb".to_owned(), tmdb_id.to_string())];
        if let Some(imdb) = details.imdb_id.as_deref().filter(|s| !s.is_empty()) {
            provider_ids.push(("Imdb".to_owned(), imdb.to_owned()));
        }
        let people = details
            .people
            .iter()
            .map(|p| PeopleEntity {
                id: guid_to_db(Uuid::new_v4()),
                name: p.name.clone(),
                person_type: Some(p.person_type.clone()),
                role: p.role.clone(),
                primary_image_url: p.profile_url.clone(),
                provider_id: Some(p.tmdb_id),
            })
            .collect();
        RemoteMetadata {
            people,
            provider_ids,
        }
    }

    /// The TheTVDB metadata pass — the TV authority. For a **series** it searches
    /// by name/year, applies the matched series' fields, and caches the details
    /// (its `tvdb_id` lets episodes resolve, its artwork feeds the image pass).
    /// For an **episode** it resolves the episode by (season, number) against the
    /// cached series id and applies its name/overview/air date. Returns the cast
    /// to persist. Best-effort — a miss returns no people and leaves the row for
    /// the TMDB fallback.
    async fn fetch_tvdb_metadata(
        &self,
        entity: &mut BaseItemEntity,
        short: &str,
        cache: &mut ArtworkCache,
    ) -> RemoteMetadata {
        let Some(tvdb) = &self.tvdb else {
            return RemoteMetadata::default();
        };
        match short {
            "Series" => {
                let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                    return RemoteMetadata::default();
                };
                let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
                let Some(hit) = tvdb.search(name, year).await.into_iter().next() else {
                    return RemoteMetadata::default();
                };
                let Some(details) = tvdb.series_details(hit.tvdb_id, METADATA_COUNTRY).await else {
                    return RemoteMetadata::default();
                };
                apply_tvdb_series(entity, &details);
                let people = tvdb_people(&details.people);
                // Persist the Tvdb id + the cross-provider ids TVDB carries (Imdb
                // keys fanart's fallback; Tmdb links the two databases).
                let mut provider_ids = vec![("Tvdb".to_owned(), details.tvdb_id.to_string())];
                if let Some(imdb) = details.imdb_id.as_deref().filter(|s| !s.is_empty()) {
                    provider_ids.push(("Imdb".to_owned(), imdb.to_owned()));
                }
                if let Some(tmdb) = details.tmdb_id.as_deref().filter(|s| !s.is_empty()) {
                    provider_ids.push(("Tmdb".to_owned(), tmdb.to_owned()));
                }
                cache.series_tvdb.insert(entity.id.clone(), details);
                RemoteMetadata {
                    people,
                    provider_ids,
                }
            }
            "Episode" => {
                let (Some(series_id), Some(season), Some(number)) = (
                    entity.series_id.clone(),
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) else {
                    return RemoteMetadata::default();
                };
                // The parent series must have matched TVDB earlier this scan.
                let Some(tvdb_id) = cache.series_tvdb.get(&series_id).map(|d| d.tvdb_id) else {
                    return RemoteMetadata::default();
                };
                let Some(ep) = tvdb
                    .episode_by_number(
                        tvdb_id,
                        ferrofin_providers::tvdb::DEFAULT_SEASON_TYPE,
                        season,
                        number,
                    )
                    .await
                else {
                    return RemoteMetadata::default();
                };
                apply_tvdb_episode(entity, &ep);
                if let Some(url) = &ep.image_url {
                    cache
                        .episode_tvdb_still
                        .insert(entity.id.clone(), url.clone());
                }
                // TVDB's per-episode credits are only the guest cast and the
                // director/writers; the series regulars live on the series
                // record. Stock Jellyfin (TMDB) persists the season cast on
                // every episode too — that is what fills the Cast & Crew
                // section of an episode detail page — so merge the cached
                // series' actors in ahead of the episode-specific credits.
                let series_people = cache
                    .series_tvdb
                    .get(&series_id)
                    .map(|d| d.people.as_slice())
                    .unwrap_or_default();
                RemoteMetadata::just_people(merge_series_cast(
                    tvdb_people(&ep.people),
                    series_people,
                ))
            }
            _ => RemoteMetadata::default(),
        }
    }

    /// Enriches credited people: downloads each one's TMDB profile image as their
    /// `Primary` artwork, and fetches a biography (bio/birthday/deathday/birthplace)
    /// for each *newly-created* person. Best-effort and cached — images skip
    /// existing files, and bios only run for new people, so re-scans stay cheap.
    ///
    /// ponytail: runs serially — a large cast makes the first scan slower, but the
    /// per-file / new-only guards make re-scans cheap. Batch if first-scan latency
    /// on huge libraries becomes a problem.
    async fn enrich_people(
        &self,
        repo: &dyn ferrofin_traits::persistence::PeopleRepository,
        written: Vec<ferrofin_traits::persistence::WrittenPerson>,
    ) {
        let (Some(tmdb), Some(meta_root)) = (&self.tmdb, &self.metadata_dir) else {
            return;
        };
        for person in written {
            let id = person.id.to_string();
            if let Some(url) = person.image_url {
                let dir = meta_root.join(&id);
                let infos = download_images(
                    tmdb,
                    &dir,
                    &id,
                    vec![RemoteImage {
                        image_type: ImageType::Primary,
                        url,
                    }],
                )
                .await;
                if !infos.is_empty()
                    && let Err(err) = self.persistence.save_item_images(person.id, &infos).await
                {
                    tracing::warn!(%err, person = %id, "failed to persist person image");
                }
            }

            // Biography: only for people still missing one, and only when TMDB
            // actually has detail to store.
            if person.needs_details
                && let Some(tmdb_id) = person.provider_id
                && let Some(details) = tmdb.person_details(tmdb_id).await
            {
                let metadata = ferrofin_traits::persistence::PersonMetadata {
                    overview: details.biography,
                    premiere_date: details.birthday.as_deref().and_then(parse_ymd),
                    end_date: details.deathday.as_deref().and_then(parse_ymd),
                    birthplace: details.place_of_birth,
                };
                if let Err(err) = repo.set_person_metadata(person.id, metadata).await {
                    tracing::warn!(%err, person = %id, "failed to persist person biography");
                }
            }
        }
    }

    /// Discovers and persists the item's artwork: local files next to the
    /// media first (poster/backdrop/logo/…), then a TMDB fallback for
    /// movies/series with none — matching Jellyfin, which fetches remote
    /// artwork automatically. Files already in the item's metadata art dir
    /// (user uploads, previously downloaded art) fill any type discovery
    /// didn't produce, so an uploaded image survives every rescan.
    /// Best-effort: a failure must not abort the rest of the scan.
    async fn persist_artwork(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        art_cache: &mut ArtworkCache,
    ) {
        let mut images = discover_local_images(entity);
        if images.is_empty() {
            images = self.fetch_remote_images(entity, art_cache).await;
        }
        self.append_art_dir_images(entity, &mut images);
        self.fill_image_metadata(&mut images).await;
        if !images.is_empty()
            && let Err(err) = self.persistence.save_item_images(item_id, &images).await
        {
            tracing::warn!(%err, item = %item_id, "failed to persist discovered artwork");
        }
    }

    /// Composites a Primary image for every library whose tile is missing or
    /// stale, from up to [`LIBRARY_COLLAGE_SOURCES`] random descendants'
    /// artwork (Backdrop > Primary > Thumb, upstream's preference).
    ///
    /// Port of `CollectionFolderImageProvider`: upstream refreshes each
    /// `CollectionFolder`'s dynamic image on library validation, composing a
    /// 960×540 collage regenerated when older than 7 days — the numbers are
    /// upstream's (`BaseDynamicImageProvider`/`HasChangedByDate`). Without
    /// this, the home screen's "My Media" tiles render the icon-on-blue
    /// fallback forever.
    async fn refresh_library_images(
        &self,
        folders: &[VirtualFolderInfo],
    ) -> Result<(), ServiceError> {
        let (Some(items), Some(processor), Some(meta_root)) = (
            &self.item_repository,
            &self.image_processor,
            &self.metadata_dir,
        ) else {
            return Ok(());
        };
        for folder in folders {
            let Some(cf) = collection_folder_id(folder) else {
                continue;
            };
            let out_dir = meta_root.join(guid_to_db(cf));
            let out = out_dir.join("primary.png");
            // Regenerate only when missing or older than 7 days (upstream's
            // HasChangedByDate window).
            let fresh = std::fs::metadata(&out)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age < std::time::Duration::from_hours(7 * 24));
            if fresh {
                continue;
            }
            // Random content sample; over-fetch since not every row has art.
            let sample = InternalItemsQuery {
                ancestor_ids: vec![cf],
                recursive: true,
                is_folder: Some(false),
                is_virtual_item: Some(false),
                limit: Some(LIBRARY_COLLAGE_SOURCES * 3),
                order_by: vec![(
                    ferrofin_model::live_tv::ItemSortBy::Random,
                    ferrofin_model::dto::SortOrder::Descending,
                )],
                ..Default::default()
            };
            let mut inputs = Vec::new();
            for id in items.get_item_ids(&sample).await? {
                if inputs.len() >= usize::try_from(LIBRARY_COLLAGE_SOURCES).unwrap_or(8) {
                    break;
                }
                let infos = items.get_image_infos(id).await?;
                let best = infos
                    .iter()
                    .find(|i| i.image_type == ImageType::Backdrop)
                    .or_else(|| infos.iter().find(|i| i.image_type == ImageType::Primary))
                    .or_else(|| infos.iter().find(|i| i.image_type == ImageType::Thumb));
                if let Some(image) = best
                    && image.is_local_file()
                    && std::path::Path::new(&image.path).exists()
                {
                    inputs.push(image.path.clone());
                }
            }
            if inputs.is_empty() {
                continue; // an empty library keeps the icon tile
            }
            if let Err(err) = std::fs::create_dir_all(&out_dir) {
                tracing::warn!(%err, library = %cf, "failed to create the library art dir");
                continue;
            }
            let options = ferrofin_traits::options::ImageCollageOptions {
                input_paths: inputs,
                output_path: out.to_string_lossy().into_owned(),
                width: 960,
                height: 540,
            };
            if let Err(err) = processor
                .create_image_collage(&options, folder.name.as_deref())
                .await
            {
                tracing::warn!(%err, library = %cf, "failed to composite the library image");
                continue;
            }
            let info = ItemImageInfo {
                path: options.output_path.clone(),
                image_type: ImageType::Primary,
                date_modified: Utc::now(),
                width: options.width,
                height: options.height,
                blur_hash: None,
            };
            if let Err(err) = self.persistence.save_item_images(cf, &[info]).await {
                tracing::warn!(%err, library = %cf, "failed to persist the library image");
            }
        }
        Ok(())
    }

    /// Whether the stored row for `id` is locked (`IsLocked`, the metadata
    /// editor's "lock this item"). Absent repository (unit-test builds) or a
    /// missing/new row → unlocked.
    async fn is_item_locked(&self, id: Uuid) -> bool {
        let Some(repo) = &self.item_repository else {
            return false;
        };
        repo.retrieve_item(id)
            .await
            .ok()
            .flatten()
            .is_some_and(|row| row.is_locked)
    }

    /// Appends rows for art files already sitting in the item's metadata art
    /// dir (`{meta}/library/{id}` — user uploads and previously downloaded
    /// artwork) whose image type discovery did not produce, so an uploaded
    /// image of any type survives the scan's image rewrite. Types discovery
    /// did produce are left alone (media-adjacent files outrank the metadata
    /// dir, matching Jellyfin's local-image precedence).
    fn append_art_dir_images(&self, entity: &BaseItemEntity, images: &mut Vec<ItemImageInfo>) {
        let Some(meta_root) = &self.metadata_dir else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(meta_root.join(&entity.id)) else {
            return;
        };
        // Snapshot the types discovery produced up front, so several art-dir
        // files of one type (backdrop.jpg, backdrop1.jpg, …) all append.
        let discovered: std::collections::HashSet<ImageType> =
            images.iter().map(|i| i.image_type).collect();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(image_type) = parse_art_file_stem(&path) else {
                continue;
            };
            if discovered.contains(&image_type) {
                continue;
            }
            images.push(ItemImageInfo {
                path: path.to_string_lossy().into_owned(),
                image_type,
                date_modified: file_date_modified(&path),
                width: 0,
                height: 0,
                blur_hash: None,
            });
        }
    }

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
                let mut images = tmdb.images_for(TmdbKind::Movie, name, year).await;
                // fanart.tv supplements TMDB's poster/backdrop with the types it
                // lacks (logo/clear-art/disc/banner), keyed off the movie's
                // Tmdb/Imdb id persisted earlier this scan.
                if let Some(fanart) = &self.fanart
                    && let Some(id) = cache
                        .item_provider_ids
                        .get(&entity.id)
                        .and_then(|ids| fanart_movie_id(ids))
                {
                    append_fanart(&mut images, fanart.movie_images(&id).await);
                }
                download_images(tmdb, &item_dir, &entity.id, dedup_images_by_type(images)).await
            }
            "Series" => {
                // TVDB is the TV authority: when it matched this series during the
                // metadata pass, reuse its artwork (no second fetch); else fall
                // back to a TMDB series match. The Tvdb id (when present) also
                // keys fanart's series artwork.
                let tvdb_id = cache.series_tvdb.get(&entity.id).map(|d| d.tvdb_id);
                let mut images = if let Some(details) = cache.series_tvdb.get(&entity.id) {
                    details.download_images()
                } else {
                    let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                        return Vec::new();
                    };
                    let Some(matched) = tmdb.series_match(name, year).await else {
                        return Vec::new();
                    };
                    // Remember the TMDB id so this series' seasons/episodes resolve.
                    cache.series_tmdb.insert(entity.id.clone(), matched.tmdb_id);
                    matched.images
                };
                if let (Some(fanart), Some(tvdb_id)) = (&self.fanart, tvdb_id) {
                    append_fanart(
                        &mut images,
                        fanart.series_images(&tvdb_id.to_string()).await,
                    );
                }
                download_images(tmdb, &item_dir, &entity.id, dedup_images_by_type(images)).await
            }
            "Season" | "Episode" => {
                self.fetch_tv_still_images(entity, short, cache, tmdb, &item_dir)
                    .await
            }
            _ => Vec::new(),
        }
    }

    /// The season-poster / episode-still image pass, split out of
    /// [`fetch_remote_images`](Self::fetch_remote_images). A **season** fetches
    /// its poster + every episode still from TMDB in one request (caching the
    /// stills); an **episode** downloads the still cached earlier this scan
    /// (TVDB's, else TMDB's season-stills map).
    async fn fetch_tv_still_images(
        &self,
        entity: &BaseItemEntity,
        short: &str,
        cache: &mut ArtworkCache,
        tmdb: &TmdbClient,
        item_dir: &Path,
    ) -> Vec<ItemImageInfo> {
        if short == "Season" {
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
            return download_images(tmdb, item_dir, &entity.id, images).await;
        }
        // Episode: prefer the TVDB still cached during the metadata pass; else
        // fall back to the TMDB season-stills cache.
        let url = cache
            .episode_tvdb_still
            .get(&entity.id)
            .cloned()
            .or_else(|| {
                let (Some(series_id), Some(season_num), Some(ep_num)) = (
                    entity.series_id.as_deref(),
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) else {
                    return None;
                };
                cache
                    .season_stills
                    .get(&(series_id.to_owned(), season_num))
                    .and_then(|stills| stills.get(&ep_num))
                    .cloned()
            });
        let Some(url) = url else {
            return Vec::new();
        };
        let images = vec![RemoteImage {
            image_type: ImageType::Primary,
            url,
        }];
        download_images(tmdb, item_dir, &entity.id, images).await
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
            let Some(cf) = collection_folder_id(folder) else {
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
                    ) => self.plan_movies(location, location, cf, &naming, &mut out),
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
        &self,
        kind: BaseItemKind,
        cf: Uuid,
        parent: Uuid,
        name: String,
        path: &str,
        is_folder: bool,
    ) -> Option<(Uuid, BaseItemEntity)> {
        let id = item_type_lookup::derive_item_id_with(&self.id_derivation, kind, path)?;
        let sort_name = create_sort_name(&name);
        let entity = BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(kind)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name),
            sort_name: Some(sort_name),
            path: Some(path.to_owned()),
            parent_id: Some(guid_to_db(parent)),
            top_parent_id: Some(guid_to_db(cf)),
            is_folder,
            // "Date Added" is the FILE's creation time (upstream sets
            // `DateCreated = info.CreationTimeUtc` at resolve): scan wall-clock
            // made a first scan order the whole library by directory traversal.
            date_created: Some(file_date_created(path)),
            ..BaseItemEntity::default()
        };
        Some((id, entity))
    }

    /// Video library: every video file becomes a `Movie` directly under the collection
    /// folder (per-title folders are recursed into). `root` is the library location, used
    /// to name folder-based movies from their folder.
    fn plan_movies(
        &self,
        dir: &str,
        root: &str,
        cf: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_movies(&entry.path, root, cf, naming, out);
                continue;
            }
            if !video_resolver::is_video_file(&entry.path, naming) {
                continue;
            }
            let (clean_name, year) = video_resolver::resolve_file(Some(&entry.path), naming, None)
                .map_or_else(|| (entry.name.clone(), None), |info| (info.name, info.year));
            // Port of MovieResolver: a movie in its OWN folder is named from that folder
            // (`Name = Path.GetFileName(ContainingFolderPath)`, raw — year kept), while a flat
            // file in the library root keeps its clean_date_time-parsed name (year stripped).
            // ProductionYear is still populated either way.
            let name = if dir == root {
                clean_name
            } else {
                folder_name(dir).unwrap_or(clean_name)
            };
            let Some((id, mut entity)) =
                self.base_item(BaseItemKind::Movie, cf, cf, name, &entry.path, false)
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
            let series_name = name.clone();
            let Some((series_id, mut series)) =
                self.base_item(BaseItemKind::Series, cf, cf, name, &entry.path, true)
            else {
                continue;
            };
            series.production_year = info.year.map(i64::from);
            // The series' presentation key groups its seasons/episodes: the
            // `/Shows/{id}/{Seasons,Episodes}` queries filter on
            // `SeriesPresentationUniqueKey`, and `series_presentation_key` falls
            // back to this. Use the series id so children can match it.
            series.presentation_unique_key = Some(series_id.simple().to_string());
            out.push(Planned {
                id: series_id,
                entity: series,
                ancestors: vec![cf],
            });
            self.plan_series(&entry.path, cf, series_id, &series_name, naming, out);
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
        series_name: &str,
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
                    let Some((season_id, mut e)) = self.base_item(
                        BaseItemKind::Season,
                        cf,
                        series_id,
                        name.clone(),
                        &entry.path,
                        true,
                    ) else {
                        continue;
                    };
                    e.index_number = num.map(i64::from);
                    e.series_id = Some(guid_to_db(series_id));
                    e.series_name = Some(series_name.to_owned());
                    e.series_presentation_unique_key = Some(series_id.simple().to_string());
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
                        series_name,
                        Some(&name),
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
        self.plan_loose_episodes(&loose, cf, series_id, series_name, series_dir, naming, out);
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
    // The C# planner's parameter list, plus the scanner receiver.
    #[allow(clippy::too_many_arguments)]
    fn plan_loose_episodes(
        &self,
        paths: &[String],
        cf: Uuid,
        series_id: Uuid,
        series_name: &str,
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
                self.base_item(BaseItemKind::Season, cf, series_id, name, &synthetic, true)
            else {
                continue;
            };
            e.path = None;
            e.index_number = num.map(i64::from);
            e.series_id = Some(guid_to_db(series_id));
            e.series_name = Some(series_name.to_owned());
            e.series_presentation_unique_key = Some(series_id.simple().to_string());
            out.push(Planned {
                id: season_id,
                entity: e,
                ancestors: vec![cf, series_id],
            });
            season_ids.insert(num, season_id);
        }

        for (path, num) in resolved {
            let season = season_ids.get(&num).map(|&sid| (sid, num));
            let season_name = num.map_or_else(|| "Season Unknown".to_owned(), season_display_name);
            self.emit_episode(
                path,
                cf,
                series_id,
                season,
                series_name,
                Some(&season_name),
                naming,
                out,
            );
        }
    }

    /// Plans every video under `dir` (recursively) as an `Episode`. `season` is the
    /// `(season_id, season_number)` when the files live in a season folder.
    #[allow(clippy::too_many_arguments)]
    fn plan_episodes(
        &self,
        dir: &str,
        cf: Uuid,
        series_id: Uuid,
        season: Option<(Uuid, Option<i32>)>,
        series_name: &str,
        season_name: Option<&str>,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_episodes(
                    &entry.path,
                    cf,
                    series_id,
                    season,
                    series_name,
                    season_name,
                    naming,
                    out,
                );
            } else if video_resolver::is_video_file(&entry.path, naming) {
                self.emit_episode(
                    &entry.path,
                    cf,
                    series_id,
                    season,
                    series_name,
                    season_name,
                    naming,
                    out,
                );
            }
        }
    }

    /// Emits one `Episode` row, parented to its season (or the series when there is
    /// no season folder), carrying `IndexNumber`/`ParentIndexNumber` from the
    /// filename's episode/season numbers.
    #[allow(clippy::too_many_arguments)]
    fn emit_episode(
        &self,
        path: &str,
        cf: Uuid,
        series_id: Uuid,
        season: Option<(Uuid, Option<i32>)>,
        series_name: &str,
        season_name: Option<&str>,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let info = EpisodeResolver::new(naming).resolve_simple(path, false);
        let (parent, ancestors) = match season {
            Some((season_id, _)) => (season_id, vec![cf, series_id, season_id]),
            None => (series_id, vec![cf, series_id]),
        };
        let Some((id, mut entity)) = self.base_item(
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
        // The series/season display names come from the parent entities, never
        // the filename parse (upstream EpisodeResolver: `episode.SeriesName =
        // series.Name`). A filename-derived series name is release-group noise
        // ("Show.2009", "Show - 4x09 - Title") and fragments the parent line on
        // every episode card.
        entity.series_name = Some(series_name.to_owned());
        entity.season_name = season_name.map(str::to_owned);
        // Link the episode to its series/season so the `/Shows/{id}/Episodes`
        // query (which filters on `SeriesPresentationUniqueKey`) returns it.
        entity.series_id = Some(guid_to_db(series_id));
        entity.series_presentation_unique_key = Some(series_id.simple().to_string());
        entity.season_id = season.map(|(sid, _)| guid_to_db(sid));
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
            if let Some((album_id, album)) = self.base_item(
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
                    let Some((id, mut entity)) = self.base_item(
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
                    // A placeholder the probe's ALBUM tag replaces (see
                    // `apply_audio_metadata`); kept for tagless files.
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

/// Whether `path` equals `root` or lies underneath it (component-boundary
/// aware: `/media/tv2` is not under `/media/tv`).
fn path_is_under(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The file's creation time (falling back to modification time, then to now)
/// — what "Date Added" sorts by, as upstream's resolvers stamp it.
fn file_date_created(path: &str) -> chrono::DateTime<Utc> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
        .map_or_else(Utc::now, chrono::DateTime::<Utc>::from)
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
/// Resolves the candidate `.nfo` sidecar paths for an item, in Jellyfin's search
/// order. Movies/episodes (files) look next to the media (`<stem>.nfo`, then
/// `movie.nfo` in the folder); series/seasons (folders) look for `tvshow.nfo` /
/// `season.nfo` inside. `is_folder` disambiguates a single-folder movie.
fn nfo_candidates(
    path: &str,
    is_folder: bool,
    kind: ferrofin_providers::xbmc::item::NfoItemKind,
) -> Vec<PathBuf> {
    use ferrofin_providers::xbmc::item::NfoItemKind;
    let p = Path::new(path);
    match kind {
        NfoItemKind::Series => vec![p.join("tvshow.nfo")],
        NfoItemKind::Season => vec![p.join("season.nfo")],
        NfoItemKind::Movie if is_folder => vec![p.join("movie.nfo")],
        NfoItemKind::Movie => {
            let mut c = vec![p.with_extension("nfo")];
            if let Some(dir) = p.parent() {
                c.push(dir.join("movie.nfo"));
            }
            c
        }
        _ => vec![p.with_extension("nfo")], // Episode + any leaf kind
    }
}

/// Merges parsed NFO fields onto `entity`, filling only what the row still lacks
/// (mirrors [`apply_details`]). Pipe-joins the multi-valued genre/studio/tag sets
/// so [`item_values_of`] mirrors them into the browse/filter indexes.
fn apply_nfo(entity: &mut BaseItemEntity, n: &ferrofin_providers::xbmc::item::NfoBaseItem) {
    // The NFO `<title>` is authoritative for the display name: Jellyfin's local
    // metadata provider overwrites the resolver's folder/file-derived name with
    // it, so a `Movie 0001 (2020)/` folder resolves to the NFO's clean
    // `Movie 0001` (not the raw, year-bearing folder name). The derived sort name
    // follows — an explicit NFO `<sortname>` wins, else it is recomputed from the
    // new title (otherwise SortName keeps the stale folder-derived value).
    if let Some(title) = n.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        entity.name = Some(title.to_owned());
        entity.sort_name = n
            .sort_name
            .clone()
            .or_else(|| Some(create_sort_name(title)));
    }
    if entity.overview.is_none() {
        entity.overview.clone_from(&n.overview);
    }
    if entity.tagline.is_none() {
        entity.tagline.clone_from(&n.tagline);
    }
    if entity.official_rating.is_none() {
        entity.official_rating.clone_from(&n.official_rating);
    }
    if entity.custom_rating.is_none() {
        entity.custom_rating.clone_from(&n.custom_rating);
    }
    if entity.original_title.is_none() {
        entity.original_title.clone_from(&n.original_title);
    }
    if entity.sort_name.is_none() {
        entity.sort_name.clone_from(&n.sort_name);
    }
    if entity.community_rating.is_none() {
        entity.community_rating = n.community_rating.map(f64::from);
    }
    if entity.critic_rating.is_none() {
        entity.critic_rating = n.critic_rating.map(f64::from);
    }
    if entity.production_year.is_none() {
        entity.production_year = n.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = n.premiere_date;
    }
    if entity.end_date.is_none() {
        entity.end_date = n.end_date;
    }
    merge_multi_value(&mut entity.genres, &n.genres);
    merge_multi_value(&mut entity.studios, &n.studios);
    merge_multi_value(&mut entity.tags, &n.tags);
}

/// Maps an NFO-parsed [`PersonInfo`](ferrofin_providers::container_types::PersonInfo)
/// to a persistable [`PeopleEntity`]. NFO people carry no remote id/image, so those
/// are left empty; the person-type key is the Jellyfin `PersonType` name.
fn person_to_entity(p: ferrofin_providers::container_types::PersonInfo) -> PeopleEntity {
    PeopleEntity {
        id: guid_to_db(Uuid::new_v4()),
        name: p.name,
        person_type: Some(format!("{:?}", p.type_)),
        role: p.role,
        primary_image_url: None,
        provider_id: None,
    }
}

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
    merge_multi_value(&mut entity.genres, &d.genres);
    merge_multi_value(&mut entity.studios, &d.studios);
    if entity.production_year.is_none() {
        entity.production_year = d.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = d.premiere_date.as_deref().and_then(parse_ymd);
    }
}

/// The three-letter country code TVDB content ratings are resolved against
/// (USA fallback is built into [`TvdbClient::series_details`]). Jellyfin uses the
/// server's metadata country; Ferrofin fixes it to USA for now.
// ponytail: fixed to "usa" — thread the server metadata-country setting through
// if per-region ratings matter.
const METADATA_COUNTRY: &str = "usa";

/// Applies matched TheTVDB **series** fields to the row, filling only what is
/// still empty (a local NFO or prior scan wins), mirroring [`apply_details`].
fn apply_tvdb_series(entity: &mut BaseItemEntity, d: &ferrofin_providers::TvdbSeriesDetails) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&d.overview);
    }
    if entity.official_rating.is_none() {
        entity.official_rating.clone_from(&d.official_rating);
    }
    merge_multi_value(&mut entity.genres, &d.genres);
    merge_multi_value(&mut entity.studios, &d.studios);
    if entity.production_year.is_none() {
        entity.production_year = d.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = d.premiere_date.as_deref().and_then(parse_ymd);
    }
    if entity.end_date.is_none() {
        entity.end_date = d.end_date.as_deref().and_then(parse_ymd);
    }
}

/// Applies matched TheTVDB **episode** fields to the row (fill-if-empty for
/// everything except the title, where the provider outranks the resolver's
/// filename placeholder).
fn apply_tvdb_episode(entity: &mut BaseItemEntity, d: &ferrofin_providers::TvdbEpisodeDetails) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&d.overview);
    }
    // The episode's own title is authoritative over the resolver's
    // filename-derived placeholder (upstream MetadataService.MergeBaseItemData
    // runs with replaceData=true on a standard scan, so the provider title
    // replaces the stem the resolver stamped). An NFO `<title>` still wins:
    // `apply_nfo` ran first and changed the name away from the stem, which is
    // exactly what the placeholder check detects. The derived sort name follows
    // the new title, as `apply_nfo` does.
    let name_is_placeholder = match (entity.name.as_deref(), entity.path.as_deref()) {
        (Some(name), Some(path)) => name == file_stem(path),
        (None | Some(""), _) => true,
        _ => false,
    };
    if name_is_placeholder
        && let Some(title) = d.name.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        entity.name = Some(title.to_owned());
        entity.sort_name = Some(create_sort_name(title));
    }
    if entity.production_year.is_none() {
        entity.production_year = d.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = d.aired.as_deref().and_then(parse_ymd);
    }
}

/// Applies embedded audio-tag metadata to a music row (fill-if-empty, so an NFO
/// or prior scan wins), and returns the embedded MusicBrainz ids to persist.
/// The port of `AudioFileProber`'s tag→item mapping (multi-values pipe-joined,
/// matching Ferrofin's `Artists`/`AlbumArtists`/`Genres` column convention).
fn apply_audio_metadata(entity: &mut BaseItemEntity, info: &MediaInfo) -> Vec<(String, String)> {
    // The TITLE tag is authoritative for the track name (AudioFileProber:
    // `audio.Name = trackTitle` unconditionally, bar locked fields — the scan
    // loop already skips locked items). The resolver's file-stem name is a
    // placeholder ("03. Artist - Title"), never the display name.
    if let Some(title) = info
        .media_source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        entity.name = Some(title.to_owned());
        entity.sort_name = Some(create_sort_name(title));
    }
    // The ALBUM tag replaces the plan's folder-stem placeholder (upstream's
    // `audio.Album ??= trackAlbum` works on a null the resolver left; here the
    // placeholder marks "no real value yet" so tagless files keep the folder
    // name). An NFO/edited album — no longer equal to the folder stem — wins.
    let album_is_placeholder = match (entity.album.as_deref(), entity.path.as_deref()) {
        (Some(album), Some(path)) => std::path::Path::new(path)
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .is_some_and(|dir| album == file_stem(&dir)),
        (None, _) => true,
        _ => false,
    };
    if album_is_placeholder
        && let Some(album) = info
            .album
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        entity.album = Some(album.to_owned());
    }
    if entity.artists.as_deref().unwrap_or_default().is_empty() && !info.artists.is_empty() {
        entity.artists = Some(info.artists.join("|"));
    }
    if entity
        .album_artists
        .as_deref()
        .unwrap_or_default()
        .is_empty()
        && !info.album_artists.is_empty()
    {
        entity.album_artists = Some(info.album_artists.join("|"));
    }
    if entity.genres.as_deref().unwrap_or_default().is_empty() && !info.genres.is_empty() {
        entity.genres = Some(info.genres.join("|"));
    }
    if entity.index_number.is_none() {
        entity.index_number = info.index_number.map(i64::from);
    }
    if entity.parent_index_number.is_none() {
        entity.parent_index_number = info.parent_index_number.map(i64::from);
    }
    if entity.production_year.is_none() {
        entity.production_year = info.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = info.premiere_date;
    }
    info.provider_ids
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Unions a provider's multi-value names into a pipe-joined column, preserving
/// existing order and deduplicating case-insensitively.
///
/// Upstream runs the whole provider chain and merges each field's values;
/// Ferrofin's old fill-if-empty froze coverage at whichever provider answered
/// first — the reason the genre filter offered fewer genres than Jellyfin and
/// tags existed only for NFO'd items.
fn merge_multi_value(existing: &mut Option<String>, incoming: &[String]) {
    if incoming.is_empty() {
        return;
    }
    let mut values = split_pipe(existing.as_deref());
    let mut seen: std::collections::HashSet<String> =
        values.iter().map(|v| v.to_lowercase()).collect();
    for value in incoming {
        let value = value.trim();
        if !value.is_empty() && seen.insert(value.to_lowercase()) {
            values.push(value.to_owned());
        }
    }
    if !values.is_empty() {
        *existing = Some(values.join("|"));
    }
}

/// Splits a pipe-joined multi-value field (artists/album_artists) into trimmed,
/// non-empty names.
fn split_pipe(field: Option<&str>) -> Vec<String> {
    field
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses a stored hyphenated id string to a [`Uuid`], or `None`.
fn parse_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok()
}

/// The library's CollectionFolder id (`VirtualFolderInfo::item_id`, projected
/// by the virtual-folder manager), parsed; items hang beneath it.
fn collection_folder_id(folder: &VirtualFolderInfo) -> Option<Uuid> {
    folder.item_id.as_deref().and_then(parse_id)
}

/// The fanart movie id from the persisted provider ids: TMDb preferred (fanart
/// keys on it), else IMDb.
fn fanart_movie_id(ids: &[(String, String)]) -> Option<String> {
    ids.iter()
        .find(|(k, _)| k == "Tmdb")
        .or_else(|| ids.iter().find(|(k, _)| k == "Imdb"))
        .map(|(_, v)| v.clone())
}

/// Appends fanart rich images to a download list as type+URL [`RemoteImage`]s.
fn append_fanart(images: &mut Vec<RemoteImage>, fanart: Vec<ferrofin_providers::TmdbImage>) {
    images.extend(fanart.into_iter().map(|img| RemoteImage {
        image_type: img.image_type,
        url: img.url,
    }));
}

/// Keeps the first image of each type (the primary provider's, then fanart's
/// best per type after its sort), so the one-file-per-type downloader emits no
/// duplicate rows.
fn dedup_images_by_type(images: Vec<RemoteImage>) -> Vec<RemoteImage> {
    let mut seen = std::collections::HashSet::new();
    images
        .into_iter()
        .filter(|i| seen.insert(i.image_type))
        .collect()
}

/// Merges a series' regular cast into an episode's own credited people.
///
/// The series' `Actor`-typed people (the regulars, in TVDB billing order) come
/// first, followed by the episode's own credits (guest stars, director,
/// writers) — the order stock Jellyfin's TMDB episode credits produce. A
/// regular already credited on the episode itself is dropped in favour of the
/// episode's entry (its role is the more specific one).
fn merge_series_cast(
    episode_people: Vec<PeopleEntity>,
    series_people: &[ferrofin_providers::TvdbPerson],
) -> Vec<PeopleEntity> {
    let credited: std::collections::HashSet<String> = episode_people
        .iter()
        .map(|p| p.name.to_lowercase())
        .collect();
    let mut people: Vec<PeopleEntity> = tvdb_people(series_people)
        .into_iter()
        .filter(|p| {
            p.person_type.as_deref() == Some("Actor") && !credited.contains(&p.name.to_lowercase())
        })
        .collect();
    people.extend(episode_people);
    people
}

/// Maps TVDB credited people to persistable [`PeopleEntity`] rows. TVDB carries
/// no numeric person provider id (so `provider_id` is `None`, and the TMDB bio
/// enrichment skips them); their profile image URL is preserved.
fn tvdb_people(people: &[ferrofin_providers::TvdbPerson]) -> Vec<PeopleEntity> {
    people
        .iter()
        .map(|p| PeopleEntity {
            id: guid_to_db(Uuid::new_v4()),
            name: p.name.clone(),
            person_type: Some(p.person_type.clone()),
            role: p.role.clone(),
            primary_image_url: p.image_url.clone(),
            provider_id: None,
        })
        .collect()
}

/// Collects an item's genres/studios/tags as `(ItemValueType discriminant, value)`
/// pairs for the `ItemValues` filter tables (Genre = 2, Studios = 3, Tags = 4).
pub(crate) fn item_values_of(entity: &BaseItemEntity) -> Vec<(i32, String)> {
    let split = |field: Option<&str>| -> Vec<String> {
        field
            .unwrap_or_default()
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut out = Vec::new();
    // Artist (0) / AlbumArtist (1) materialize browsable MusicArtist items and
    // back the artist filters; genres (2), studios (3), tags (4) as before.
    out.extend(split(entity.artists.as_deref()).into_iter().map(|a| (0, a)));
    out.extend(
        split(entity.album_artists.as_deref())
            .into_iter()
            .map(|a| (1, a)),
    );
    out.extend(split(entity.genres.as_deref()).into_iter().map(|g| (2, g)));
    out.extend(split(entity.studios.as_deref()).into_iter().map(|s| (3, s)));
    out.extend(split(entity.tags.as_deref()).into_iter().map(|t| (4, t)));
    out
}

/// Maps a probed [`ChapterInfo`](ferrofin_model::entities_media::ChapterInfo) to a
/// persistable [`ChapterEntity`], numbered by its position in the file.
fn chapter_to_entity(
    item_id: &str,
    index: usize,
    chapter: &ferrofin_model::entities_media::ChapterInfo,
) -> ChapterEntity {
    ChapterEntity {
        item_id: item_id.to_owned(),
        chapter_index: i64::try_from(index).unwrap_or(i64::MAX),
        start_position_ticks: chapter.start_position_ticks,
        name: chapter.name.clone(),
        image_path: chapter.image_path.clone(),
        image_date_modified: chapter
            .image_path
            .as_ref()
            .map(|_| chapter.image_date_modified),
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

/// Port of C# `BaseItem.CreateSortName` + `ModifySortChunks`: lowercase the name, apply the
/// default `SortReplace`/`SortRemove` characters and strip a leading article, then left-pad each
/// run of digits to 10 so numbers sort naturally (e.g. `Movie 0001 (2020)` →
/// `movie 0000000001 (0000002020)`).
fn create_sort_name(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    for c in [',', '&', '-', '{', '}', '\''] {
        s = s.replace(c, ""); // default SortRemoveCharacters
    }
    for c in ['.', '+', '%'] {
        s = s.replace(c, " "); // default SortReplaceCharacters → space
    }
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = s.strip_prefix(article) {
            s = rest.to_owned();
            break;
        }
    }
    modify_sort_chunks(&s)
}

/// Left-pads each maximal run of ASCII digits in `name` to width 10 with `0`.
fn modify_sort_chunks(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                digits.push(chars.next().unwrap());
            }
            for _ in digits.len()..10 {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            out.push(c);
            chars.next();
        }
    }
    out
}

/// The final path segment (folder name) of a directory path, if any. Mirrors C#
/// `Path.GetFileName(ContainingFolderPath)` for naming folder-based movies.
fn folder_name(dir: &str) -> Option<String> {
    std::path::Path::new(dir)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
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
            date_modified: file_date_modified(Path::new(&local.file_info.full_name)),
            path: local.file_info.full_name,
            image_type: local.type_,
            width: 0,
            height: 0,
            blur_hash: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::LibraryScanner;
    use crate::file_system::FerrofinFileSystem;
    use crate::item_persistence_service::FerrofinItemPersistenceService;

    // date_modified must be the file's mtime (stable across rescans), never the
    // scan time: it feeds ImageTags and the resize-cache key, so a churning value
    // busts every client and server image cache on every scan.
    #[test]
    fn image_date_modified_is_file_mtime_and_stable() {
        let dir = std::env::temp_dir().join(format!("ferrofin-scan-dm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("poster.jpg");
        std::fs::write(&file, b"jpeg").unwrap();

        let first = super::file_date_modified(&file);
        let again = super::file_date_modified(&file);
        assert_eq!(first, again, "unchanged file must yield a stable timestamp");
        let mtime = chrono::DateTime::<chrono::Utc>::from(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
        );
        assert_eq!(first, mtime, "timestamp must be the file mtime");

        // A missing file falls back to "now" rather than erroring the scan.
        let missing = super::file_date_modified(&dir.join("nope.jpg"));
        assert!((chrono::Utc::now() - missing).num_seconds().abs() < 60);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Port of C# CreateSortName: lowercase, strip a leading article, zero-pad digit runs to 10.
    #[test]
    fn create_sort_name_matches_jellyfin() {
        assert_eq!(
            super::create_sort_name("Movie 0001 (2020)"),
            "movie 0000000001 (0000002020)"
        );
        assert_eq!(super::create_sort_name("The Matrix"), "matrix");
        assert_eq!(super::create_sort_name("Se7en"), "se0000000007en");
    }

    // NFO sidecar search order: files look next to the media then for movie.nfo;
    // folders look inside for tvshow.nfo/season.nfo; a single-folder movie for movie.nfo.
    #[test]
    fn nfo_candidates_match_jellyfin_search_order() {
        use ferrofin_providers::xbmc::item::NfoItemKind;
        assert_eq!(
            super::nfo_candidates("/m/Movie (2020)/Movie.mkv", false, NfoItemKind::Movie),
            vec![
                std::path::PathBuf::from("/m/Movie (2020)/Movie.nfo"),
                std::path::PathBuf::from("/m/Movie (2020)/movie.nfo"),
            ]
        );
        assert_eq!(
            super::nfo_candidates("/m/Movie (2020)", true, NfoItemKind::Movie),
            vec![std::path::PathBuf::from("/m/Movie (2020)/movie.nfo")]
        );
        assert_eq!(
            super::nfo_candidates("/tv/Series", true, NfoItemKind::Series),
            vec![std::path::PathBuf::from("/tv/Series/tvshow.nfo")]
        );
        assert_eq!(
            super::nfo_candidates("/tv/Series/Season 01", true, NfoItemKind::Season),
            vec![std::path::PathBuf::from("/tv/Series/Season 01/season.nfo")]
        );
        assert_eq!(
            super::nfo_candidates("/tv/Series/S01E01.mkv", false, NfoItemKind::Episode),
            vec![std::path::PathBuf::from("/tv/Series/S01E01.nfo")]
        );
    }

    // apply_nfo fills only empty fields (mirrors apply_details) and pipe-joins the sets.
    #[test]
    fn apply_nfo_fills_only_empty_fields() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_providers::xbmc::item::{NfoBaseItem, NfoItemKind};
        let mut n = NfoBaseItem::new(NfoItemKind::Movie);
        n.overview = Some("from nfo".into());
        n.production_year = Some(1999);
        n.genres = vec!["Action".into(), "Drama".into()];
        n.studios = vec!["ACME".into()];
        n.community_rating = Some(7.5);

        let mut e = BaseItemEntity {
            overview: Some("already set".into()),
            ..Default::default()
        };
        super::apply_nfo(&mut e, &n);
        assert_eq!(e.overview.as_deref(), Some("already set")); // not overwritten
        assert_eq!(e.production_year, Some(1999)); // filled
        assert_eq!(e.genres.as_deref(), Some("Action|Drama"));
        assert_eq!(e.studios.as_deref(), Some("ACME"));
        assert_eq!(e.community_rating, Some(7.5));
    }

    // The post-scan music pass resolves each album's + artist's MusicBrainz ids
    // from the embedded ids on its tracks (no network when they're all present),
    // and aggregates the album-artist onto the album. Seeds through the seams.
    #[tokio::test]
    async fn enrich_music_resolves_ids_from_embedded_track_tags() {
        use crate::item_persistence_service::FerrofinItemPersistenceService;
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
        use crate::test_support::test_db;
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let items: Arc<dyn ItemRepository> =
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup));

        let album_id = uuid::Uuid::new_v4();
        let track_id = uuid::Uuid::new_v4();
        let stored = |k| stored_type_name(k).unwrap().to_owned();
        persistence
            .save_items(&[
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(album_id),
                    type_: stored(BaseItemKind::MusicAlbum),
                    name: Some("Kind of Blue".into()),
                    ..Default::default()
                },
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(track_id),
                    type_: stored(BaseItemKind::Audio),
                    name: Some("So What".into()),
                    parent_id: Some(ferrofin_db::store::guid_to_db(album_id)),
                    album_artists: Some("Miles Davis".into()),
                    production_year: Some(1959),
                    ..Default::default()
                },
            ])
            .await
            .expect("seed");
        // Embedded MusicBrainz ids on the track, and the MusicArtist item.
        for (k, v) in [
            ("MusicBrainzAlbum", "rel-x"),
            ("MusicBrainzReleaseGroup", "rg-x"),
            ("MusicBrainzAlbumArtist", "aa-x"),
        ] {
            persistence.save_provider_id(track_id, k, v).await.unwrap();
        }
        persistence
            .save_item_values(track_id, &[(1, "Miles Davis".into())])
            .await
            .expect("materialize artist");

        // A minimal scanner with the music pass wired (MB client never hits the
        // network because every id is already embedded).
        let tmp = tempfile::tempdir().unwrap();
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_music(
                Arc::new(ferrofin_providers::MusicBrainzClient::new("", "test")),
                Arc::clone(&items),
            );

        scanner.enrich_music().await.expect("enrich");

        // The album adopted its tracks' release + release-group ids...
        let album_rel = items
            .get_items_with_provider_id("MusicBrainzAlbum")
            .await
            .unwrap();
        assert!(album_rel.contains(&(album_id, "rel-x".to_owned())));
        let album_rg = items
            .get_items_with_provider_id("MusicBrainzReleaseGroup")
            .await
            .unwrap();
        assert!(album_rg.contains(&(album_id, "rg-x".to_owned())));
        // ...and its album-artist aggregated from the track.
        let album = items.retrieve_item(album_id).await.unwrap().unwrap();
        assert_eq!(album.album_artists.as_deref(), Some("Miles Davis"));
        // The MusicArtist got the embedded album-artist mbid.
        let artist_ids = items
            .get_items_with_provider_id("MusicBrainzArtist")
            .await
            .unwrap();
        assert!(
            artist_ids.iter().any(|(_, v)| v == "aa-x"),
            "artist resolved to embedded mbid: {artist_ids:?}"
        );
    }

    // apply_audio_metadata fills empty music fields from probed tags (pipe-joined
    // multi-values) and returns the embedded MusicBrainz ids to persist.
    #[test]
    fn apply_audio_metadata_fills_music_fields_and_returns_mb_ids() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_model::media_info::MediaInfo;

        let mut info = MediaInfo {
            album: Some("Kind of Blue".into()),
            artists: vec!["Miles Davis".into()],
            album_artists: vec!["Miles Davis".into()],
            genres: vec!["Jazz".into(), "Modal".into()],
            index_number: Some(3),
            parent_index_number: Some(1),
            production_year: Some(1959),
            ..MediaInfo::default()
        };
        info.provider_ids
            .insert("MusicBrainzAlbum".into(), "album-mbid".into());
        info.provider_ids
            .insert("MusicBrainzArtist".into(), "artist-mbid".into());

        // Empty entity (a bare Audio row) → filled.
        let mut e = BaseItemEntity {
            media_type: Some("Audio".into()),
            ..Default::default()
        };
        let ids = super::apply_audio_metadata(&mut e, &info);
        assert_eq!(e.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(e.artists.as_deref(), Some("Miles Davis"));
        assert_eq!(e.album_artists.as_deref(), Some("Miles Davis"));
        assert_eq!(e.genres.as_deref(), Some("Jazz|Modal"));
        assert_eq!(e.index_number, Some(3));
        assert_eq!(e.parent_index_number, Some(1));
        assert_eq!(e.production_year, Some(1959));
        // MB ids returned for persistence.
        let mut keys: Vec<&str> = ids.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["MusicBrainzAlbum", "MusicBrainzArtist"]);

        // A field already set (e.g. an NFO year) is not overwritten.
        let mut pre = BaseItemEntity {
            production_year: Some(2000),
            album: Some("Existing".into()),
            ..Default::default()
        };
        super::apply_audio_metadata(&mut pre, &info);
        assert_eq!(pre.production_year, Some(2000));
        assert_eq!(pre.album.as_deref(), Some("Existing"));

        // The TITLE tag replaces the resolver's file-stem name, and the ALBUM
        // tag replaces the plan's folder-stem placeholder (the reported music
        // bug: every track/album displayed release-folder noise).
        info.media_source.name = Some("Scar Tissue".into());
        let mut tagged = BaseItemEntity {
            name: Some("03. Red Hot Chili Peppers - Scar Tissue".into()),
            album: Some("RHCP - Californication (1999) FLAC".into()),
            path: Some(
                "/music/RHCP - Californication (1999) FLAC/03. Red Hot Chili Peppers - Scar Tissue.flac"
                    .into(),
            ),
            ..Default::default()
        };
        super::apply_audio_metadata(&mut tagged, &info);
        assert_eq!(tagged.name.as_deref(), Some("Scar Tissue"));
        assert_eq!(tagged.album.as_deref(), Some("Kind of Blue"));
    }

    // fanart id selection prefers Tmdb over Imdb; dedup keeps the first image of
    // each type so the one-file-per-type downloader emits no duplicate rows.
    #[test]
    fn fanart_id_selection_and_type_dedup() {
        use ferrofin_model::entities::ImageType;

        // Tmdb preferred.
        let ids = vec![
            ("Imdb".to_owned(), "tt0137523".to_owned()),
            ("Tmdb".to_owned(), "550".to_owned()),
        ];
        assert_eq!(super::fanart_movie_id(&ids).as_deref(), Some("550"));
        // Imdb fallback when no Tmdb.
        let ids = vec![("Imdb".to_owned(), "tt1".to_owned())];
        assert_eq!(super::fanart_movie_id(&ids).as_deref(), Some("tt1"));
        // Neither → None.
        assert!(super::fanart_movie_id(&[("Tvdb".to_owned(), "1".to_owned())]).is_none());

        let img = |t, u: &str| super::RemoteImage {
            image_type: t,
            url: u.to_owned(),
        };
        // append_fanart converts fanart's rich images to type+URL and appends
        // them after the primary provider's; dedup then keeps the first per type.
        let mut images = vec![img(ImageType::Primary, "tmdb-poster")];
        super::append_fanart(
            &mut images,
            vec![
                ferrofin_providers::TmdbImage {
                    image_type: ImageType::Primary,
                    url: "fanart-poster".into(),
                    width: Some(1000),
                    height: None,
                    community_rating: None,
                    vote_count: None,
                    language: None,
                },
                ferrofin_providers::TmdbImage {
                    image_type: ImageType::Logo,
                    url: "fanart-logo".into(),
                    width: Some(800),
                    height: None,
                    community_rating: None,
                    vote_count: None,
                    language: None,
                },
            ],
        );
        let deduped = super::dedup_images_by_type(images);
        let urls: Vec<&str> = deduped.iter().map(|i| i.url.as_str()).collect();
        // TMDB poster keeps Primary; fanart contributes the Logo it lacks.
        assert_eq!(urls, vec!["tmdb-poster", "fanart-logo"]);
    }

    // apply_tvdb_series fills only empty fields and sets the end date for ended
    // series; TVDB people carry no numeric provider id.
    #[test]
    fn apply_tvdb_series_fills_empty_fields_and_end_date() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_providers::{TvdbPerson, TvdbSeriesDetails};

        let details = TvdbSeriesDetails {
            tvdb_id: 121_361,
            name: Some("Game of Thrones".into()),
            overview: Some("from tvdb".into()),
            official_rating: Some("TV-MA".into()),
            genres: vec!["Drama".into(), "Fantasy".into()],
            studios: vec!["HBO".into()],
            production_year: Some(2011),
            premiere_date: Some("2011-04-17".into()),
            end_date: Some("2019-05-19".into()),
            status: Some("Ended".into()),
            people: vec![TvdbPerson {
                name: "Emilia Clarke".into(),
                person_type: "Actor".into(),
                role: Some("Daenerys".into()),
                image_url: Some("https://artworks.thetvdb.com/e.jpg".into()),
            }],
            ..TvdbSeriesDetails::default()
        };
        let mut e = BaseItemEntity {
            overview: Some("already set".into()),
            ..Default::default()
        };
        super::apply_tvdb_series(&mut e, &details);
        assert_eq!(e.overview.as_deref(), Some("already set")); // kept
        assert_eq!(e.official_rating.as_deref(), Some("TV-MA")); // filled
        assert_eq!(e.genres.as_deref(), Some("Drama|Fantasy"));
        assert_eq!(e.studios.as_deref(), Some("HBO"));
        assert_eq!(e.production_year, Some(2011));
        assert!(e.premiere_date.is_some());
        assert!(e.end_date.is_some());

        let people = super::tvdb_people(&details.people);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, "Emilia Clarke");
        assert_eq!(people[0].person_type.as_deref(), Some("Actor"));
        assert!(people[0].provider_id.is_none());
        assert_eq!(
            people[0].primary_image_url.as_deref(),
            Some("https://artworks.thetvdb.com/e.jpg")
        );
    }

    // apply_tvdb_episode fills a blank name + air date but never overwrites an
    // existing (NFO/filename) title.
    #[test]
    fn apply_tvdb_episode_fills_blank_name_and_air_date() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_providers::TvdbEpisodeDetails;

        let ep = TvdbEpisodeDetails {
            name: Some("Winter Is Coming".into()),
            overview: Some("Ned is summoned.".into()),
            aired: Some("2011-04-17".into()),
            production_year: Some(2011),
            image_url: Some("https://artworks.thetvdb.com/s.jpg".into()),
            people: Vec::new(),
        };
        // Blank name → filled.
        let mut blank = BaseItemEntity::default();
        super::apply_tvdb_episode(&mut blank, &ep);
        assert_eq!(blank.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(blank.overview.as_deref(), Some("Ned is summoned."));
        assert!(blank.premiere_date.is_some());
        assert_eq!(blank.production_year, Some(2011));
        // The resolver's filename placeholder → replaced (the reported bug:
        // every episode displayed its file name because this guard used to be
        // "only if empty", which the placeholder made permanently false).
        let mut placeholder = BaseItemEntity {
            name: Some("GoT.S01E01.1080p.Bluray".into()),
            sort_name: Some("got.s01e01.1080p.bluray".into()),
            path: Some("/tv/GoT/Season 1/GoT.S01E01.1080p.Bluray.mkv".into()),
            ..Default::default()
        };
        super::apply_tvdb_episode(&mut placeholder, &ep);
        assert_eq!(placeholder.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(
            placeholder.sort_name.as_deref(),
            Some(super::create_sort_name("Winter Is Coming").as_str())
        );
        // A name that differs from the stem (an NFO <title>) → kept.
        let mut named = BaseItemEntity {
            name: Some("The Real Title".into()),
            path: Some("/tv/GoT/Season 1/GoT.S01E01.1080p.Bluray.mkv".into()),
            ..Default::default()
        };
        super::apply_tvdb_episode(&mut named, &ep);
        assert_eq!(named.name.as_deref(), Some("The Real Title"));
    }

    // merge_series_cast: the series regulars (actors only) lead, the episode's
    // own credits follow, and a regular already credited on the episode keeps
    // the episode's (more specific) entry.
    #[test]
    fn merge_series_cast_prepends_regulars_and_dedupes() {
        use ferrofin_providers::TvdbPerson;
        let ep_people = super::tvdb_people(&[
            TvdbPerson {
                name: "Guest Star".into(),
                person_type: "GuestStar".into(),
                role: Some("Villain".into()),
                image_url: None,
            },
            TvdbPerson {
                name: "Bill Burr".into(),
                person_type: "Actor".into(),
                role: Some("Frank (voice)".into()),
                image_url: None,
            },
        ]);
        let series_people = vec![
            TvdbPerson {
                name: "Bill Burr".into(),
                person_type: "Actor".into(),
                role: Some("Frank Murphy".into()),
                image_url: None,
            },
            TvdbPerson {
                name: "Laura Dern".into(),
                person_type: "Actor".into(),
                role: Some("Sue Murphy".into()),
                image_url: None,
            },
            TvdbPerson {
                name: "Series Writer".into(),
                person_type: "Writer".into(),
                role: None,
                image_url: None,
            },
        ];
        let merged = super::merge_series_cast(ep_people, &series_people);
        let names: Vec<&str> = merged.iter().map(|p| p.name.as_str()).collect();
        // Laura Dern (regular, not on the episode) leads; the episode's own
        // Bill Burr credit wins over the series one; the series' non-actor
        // (Writer) is not merged in.
        assert_eq!(names, vec!["Laura Dern", "Guest Star", "Bill Burr"]);
        let bill = merged.iter().find(|p| p.name == "Bill Burr").unwrap();
        assert_eq!(bill.role.as_deref(), Some("Frank (voice)"));
        // Empty series cache → the episode credits pass through untouched.
        let alone = super::merge_series_cast(super::tvdb_people(&series_people[..1]), &[]);
        assert_eq!(alone.len(), 1);
    }

    // fetch_tvdb_metadata short-circuits (no network) when it can't act: a series
    // with a blank name, and an episode whose parent series never matched TVDB.
    #[tokio::test]
    async fn fetch_tvdb_metadata_guards_short_circuit_without_network() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_tvdb(Arc::new(ferrofin_providers::TvdbClient::new()));
        let mut cache = super::ArtworkCache::default();

        // Series with no name → no search, no people.
        let mut nameless = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
            ..Default::default()
        };
        let result = scanner
            .fetch_tvdb_metadata(&mut nameless, "Series", &mut cache)
            .await;
        assert!(result.people.is_empty() && result.provider_ids.is_empty());
        assert!(cache.series_tvdb.is_empty());

        // Episode whose series isn't in the TVDB cache → skipped (no network).
        let mut orphan_ep = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
            series_id: Some(uuid::Uuid::new_v4().to_string()),
            parent_index_number: Some(1),
            index_number: Some(1),
            ..Default::default()
        };
        let result = scanner
            .fetch_tvdb_metadata(&mut orphan_ep, "Episode", &mut cache)
            .await;
        assert!(result.people.is_empty());
    }

    // PersonInfo → PeopleEntity: type key is the Jellyfin PersonType name; no remote id/image.
    #[test]
    fn person_to_entity_maps_type_name() {
        use ferrofin_model::data::PersonKind;
        use ferrofin_providers::container_types::PersonInfo;
        let p = PersonInfo {
            type_: PersonKind::Director,
            role: Some("Self".into()),
            ..PersonInfo::new("Jane Doe")
        };
        let e = super::person_to_entity(p);
        assert_eq!(e.name, "Jane Doe");
        assert_eq!(e.person_type.as_deref(), Some("Director"));
        assert_eq!(e.role.as_deref(), Some("Self"));
        assert!(e.provider_id.is_none());
        assert!(e.primary_image_url.is_none());
    }

    use crate::media_stream_repository::FerrofinMediaStreamRepository;
    use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
    use async_trait::async_trait;
    use ferrofin_db::Database;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType};
    use ferrofin_model::entities_media::MediaStream;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::library::VirtualFolderManager;
    use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
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
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
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
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_probe(
                    Arc::new(FakeProbe),
                    Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
                    Arc::new(crate::chapter_repository::FerrofinChapterRepository::new(
                        db.clone(),
                    )),
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

    /// Reads back the scanned movies' `(Name, SortName, ProductionYear)` rows,
    /// name-ordered — the one shared movie read-back, so each test doesn't add
    /// its own raw query (the ferrofin-db sql_boundary ratchet counts them).
    async fn movie_rows(db: &Database) -> Vec<(String, Option<String>, Option<i64>)> {
        sqlx::query_as(
            r#"SELECT "Name","SortName","ProductionYear" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie' ORDER BY "Name""#,
        )
        .fetch_all(db.pool())
        .await
        .unwrap()
    }

    // Port of MovieResolver: a movie in its own folder is named from the FOLDER (raw, year
    // kept); a flat file in the library root keeps its clean_date_time-parsed name (year
    // stripped). Both populate ProductionYear. Matches Jellyfin (verified against a live server).
    #[tokio::test]
    async fn folder_movie_keeps_folder_name_flat_file_is_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let folder = media.join("Movie 0001 (2020)"); // Radarr layout: Title (Year)/Title (Year).mkv
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap(); // flat file in root

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
        scanner.scan_all().await.unwrap();

        let names: Vec<(String, Option<i64>)> = movie_rows(&db)
            .await
            .into_iter()
            .map(|(name, _, year)| (name, year))
            .collect();

        assert!(
            names
                .iter()
                .any(|(n, y)| n == "Movie 0001 (2020)" && *y == Some(2020)),
            "folder movie should keep the folder name incl. year; got {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|(n, y)| n == "The Matrix" && *y == Some(1999)),
            "flat file should be cleaned (year stripped); got {names:?}"
        );
    }

    // A movie.nfo `<title>` is authoritative for the item name (Jellyfin's local
    // NFO provider overwrites the resolver's folder-derived name). A
    // `Movie 0001 (2020)/` folder with a clean `<title>Movie 0001</title>` must
    // surface as "Movie 0001" with the sort name recomputed to match — not the
    // raw folder name. Regression guard for the parity Name/SortName diff.
    #[tokio::test]
    async fn nfo_title_overrides_the_folder_derived_name() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let folder = media.join("Movie 0001 (2020)");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").unwrap();
        std::fs::write(
            folder.join("movie.nfo"),
            b"<movie><title>Movie 0001</title><year>2020</year></movie>",
        )
        .unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
        scanner.scan_all().await.unwrap();

        let (name, sort_name, year) = movie_rows(&db)
            .await
            .into_iter()
            .next()
            .expect("one scanned movie");

        assert_eq!(
            name, "Movie 0001",
            "NFO <title> must win over the folder name"
        );
        assert_eq!(
            sort_name.as_deref(),
            Some("movie 0000000001"),
            "sort name must be recomputed from the NFO title"
        );
        assert_eq!(year, Some(2020));
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
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
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

    // With an image processor wired, the scan fills each artwork's pixel dimensions and
    // blurhash (so the DTO layer can surface Width/Height + ImageBlurHashes).
    #[tokio::test]
    async fn scan_fills_image_dimensions_and_blurhash() {
        use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();
        // A real 40x30 JPEG poster next to the movie → its Primary image.
        let mut poster = image::RgbImage::new(40, 30);
        for (x, _y, px) in poster.enumerate_pixels_mut() {
            *px = if x < 20 {
                image::Rgb([180, 30, 30])
            } else {
                image::Rgb([30, 30, 180])
            };
        }
        poster
            .save(media.join("The Matrix (1999)-poster.jpg"))
            .unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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

        let image_processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
            ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
        );
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_image_processor(image_processor);
        scanner.scan_all().await.unwrap();

        // Dimensions come from the poster; the blurhash (a BLOB) is non-empty.
        let (w, h, blur_len): (i64, i64, Option<i64>) = sqlx::query_as(
            r#"SELECT "Width","Height",LENGTH("Blurhash") FROM "BaseItemImageInfos" WHERE "ImageType" = 0"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            (w, h),
            (40, 30),
            "primary image dims filled from the poster"
        );
        assert!(
            blur_len.unwrap_or(0) > 0,
            "blurhash should be computed and stored"
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
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
        assert_eq!(
            scanner.scan_all().await.unwrap(),
            2,
            "two movies (flat + nested), poster ignored"
        );

        // The DTO projects the id in display form; the stored form is canonical.
        let cf = ferrofin_db::store::guid_to_db(
            uuid::Uuid::parse_str(
                vf.get_virtual_folders().await.unwrap()[0]
                    .item_id
                    .as_deref()
                    .unwrap(),
            )
            .unwrap(),
        );
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
        assert_eq!(
            count_type_like(&db, "%Movies.Movie").await,
            2,
            "re-scan did not duplicate"
        );
    }

    // Deleting media from disk must remove it from the library on the next
    // scan — and an unreachable library location (unmounted share) must NOT be
    // treated as "everything deleted".
    #[tokio::test]
    async fn rescan_prunes_items_whose_files_were_deleted() {
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;
        use ferrofin_traits::persistence::ItemRepository;

        let tmp = tempfile::tempdir().unwrap();
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        std::fs::write(tv.join("Firefly/Season 01/Firefly S01E01.mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Dollhouse/Season 01")).unwrap();
        std::fs::write(tv.join("Dollhouse/Season 01/Dollhouse S01E01.mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "TV",
            Some(CollectionTypeOptions::tvshows),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: tv.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_items(items);

        scanner.scan_all().await.unwrap();
        assert_eq!(count_type_like(&db, "%TV.Series").await, 2);
        assert_eq!(count_type_like(&db, "%TV.Episode").await, 2);

        // Delete one whole series from disk → its series/season/episode rows go.
        std::fs::remove_dir_all(tv.join("Firefly")).unwrap();
        scanner.scan_all().await.unwrap();
        assert_eq!(
            count_type_like(&db, "%TV.Series").await,
            1,
            "deleted series pruned"
        );
        assert_eq!(count_type_like(&db, "%TV.Season").await, 1);
        assert_eq!(count_type_like(&db, "%TV.Episode").await, 1);

        // Unmounted-share guard: a missing library ROOT walks as empty but must
        // not be mistaken for mass deletion — the surviving series is retained.
        std::fs::remove_dir_all(&tv).unwrap();
        scanner.scan_all().await.unwrap();
        assert_eq!(
            count_type_like(&db, "%TV.Episode").await,
            1,
            "unreachable location skips the prune"
        );
    }

    // A scan must announce what changed: new items → a `LibraryChanged` event
    // with `ItemsAdded` (plus `RefreshProgress` completion), deletions →
    // `ItemsRemoved`, and an unchanged rescan → silence (no stale pushes).
    #[tokio::test]
    async fn scan_publishes_library_changed_and_refresh_progress() {
        use crate::event_manager::FerrofinEventManager;
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;
        use ferrofin_traits::persistence::ItemRepository;
        use std::sync::Mutex;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::write(movies.join("Dune (2021).mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: movies.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let events = FerrofinEventManager::new();
        let changes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let progress: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        for (name, sink) in [("LibraryChanged", &changes), ("RefreshProgress", &progress)] {
            let sink = Arc::clone(sink);
            events.subscribe(
                name,
                Arc::new(move |payload: &str| {
                    sink.lock().unwrap().push(payload.to_owned());
                    Ok(())
                }),
            );
        }
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_items(items)
                .with_events(Arc::new(events));

        // First scan: both movies are new → ItemsAdded, and the library's
        // completion RefreshProgress fires at 100%.
        scanner.scan_all().await.unwrap();
        let first: serde_json::Value = serde_json::from_str(&changes.lock().unwrap()[0]).unwrap();
        assert_eq!(first["ItemsAdded"].as_array().unwrap().len(), 2);
        assert_eq!(first["IsEmpty"], false);
        let last_progress: serde_json::Value =
            serde_json::from_str(progress.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(last_progress["Progress"], "100.00");

        // Unchanged rescan: nothing added or removed → no LibraryChanged push.
        scanner.scan_all().await.unwrap();
        assert_eq!(
            changes.lock().unwrap().len(),
            1,
            "unchanged rescan is silent"
        );

        // Delete a movie → the next scan announces the removal.
        std::fs::remove_file(movies.join("Dune (2021).mkv")).unwrap();
        scanner.scan_all().await.unwrap();
        let third: serde_json::Value =
            serde_json::from_str(changes.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(third["ItemsRemoved"].as_array().unwrap().len(), 1);
        assert!(third["ItemsAdded"].as_array().unwrap().is_empty());
    }

    // `POST /Items/{id}/Refresh` on one library must not scan the other three:
    // a scan scoped to a CollectionFolder id walks only that library, and an
    // unknown scope falls back to a full scan (never a silent no-op).
    #[tokio::test]
    async fn scoped_scan_only_walks_the_matching_library() {
        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        std::fs::write(tv.join("Firefly/Season 01/Firefly S01E01.mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, ct, media) in [
            ("Movies", CollectionTypeOptions::movies, &movies),
            ("TV", CollectionTypeOptions::tvshows, &tv),
        ] {
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
        }
        let tv_cf = vf
            .get_virtual_folders()
            .await
            .unwrap()
            .iter()
            .find(|f| f.name.as_deref() == Some("TV"))
            .and_then(super::collection_folder_id)
            .unwrap();

        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
        let created = scanner.scan(Some(tv_cf)).await.unwrap();
        assert!(created > 0, "the TV library itself must still be scanned");
        assert_eq!(
            count_type_like(&db, "%Movies.Movie").await,
            0,
            "scoped scan must not touch the movie library"
        );
        assert_eq!(
            count_type_like(&db, "%TV.Episode").await,
            1,
            "the scoped TV library was scanned"
        );

        // A scope matching no library scans everything.
        scanner
            .scan(Some(uuid::Uuid::from_u128(0xBAD)))
            .await
            .unwrap();
        assert_eq!(
            count_type_like(&db, "%Movies.Movie").await,
            1,
            "unknown scope falls back to a full scan"
        );
    }

    // The path-scoped ingest: a new file's report plans just that file, and a
    // deleted file's report prunes just its row — the rest of the library is
    // neither re-planned nor mass-pruned.
    #[tokio::test]
    async fn scan_paths_ingests_and_prunes_only_the_changed_paths() {
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;

        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::write(movies.join("Alien (1979).mkv"), b"").unwrap();
        std::fs::write(movies.join("Stalker (1979).mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: movies.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        let items = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_items(items);
        scanner.scan_all().await.unwrap();
        assert_eq!(count_type_like(&db, "%Movies.Movie").await, 2);

        // A new file: only it is planned and created.
        let new_path = movies.join("Solaris (1972).mkv");
        std::fs::write(&new_path, b"").unwrap();
        let created = scanner
            .scan_paths(&[new_path.to_string_lossy().into_owned()])
            .await
            .unwrap();
        assert_eq!(created, 1, "only the new file is planned");
        assert_eq!(count_type_like(&db, "%Movies.Movie").await, 3);

        // A deleted file: exactly its row is pruned.
        let gone = movies.join("Alien (1979).mkv");
        std::fs::remove_file(&gone).unwrap();
        let created = scanner
            .scan_paths(&[gone.to_string_lossy().into_owned()])
            .await
            .unwrap();
        assert_eq!(created, 0, "a deletion plans nothing");
        assert_eq!(count_type_like(&db, "%Movies.Movie").await, 2);

        // A path outside every library is ignored.
        assert_eq!(
            scanner
                .scan_paths(&["/nowhere/else.mkv".to_owned()])
                .await
                .unwrap(),
            0
        );
        assert_eq!(count_type_like(&db, "%Movies.Movie").await, 2);
    }

    // A file landing in a brand-new season folder brings its ancestor
    // hierarchy (series + season rows) with it, while sibling seasons and
    // episodes stay untouched by the plan.
    #[tokio::test]
    async fn scan_paths_creates_the_new_hierarchy_around_a_changed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tv = tmp.path().join("tv");
        std::fs::create_dir_all(tv.join("Firefly/Season 01")).unwrap();
        std::fs::write(tv.join("Firefly/Season 01/Firefly S01E01.mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "TV",
            Some(CollectionTypeOptions::tvshows),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: tv.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence);
        scanner.scan_all().await.unwrap();
        assert_eq!(count_type_like(&db, "%TV.Episode").await, 1);

        std::fs::create_dir_all(tv.join("Firefly/Season 02")).unwrap();
        let ep2 = tv.join("Firefly/Season 02/Firefly S02E01.mkv");
        std::fs::write(&ep2, b"").unwrap();
        let created = scanner
            .scan_paths(&[ep2.to_string_lossy().into_owned()])
            .await
            .unwrap();
        assert_eq!(
            created, 3,
            "the new episode plus its series/season ancestors"
        );
        assert_eq!(count_type_like(&db, "%TV.Episode").await, 2);
        assert_eq!(count_type_like(&db, "%TV.Season").await, 2);
        assert_eq!(count_type_like(&db, "%TV.Series").await, 1);
    }

    #[test]
    fn path_is_under_respects_component_boundaries() {
        assert!(super::path_is_under("/media/tv", "/media/tv"));
        assert!(super::path_is_under("/media/tv/Show/e.mkv", "/media/tv"));
        assert!(super::path_is_under("/media/tv/Show/e.mkv", "/media/tv/"));
        assert!(!super::path_is_under("/media/tv2/e.mkv", "/media/tv"));
        assert!(!super::path_is_under("/media", "/media/tv"));
    }

    /// Counts `BaseItems` whose stored `Type` matches a `LIKE` pattern. Routes
    /// the repeated row-count assertions through one query so the SQL-boundary
    /// ratchet (`ferrofin-db` `sql_boundary`) stays honest as tests are added.
    async fn count_type_like(db: &Database, like: &str) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE ?1"#)
            .bind(like)
            .fetch_one(db.pool())
            .await
            .unwrap()
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
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(media.parent().unwrap().join(".views"))
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
        LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
            .scan_all()
            .await
            .unwrap();
        // The DTO projects the id in display form; the stored form is canonical.
        let cf = ferrofin_db::store::guid_to_db(
            uuid::Uuid::parse_str(
                vf.get_virtual_folders().await.unwrap()[0]
                    .item_id
                    .as_deref()
                    .unwrap(),
            )
            .unwrap(),
        );
        (db, cf)
    }

    #[tokio::test]
    async fn scan_reads_local_movie_nfo_into_row_and_people() {
        // A bare movie with a Kodi `movie.nfo` sidecar — the scan must read it (Jellyfin's
        // default local metadata reader) and persist genres/studio/overview + the cast.
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let dir = media.join("The Matrix (1999)");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("The Matrix (1999).mkv"), b"").unwrap();
        std::fs::write(
            dir.join("movie.nfo"),
            r#"<?xml version="1.0"?>
<movie><title>The Matrix</title><year>1999</year><plot>Neo wakes up.</plot>
<genre>Action</genre><genre>SciFi</genre><studio>Warner</studio>
<actor><name>Keanu Reeves</name><role>Neo</role><type>Actor</type></actor>
<director>Lana Wachowski</director></movie>"#,
        )
        .unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join(".views"))
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
        let people = Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
            db.clone(),
        ));
        LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
            .with_people(people)
            .scan_all()
            .await
            .unwrap();

        let (genres, studios, overview): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                r#"SELECT "Genres", "Studios", "Overview" FROM "BaseItems"
                   WHERE "Type" LIKE '%Movies.Movie'"#,
            )
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(genres.as_deref(), Some("Action|SciFi"));
        assert_eq!(studios.as_deref(), Some("Warner"));
        assert_eq!(overview.as_deref(), Some("Neo wakes up."));

        // Genres mirrored into ItemValues (type 2) so genre browse/filter matches.
        let genre_vals: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "ItemValues" iv
               JOIN "ItemValuesMap" m ON m."ItemValueId" = iv."ItemValueId"
               WHERE iv."Type" = 2"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap_or(0);
        assert!(genre_vals >= 2, "both NFO genres indexed, got {genre_vals}");

        // Cast + director persisted from the NFO.
        let people: Vec<(String, Option<String>)> =
            sqlx::query_as(r#"SELECT "Name", "PersonType" FROM "Peoples" ORDER BY "Name""#)
                .fetch_all(db.pool())
                .await
                .unwrap();
        let names: Vec<&str> = people.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"Keanu Reeves"),
            "actor persisted: {names:?}"
        );
        assert!(
            names.contains(&"Lana Wachowski"),
            "director persisted: {names:?}"
        );
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

        // Season carries its SeriesName; episodes carry both SeriesName + SeasonName
        // (Jellyfin surfaces these on /Shows/{id}/Seasons and /Episodes).
        let season_series_name: Option<String> = sqlx::query_scalar(
            r#"SELECT "SeriesName" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Season'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(season_series_name.as_deref(), Some("Breaking Bad"));
        let ep_names: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            r#"SELECT "SeriesName","SeasonName" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.TV.Episode'"#,
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert!(
            ep_names
                .iter()
                .all(|(sr, sn)| sr.as_deref() == Some("Breaking Bad")
                    && sn.as_deref() == Some("Season 1")),
            "episodes carry SeriesName + SeasonName: {ep_names:?}"
        );

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
        // The presentation key keeps its own (display) format; compare as GUIDs.
        assert_eq!(
            season
                .3
                .as_deref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok()),
            uuid::Uuid::parse_str(&series_id).ok(),
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
        // The presentation key keeps its own (display) format; compare as GUIDs.
        assert_eq!(
            ep.4.as_deref().and_then(|s| uuid::Uuid::parse_str(s).ok()),
            uuid::Uuid::parse_str(&series_id).ok(),
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

    // The post-scan library-image pass composites each library's Primary tile
    // from its own content, so "My Media" stops rendering the icon-on-blue
    // fallback (port of CollectionFolderImageProvider).
    #[tokio::test]
    async fn scan_composites_library_tile_images() {
        use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
        use ferrofin_model::entities::ImageType;
        use ferrofin_traits::persistence::ItemRepository;
        use uuid::Uuid;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("Heat (1995).mkv"), b"").unwrap();
        let mut poster = image::RgbImage::new(32, 48);
        for (_x, y, px) in poster.enumerate_pixels_mut() {
            *px = image::Rgb([u8::try_from(y % 256).unwrap_or(0), 90, 200]);
        }
        poster.save(media.join("Heat (1995)-poster.jpg")).unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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

        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo: Arc<dyn ItemRepository> =
            Arc::new(crate::FerrofinItemRepository::new(db.clone(), lookup));
        let image_processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
            ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
        );
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_image_processor(image_processor)
                .with_items(repo.clone())
                .with_metadata_dir(tmp.path().join("meta"));
        scanner.scan_all().await.unwrap();

        // The library folder's id, via the virtual-folder projection.
        let folders = vf.get_virtual_folders().await.unwrap();
        let cf = folders[0]
            .item_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap();
        let infos = repo.get_image_infos(cf).await.unwrap();
        let primary = infos
            .iter()
            .find(|i| i.image_type == ImageType::Primary)
            .expect("library tile image row");
        let bytes = std::fs::read(&primary.path).expect("tile file on disk");
        assert!(!bytes.is_empty());
        assert_eq!(primary.width, 960);
        assert_eq!(primary.height, 540);
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

    // The art-dir helpers behind uploaded-image survival: stem parsing is the
    // inverse of the upload endpoint's file naming, and the extension-agnostic
    // lookup is what lets an uploaded PNG win over a cached JPG re-download.
    #[test]
    fn art_dir_stems_parse_and_existing_files_resolve() {
        use std::path::Path;

        assert_eq!(
            super::parse_art_file_stem(Path::new("/m/ID/primary.jpg")),
            Some(ferrofin_model::entities::ImageType::Primary)
        );
        assert_eq!(
            super::parse_art_file_stem(Path::new("/m/ID/backdrop1.png")),
            Some(ferrofin_model::entities::ImageType::Backdrop)
        );
        assert_eq!(
            super::parse_art_file_stem(Path::new("/m/ID/logo.webp")),
            Some(ferrofin_model::entities::ImageType::Logo)
        );
        // Unrecognized stems and non-image files parse to nothing.
        assert_eq!(
            super::parse_art_file_stem(Path::new("/m/ID/chapter1.jpg")),
            None
        );
        assert_eq!(
            super::parse_art_file_stem(Path::new("/m/ID/primary.txt")),
            None
        );

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("primary.png"), b"png").unwrap();
        assert_eq!(
            super::existing_art_file(tmp.path(), "primary"),
            Some(tmp.path().join("primary.png")),
            "a non-jpg upload is found by stem"
        );
        assert_eq!(super::existing_art_file(tmp.path(), "backdrop"), None);
    }
}
