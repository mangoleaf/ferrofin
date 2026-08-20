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
use ferrofin_model::configuration::LibraryOptions as LibraryOptionsModel;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType, VideoType};
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_model::media_info::MediaInfo;
use ferrofin_naming::audio::is_audio_file;
use ferrofin_naming::audiobook::AudioBookListResolver;
use ferrofin_naming::book::book_file_name_parser;
use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::{EpisodeResolver, season_path_parser, series_resolver};
use ferrofin_naming::video::video_resolver;
use ferrofin_providers::library_options::fetcher_names;
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
    /// Item id → OMDb's poster URL, captured during the metadata pass so the
    /// image pass can use it without a second OMDb request.
    omdb_poster: std::collections::HashMap<String, String>,
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

/// The per-library fetcher gate, derived from the owning library's
/// `LibraryOptions`. The advertised names in [`fetcher_names`] are the
/// currency: a saved `TypeOptions` entry for an item kind is authoritative
/// (a fetcher is enabled iff listed, ranked by its position in the order
/// list), while a library with no entry for the kind enables everything in
/// the default chain order. A default policy (no library resolved) is
/// fully permissive — exactly the pre-gating behavior.
#[derive(Clone, Copy, Default)]
struct FetcherPolicy<'a> {
    options: Option<&'a LibraryOptionsModel>,
}

impl<'a> FetcherPolicy<'a> {
    fn type_entry(self, kind: &str) -> Option<&'a ferrofin_model::configuration::TypeOptions> {
        self.options?.type_options.iter().find(|t| {
            t.type_
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(kind))
        })
    }

    /// Whether the library enabled metadata fetcher `name` for `kind`.
    fn metadata_enabled(self, kind: &str, name: &str) -> bool {
        self.type_entry(kind).is_none_or(|t| {
            t.metadata_fetchers
                .iter()
                .any(|f| f.eq_ignore_ascii_case(name))
        })
    }

    /// The fetcher's admin-order position for `kind` (lower = higher
    /// authority); a fetcher absent from the order list sorts last, which
    /// preserves the default chain among unordered fetchers.
    fn metadata_rank(self, kind: &str, name: &str) -> usize {
        self.type_entry(kind).map_or(usize::MAX, |t| {
            t.metadata_fetcher_order
                .iter()
                .position(|f| f.eq_ignore_ascii_case(name))
                .unwrap_or(usize::MAX)
        })
    }

    /// Whether the library enabled image fetcher `name` for `kind`.
    fn image_enabled(self, kind: &str, name: &str) -> bool {
        self.type_entry(kind).is_none_or(|t| {
            t.image_fetchers
                .iter()
                .any(|f| f.eq_ignore_ascii_case(name))
        })
    }

    /// The library's preferred metadata language, lowercased. Jellyfin's own
    /// default is `en`, which is what a library with no saved value gets.
    fn metadata_language(self) -> String {
        self.options
            .and_then(|o| o.preferred_metadata_language.as_deref())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("en")
            .to_lowercase()
    }

    /// The library's metadata country code, lowercased (Jellyfin defaults to
    /// `US`). OMDb's certificate is only taken for the US.
    fn country_code(self) -> String {
        self.options
            .and_then(|o| o.metadata_country_code.as_deref())
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or("US")
            .to_lowercase()
    }

    /// [`metadata_rank`](Self::metadata_rank) for the image-fetcher order.
    fn image_rank(self, kind: &str, name: &str) -> usize {
        self.type_entry(kind).map_or(usize::MAX, |t| {
            t.image_fetcher_order
                .iter()
                .position(|f| f.eq_ignore_ascii_case(name))
                .unwrap_or(usize::MAX)
        })
    }

    /// Whether the flat local-reader list still enables `name` (`Nfo`).
    fn local_reader_enabled(self, name: &str) -> bool {
        self.options.is_none_or(|o| {
            !o.disabled_local_metadata_readers
                .iter()
                .any(|f| f.eq_ignore_ascii_case(name))
        })
    }
}

/// Indexes each library's [`FetcherPolicy`] by its collection-folder id —
/// the id every [`Planned`] item carries as its first ancestor.
fn fetcher_policies(folders: &[VirtualFolderInfo]) -> HashMap<Uuid, FetcherPolicy<'_>> {
    folders
        .iter()
        .filter_map(|f| {
            let id = f.item_id.as_deref()?.parse().ok()?;
            Some((
                id,
                FetcherPolicy {
                    options: f.library_options.as_ref(),
                },
            ))
        })
        .collect()
}

/// Resolves a stored row's fetcher policy from its `TopParentId` (the
/// collection folder every scanned entity carries). Unresolvable rows get
/// the permissive default.
fn policy_of<'a>(
    policies: &HashMap<Uuid, FetcherPolicy<'a>>,
    top_parent_id: Option<&str>,
) -> FetcherPolicy<'a> {
    top_parent_id
        .and_then(|s| Uuid::parse_str(s).ok())
        .and_then(|id| policies.get(&id))
        .copied()
        .unwrap_or_default()
}

/// The `MediaStreamInfos.StreamType` discriminant for an embedded image
/// (an attached picture — cover art), matching `media_stream_type_to_disc`.
const EMBEDDED_IMAGE_STREAM_TYPE: i32 = 3;

/// The image file extensions the art-dir helpers recognize.
const ART_FILE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];

/// Magic-byte sniff for the artwork formats plugins may return, returning the
/// file extension to store the bytes under (one of [`ART_FILE_EXTENSIONS`]),
/// or `None` for anything that is not a recognized image — enough to keep an
/// arbitrary blob from persisting as a permanent zero-dimension image.
fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

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

/// Cap on how many ffprobe processes the scan keeps in flight by default.
///
/// The probe is ~95% of scan wall time (measured: 74 s of a 78 s, 2 100-item
/// scan) and is a pure per-file read, so probing one file at a time leaves
/// every core but one idle. This ceiling is deliberately modest rather than
/// core-count-wide: the win is close to linear on a local SSD, but a library on
/// a spinning disk or a network mount turns a wide window into seek thrash, and
/// a scan must never starve playback. Operators who know their storage raise it
/// with `FERROFIN_SCAN_PROBE_CONCURRENCY` / `scan_probe_concurrency`.
pub const DEFAULT_SCAN_PROBE_CONCURRENCY: usize = 4;

/// The effective default probe window: [`DEFAULT_SCAN_PROBE_CONCURRENCY`],
/// never more than the visible cores (a single-core NAS gains nothing from
/// four concurrent probes but pays for all four).
fn default_probe_concurrency() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(DEFAULT_SCAN_PROBE_CONCURRENCY)
}

/// The ffprobe request for a leaf media item, or `None` when the row must not
/// be probed at all: a folder, a non-media row, or a row with no path.
///
/// This is the *deciding* half of the probe — exactly the eligibility test the
/// inline probe applied — split out so the whole plan can be classified up
/// front, before any ffprobe runs.
fn probe_request(entity: &BaseItemEntity) -> Option<MediaInfoRequest> {
    let is_audio = entity.media_type.as_deref() == Some("Audio");
    let is_media = is_audio || entity.media_type.as_deref() == Some("Video");
    if entity.is_folder || !is_media {
        return None;
    }
    Some(MediaInfoRequest {
        media_source: MediaSourceInfo {
            path: Some(entity.path.clone()?),
            ..Default::default()
        },
        // Extract embedded chapter markers so they show on the playback
        // timeline (matching Jellyfin's `-show_chapters`).
        extract_chapters: true,
        media_is_audio: is_audio,
    })
}

/// Runs `request` on the encoder as a detached task, so the caller can keep
/// several probes in flight while it works through the scan in order.
///
/// A probe failure is logged and reported as `None` — exactly what the
/// inline probe did — so one unreadable file never aborts a scan.
fn spawn_probe(
    encoder: Arc<dyn MediaEncoder>,
    request: MediaInfoRequest,
) -> tokio::task::JoinHandle<Option<MediaInfo>> {
    tokio::task::spawn(async move {
        match encoder.get_media_info_full(&request).await {
            Ok(probed) => Some(probed),
            Err(e) => {
                let path = request.media_source.path.as_deref();
                tracing::warn!(error = %e, ?path, "media probe failed; item left unprobed");
                None
            }
        }
    })
}

/// The look-ahead probe pipeline: tracks which planned rows probe, keeps a
/// bounded set of ffprobe tasks in flight, and hands each planned item its
/// result **in plan order**.
///
/// Order is the whole point. The scan body still runs one item at a time, in
/// exactly the sequence [`LibraryScanner::plan`] produced, writing exactly the
/// same rows; only the ffprobe wait is moved off that critical path.
struct ProbePipeline<'a> {
    /// The encoder seam, cloned into each spawned probe. `None` disables the
    /// pipeline entirely (no probe wired — the unit-test and no-ffmpeg case).
    encoder: Option<Arc<dyn MediaEncoder>>,
    /// The plan the scan is walking, borrowed so a request can be rebuilt at
    /// dispatch time from the row itself.
    planned: &'a [Planned],
    /// Per planned item: `Some(is_audio)` when the row is probe-eligible,
    /// `None` when it is a folder / non-media / path-less row that never
    /// probes. Deliberately one byte per item rather than a parked
    /// [`MediaInfoRequest`] — a request carries a whole `MediaSourceInfo`
    /// (504 bytes plus its path), so parking one per item would hold ~50 MB
    /// across a 100 000-item scan; this vector holds 100 KB.
    eligible: Vec<Option<bool>>,
    /// The next planned index that has not been dispatched yet.
    next: usize,
    /// In-flight probes, in dispatch (= plan) order, tagged with their index.
    inflight: std::collections::VecDeque<(usize, tokio::task::JoinHandle<Option<MediaInfo>>)>,
    /// How many probes to keep in flight.
    window: usize,
}

impl<'a> ProbePipeline<'a> {
    /// Builds the pipeline over `planned` and primes `window` probes.
    fn new(encoder: Option<Arc<dyn MediaEncoder>>, planned: &'a [Planned], window: usize) -> Self {
        let eligible = if encoder.is_some() {
            planned
                .iter()
                .map(|p| probe_request(&p.entity).map(|r| r.media_is_audio))
                .collect()
        } else {
            Vec::new()
        };
        let mut this = Self {
            encoder,
            planned,
            eligible,
            next: 0,
            inflight: std::collections::VecDeque::new(),
            window: window.max(1),
        };
        for _ in 0..this.window {
            this.dispatch_next();
        }
        this
    }

    /// Dispatches the next probe-eligible planned item, skipping the folders
    /// and non-media rows that never probe.
    fn dispatch_next(&mut self) {
        let Some(encoder) = &self.encoder else { return };
        while self.next < self.eligible.len() {
            let index = self.next;
            self.next += 1;
            if self.eligible[index].is_some()
                && let Some(request) = probe_request(&self.planned[index].entity)
            {
                self.inflight
                    .push_back((index, spawn_probe(Arc::clone(encoder), request)));
                return;
            }
        }
    }

    /// Awaits the probe for planned item `index` and reports whether that item
    /// was probed as audio (which selects the embedded-tag branch of
    /// [`LibraryScanner::apply_probe`]), then refills the window so the
    /// following files are already being probed while the caller persists this
    /// one. The result is `None` when the item was not probe-eligible (the
    /// queue head belongs to a later index) or its probe failed.
    async fn take(&mut self, index: usize) -> (Option<MediaInfo>, bool) {
        let is_audio = self.eligible.get(index).copied().flatten().unwrap_or(false);
        if self.inflight.front().is_none_or(|(i, _)| *i != index) {
            return (None, is_audio);
        }
        let Some((_, handle)) = self.inflight.pop_front() else {
            return (None, is_audio);
        };
        let probed = match handle.await {
            Ok(probed) => probed,
            Err(err) => {
                tracing::warn!(%err, "probe task failed; item left unprobed");
                None
            }
        };
        // Refill only once this probe is done, so `window` is the exact number
        // of ffprobe processes that can ever run at the same time — dispatching
        // before the await would make it `window + 1`, and an operator who set
        // the knob to 1 to protect a fragile mount would still get two.
        self.dispatch_next();
        (probed, is_audio)
    }

    /// Cancels every probe still in flight — a scan that ended (or failed)
    /// must not leave ffprobe processes running behind it.
    fn abort(&mut self) {
        for (_, handle) in self.inflight.drain(..) {
            handle.abort();
        }
    }
}

impl Drop for ProbePipeline<'_> {
    fn drop(&mut self) {
        self.abort();
    }
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
    /// How many ffprobe processes the scan keeps in flight
    /// ([`DEFAULT_SCAN_PROBE_CONCURRENCY`]). The probe dominates scan wall
    /// time and is a pure per-file read, so the scan runs this many files
    /// ahead of the (still strictly ordered) persistence loop.
    probe_concurrency: usize,
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
    /// Dynamically-registered metadata sources (Tier-1b WASM plugins). Run
    /// per item AFTER the built-in chain; supplement-only (they fill gaps,
    /// never overwrite).
    dynamic_providers: Vec<Arc<dyn ferrofin_traits::providers::DynamicMetadataProvider>>,
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
    /// Studio artwork-repository client for the post-scan studio-thumb pass.
    /// Absent → Studio rows keep whatever images they already have.
    studios_client: Option<Arc<ferrofin_providers::StudiosClient>>,
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
            probe_concurrency: default_probe_concurrency(),
            tmdb: None,
            omdb: None,
            tvdb: None,
            dynamic_providers: Vec::new(),
            fanart: None,
            musicbrainz: None,
            audiodb: None,
            item_repository: None,
            studios_client: None,
            metadata_dir: None,
            people: None,
            chapters: None,
            image_processor: None,
            localization: None,
            progress_every: DEFAULT_SCAN_PROGRESS_EVERY,
            events: None,
        }
    }

    /// Overrides how many ffprobe processes the scan keeps in flight
    /// (`0` is clamped to 1 — the old strictly-serial behaviour).
    ///
    /// Wired from the `FERROFIN_SCAN_PROBE_CONCURRENCY` bootstrap knob; unit
    /// tests keep [`DEFAULT_SCAN_PROBE_CONCURRENCY`]. Raising it trades
    /// concurrent ffmpeg processes (and their I/O) for scan throughput: on a
    /// local disk it scales close to linearly with cores, but on a spinning
    /// disk or a network mount a wide window turns sequential reads into seek
    /// thrash, which is why the default is deliberately modest.
    #[must_use]
    pub fn with_probe_concurrency(mut self, concurrency: usize) -> Self {
        self.probe_concurrency = concurrency.max(1);
        self
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

    /// Attaches the studio artwork-repository client: after each scan, Studio
    /// by-name rows still without artwork get their repository thumb
    /// downloaded into the metadata dir (upstream's `StudiosImageProvider`,
    /// which runs as part of a studio's refresh). Needs
    /// [`with_items`](Self::with_items) (or `with_music`) and
    /// [`with_metadata`](Self::with_metadata)'s metadata dir to act.
    #[must_use]
    pub fn with_studio_images(mut self, studios: Arc<ferrofin_providers::StudiosClient>) -> Self {
        self.studios_client = Some(studios);
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

    /// Registers dynamically-loaded metadata sources (Tier-1b WASM plugins),
    /// called per item after the built-in provider chain. Supplement-only.
    #[must_use]
    pub fn with_dynamic_providers(
        mut self,
        providers: Vec<Arc<dyn ferrofin_traits::providers::DynamicMetadataProvider>>,
    ) -> Self {
        self.dynamic_providers = providers;
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
        // Per-library fetcher policies, keyed by the collection-folder id
        // every Planned item carries as its first ancestor. This is what
        // makes the dashboard's per-library "Metadata downloaders" and
        // "Image fetchers" checkboxes REAL: a fetcher the admin unchecked
        // never runs for that library's items, and the saved order picks
        // the authority when fetchers compete.
        let fetcher_policies = fetcher_policies(folders);
        // ffprobe dominates scan wall time and touches nothing but the file it
        // reads, so it runs `probe_concurrency` files ahead of this loop. The
        // loop itself is unchanged: same plan order, same rows, same writes.
        let mut probes = self.probe_pipeline(&planned);
        for (scanned, item) in planned.iter().enumerate() {
            tracing::debug!(item = %item.id, "scanning item");
            self.log_scan_progress(scanned, planned.len());
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
            let policy = policy_for(item, &fetcher_policies);
            // Probe first so the item row is saved already carrying its duration and
            // size (the streams themselves are saved after, since they FK the row).
            let mut entity = item.entity.clone();
            let (media_info, is_audio) = probes.take(scanned).await;
            let (streams, chapters, tag_provider_ids) =
                Self::apply_probe(&mut entity, media_info.as_ref(), is_audio);
            // Local Kodi/XBMC NFO sidecar first — this is Jellyfin's default local
            // metadata reader, which runs before any remote fetch. It fills
            // genres/studios/tags/overview/ratings/year from `movie.nfo` /
            // `tvshow.nfo` / `<episode>.nfo` and yields the credited cast/crew.
            let mut people = if locked {
                Vec::new()
            } else {
                self.fetch_local_nfo(&mut entity, policy).await
            };
            // Then enrich from TMDB (overview/tagline/genres/studios/ratings +
            // cast/crew) to fill any gaps the NFO left, so a bare file with no NFO
            // shows the same detail page Jellyfin does. Best-effort: failures don't
            // abort, and NFO-provided people take precedence.
            let remote = if locked {
                RemoteMetadata::default()
            } else {
                self.fetch_remote_metadata(&mut entity, &mut art_cache, policy)
                    .await
            };
            // Photos and books carry their metadata inside the file, not on any
            // remote provider. A photo's Primary image is the file itself; a
            // book's is the cover extracted from its archive.
            let (embedded_people, embedded_images) =
                self.enrich_from_file(&mut entity, locked).await;
            if people.is_empty() {
                people = embedded_people;
            }
            if people.is_empty() {
                people = remote.people;
            }
            self.apply_parental_rating_score(&mut entity);
            // Dynamic (Tier-1b WASM plugin) metadata sources run last and
            // supplement whatever the built-in chain left unfilled; the
            // helper merges their (filtered) ids with the built-ins'.
            let all_provider_ids = self
                .apply_dynamic_metadata(
                    &mut entity,
                    &remote.provider_ids,
                    tag_provider_ids,
                    locked,
                    policy,
                )
                .await;
            // Scan-variant save: preserves `PrimaryVersionId` (merge-versions
            // links) and the stored `DateCreated` on rows that already exist —
            // this entity is rebuilt from disk and would otherwise reset both
            // on every scan.
            self.persistence
                .save_scanned_items(std::slice::from_ref(&entity))
                .await?;
            self.persist_provider_ids_and_values(
                item.id,
                &entity,
                all_provider_ids,
                &mut art_cache,
            )
            .await?;
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
            self.save_chapters(item.id, &chapters).await?;
            // Artwork — locked items skip the rewrite entirely: their image
            // rows are user-owned.
            if !locked {
                let art = ArtworkPass {
                    entity: &entity,
                    streams: &streams,
                    policy,
                    embedded_images,
                };
                self.persist_artwork(item.id, art, &mut art_cache).await;
            }
            // Per-library refresh % for open dashboards (`RefreshProgress`),
            // at the same bounded cadence as the progress log plus each
            // library's completion.
            if let Some((cf, pct)) = library_progress.advance(item, self.progress_every) {
                self.publish_refresh_progress(cf, pct).await;
            }
        }
        probes.abort();
        // Drop rows whose files vanished since the last scan, so deleted media
        // stops being listed and served. Best-effort — a failure must not fail
        // the whole scan.
        let removed = self.prune_deleted(folders, &planned, prune_scope).await;
        // Announce what the scan changed (`LibraryChanged`) so open clients
        // refresh their library views without a manual reload.
        self.publish_library_changed(&items_added, &removed).await;
        self.post_scan_passes(folders).await;
        Ok(planned.len())
    }

    /// Persists the item's external provider ids (the remote match's
    /// Tmdb/Imdb/Tvdb plus the embedded MusicBrainz tag ids — they key the
    /// id-dependent providers and make re-scans stable; best-effort, a
    /// write failure is logged, not fatal), caches them for the image pass
    /// (fanart keys off them without a DB round-trip), and mirrors
    /// genres/studios/tags into ItemValues so the genre/studio/tag
    /// *filters* (More Like This, genre browse) match.
    async fn persist_provider_ids_and_values(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        all_provider_ids: Vec<(String, String)>,
        art_cache: &mut ArtworkCache,
    ) -> Result<(), ServiceError> {
        for (key, value) in &all_provider_ids {
            if let Err(err) = self.persistence.save_provider_id(item_id, key, value).await {
                tracing::warn!(%err, item = %item_id, provider = key, "failed to persist provider id");
            }
        }
        if !all_provider_ids.is_empty() {
            art_cache
                .item_provider_ids
                .insert(entity.id.clone(), all_provider_ids);
        }
        let item_values = item_values_of(entity);
        if !item_values.is_empty() {
            self.persistence
                .save_item_values(item_id, &item_values)
                .await?;
        }
        Ok(())
    }

    /// The best-effort enrichment passes that run once the item walk is done.
    /// Each is independent and logs its own failure — none may fail the scan.
    async fn post_scan_passes(&self, folders: &[VirtualFolderInfo]) {
        // The music pass honors the same per-library fetcher checkboxes as
        // the item walk — resolved per row via its `TopParentId`.
        let policies = fetcher_policies(folders);
        // Music enrichment: resolve MusicBrainz ids (and, once wired,
        // AudioDb/fanart artwork) for the MusicAlbum/MusicArtist rows created
        // above.
        if let Err(err) = self.enrich_music(&policies).await {
            tracing::warn!(%err, "music enrichment pass failed");
        }
        // Studio thumbs from the artwork repository for the by-name Studio
        // rows the item-values step materialized, so the TV Networks /
        // Studios tabs carry artwork.
        if let Err(err) = self.enrich_studio_images().await {
            tracing::warn!(%err, "studio image pass failed");
        }
        // Library tile images LAST: the collage composites each library's
        // Primary from its own content (upstream
        // CollectionFolderImageProvider), so it has to run after the passes
        // that fetch that content's artwork — otherwise a first scan sees no
        // art and the "My Media" tile keeps the icon placeholder.
        if let Err(err) = self.refresh_library_images(folders).await {
            tracing::warn!(%err, "library image pass failed");
        }
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
    /// (`boxsets`) are also skipped: their empty plan means "not managed", not
    /// "deleted".
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
                        | CollectionTypeOptions::books
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
    async fn enrich_music(
        &self,
        policies: &HashMap<Uuid, FetcherPolicy<'_>>,
    ) -> Result<(), ServiceError> {
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
            policies,
        )
        .await?;
        self.enrich_artists(items.as_ref(), mb.as_ref(), &artist_mbid, policies)
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
        policies: &HashMap<Uuid, FetcherPolicy<'_>>,
    ) -> Result<(), ServiceError> {
        let albums = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicAlbum],
                recursive: true,
                ..Default::default()
            })
            .await?;
        for album in albums {
            let policy = policy_of(policies, album.top_parent_id.as_deref());
            self.enrich_one_album(
                &album,
                items,
                mb,
                track_album,
                track_rg,
                artist_mbid,
                policy,
            )
            .await?;
        }
        Ok(())
    }

    /// Enriches one `MusicAlbum`: aggregate album-artist/year from its tracks,
    /// resolve + persist its MusicBrainz ids, then AudioDb metadata + AudioDb/
    /// fanart artwork.
    // The per-track id maps + the library policy are each one seam; a
    // params struct would only rename the coupling.
    #[allow(clippy::too_many_arguments)]
    async fn enrich_one_album(
        &self,
        album: &BaseItemEntity,
        items: &dyn ItemRepository,
        mb: &ferrofin_providers::MusicBrainzClient,
        track_album: &HashMap<Uuid, String>,
        track_rg: &HashMap<Uuid, String>,
        artist_mbid: &HashMap<String, String>,
        policy: FetcherPolicy<'_>,
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
            // The album row's name starts as the folder stem, which is usually
            // release noise ("RHCP - Californication (1999) FLAC"). The tracks'
            // ALBUM tag is authoritative when they agree — upstream's album
            // metadata comes from the tags, not the directory.
            if let Some(tagged) = album_name_consensus(&tracks)
                && updated.name.as_deref() != Some(tagged.as_str())
            {
                updated.sort_name = Some(create_sort_name(&tagged));
                updated.presentation_unique_key = Some(tagged.clone());
                updated.name = Some(tagged);
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
            // The library's MusicBrainz checkbox gates the REMOTE
            // resolution only; the tag/name aggregation above is local.
            let resolved = if policy.metadata_enabled("MusicAlbum", fetcher_names::MUSICBRAINZ) {
                mb.resolve_album(&album_name, embedded, None, album_artist.as_deref())
                    .await
            } else {
                ferrofin_providers::AlbumIds::default()
            };
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
            // The save below lives in `enrich_album_artwork` and is gated on
            // its own change flag, so a date applied here has to be reported —
            // otherwise it is fetched and silently discarded.
            let dated =
                apply_release_details(&mut updated, mb, resolved.release_id.as_deref()).await;

            self.enrich_album_artwork(
                album_uuid,
                &mut updated,
                resolved.release_group_id.as_deref(),
                album_artist.as_deref(),
                artist_mbid,
                policy,
                dated,
            )
            .await?;
            // Last resort: an album with no artwork takes its first track's
            // embedded cover (upstream's AlbumImageProvider).
            self.inherit_album_cover(album_uuid, items).await;
        }
        Ok(())
    }

    /// AudioDb album metadata (description/year) + AudioDb/fanart album artwork,
    /// keyed by the release-group id (fanart also needs the album-artist's mbid).
    #[allow(clippy::too_many_arguments)]
    async fn enrich_album_artwork(
        &self,
        album_uuid: Uuid,
        updated: &mut BaseItemEntity,
        release_group_id: Option<&str>,
        album_artist: Option<&str>,
        artist_mbid: &HashMap<String, String>,
        policy: FetcherPolicy<'_>,
        already_changed: bool,
    ) -> Result<(), ServiceError> {
        let mut changed = already_changed;
        let mut images: Vec<ferrofin_providers::TmdbImage> = Vec::new();
        if policy.metadata_enabled("MusicAlbum", fetcher_names::AUDIODB)
            && let (Some(adb), Some(rg)) = (&self.audiodb, release_group_id)
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
        policies: &HashMap<Uuid, FetcherPolicy<'_>>,
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
            let policy = policy_of(policies, artist.top_parent_id.as_deref());
            // The MusicBrainz checkbox gates the REMOTE surface only (the
            // name search and the persisted provider-id row) — an mbid
            // already present in the local tags stays usable, so AudioDb/
            // fanart below still run for tagged libraries (the same
            // remote-resolve-only gate as `enrich_one_album`).
            let mb_enabled = policy.metadata_enabled("MusicArtist", fetcher_names::MUSICBRAINZ);
            let mbid = match artist_mbid.get(name) {
                Some(id) => Some(id.clone()),
                None if mb_enabled => mb.search_artist(name).await,
                None => None,
            };
            let Some(id) = mbid else {
                continue;
            };
            if mb_enabled {
                let _ = self
                    .persistence
                    .save_provider_id(artist_uuid, "MusicBrainzArtist", &id)
                    .await;
            }

            // AudioDb bio/genre + AudioDb/fanart artist artwork, keyed by the
            // resolved MusicBrainz artist id.
            let mut updated = artist.clone();
            let mut changed = false;
            let mut images: Vec<ferrofin_providers::TmdbImage> = Vec::new();
            // MusicBrainz's own artist fields (C# `MusicBrainzArtistProvider`
            // writes more than the id): the life span, whose end is what the
            // artist NFO saver emits as `<disbanded>`.
            if mb_enabled
                && (updated.premiere_date.is_none() || updated.end_date.is_none())
                && let Some(details) = mb.artist_details(&id).await
            {
                if updated.premiere_date.is_none()
                    && let Some(begin) = details
                        .premiere_date
                        .and_then(ferrofin_providers::PartialDate::to_utc)
                {
                    updated.premiere_date = Some(begin);
                    changed = true;
                }
                if updated.end_date.is_none()
                    && let Some(end) = details
                        .end_date
                        .and_then(ferrofin_providers::PartialDate::to_utc)
                {
                    updated.end_date = Some(end);
                    changed = true;
                }
            }
            if policy.metadata_enabled("MusicArtist", fetcher_names::AUDIODB)
                && let Some(adb) = &self.audiodb
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

    /// Opens the look-ahead ffprobe pipeline over an already-planned item set.
    fn probe_pipeline<'a>(&self, planned: &'a [Planned]) -> ProbePipeline<'a> {
        ProbePipeline::new(self.media_encoder.clone(), planned, self.probe_concurrency)
    }

    /// Folds a completed probe onto the item row — the probed
    /// `run_time_ticks`/`size` plus, for audio, the embedded tags — and returns
    /// the media-stream, chapter and provider-id rows to persist.
    ///
    /// The *mutating* half of the probe, split from
    /// [`probe_request`](Self::probe_request) so the ffprobe call itself can run
    /// ahead of the scan loop ([`spawn_probe`]) while the row updates stay
    /// strictly in scan order. `probed` is `None` when the item was not
    /// probe-eligible or ffprobe failed (missing ffmpeg, unreadable file) —
    /// both leave the row exactly as the plan built it, still browsable, so one
    /// bad file never aborts a whole library scan.
    fn apply_probe(
        entity: &mut BaseItemEntity,
        probed: Option<&MediaInfo>,
        media_is_audio: bool,
    ) -> (
        Vec<MediaStreamInfoEntity>,
        Vec<ChapterEntity>,
        Vec<(String, String)>,
    ) {
        let empty = (Vec::new(), Vec::new(), Vec::new());
        let Some(probed) = probed else {
            return empty;
        };
        let source = &probed.media_source;
        entity.run_time_ticks = source.run_time_ticks.or(entity.run_time_ticks);
        entity.size = source.size.or(entity.size);
        // Embedded audio tags (album/artists/track/disc/year/genres + the
        // MusicBrainz ids) — the port of `AudioFileProber`. Fill-if-empty so an
        // NFO/prior scan wins; the ids are returned for persistence.
        let provider_ids = if media_is_audio {
            apply_audio_metadata(entity, probed)
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
    async fn fetch_local_nfo(
        &self,
        entity: &mut BaseItemEntity,
        policy: FetcherPolicy<'_>,
    ) -> Vec<PeopleEntity> {
        use ferrofin_providers::xbmc::{
            self, base_parser::NoDirectoryService, config::NfoConfiguration, item::NfoItemKind,
        };
        if !policy.local_reader_enabled(fetcher_names::NFO) {
            return Vec::new();
        }
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        let kind = match short {
            "Movie" => NfoItemKind::Movie,
            "Series" => NfoItemKind::Series,
            "Season" => NfoItemKind::Season,
            "Episode" => NfoItemKind::Episode,
            "MusicAlbum" => NfoItemKind::MusicAlbum,
            "MusicArtist" => NfoItemKind::MusicArtist,
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
        // The id tags a document may carry are per-kind: C# builds
        // `_validProviderIds` from `GetExternalIdInfos(item)`. A fixed
        // video-only list silently discards an `album.nfo`'s
        // `<musicbrainzalbumid>` and an `artist.nfo`'s `<musicbrainzartistid>`.
        let ext_ids = xbmc::StaticExternalIds::new(
            ferrofin_providers::external_id_infos(
                crate::item_type_lookup::kind_from_type_name(&entity.type_)
                    .unwrap_or(BaseItemKind::Folder),
            )
            .into_iter()
            .filter_map(|info| info.key),
        );
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
            NfoItemKind::MusicAlbum | NfoItemKind::MusicArtist => {
                xbmc::fetch_music(&mut result, &nfo_path, &xml, &config, &ext_ids, &ds)
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
    /// Runs every registered [`DynamicMetadataProvider`] for one item and
    /// applies the results **supplement-only**: a field is taken only when
    /// the entity still lacks a value, so dynamic sources can never
    /// overwrite the built-in chain or user edits. Returns the FULL
    /// provider-id list to persist: the built-ins' (remote + tag) followed
    /// by the sources' contributions, filtered so a plugin id can never
    /// replace a built-in one (`save_provider_id` is INSERT OR REPLACE).
    ///
    /// [`DynamicMetadataProvider`]: ferrofin_traits::providers::DynamicMetadataProvider
    async fn apply_dynamic_metadata(
        &self,
        entity: &mut BaseItemEntity,
        remote_ids: &[(String, String)],
        tag_ids: Vec<(String, String)>,
        locked: bool,
        policy: FetcherPolicy<'_>,
    ) -> Vec<(String, String)> {
        let known_ids: Vec<(String, String)> = remote_ids.iter().cloned().chain(tag_ids).collect();
        // Locked items and provider-less scans skip the pass entirely;
        // best-effort per source — one bad plugin never fails a scan.
        if locked || self.dynamic_providers.is_empty() {
            return known_ids;
        }
        // An unparseable row id must not reach guests as a nil UUID they
        // could then write segments against — skip the item instead.
        let Ok(item_id) = Uuid::parse_str(&entity.id) else {
            tracing::warn!(
                id = entity.id,
                "skipping dynamic metadata: unparseable item id"
            );
            return Vec::new();
        };
        let lookup = ferrofin_traits::providers::DynamicMetadataLookup {
            item_id,
            kind: entity
                .type_
                .rsplit('.')
                .next()
                .unwrap_or(&entity.type_)
                .to_owned(),
            name: entity.name.clone().unwrap_or_default(),
            production_year: entity.production_year.and_then(|y| i32::try_from(y).ok()),
            path: entity.path.clone(),
            provider_ids: known_ids.clone(),
        };
        let mut contributed_ids = Vec::new();
        // NAMED providers (declared provider-info) honor the library's
        // fetcher checkboxes and order; unnamed sources always supplement,
        // last, in registration order (stable sort keeps their relative
        // order at equal rank).
        let mut sources: Vec<_> = self
            .dynamic_providers
            .iter()
            .filter(|p| !p.library_gated() || policy.metadata_enabled(&lookup.kind, p.name()))
            .collect();
        sources.sort_by_key(|p| {
            if p.library_gated() {
                policy.metadata_rank(&lookup.kind, p.name())
            } else {
                usize::MAX
            }
        });
        for provider in sources {
            let result = match provider.lookup(&lookup).await {
                Ok(Some(result)) => result,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(
                        provider = provider.name(),
                        item = %lookup.item_id,
                        %err,
                        "dynamic metadata provider failed; continuing"
                    );
                    continue;
                }
            };
            if entity.overview.as_deref().is_none_or(str::is_empty) {
                entity.overview = result.overview.filter(|o| !o.is_empty());
            }
            if entity.production_year.is_none() {
                entity.production_year = result.production_year.map(i64::from);
            }
            if entity.community_rating.is_none() {
                entity.community_rating = result.community_rating;
            }
            if entity.tagline.as_deref().is_none_or(str::is_empty) {
                entity.tagline = result.tagline.clone().filter(|t| !t.is_empty());
            }
            if entity.studios.as_deref().is_none_or(str::is_empty) && !result.studios.is_empty() {
                entity.studios = Some(result.studios.join("|"));
            }
            if entity.tags.as_deref().is_none_or(str::is_empty) && !result.tags.is_empty() {
                entity.tags = Some(result.tags.join("|"));
            }
            if entity.official_rating.as_deref().is_none_or(str::is_empty) {
                entity.official_rating = result.official_rating.clone().filter(|r| !r.is_empty());
            }
            if entity.end_date.is_none() {
                entity.end_date = result
                    .end_date
                    .as_deref()
                    .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                    .map(|d| d.with_timezone(&chrono::Utc));
            }
            if entity.genres.as_deref().unwrap_or_default().is_empty() && !result.genres.is_empty()
            {
                entity.genres = Some(result.genres.join("|"));
            }
            // Supplement-only holds for ids too: a key the built-in chain
            // (or an earlier plugin) already recorded is not replaceable.
            for (key, value) in result.provider_ids {
                let taken = known_ids
                    .iter()
                    .chain(contributed_ids.iter())
                    .any(|(k, _)| k.eq_ignore_ascii_case(&key));
                if !taken {
                    contributed_ids.push((key, value));
                }
            }
        }
        let mut all = known_ids;
        all.extend(contributed_ids);
        all
    }

    // `tvdb_on`/`tmdb_on` ARE the point of this function — the two
    // competitors the admin order arbitrates.
    #[allow(clippy::similar_names)]
    async fn fetch_remote_metadata(
        &self,
        entity: &mut BaseItemEntity,
        cache: &mut ArtworkCache,
        policy: FetcherPolicy<'_>,
    ) -> RemoteMetadata {
        let short = entity
            .type_
            .rsplit('.')
            .next()
            .unwrap_or(&entity.type_)
            .to_owned();
        // Per-library gate + order over the advertised fetcher names: a
        // fetcher the library disabled never runs, and for a series the
        // saved fetcher order decides whether TVDB or TMDB is the authority
        // (the other stays the miss-fallback). The default (no saved
        // TypeOptions) preserves the historic chain: TheTVDB is the TV
        // authority, TMDB the fallback; movies are TMDB-only.
        let tvdb_on = self.tvdb.is_some()
            && matches!(short.as_str(), "Series" | "Episode")
            && policy.metadata_enabled(&short, fetcher_names::TVDB);
        let tmdb_on = policy.metadata_enabled(&short, fetcher_names::TMDB);
        let tvdb_first = policy.metadata_rank(&short, fetcher_names::TVDB)
            <= policy.metadata_rank(&short, fetcher_names::TMDB);
        if tvdb_on && (tvdb_first || !tmdb_on) {
            let result = self.fetch_tvdb_metadata(entity, &short, cache).await;
            // A TVDB hit (series cached, or episode text applied) is authoritative;
            // only fall through to TMDB for a series TVDB could not resolve.
            if short == "Episode" || cache.series_tvdb.contains_key(&entity.id) {
                return result;
            }
        }
        let omdb_on = policy.metadata_enabled(&short, fetcher_names::OMDB);
        if !tmdb_on {
            // Each fetcher's checkbox gates only itself: unchecking TheMovieDb
            // must not silently disable OMDb as well.
            return if omdb_on {
                self.fetch_omdb_metadata(entity, &short, cache, policy)
                    .await
            } else {
                RemoteMetadata::default()
            };
        }
        if let Some(result) = self.fetch_tmdb_metadata(entity, &short, omdb_on).await {
            return result;
        }
        // The library ranked TMDB above TVDB and TMDB missed the series:
        // TVDB is the fallback.
        if tvdb_on && short == "Series" && !tvdb_first {
            let result = self.fetch_tvdb_metadata(entity, &short, cache).await;
            if cache.series_tvdb.contains_key(&entity.id) {
                return result;
            }
        }
        // Nothing upstream matched: OMDb closes the chain, matching its C#
        // `Order = 2` (behind TMDB and TVDB, ahead of nothing).
        if omdb_on {
            return self
                .fetch_omdb_metadata(entity, &short, cache, policy)
                .await;
        }
        RemoteMetadata::default()
    }

    /// The photo embedded-information pass — port of `Emby.Photos.PhotoProvider`.
    ///
    /// Sets the Primary image to the photo file itself, reads the EXIF tags off
    /// it, and writes them onto the row: the dedicated columns Ferrofin has
    /// (name/overview/rating/dates/width/height) plus the EXIF-only fields,
    /// which live in the `Data` blob under Jellyfin's own property names.
    ///
    /// Returns the Primary image row for the artwork pass to persist. A locked
    /// item is skipped entirely, as it is for every other provider.
    async fn enrich_photo(&self, entity: &mut BaseItemEntity, locked: bool) -> Vec<ItemImageInfo> {
        if !entity.type_.ends_with(".Photo") {
            return Vec::new();
        }
        let Some(processor) = self.image_processor.as_ref() else {
            return Vec::new();
        };
        let Some(path) = entity.path.clone().filter(|p| !p.is_empty()) else {
            return Vec::new();
        };
        let item_id = Uuid::parse_str(&entity.id).unwrap_or_else(|_| Uuid::nil());
        let mut photo = ferrofin_drawing::photo_provider::PhotoItem {
            id: item_id,
            path,
            is_file_protocol: true,
            width: entity
                .width
                .and_then(|w| i32::try_from(w).ok())
                .unwrap_or(0),
            height: entity
                .height
                .and_then(|h| i32::try_from(h).ok())
                .unwrap_or(0),
            name: entity.name.clone(),
            name_locked: locked,
            ..Default::default()
        };
        let provider = ferrofin_drawing::photo_provider::PhotoProvider::new(Arc::clone(processor));
        if let Err(err) = provider.fetch(&mut photo).await {
            tracing::warn!(%err, item = %entity.id, "photo embedded-information pass failed");
            return Vec::new();
        }
        if photo.width > 0 {
            entity.width = Some(i64::from(photo.width));
        }
        if photo.height > 0 {
            entity.height = Some(i64::from(photo.height));
        }
        if !locked {
            if let Some(name) = photo.name.filter(|n| !n.is_empty()) {
                // The sort name follows the title, or the album keeps sorting
                // by filename while displaying the EXIF title.
                entity.sort_name = Some(derived_sort_name(entity, &name));
                entity.name = Some(name);
            }
            if photo.overview.is_some() {
                entity.overview = photo.overview;
            }
            if photo.community_rating.is_some() {
                entity.community_rating = photo.community_rating;
            }
            if let Some(taken) = photo.date_taken {
                entity.premiere_date = Some(taken);
                entity.production_year = photo.production_year.map(i64::from);
            }
        }
        if let Some(data) = crate::item_data::merge_data_fields(
            entity.data.as_deref(),
            &photo_exif_fields(&photo.exif),
        ) {
            entity.data = Some(data);
        }
        photo.images
    }

    /// The OMDb fetcher — port of `OmdbItemProvider` and `OmdbEpisodeProvider`.
    ///
    /// Movies and series resolve through OMDb's exact-title endpoint (upstream's
    /// `GetImdbId` fallback when the item carries no IMDb id); an episode is read
    /// out of its series' season listing, keyed by the series' IMDb id recorded
    /// earlier in this scan. A row that already has an overview and both ratings
    /// has nothing left for OMDb to fill, so a re-scan makes no request.
    ///
    /// `Rated` and `Genres` are only taken for an English library (OMDb has no
    /// localization — C# `IsConfiguredForEnglish`), and `Rated` additionally only
    /// for a US metadata country, both exactly as upstream gates them.
    async fn fetch_omdb_metadata(
        &self,
        entity: &mut BaseItemEntity,
        short: &str,
        cache: &mut ArtworkCache,
        policy: FetcherPolicy<'_>,
    ) -> RemoteMetadata {
        let Some(omdb) = self.omdb.as_ref().filter(|o| o.is_enabled()) else {
            return RemoteMetadata::default();
        };
        let has_overview = entity.overview.as_deref().is_some_and(|o| !o.is_empty());
        if has_overview && entity.community_rating.is_some() && entity.critic_rating.is_some() {
            return RemoteMetadata::default();
        }
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
        let name = entity.name.as_deref().filter(|n| !n.is_empty());
        let item = match short {
            "Movie" | "Series" => {
                let kind = if short == "Movie" {
                    ferrofin_providers::OmdbKind::Movie
                } else {
                    ferrofin_providers::OmdbKind::Series
                };
                match name {
                    Some(name) => omdb.find_by_title(kind, name, year).await,
                    None => None,
                }
            }
            "Episode" => {
                let series_imdb = entity
                    .series_id
                    .as_deref()
                    .and_then(|series| cache.item_provider_ids.get(series))
                    .and_then(|ids| {
                        ids.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("Imdb"))
                            .map(|(_, v)| v.clone())
                    });
                match (
                    series_imdb,
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) {
                    (Some(series), Some(season), Some(number)) => {
                        omdb.episode(&series, season, number, None).await
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(item) = item else {
            return RemoteMetadata::default();
        };
        let english = policy.metadata_language() == "en";
        let us = policy.country_code() == "us";
        apply_omdb(entity, &item, english, us);
        if let Some(poster) = item.poster.as_deref().filter(|p| p.starts_with("http")) {
            cache
                .omdb_poster
                .insert(entity.id.clone(), poster.to_owned());
        }
        let mut provider_ids = Vec::new();
        if let Some(id) = item.imdb_id.as_deref().filter(|s| !s.is_empty()) {
            provider_ids.push(("Imdb".to_owned(), id.to_owned()));
        }
        RemoteMetadata {
            // C# `ParseAdditionalMetadata` returns before adding the
            // director/writer/actors unless the OMDb plugin's `CastAndCrew`
            // flag is set, and that bool has no initializer — so upstream's
            // default is OFF and Ferrofin matches it.
            people: if OMDB_CAST_AND_CREW {
                omdb_people(&item)
            } else {
                Vec::new()
            },
            provider_ids,
        }
    }

    /// The TMDB half of the remote-metadata pass. `None` means TMDB had no
    /// match for the item (the caller may fall back to another fetcher);
    /// `Some(default)` means the item needed no fetch at all. `omdb_on`
    /// gates the OMDb (Rotten Tomatoes) supplement, which rides TMDB's
    /// IMDb id.
    async fn fetch_tmdb_metadata(
        &self,
        entity: &mut BaseItemEntity,
        short: &str,
        omdb_on: bool,
    ) -> Option<RemoteMetadata> {
        let tmdb = self.tmdb.as_ref()?;
        let kind = match short {
            "Movie" => TmdbKind::Movie,
            "Series" => TmdbKind::Series,
            _ => return None,
        };
        // Fetch when the row still lacks core metadata OR still lacks a Rotten
        // Tomatoes rating (with OMDb enabled) — the latter backfills the RT score
        // for titles scanned before OMDb was configured. A fully-enriched title is
        // skipped, so re-scans stay cheap.
        let has_overview = entity.overview.as_deref().is_some_and(|o| !o.is_empty());
        let wants_rating = omdb_on
            && self.omdb.as_ref().is_some_and(|o| o.is_enabled())
            && entity.critic_rating.is_none();
        // Also fetch when the row still carries no remote trailers, so titles
        // scanned before trailers were persisted backfill their YouTube links
        // (the client's Trailer button is gated on them).
        // ponytail: a title TMDB has no trailer for re-fetches on every scan,
        // exactly like the RT backfill above. Store a "checked" marker if that
        // ever costs real time.
        let wants_trailers =
            crate::item_data::read_remote_trailers(entity.data.as_deref()).is_empty();
        if has_overview && !wants_rating && !wants_trailers {
            return Some(RemoteMetadata::default());
        }
        let name = entity.name.clone().filter(|n| !n.is_empty())?;
        let name = name.as_str();
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
        let tmdb_id = tmdb
            .search(kind, name, year)
            .await
            .into_iter()
            .next()
            .map(|h| h.tmdb_id)?;
        let details = tmdb.details(kind, tmdb_id).await?;
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
        // `Some(..)` because this fn returns Option: None is a TMDB miss the
        // fetcher-order gate falls back from (G1.3); main's tmdb_people helper
        // replaces the inline people mapping this branch used to carry.
        Some(RemoteMetadata {
            people: tmdb_people(&details.people),
            provider_ids,
        })
    }

    /// The people credited on ONE episode: TMDB's per-episode credits when the
    /// series carries a TMDB id, else TVDB's episode credits.
    ///
    /// Port of `TmdbEpisodeProvider`: an episode's Cast & Crew is that
    /// episode's own credits — the regulars credited in it, its guest stars,
    /// then its crew. The series' full regular cast is deliberately NOT merged
    /// in: doing that made every episode page show the series list verbatim,
    /// burying the guest stars and director the page exists to show.
    async fn episode_people(
        &self,
        series_tmdb_id: Option<i64>,
        season: i32,
        number: i32,
        ep: &ferrofin_providers::TvdbEpisodeDetails,
    ) -> Vec<PeopleEntity> {
        if let (Some(tmdb), Some(series_id)) = (&self.tmdb, series_tmdb_id) {
            let credits = tmdb.episode_credits(series_id, season, number).await;
            if !credits.is_empty() {
                return tmdb_people(&credits);
            }
        }
        tvdb_people(&ep.people)
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
                let Some(hit) = pick_series_hit(tvdb.search(name, year).await, year) else {
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
                // Cast & Crew on an episode page is the EPISODE's credits, not
                // the series'. Upstream reads TMDB's per-episode credits (the
                // regulars credited in that episode, its guest stars, then its
                // crew), so prefer those; TVDB's episode credits (guest cast +
                // director/writers) are the fallback when the series has no
                // TMDB id or TMDB has nothing for the episode.
                let series_tmdb_id = cache
                    .series_tvdb
                    .get(&series_id)
                    .and_then(|d| d.tmdb_id.as_deref())
                    .and_then(|id| id.parse::<i64>().ok());
                let people = self
                    .episode_people(series_tmdb_id, season, number, &ep)
                    .await;
                RemoteMetadata::just_people(people)
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
                let mut infos = download_images(
                    tmdb,
                    &dir,
                    &id,
                    vec![RemoteImage {
                        image_type: ImageType::Primary,
                        url,
                    }],
                )
                .await;
                // Probe the file once, here, exactly as every other image-persisting
                // path does. Without it a person image is stored 0x0, and because
                // nothing caches a probed dimension, `DtoService` re-opens and
                // re-parses that JPEG header on *every* request that asks for
                // PrimaryImageAspectRatio — a whole page of blocking probes per
                // request, forever. (Jellyfin gets away with the lazy probe because
                // it writes the result back onto its in-memory BaseItem; Ferrofin
                // reloads image rows from the DB each time, so the DB is where the
                // answer has to live.)
                self.fill_image_metadata(&mut infos).await;
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

    /// Downloads the artwork-repository thumb for every Studio row still
    /// without images (port of upstream's `StudiosImageProvider`). Idempotent:
    /// studios with any image row are skipped, and downloads reuse on-disk
    /// files. Best-effort per studio — one failure skips that studio only.
    async fn enrich_studio_images(&self) -> Result<(), ServiceError> {
        let (Some(studios), Some(repo), Some(meta_root)) = (
            &self.studios_client,
            &self.item_repository,
            &self.metadata_dir,
        ) else {
            return Ok(());
        };
        let result = repo
            .get_studios(&ferrofin_traits::options::InternalItemsQuery::default())
            .await?;
        for row in result.items {
            let entity = row.item;
            let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(&entity.id) else {
                continue;
            };
            if !repo.get_image_infos(id).await?.is_empty() {
                continue;
            }
            let Some(url) = studios.thumb_url(name).await else {
                continue;
            };
            let dir = meta_root.join(id.to_string());
            let stem = image_type_file_stem(ImageType::Thumb);
            let dest = if let Some(existing) = existing_art_file(&dir, stem) {
                existing
            } else {
                let dest = dir.join(format!("{stem}.jpg"));
                let Some(bytes) = studios.download(&url).await else {
                    continue;
                };
                if let Err(err) =
                    std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&dest, &bytes))
                {
                    tracing::warn!(%err, studio = name, "failed to write studio thumb");
                    continue;
                }
                dest
            };
            let mut images = vec![ItemImageInfo {
                path: dest.to_string_lossy().into_owned(),
                image_type: ImageType::Thumb,
                date_modified: file_date_modified(&dest),
                width: 0,
                height: 0,
                blur_hash: None,
            }];
            self.fill_image_metadata(&mut images).await;
            if let Err(err) = self.persistence.save_item_images(id, &images).await {
                tracing::warn!(%err, studio = name, "failed to persist studio thumb");
            }
        }
        Ok(())
    }

    /// Extracts a track's embedded cover art (ID3 `APIC` / FLAC picture) into
    /// the item's metadata dir as its `Primary` image, when the probe found an
    /// embedded-image stream and the file has no local artwork.
    ///
    /// Port of upstream's `AudioImageProvider`. Best-effort: no encoder, no
    /// metadata dir, no image stream, or a failed extraction all yield nothing.
    /// Idempotent — an already-extracted file is reused without re-running
    /// ffmpeg.
    async fn extract_embedded_cover(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        streams: &[MediaStreamInfoEntity],
    ) -> Vec<ItemImageInfo> {
        if image_item_kind(&entity.type_) != ImageItemKind::Audio {
            return Vec::new();
        }
        let (Some(encoder), Some(meta_root), Some(path)) = (
            &self.media_encoder,
            &self.metadata_dir,
            entity.path.as_deref(),
        ) else {
            return Vec::new();
        };
        // `StreamType` 3 is EmbeddedImage (the probe's classification of an
        // attached picture); without one there is nothing to extract.
        let Some(index) = streams
            .iter()
            .find(|s| s.stream_type == EMBEDDED_IMAGE_STREAM_TYPE)
            .map(|s| s.stream_index)
        else {
            return Vec::new();
        };
        let dir = meta_root.join(item_id.to_string());
        let stem = image_type_file_stem(ImageType::Primary);
        let dest = dir.join(format!("{stem}.jpg"));
        if !dest.exists() {
            let index = i32::try_from(index).ok();
            let Ok(extracted) = encoder.extract_audio_image(path, index).await else {
                return Vec::new();
            };
            // ffmpeg writes next to the media file; move it into the metadata
            // dir so the user's library stays untouched.
            if let Err(err) = std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::copy(&extracted, &dest).map(|_| ()))
            {
                tracing::warn!(%err, item = %item_id, "failed to store embedded cover art");
                let _ = std::fs::remove_file(&extracted);
                return Vec::new();
            }
            let _ = std::fs::remove_file(&extracted);
        }
        vec![ItemImageInfo {
            path: dest.to_string_lossy().into_owned(),
            image_type: ImageType::Primary,
            date_modified: file_date_modified(&dest),
            width: 0,
            height: 0,
            blur_hash: None,
        }]
    }

    /// Gives an album with no artwork of its own its first track's image
    /// (upstream's `AlbumImageProvider`, which takes the album's cover from a
    /// child song). Best-effort; runs in the post-scan music pass, once every
    /// track's own art exists.
    async fn inherit_album_cover(&self, album_uuid: Uuid, items: &dyn ItemRepository) {
        let Ok(existing) = items.get_image_infos(album_uuid).await else {
            return;
        };
        if !existing.is_empty() {
            return;
        }
        let Ok(tracks) = items
            .get_item_list(&InternalItemsQuery {
                parent_id: album_uuid,
                include_item_types: vec![BaseItemKind::Audio],
                ..Default::default()
            })
            .await
        else {
            return;
        };
        for track in tracks {
            let Ok(track_id) = Uuid::parse_str(&track.id) else {
                continue;
            };
            let Ok(images) = items.get_image_infos(track_id).await else {
                continue;
            };
            // Keep looking until a track actually has a Primary image.
            let Some(primary) = images
                .into_iter()
                .find(|i| i.image_type == ImageType::Primary)
            else {
                continue;
            };
            if let Err(err) = self
                .persistence
                .save_item_images(album_uuid, std::slice::from_ref(&primary))
                .await
            {
                tracing::warn!(%err, album = %album_uuid, "failed to inherit album cover");
            }
            return;
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
        art: ArtworkPass<'_>,
        art_cache: &mut ArtworkCache,
    ) {
        let ArtworkPass {
            entity,
            streams,
            policy,
            embedded_images,
        } = art;
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        // A photo's own file IS its Primary image (C# `PhotoProvider` sets it
        // before any discovery runs) and a book's cover comes out of its own
        // archive, so neither needs the *remote* chain. They still get local
        // discovery, the metadata art dir (uploads and earlier downloads) and
        // the dynamic providers, exactly like every other kind — upstream runs
        // `ILocalImageProvider` ahead of the embedded-cover providers, so a
        // sidecar `folder.jpg` wins over the extracted cover.
        if !embedded_images.is_empty() {
            let mut images = if policy.image_enabled(short, fetcher_names::LOCAL_IMAGES) {
                discover_local_images(entity)
            } else {
                Vec::new()
            };
            // Local discovery wins per type; the embedded cover fills what it
            // left empty (`dedup_images_by_type` is the `RemoteImage` twin of
            // this, which cannot be reused for `ItemImageInfo`).
            let mut seen: std::collections::HashSet<ImageType> =
                images.iter().map(|i| i.image_type).collect();
            images.extend(
                embedded_images
                    .into_iter()
                    .filter(|i| seen.insert(i.image_type)),
            );
            self.append_art_dir_images(entity, &mut images);
            self.apply_dynamic_images(entity, &mut images, policy).await;
            self.fill_image_metadata(&mut images).await;
            if let Err(err) = self.persistence.save_item_images(item_id, &images).await {
                tracing::warn!(%err, item = %item_id, "failed to persist embedded artwork");
            }
            return;
        }
        // "Local Images" is the media-adjacent discovery (poster.jpg next
        // to the file); the metadata art dir below is Ferrofin-owned
        // (uploads + earlier downloads) and is never gated.
        let mut images = if policy.image_enabled(short, fetcher_names::LOCAL_IMAGES) {
            discover_local_images(entity)
        } else {
            Vec::new()
        };
        if images.is_empty() && policy.image_enabled(short, fetcher_names::EMBEDDED_IMAGES) {
            images = self.extract_embedded_cover(item_id, entity, streams).await;
        }
        if images.is_empty() {
            images = self.fetch_remote_images(entity, art_cache, policy).await;
        }
        self.append_art_dir_images(entity, &mut images);
        self.apply_dynamic_images(entity, &mut images, policy).await;
        self.fill_image_metadata(&mut images).await;
        if !images.is_empty()
            && let Err(err) = self.persistence.save_item_images(item_id, &images).await
        {
            tracing::warn!(%err, item = %item_id, "failed to persist discovered artwork");
        }
    }

    /// Dynamic (Tier-1b WASM plugin) artwork pass: for the Primary/Backdrop
    /// slots the built-in chain left empty, ask each dynamic provider for
    /// image BYTES (the plugin host downloads candidates through the
    /// plugin's declared egress — the scanner never sees a URL) and write
    /// them exactly like the built-in downloads (`{metadata}/library/{id}/
    /// {stem}.jpg`). First provider to fill a slot wins; art already on
    /// disk (an earlier scan or a user upload) always wins. Best-effort per
    /// provider — one bad plugin never fails a scan.
    async fn apply_dynamic_images(
        &self,
        entity: &BaseItemEntity,
        images: &mut Vec<ItemImageInfo>,
        policy: FetcherPolicy<'_>,
    ) {
        if self.dynamic_providers.is_empty() {
            return;
        }
        let Some(meta_root) = &self.metadata_dir else {
            return;
        };
        let item_dir = meta_root.join(&entity.id);
        let mut wanted: Vec<ImageType> = [ImageType::Primary, ImageType::Backdrop]
            .into_iter()
            .filter(|kind| !images.iter().any(|i| i.image_type == *kind))
            .filter(|kind| existing_art_file(&item_dir, image_type_file_stem(*kind)).is_none())
            .collect();
        if wanted.is_empty() {
            return;
        }
        // Same rule as the metadata pass: an unparseable row id never
        // reaches a guest.
        let Ok(item_id) = Uuid::parse_str(&entity.id) else {
            return;
        };
        let lookup = ferrofin_traits::providers::DynamicMetadataLookup {
            item_id,
            kind: entity
                .type_
                .rsplit('.')
                .next()
                .unwrap_or(&entity.type_)
                .to_owned(),
            name: entity.name.clone().unwrap_or_default(),
            production_year: entity.production_year.and_then(|y| i32::try_from(y).ok()),
            path: entity.path.clone(),
            provider_ids: Vec::new(),
        };
        // Artwork is a NAMED-provider surface (the plan scopes it to plugins
        // that declared provider-info — everything else always returns `[]`,
        // so asking would be a wasted guest round-trip per item per scan),
        // under the same admin control as the metadata pass: the library's
        // image-fetcher checkboxes/order.
        let mut sources: Vec<_> = self
            .dynamic_providers
            .iter()
            .filter(|p| p.library_gated() && policy.image_enabled(&lookup.kind, p.name()))
            .collect();
        sources.sort_by_key(|p| policy.image_rank(&lookup.kind, p.name()));
        for provider in sources {
            if wanted.is_empty() {
                return;
            }
            let contributed = match provider.images(&lookup, &wanted).await {
                Ok(contributed) => contributed,
                Err(err) => {
                    tracing::warn!(
                        provider = provider.name(),
                        item = %lookup.item_id,
                        %err,
                        "dynamic image provider failed; continuing"
                    );
                    continue;
                }
            };
            for (kind, bytes) in contributed {
                if !wanted.contains(&kind) {
                    continue;
                }
                // Sniff before persisting: an undecodable blob would land as
                // a permanent 0×0 "image" (art on disk always wins, so the
                // slot never recovers). Store under the sniffed format's
                // extension — a PNG as `.png`, not a mislabeled `.jpg`; the
                // art-dir readers accept every ART_FILE_EXTENSIONS entry.
                let Some(ext) = sniff_image_ext(&bytes) else {
                    tracing::warn!(
                        provider = provider.name(),
                        item = %entity.id,
                        kind = ?kind,
                        "plugin artwork bytes are not a recognized image format; skipping"
                    );
                    continue;
                };
                let dest = item_dir.join(format!("{}.{ext}", image_type_file_stem(kind)));
                if let Err(err) =
                    std::fs::create_dir_all(&item_dir).and_then(|()| std::fs::write(&dest, &bytes))
                {
                    tracing::warn!(%err, item = %entity.id, "failed to write plugin artwork");
                    continue;
                }
                images.push(ItemImageInfo {
                    path: dest.to_string_lossy().into_owned(),
                    image_type: kind,
                    date_modified: file_date_modified(&dest),
                    width: 0,
                    height: 0,
                    blur_hash: None,
                });
                wanted.retain(|k| *k != kind);
            }
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
            // Upstream samples the collection type's own kinds (a music
            // library's tile comes from its ALBUMS, whose covers are the art
            // that exists — sampling leaf tracks found nothing and left the
            // note-icon placeholder). The leaf fallback keeps a library whose
            // typed rows have no art from losing its tile.
            let kinds = collage_item_kinds(folder);
            let sample = |include_item_types: Vec<BaseItemKind>, is_folder| InternalItemsQuery {
                ancestor_ids: vec![cf],
                recursive: true,
                include_item_types,
                is_folder,
                is_virtual_item: Some(false),
                limit: Some(LIBRARY_COLLAGE_SOURCES * 3),
                order_by: vec![(
                    ferrofin_model::live_tv::ItemSortBy::Random,
                    ferrofin_model::dto::SortOrder::Descending,
                )],
                ..Default::default()
            };
            let mut candidates = items.get_item_ids(&sample(kinds, None)).await?;
            if candidates.is_empty() {
                candidates = items.get_item_ids(&sample(Vec::new(), Some(false))).await?;
            }
            let mut inputs = Vec::new();
            for id in candidates {
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
        policy: FetcherPolicy<'_>,
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
                let mut images = if policy.image_enabled(short, fetcher_names::TMDB) {
                    tmdb.images_for(TmdbKind::Movie, name, year).await
                } else {
                    Vec::new()
                };
                // fanart.tv supplements TMDB's poster/backdrop with the types it
                // lacks (logo/clear-art/disc/banner), keyed off the movie's
                // Tmdb/Imdb id persisted earlier this scan.
                if policy.image_enabled(short, fetcher_names::FANART)
                    && let Some(fanart) = &self.fanart
                    && let Some(id) = cache
                        .item_provider_ids
                        .get(&entity.id)
                        .and_then(|ids| fanart_movie_id(ids))
                {
                    append_fanart(&mut images, fanart.movie_images(&id).await);
                }
                // OMDb's poster is the last-resort Primary (C# `Order = 90`,
                // "after other internet providers, because they're better").
                // Appending it last means the dedup keeps it only when nothing
                // above supplied a Primary. The URL was captured during the
                // metadata pass, so this costs no extra request.
                append_omdb_poster(&mut images, entity, cache, policy, short);
                download_images(tmdb, &item_dir, &entity.id, dedup_images_by_type(images)).await
            }
            "Series" => {
                // TVDB is the TV authority: when it matched this series during the
                // metadata pass, reuse its artwork (no second fetch); else fall
                // back to a TMDB series match. The Tvdb id (when present) also
                // keys fanart's series artwork.
                let tvdb_id = cache.series_tvdb.get(&entity.id).map(|d| d.tvdb_id);
                let tvdb_art = policy
                    .image_enabled(short, fetcher_names::TVDB)
                    .then(|| cache.series_tvdb.get(&entity.id))
                    .flatten();
                let mut images = if let Some(details) = tvdb_art {
                    details.download_images()
                } else if policy.image_enabled(short, fetcher_names::TMDB) {
                    let Some(name) = entity.name.as_deref().filter(|n| !n.is_empty()) else {
                        return Vec::new();
                    };
                    let Some(matched) = tmdb.series_match(name, year).await else {
                        return Vec::new();
                    };
                    // Remember the TMDB id so this series' seasons/episodes resolve.
                    cache.series_tmdb.insert(entity.id.clone(), matched.tmdb_id);
                    matched.images
                } else {
                    Vec::new()
                };
                if policy.image_enabled(short, fetcher_names::FANART)
                    && let (Some(fanart), Some(tvdb_id)) = (&self.fanart, tvdb_id)
                {
                    append_fanart(
                        &mut images,
                        fanart.series_images(&tvdb_id.to_string()).await,
                    );
                }
                download_images(tmdb, &item_dir, &entity.id, dedup_images_by_type(images)).await
            }
            "Season" | "Episode" => {
                self.fetch_tv_still_images(entity, short, cache, tmdb, &item_dir, policy)
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
        policy: FetcherPolicy<'_>,
    ) -> Vec<ItemImageInfo> {
        if short == "Season" {
            // Season posters (and the episode-still cache) are TMDB's.
            if !policy.image_enabled(short, fetcher_names::TMDB) {
                return Vec::new();
            }
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
        // fall back to the TMDB season-stills cache, and to OMDb's poster last
        // (C# `OmdbImageProvider.Supports` covers Movie, Trailer and Episode).
        let url = policy
            .image_enabled(short, fetcher_names::TVDB)
            .then(|| cache.episode_tvdb_still.get(&entity.id).cloned())
            .flatten()
            .or_else(|| {
                if !policy.image_enabled(short, fetcher_names::TMDB) {
                    return None;
                }
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
        let mut images = url
            .map(|url| {
                vec![RemoteImage {
                    image_type: ImageType::Primary,
                    url,
                }]
            })
            .unwrap_or_default();
        append_omdb_poster(&mut images, entity, cache, policy, short);
        if images.is_empty() {
            return Vec::new();
        }
        download_images(tmdb, item_dir, &entity.id, dedup_images_by_type(images)).await
    }

    /// The synchronous plan pass: resolve every library's files into [`Planned`]
    /// items. Owns the `NamingOptions` so its `!Sync` cells stay off the async path.
    ///
    /// Dispatches by the library's collection type: `tvshows` builds the
    /// Series→Season→Episode hierarchy, `music` builds MusicAlbum→Audio, and the
    /// video types (`movies`/`homevideos`/`musicvideos`/`mixed`) plus an untyped
    /// library flatten every video file to a `Movie`, and `books` flattens every
    /// document to a `Book` and every audio file to an `AudioBook`. `boxsets` is
    /// the one remaining type that is not scanned (its members are curated
    /// through the collection API, not resolved off disk). Upstream's separate
    /// `photos` type has no `CollectionTypeOptions` variant here — Ferrofin
    /// folds it into `homevideos`.
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
                    Some(CollectionTypeOptions::books) => {
                        self.plan_books(location, location, cf, &naming, &mut out);
                    }
                    None
                    | Some(
                        CollectionTypeOptions::movies
                        | CollectionTypeOptions::homevideos
                        | CollectionTypeOptions::musicvideos
                        | CollectionTypeOptions::mixed,
                    ) => {
                        self.plan_movies(location, location, cf, &naming, &mut out);
                        // Upstream folds the separate `photos` collection type
                        // into `homevideos`, where photos are resolved only when
                        // the library enables them (`PhotoResolver.Resolve`).
                        if folder.collection_type == Some(CollectionTypeOptions::homevideos)
                            && folder
                                .library_options
                                .as_ref()
                                .is_none_or(|o| o.enable_photos)
                        {
                            self.plan_photos(location, location, cf, cf, &naming, &mut out);
                        }
                    }
                    // `boxsets` is the only type left: a BoxSet's members are
                    // curated through the collection API, not resolved off disk.
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

    /// Video library: every video file becomes a `Movie` directly under the
    /// collection folder (per-title folders are recursed into) — except files
    /// the naming rules classify as EXTRAS (`-trailer` suffixes, `trailers/` /
    /// `theme-music/` / `extras/` directories …), which become owned rows
    /// (`OwnerId` + `ExtraType`) attached to the movie they belong to. Owned
    /// rows are what `/Items/{id}/LocalTrailers`, `/SpecialFeatures`, and the
    /// hasTrailer/hasThemeSong/… filters read; the browse queries' "unowned"
    /// predicate keeps them out of the library grid.
    fn plan_movies(
        &self,
        dir: &str,
        root: &str,
        cf: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let mut extras: Vec<(String, ferrofin_model::entities::ExtraType)> = Vec::new();
        // Movies emitted per directory, for extras owner resolution.
        let mut movies_by_dir: std::collections::HashMap<String, Vec<(Uuid, String)>> =
            std::collections::HashMap::new();
        self.collect_movie_plan(dir, root, cf, naming, out, &mut extras, &mut movies_by_dir);
        for (path, extra_type) in extras {
            let Some(owner) = owner_for_extra(&path, &movies_by_dir) else {
                continue; // an extra with no resolvable movie is skipped
            };
            let Some((id, mut entity)) =
                self.base_item(BaseItemKind::Video, cf, cf, file_stem(&path), &path, false)
            else {
                continue;
            };
            entity.media_type = Some("Video".to_owned());
            entity.extra_type = Some(extra_type as i32);
            entity.owner_id = Some(guid_to_db(owner));
            out.push(Planned {
                id,
                entity,
                ancestors: vec![cf],
            });
        }
    }

    /// The recursive walk behind [`Self::plan_movies`]: emits movie rows,
    /// collects extras for post-resolution, and records each directory's
    /// movies for extras ownership.
    #[allow(clippy::too_many_arguments)]
    fn collect_movie_plan(
        &self,
        dir: &str,
        root: &str,
        cf: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
        extras: &mut Vec<(String, ferrofin_model::entities::ExtraType)>,
        movies_by_dir: &mut std::collections::HashMap<String, Vec<(Uuid, String)>>,
    ) {
        // A disc rip (`…/Movie (2009)/BDMV/` or `VIDEO_TS/`) is ONE item for the
        // containing folder, not one per .vob/.m2ts inside — port of
        // `BaseVideoResolver.ResolveVideo`'s directory arm.
        if dir != root
            && let Some(video_type) = self.disc_video_type(dir)
        {
            self.plan_disc_movie(dir, cf, video_type, out);
            return;
        }
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.collect_movie_plan(&entry.path, root, cf, naming, out, extras, movies_by_dir);
                continue;
            }
            if !video_resolver::is_video_file(&entry.path, naming) {
                continue;
            }
            // Extras never become movies (upstream resolves extras first).
            let extra = ferrofin_naming::video::extra_rule_resolver::get_extra_info(
                &entry.path,
                naming,
                Some(root),
            );
            if let Some(extra_type) = extra.extra_type {
                extras.push((entry.path.clone(), extra_type));
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
            set_video_type(&mut entity, file_video_type(&entry.path));
            movies_by_dir
                .entry(dir.to_owned())
                .or_default()
                .push((id, file_stem(&entry.path)));
            out.push(Planned {
                id,
                entity,
                ancestors: vec![cf],
            });
        }
    }

    /// The disc structure `dir` holds, if any: a `VIDEO_TS` subfolder with
    /// `.vob` files is a DVD rip, a `BDMV` subfolder a Blu-ray rip. Port of
    /// `IsDvdDirectory` / `IsBluRayDirectory`.
    fn disc_video_type(&self, dir: &str) -> Option<VideoType> {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ != FileSystemEntryType::Directory {
                continue;
            }
            let name = file_stem(&entry.path);
            if name.eq_ignore_ascii_case("BDMV") {
                return Some(VideoType::BluRay);
            }
            if name.eq_ignore_ascii_case("VIDEO_TS")
                && self
                    .file_system
                    .get_file_system_entries(&entry.path)
                    .iter()
                    .any(|f| {
                        f.type_ != FileSystemEntryType::Directory
                            && std::path::Path::new(&f.path)
                                .extension()
                                .is_some_and(|e| e.eq_ignore_ascii_case("vob"))
                    })
            {
                return Some(VideoType::Dvd);
            }
        }
        None
    }

    /// Emits the single `Movie` row for a disc-rip folder (the folder itself is
    /// the item, as upstream resolves it).
    fn plan_disc_movie(&self, dir: &str, cf: Uuid, video_type: VideoType, out: &mut Vec<Planned>) {
        let name = folder_name(dir).unwrap_or_else(|| file_stem(dir));
        let Some((id, mut entity)) = self.base_item(BaseItemKind::Movie, cf, cf, name, dir, true)
        else {
            return;
        };
        entity.is_movie = true;
        entity.media_type = Some("Video".to_owned());
        set_video_type(&mut entity, video_type);
        out.push(Planned {
            id,
            entity,
            ancestors: vec![cf],
        });
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
                    e.sort_name = Some(season_sort_name(e.index_number, &name));
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
            e.sort_name = Some(season_sort_name(
                e.index_number,
                e.name.as_deref().unwrap_or_default(),
            ));
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
        // Episodes sort by position, not title (`Episode.CreateSortName`) — the
        // numbers are only known here, after the resolver ran.
        entity.sort_name = Some(episode_sort_name(
            entity.parent_index_number,
            entity.index_number,
            entity.name.as_deref().unwrap_or_default(),
        ));
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
        // The library root itself is the CollectionFolder — never an artist or
        // an album (upstream's `args.Parent.IsRoot` guard), so recurse into it.
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_music_node(&entry.path, cf, naming, out);
            }
        }
        // Loose audio directly in the library root still becomes an album, so
        // stray files are browsable rather than invisible.
        self.plan_music_album(dir, cf, naming, out);
    }

    /// Persists an item's probed chapter markers, when there are any and a
    /// chapter repository is wired.
    async fn save_chapters(
        &self,
        item_id: Uuid,
        chapters: &[ferrofin_db::entities::base_items::ChapterEntity],
    ) -> Result<(), ServiceError> {
        if let (false, Some(repo)) = (chapters.is_empty(), &self.chapters) {
            repo.save_chapters(item_id, chapters).await?;
        }
        Ok(())
    }

    /// Emits the bounded per-item progress line (RULES_LOGGING volume rule);
    /// a `progress_every` of `0` disables it.
    fn log_scan_progress(&self, scanned: usize, total: usize) {
        if scanned > 0 && self.progress_every > 0 && scanned.is_multiple_of(self.progress_every) {
            tracing::info!(scanned, total, "library scan progress");
        }
    }

    /// The embedded-metadata passes for the kinds whose metadata lives inside
    /// the file rather than on a remote provider: photos (EXIF) and books
    /// (`ComicInfo`/`ComicBookInfo`/OPF). Returns the credits and the image
    /// extracted from the file, for the artwork pass.
    async fn enrich_from_file(
        &self,
        entity: &mut BaseItemEntity,
        locked: bool,
    ) -> (Vec<PeopleEntity>, Vec<ItemImageInfo>) {
        let mut images = self.enrich_photo(entity, locked).await;
        let (people, book_images) = self.enrich_book(entity, locked).await;
        images.extend(book_images);
        (people, images)
    }

    /// The book embedded-metadata pass — port of `ComicProvider`,
    /// `ComicBookInfoProvider`, `EpubProvider` and `OpfProvider`, plus the two
    /// cover extractors.
    ///
    /// Fills only what the row still lacks, like every other local reader, and
    /// returns the cast (writer/penciller/…) plus the cover image to persist.
    async fn enrich_book(
        &self,
        entity: &mut BaseItemEntity,
        locked: bool,
    ) -> (Vec<PeopleEntity>, Vec<ItemImageInfo>) {
        if !entity.type_.ends_with(".Book") || locked {
            return (Vec::new(), Vec::new());
        }
        let Some(path) = entity.path.clone().filter(|p| !p.is_empty()) else {
            return (Vec::new(), Vec::new());
        };
        let people = match ferrofin_providers::read_book_metadata(&path) {
            Some(book) => {
                apply_book(entity, &book);
                book_people(&book)
            }
            None => Vec::new(),
        };
        (people, self.extract_book_cover(entity, &path).await)
    }

    /// Writes a book's embedded cover into its metadata art directory and
    /// returns the image row — the shape `download_images` produces for a
    /// remote fetch, so the artwork pass treats both the same.
    async fn extract_book_cover(&self, entity: &BaseItemEntity, path: &str) -> Vec<ItemImageInfo> {
        let Some(meta_root) = &self.metadata_dir else {
            return Vec::new();
        };
        let item_dir = meta_root.join(&entity.id);
        if let Some(existing) = existing_art_file(&item_dir, "primary") {
            return vec![ItemImageInfo {
                path: existing.to_string_lossy().into_owned(),
                image_type: ImageType::Primary,
                date_modified: file_date_modified(&existing),
                width: 0,
                height: 0,
                blur_hash: None,
            }];
        }
        let Some((name, bytes)) = ferrofin_providers::read_book_cover(path) else {
            return Vec::new();
        };
        let extension = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_ascii_lowercase();
        if tokio::fs::create_dir_all(&item_dir).await.is_err() {
            return Vec::new();
        }
        let out = item_dir.join(format!("primary.{extension}"));
        if let Err(err) = tokio::fs::write(&out, &bytes).await {
            tracing::warn!(%err, item = %entity.id, "failed to write book cover");
            return Vec::new();
        }
        vec![ItemImageInfo {
            path: out.to_string_lossy().into_owned(),
            image_type: ImageType::Primary,
            date_modified: file_date_modified(&out),
            width: 0,
            height: 0,
            blur_hash: None,
        }]
    }

    /// Resolves the photos under `dir` — port of `PhotoResolver` and
    /// `PhotoAlbumResolver`.
    ///
    /// Every image file that is not artwork owned by a sibling video becomes a
    /// `Photo`; a **sub**directory holding at least one such image becomes a
    /// `PhotoAlbum` its photos hang off (the library root is the collection
    /// folder itself and never an album, matching upstream's `args.Parent.IsRoot`
    /// guard elsewhere in the resolver chain).
    fn plan_photos(
        &self,
        dir: &str,
        root: &str,
        cf: Uuid,
        parent: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let entries = self.file_system.get_file_system_entries(dir);
        let files: Vec<&str> = entries
            .iter()
            .filter(|e| e.type_ != FileSystemEntryType::Directory)
            .map(|e| e.path.as_str())
            .collect();
        let photos: Vec<&str> = files
            .iter()
            .copied()
            .filter(|path| is_photo_file(path))
            .filter(|path| !is_owned_by_media(&files, path, naming))
            .collect();

        // A sub-directory with photos in it is a PhotoAlbum; the library root
        // is the collection folder itself and never an album, so its loose
        // photos hang off it directly.
        let (photo_parent, ancestors) = if !photos.is_empty() && dir != root {
            let name = file_stem(dir);
            match self.base_item(BaseItemKind::PhotoAlbum, cf, parent, name, dir, true) {
                Some((album_id, album)) => {
                    let mut ancestors = vec![cf];
                    if parent != cf {
                        ancestors.push(parent);
                    }
                    out.push(Planned {
                        id: album_id,
                        entity: album,
                        ancestors: ancestors.clone(),
                    });
                    ancestors.push(album_id);
                    (album_id, ancestors)
                }
                None => (parent, vec![cf]),
            }
        } else {
            (parent, vec![cf])
        };

        for path in photos {
            let Some((id, mut entity)) = self.base_item(
                BaseItemKind::Photo,
                cf,
                photo_parent,
                file_stem(path),
                path,
                false,
            ) else {
                continue;
            };
            entity.media_type = Some("Photo".to_owned());
            out.push(Planned {
                id,
                entity,
                ancestors: ancestors.clone(),
            });
        }

        for entry in &entries {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_photos(&entry.path, root, cf, photo_parent, naming, out);
            }
        }
    }

    /// Resolves one directory beneath a music library: a `MusicAlbum` (port of
    /// `MusicAlbumResolver`), or a container to walk through — an artist folder
    /// or one of its release subfolders (`albums`, `live`, …).
    ///
    /// An artist folder is walked, not turned into a row: the browsable
    /// `MusicArtist` item comes from the by-name materializer, keyed by its
    /// `ItemValues` id, and emitting a second path-keyed row here would list
    /// every artist twice (the duplicate-identity trap the person work had to
    /// unwind). Unifying the two identities is the prerequisite for resolving
    /// the folder itself — see `brain/plans/PLAN_MUSIC_LIBRARY.md`.
    fn plan_music_node(&self, dir: &str, cf: Uuid, naming: &NamingOptions, out: &mut Vec<Planned>) {
        if self.is_music_album(dir, naming, true) {
            self.plan_music_album(dir, cf, naming, out);
            return;
        }
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_music_node(&entry.path, cf, naming, out);
            }
        }
    }

    /// Emits the `MusicAlbum` row for `dir` plus its tracks — the audio files
    /// directly inside, and those in any multi-disc subfolder (`CD1`, `Disc 2`,
    /// …), which fold into the same album rather than becoming albums of their
    /// own.
    fn plan_music_album(
        &self,
        dir: &str,
        cf: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let tracks = self.collect_album_tracks(dir, naming);
        if tracks.is_empty() {
            return;
        }
        let album_name = file_stem(dir);
        let Some((album_id, mut album)) = self.base_item(
            BaseItemKind::MusicAlbum,
            cf,
            cf,
            album_name.clone(),
            dir,
            true,
        ) else {
            return;
        };
        // Upstream keys an album by "{AlbumArtist}-{Name}"; the album-artist is
        // only known once the tracks are tagged, so the artist folder's name
        // stands in when there is one (`enrich_one_album` owns the rest).
        album.presentation_unique_key = Some(album_name.clone());
        out.push(Planned {
            id: album_id,
            entity: album,
            ancestors: vec![cf],
        });
        let ancestors = vec![cf, album_id];
        for track in tracks {
            let Some((id, mut entity)) = self.base_item(
                BaseItemKind::Audio,
                cf,
                album_id,
                file_stem(&track),
                &track,
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
                ancestors: ancestors.clone(),
            });
        }
    }

    /// The audio files belonging to the album at `dir`: those directly inside,
    /// plus those in each multi-disc subfolder.
    fn collect_album_tracks(&self, dir: &str, naming: &NamingOptions) -> Vec<String> {
        let entries = self.file_system.get_file_system_entries(dir);
        let mut tracks: Vec<String> = entries
            .iter()
            .filter(|e| e.type_ != FileSystemEntryType::Directory && is_audio_file(&e.path, naming))
            .map(|e| e.path.clone())
            .collect();
        let parser = ferrofin_naming::audio::AlbumParser::new(naming);
        for entry in &entries {
            if entry.type_ == FileSystemEntryType::Directory && parser.is_multi_part(&entry.path) {
                tracks.extend(
                    self.file_system
                        .get_file_system_entries(&entry.path)
                        .into_iter()
                        .filter(|e| {
                            e.type_ != FileSystemEntryType::Directory
                                && is_audio_file(&e.path, naming)
                        })
                        .map(|e| e.path),
                );
            }
        }
        tracks
    }

    /// Whether `dir` resolves as a music album — port of
    /// `MusicAlbumResolver.ContainsMusic`: any audio file directly inside, or
    /// (with `allow_subfolders`) at least one multi-disc subfolder holding
    /// music and no non-disc subfolder holding music.
    fn is_music_album(&self, dir: &str, naming: &NamingOptions, allow_subfolders: bool) -> bool {
        let entries = self.file_system.get_file_system_entries(dir);
        if entries
            .iter()
            .any(|e| e.type_ != FileSystemEntryType::Directory && is_audio_file(&e.path, naming))
        {
            return true;
        }
        if !allow_subfolders {
            return false;
        }
        let parser = ferrofin_naming::audio::AlbumParser::new(naming);
        let mut disc_subfolders = 0;
        for entry in &entries {
            if entry.type_ != FileSystemEntryType::Directory {
                continue;
            }
            if !self.is_music_album(&entry.path, naming, false) {
                continue;
            }
            if parser.is_multi_part(&entry.path) {
                disc_subfolders += 1;
            } else {
                // Music in a non-disc subfolder → this is an artist, not an
                // album.
                return false;
            }
        }
        disc_subfolders > 0
    }

    /// Books library: every document flattens to a `Book` and every audio file
    /// to an `AudioBook`, both directly under the collection folder.
    ///
    /// Port of `BookResolver` + the `books` arms of `AudioResolver`. The order
    /// of the two directory checks is upstream's resolver priority:
    /// `BookResolver` is `ResolverPriority.First`, `AudioResolver` `Fifth`, so a
    /// folder holding both one document and audio is the book.
    /// - a directory holding **exactly one** document *is* that book, named
    ///   after the directory ("other library structures with multiple books to
    ///   a directory will get picked up as individual files"). A resolved book
    ///   is not a `Folder`, so the directory is not descended into — same as
    ///   upstream;
    /// - a directory whose audio resolves to a single one-file audiobook *is*
    ///   that audiobook, named after the directory
    ///   (`AudioResolver.FindAudioBook`);
    /// - anything else recurses, and each loose file becomes its own row.
    ///
    /// Two deliberate divergences, both pre-existing patterns in this scanner:
    /// - **Flattening.** Upstream resolves an unclaimed directory to a `Folder`
    ///   item and parents its books under *that*; Ferrofin parents every book
    ///   directly to the collection folder, exactly as [`Self::plan_movies`]
    ///   does for per-title folders. This scanner materializes no intermediate
    ///   `Folder` rows.
    /// - **Name/series/index/year parsing** comes from `BookFileNameParser`,
    ///   which post-dates the pinned 10.11.8 contract (it is on upstream
    ///   `master`). Against 10.11.8 a book is named from its bare filename.
    ///
    /// Both are recorded in `docs/FEATURES.md`.
    fn plan_books(
        &self,
        dir: &str,
        root: &str,
        cf: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let entries = self.file_system.get_file_system_entries(dir);
        // The library root itself is the CollectionFolder, never an item — so
        // neither directory rule applies to it and its loose files each become
        // their own row. (Upstream reaches the root through the multi-item
        // resolver instead, which for a root holding exactly ONE audio file
        // names that audiobook after the LIBRARY folder and dates it from that
        // folder's name too. Naming a book after the library it sits in is an
        // upstream wart, not behaviour worth reproducing; the file stem and no
        // year are used here. Every other root shape matches upstream exactly.
        // Recorded as an accepted divergence in `docs/FEATURES.md`.)
        if dir != root {
            if let Some(book) = single_book_file(&entries) {
                self.push_book(&book, folder_name(dir), cf, out);
                return;
            }
            if let Some((audio, year)) = single_audio_book(&entries, naming) {
                self.push_audio_book(&audio, folder_name(dir), year, cf, out);
                return;
            }
        }
        for entry in &entries {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_books(&entry.path, root, cf, naming, out);
            } else if is_book_file(&entry.path) {
                self.push_book(&entry.path, None, cf, out);
            } else if is_audio_file(&entry.path, naming) && !is_cue_sheet(&entry.path) {
                self.push_audio_book(&entry.path, None, None, cf, out);
            }
        }
    }

    /// Emits the `Book` row for the document at `path`.
    ///
    /// `folder` is the directory name when the containing directory *is* the
    /// book (`BookResolver.GetBook`): the name is parsed from it and a missing
    /// series becomes the empty string, which is what upstream stores and — via
    /// `WhenWritingNull` — what it serializes. Omitting the key instead would be
    /// a `SeriesName` body diff on the common `Dracula/dracula.epub` shape.
    /// Otherwise the file's own stem is parsed and the containing directory name
    /// stands in for a missing series (`BookResolver.Resolve`).
    fn push_book(&self, path: &str, folder: Option<String>, cf: Uuid, out: &mut Vec<Planned>) {
        let (parsed_from, series_fallback) = match folder {
            Some(name) => (name, Some(String::new())),
            None => (file_stem(path), parent_folder_name(path)),
        };
        let parsed = book_file_name_parser::parse(Some(&parsed_from));
        // Upstream leaves an unparsed name empty and lets
        // `ResolverHelper.EnsureName` fill it from the file/folder name.
        let name = parsed
            .name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| parsed_from.clone());
        let Some((id, mut entity)) = self.base_item(BaseItemKind::Book, cf, cf, name, path, false)
        else {
            return;
        };
        entity.media_type = Some("Book".to_owned());
        entity.run_time_ticks = Some(BOOK_RUN_TIME_TICKS);
        entity.index_number = parsed.index.map(i64::from);
        entity.parent_index_number = parsed.parent_index.map(i64::from);
        entity.production_year = parsed.year.map(i64::from);
        entity.series_name = parsed.series_name.or(series_fallback);
        out.push(Planned {
            id,
            entity,
            ancestors: vec![cf],
        });
    }

    /// Emits the `AudioBook` row for the audio file at `path`, named after
    /// `folder` when the containing directory is the audiobook and after the
    /// file otherwise. `MediaType = Audio` is what makes the scan ffprobe it,
    /// so it gets a runtime, streams, and its embedded tags — upstream probes
    /// `AudioBook` through the same `ProbeProvider` as `Audio`.
    ///
    /// `year` is set only for the folder-is-an-audiobook shape, which is the
    /// only one upstream dates: `ResolveMultipleAudio` carries the parsed year
    /// onto the item, while the per-file fallback goes through
    /// `ResolverHelper.EnsureName` and sets none.
    fn push_audio_book(
        &self,
        path: &str,
        folder: Option<String>,
        year: Option<i32>,
        cf: Uuid,
        out: &mut Vec<Planned>,
    ) {
        let name = folder.unwrap_or_else(|| file_stem(path));
        let Some((id, mut entity)) =
            self.base_item(BaseItemKind::AudioBook, cf, cf, name, path, false)
        else {
            return;
        };
        entity.media_type = Some("Audio".to_owned());
        entity.production_year = year.map(i64::from);
        out.push(Planned {
            id,
            entity,
            ancestors: vec![cf],
        });
    }
}

/// The document extensions a `books` library resolves as a `Book` — copied from
/// `BookResolver._validExtensions`.
const BOOK_EXTENSIONS: &[&str] = &[
    "azw", "azw3", "cb7", "cbr", "cbt", "cbz", "epub", "mobi", "pdf",
];

/// A `Book`'s runtime: upstream's `Book()` constructor hardcodes
/// `TimeSpan.TicksPerSecond`, so a book is nominally one second long and
/// position-ticks resume has a non-zero denominator to work against.
const BOOK_RUN_TIME_TICKS: i64 = 10_000_000;

/// Whether `path` is a `.cue` sheet.
///
/// A cue sheet counts as audio by extension (it is in `AudioFileExtensions`),
/// but upstream `AudioResolver.Resolve` bails on it explicitly before building
/// any item: a cue sheet *describes* a rip, it is not one, so resolving it puts
/// a phantom row in the library next to the file it indexes.
///
/// Only the per-file arm needs this. `single_audio_book` passes every file to
/// `AudioBookListResolver` exactly as `ResolveMultipleAudio` does, and a
/// `book.m4b` + `book.cue` pair fails the single-audiobook guard there on both
/// sides, falling through to this arm.
fn is_cue_sheet(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// Whether `path` is one of the [`BOOK_EXTENSIONS`] documents.
fn is_book_file(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| BOOK_EXTENSIONS.iter().any(|b| b.eq_ignore_ascii_case(ext)))
}

/// The one document in `entries`, or `None` when there is none or several —
/// port of `BookResolver.GetBook`'s `bookFiles.Count != 1` guard.
fn single_book_file(entries: &[FileSystemEntryInfo]) -> Option<String> {
    let mut books = entries
        .iter()
        .filter(|e| e.type_ != FileSystemEntryType::Directory && is_book_file(&e.path));
    let first = books.next()?.path.clone();
    books.next().is_none().then_some(first)
}

/// The single audio file a directory resolves to as one `AudioBook`, with the
/// year parsed off the directory name — or `None` when the directory holds no
/// audiobook or several.
///
/// Port of `AudioResolver.FindAudioBook`: the directory's audio must group into
/// exactly one audiobook, of exactly one file, with no extras and no alternate
/// versions — upstream skips the rest "until multi-part books are handled", so
/// a multi-file audiobook folder falls back to a row per file, exactly as
/// `ResolvePaths` does when the multi-item resolver claims nothing.
///
/// The year rides along because `ResolveMultipleAudio` sets
/// `ProductionYear = resolvedItem.Year` and `FindAudioBook` overrides only
/// `Name` — so the usual `Author/Title (2011)/book.m4b` layout is dated by its
/// folder. `AudioBookListResolver` parses it off the stack name (the directory).
fn single_audio_book(
    entries: &[FileSystemEntryInfo],
    naming: &NamingOptions,
) -> Option<(String, Option<i32>)> {
    let files: Vec<ferrofin_naming::io::FileSystemMetadata> = entries
        .iter()
        .filter(|e| e.type_ != FileSystemEntryType::Directory)
        .map(|e| ferrofin_naming::io::FileSystemMetadata::new(e.path.clone(), false))
        .collect();
    let mut resolved = AudioBookListResolver::new(naming).resolve(&files);
    if resolved.len() != 1 {
        return None;
    }
    let info = resolved.remove(0);
    (info.files.len() == 1 && info.extras.is_empty() && info.alternate_versions.is_empty())
        .then(|| (info.files[0].path.clone(), info.year))
}

/// The name of the directory containing `path`, if any (C#
/// `Path.GetFileName(Path.GetDirectoryName(path))`).
fn parent_folder_name(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(|s| s.to_str())
        .map(str::to_owned)
}

/// The album name the tracks agree on: the `Album` tag every tagged track
/// shares, or `None` when they disagree or none is tagged.
fn album_name_consensus(tracks: &[BaseItemEntity]) -> Option<String> {
    let mut names = tracks
        .iter()
        .filter_map(|t| t.album.as_deref().map(str::trim).filter(|s| !s.is_empty()));
    let first = names.next()?.to_owned();
    names.all(|n| n == first).then_some(first)
}

/// Picks the TVDB series candidate for a folder-derived `year`.
///
/// TVDB's `year` query parameter only biases the ranking — it does not filter —
/// so a same-titled remake can outrank the year the folder actually names
/// ("Doctor Who (1963)" matching the 2005 series). An exact year match wins;
/// otherwise the API's own ordering stands.
fn pick_series_hit(
    hits: Vec<ferrofin_providers::TvdbSearchHit>,
    year: Option<i32>,
) -> Option<ferrofin_providers::TvdbSearchHit> {
    year.and_then(|y| hits.iter().find(|h| h.year == Some(y)).cloned())
        .or_else(|| hits.into_iter().next())
}

/// The item kinds a library's tile collage samples — port of
/// `DtoExtensions.GetBaseItemKindsForCollectionType`, which
/// `CollectionFolderImageProvider` uses to pick the rows whose Primary images
/// make up the tile. An unknown/absent collection type samples the mixed set.
fn collage_item_kinds(folder: &VirtualFolderInfo) -> Vec<BaseItemKind> {
    match folder.collection_type {
        Some(CollectionTypeOptions::movies) => vec![BaseItemKind::Movie],
        Some(CollectionTypeOptions::tvshows) => vec![BaseItemKind::Series],
        Some(CollectionTypeOptions::music) => vec![BaseItemKind::MusicAlbum],
        Some(CollectionTypeOptions::musicvideos) => vec![BaseItemKind::MusicVideo],
        Some(CollectionTypeOptions::books) => vec![BaseItemKind::Book, BaseItemKind::AudioBook],
        Some(CollectionTypeOptions::boxsets) => vec![BaseItemKind::BoxSet],
        // Ferrofin folds upstream's separate `photos` type into `homevideos`.
        Some(CollectionTypeOptions::homevideos) => vec![BaseItemKind::Video, BaseItemKind::Photo],
        _ => vec![
            BaseItemKind::Video,
            BaseItemKind::Audio,
            BaseItemKind::Photo,
            BaseItemKind::Movie,
            BaseItemKind::Series,
        ],
    }
}

/// Projects the EXIF fields onto their `Data` keys, in C#'s `Photo` property
/// spelling. A `None` clears the key (see
/// [`merge_data_fields`](crate::item_data::merge_data_fields)).
fn photo_exif_fields(
    exif: &ferrofin_drawing::photo_provider::PhotoExif,
) -> Vec<(&'static str, Option<serde_json::Value>)> {
    let text = |value: &Option<String>| value.clone().map(serde_json::Value::String);
    let number = |value: Option<f64>| value.and_then(serde_json::Number::from_f64).map(Into::into);
    vec![
        ("CameraMake", text(&exif.camera_make)),
        ("CameraModel", text(&exif.camera_model)),
        ("Software", text(&exif.software)),
        ("ExposureTime", number(exif.exposure_time)),
        ("FocalLength", number(exif.focal_length)),
        (
            "Orientation",
            exif.orientation
                .map(|o| serde_json::Value::String(format!("{o:?}"))),
        ),
        ("Aperture", number(exif.aperture)),
        ("ShutterSpeed", number(exif.shutter_speed)),
        ("Latitude", number(exif.latitude)),
        ("Longitude", number(exif.longitude)),
        ("Altitude", number(exif.altitude)),
        (
            "IsoSpeedRating",
            exif.iso_speed_rating.map(serde_json::Value::from),
        ),
    ]
}

/// One item's inputs to the artwork pass, grouped so
/// [`LibraryScanner::persist_artwork`] keeps a readable signature.
struct ArtworkPass<'a> {
    /// The scanned row (already enriched, not yet re-read from the DB).
    entity: &'a BaseItemEntity,
    /// The row's probed media streams — the embedded-cover source.
    streams: &'a [MediaStreamInfoEntity],
    /// The owning library's image-fetcher gate.
    policy: FetcherPolicy<'a>,
    /// The image embedded in the item's own file — a photo (the file itself)
    /// or a book cover — when the row has one.
    embedded_images: Vec<ItemImageInfo>,
}

/// The fetcher policy of the library an item belongs to — every `Planned` item
/// carries its collection-folder id as its first ancestor. A library with no
/// resolved options gets the permissive default.
fn policy_for<'a>(
    item: &Planned,
    policies: &'a std::collections::HashMap<Uuid, FetcherPolicy<'a>>,
) -> FetcherPolicy<'a> {
    item.ancestors
        .first()
        .and_then(|id| policies.get(id))
        .copied()
        .unwrap_or_default()
}

/// Applies a book's embedded metadata to the row, filling only what is still
/// empty (mirrors [`apply_details`]).
fn apply_book(entity: &mut BaseItemEntity, book: &ferrofin_providers::BookMetadata) {
    if let Some(name) = book
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        entity.name = Some(name.to_owned());
        entity.sort_name = Some(derived_sort_name(entity, name));
    }
    if entity.original_title.is_none() {
        entity.original_title.clone_from(&book.original_title);
    }
    // C# `BookMetadataService.MergeData` fills SeriesName when the target's is
    // empty. The resolver writes `Some("")` for the folder shape, so an
    // `is_none()` guard would never fire.
    if entity.series_name.as_deref().unwrap_or_default().is_empty()
        && let Some(series) = book.series_name.as_deref().filter(|s| !s.is_empty())
    {
        entity.series_name = Some(series.to_owned());
    }
    if entity.overview.is_none() {
        entity.overview.clone_from(&book.overview);
    }
    if entity.production_year.is_none() {
        entity.production_year = book.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = book.premiere_date;
    }
    if entity.index_number.is_none() {
        entity.index_number = book.index_number.map(i64::from);
    }
    if entity.community_rating.is_none() {
        entity.community_rating = book.community_rating;
    }
    merge_multi_value(&mut entity.genres, &book.genres);
    merge_multi_value(&mut entity.studios, &book.studios);
    merge_multi_value(&mut entity.tags, &book.tags);
}

/// Maps a book's credits to persistable rows. Comic and OPF sources carry no
/// per-person id or image.
fn book_people(book: &ferrofin_providers::BookMetadata) -> Vec<PeopleEntity> {
    book.people
        .iter()
        .map(|(name, kind)| PeopleEntity {
            id: guid_to_db(Uuid::new_v4()),
            name: name.clone(),
            person_type: Some(kind.clone()),
            role: None,
            primary_image_url: None,
            provider_id: None,
        })
        .collect()
}

/// Filename prefixes that mark an image as *artwork*, never a photo. Port of
/// `PhotoResolver._ignoreFiles` (matched case-insensitively as a prefix).
const PHOTO_IGNORE_PREFIXES: [&str; 9] = [
    "folder",
    "thumb",
    "landscape",
    "fanart",
    "backdrop",
    "poster",
    "cover",
    "logo",
    "default",
];

/// The image extensions a photo may have — port of the C# encoder's
/// `SupportedInputFormats`, which is what `PhotoResolver.IsImageFile` tests
/// against.
const PHOTO_EXTENSIONS: [&str; 17] = [
    "jpeg", "jpg", "png", "dng", "webp", "gif", "bmp", "ico", "astc", "ktx", "pkm", "wbmp", "cr2",
    "nef", "arw", "svg", "tiff",
];

/// Whether `path` is an image file that is not artwork — port of
/// `PhotoResolver.IsImageFile`.
fn is_photo_file(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !PHOTO_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return false;
    }
    let stem = stem.to_ascii_lowercase();
    !PHOTO_IGNORE_PREFIXES
        .iter()
        .any(|prefix| stem.starts_with(prefix))
}

/// Whether `path` is artwork belonging to one of the `siblings` videos — port of
/// `PhotoResolver.IsOwnedByMedia`: a video file in the same directory whose own
/// stem is a prefix of the image's stem (so `Movie-thumb.jpg` belongs to
/// `Movie.mkv`).
fn is_owned_by_media(siblings: &[&str], path: &str, naming: &NamingOptions) -> bool {
    let stem = file_stem(path).to_ascii_lowercase();
    siblings.iter().any(|sibling| {
        video_resolver::is_video_file(sibling, naming)
            && stem.starts_with(&file_stem(sibling).to_ascii_lowercase())
    })
}

/// The `VideoType` of a plain video file: `.iso`/`.img` are disc images,
/// everything else a video file (port of `SetVideoType`).
fn file_video_type(path: &str) -> VideoType {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if ext.eq_ignore_ascii_case("iso") || ext.eq_ignore_ascii_case("img") {
        VideoType::Iso
    } else {
        VideoType::VideoFile
    }
}

/// Writes `video_type` (and, for an image, its `IsoType`) into the row's `Data`
/// blob — where Jellyfin keeps them, and what the `videoTypes` browse filter
/// matches on. `IsoType` follows upstream's path-substring heuristic.
fn set_video_type(entity: &mut BaseItemEntity, video_type: VideoType) {
    let name = match video_type {
        VideoType::VideoFile => "VideoFile",
        VideoType::Iso => "Iso",
        VideoType::Dvd => "Dvd",
        VideoType::BluRay => "BluRay",
    };
    if let Some(data) = crate::item_data::set_data_field(entity.data.as_deref(), "VideoType", name)
    {
        entity.data = Some(data);
    }
    if video_type != VideoType::Iso {
        return;
    }
    let path = entity.path.as_deref().unwrap_or_default().to_lowercase();
    let iso_type = if path.contains("dvd") {
        "Dvd"
    } else if path.contains("bluray") {
        "BluRay"
    } else {
        return;
    };
    if let Some(data) =
        crate::item_data::set_data_field(entity.data.as_deref(), "IsoType", iso_type)
    {
        entity.data = Some(data);
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

/// Resolves which movie an extra belongs to: a movie in the extra's own
/// directory whose file stem prefixes the extra's (`Movie-trailer.mkv` beside
/// `Movie.mkv`), the directory's single movie, or the parent directory's
/// single movie (`Movie (2020)/trailers/x.mkv`). Mirrors upstream's ownership
/// (extras attach to the item owning their folder).
fn owner_for_extra(
    path: &str,
    movies_by_dir: &std::collections::HashMap<String, Vec<(Uuid, String)>>,
) -> Option<Uuid> {
    let dir = std::path::Path::new(path)
        .parent()?
        .to_string_lossy()
        .into_owned();
    let stem = file_stem(path);
    if let Some(movies) = movies_by_dir.get(&dir) {
        if let Some((id, _)) = movies
            .iter()
            .find(|(_, movie_stem)| stem.to_lowercase().starts_with(&movie_stem.to_lowercase()))
        {
            return Some(*id);
        }
        if let [(id, _)] = movies.as_slice() {
            return Some(*id);
        }
    }
    let parent = std::path::Path::new(&dir)
        .parent()?
        .to_string_lossy()
        .into_owned();
    match movies_by_dir.get(&parent).map(Vec::as_slice) {
        Some([(id, _)]) => Some(*id),
        _ => None,
    }
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
        // C# `AlbumNfoProvider`/`ArtistNfoProvider.GetXmlFile`: a fixed
        // filename inside the album/artist folder.
        NfoItemKind::MusicAlbum => vec![p.join("album.nfo")],
        NfoItemKind::MusicArtist => vec![p.join("artist.nfo")],
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
            .or_else(|| Some(derived_sort_name(entity, title)));
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
    // NFO `<trailer>` URLs join the same `Data.RemoteTrailers` array the TMDB
    // path writes (upstream keeps one merged list, deduped by URL).
    let trailers: Vec<(Option<String>, String)> = n
        .remote_trailers
        .iter()
        .map(|url| (None, url.clone()))
        .collect();
    if let Some(data) = crate::item_data::merge_remote_trailers(entity.data.as_deref(), &trailers) {
        entity.data = Some(data);
    }
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
    // Trailers live in the serialized `Data` blob — Jellyfin's only home for
    // them — so the client's Trailer button links out to YouTube like upstream.
    let trailers: Vec<(Option<String>, String)> = d
        .trailers
        .iter()
        .map(|t| (Some(t.name.clone()), t.url.clone()))
        .collect();
    if let Some(data) = crate::item_data::merge_remote_trailers(entity.data.as_deref(), &trailers) {
        entity.data = Some(data);
    }
}

/// Fills an album's release date and year from MusicBrainz — C#
/// `MusicBrainzAlbumProvider` writes more onto a `MusicAlbum` than its ids, and
/// for a tagless album this is the only year there is. A row that already has a
/// date makes no request.
async fn apply_release_details(
    album: &mut BaseItemEntity,
    mb: &ferrofin_providers::MusicBrainzClient,
    release_id: Option<&str>,
) -> bool {
    if album.premiere_date.is_some() {
        return false;
    }
    let Some(release) = release_id else {
        return false;
    };
    let Some(details) = mb.release_details(release).await else {
        return false;
    };
    let mut changed = false;
    if let Some(date) = details
        .premiere_date
        .and_then(ferrofin_providers::PartialDate::to_utc)
    {
        album.premiere_date = Some(date);
        changed = true;
    }
    if album.production_year.is_none()
        && let Some(year) = details.production_year
    {
        album.production_year = Some(i64::from(year));
        changed = true;
    }
    changed
}

/// Whether OMDb's director/writer/actor credits are added to an item.
///
/// Port of the OMDb plugin's `PluginConfiguration.CastAndCrew`, an
/// uninitialized `bool` — so upstream's default is `false` and OMDb adds no
/// people. Ferrofin has no per-plugin config page for OMDb, so it matches the
/// upstream default rather than inventing a different one.
const OMDB_CAST_AND_CREW: bool = false;

/// Applies an OMDb record to the row, filling only what is still empty (a local
/// NFO, an earlier fetcher, or a prior scan wins), mirroring [`apply_details`].
///
/// `english` and `us` are C#'s two localization gates: OMDb serves English data
/// only, so `Genres` (which upstream prefers over TVDB's) and the certificate
/// are skipped for a non-English library, and the certificate additionally
/// requires a US metadata country.
fn apply_omdb(
    entity: &mut BaseItemEntity,
    item: &ferrofin_providers::OmdbItem,
    english: bool,
    us: bool,
) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&item.plot);
    }
    if entity.community_rating.is_none() {
        entity.community_rating = item.community_rating().map(f64::from);
    }
    if entity.critic_rating.is_none() {
        entity.critic_rating = item.rotten_tomatoes().map(f64::from);
    }
    if entity.production_year.is_none() {
        entity.production_year = item.production_year().map(i64::from);
    }
    if entity.run_time_ticks.is_none() {
        entity.run_time_ticks = item.run_time_ticks();
    }
    if english {
        if us && entity.official_rating.is_none() {
            entity.official_rating.clone_from(&item.rated);
        }
        merge_multi_value(&mut entity.genres, &item.genres());
    }
}

/// Maps OMDb's credited people to persistable rows: the director, the writer,
/// then each actor. OMDb carries no per-person id or image.
fn omdb_people(item: &ferrofin_providers::OmdbItem) -> Vec<PeopleEntity> {
    item.people()
        .into_iter()
        .map(|(name, kind)| PeopleEntity {
            id: guid_to_db(Uuid::new_v4()),
            name,
            person_type: Some(
                match kind {
                    ferrofin_providers::OmdbPersonKind::Director => "Director",
                    ferrofin_providers::OmdbPersonKind::Writer => "Writer",
                    ferrofin_providers::OmdbPersonKind::Actor => "Actor",
                }
                .to_owned(),
            ),
            role: None,
            primary_image_url: None,
            provider_id: None,
        })
        .collect()
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
        entity.sort_name = Some(derived_sort_name(entity, title));
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

/// Appends OMDb's poster as a `Primary` candidate, when the library enabled the
/// OMDb image fetcher and the metadata pass captured a poster URL for the item.
///
/// Always appended **last** so [`dedup_images_by_type`] keeps it only when no
/// better provider supplied a Primary — C# gives `OmdbImageProvider` `Order = 90`
/// for the same reason.
fn append_omdb_poster(
    images: &mut Vec<RemoteImage>,
    entity: &BaseItemEntity,
    cache: &ArtworkCache,
    policy: FetcherPolicy<'_>,
    short: &str,
) {
    if !policy.image_enabled(short, fetcher_names::OMDB) {
        return;
    }
    if let Some(url) = cache.omdb_poster.get(&entity.id) {
        images.push(RemoteImage {
            image_type: ImageType::Primary,
            url: url.clone(),
        });
    }
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

/// Maps TMDB credited people to persistable [`PeopleEntity`] rows, keeping
/// TMDB's person id (which keys the biography/headshot enrichment).
fn tmdb_people(people: &[ferrofin_providers::TmdbPerson]) -> Vec<PeopleEntity> {
    people
        .iter()
        .map(|p| PeopleEntity {
            id: guid_to_db(Uuid::new_v4()),
            name: p.name.clone(),
            person_type: Some(p.person_type.clone()),
            role: p.role.clone(),
            primary_image_url: p.profile_url.clone(),
            provider_id: Some(p.tmdb_id),
        })
        .collect()
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

/// Port of C# `Episode.CreateSortName`: the zero-padded season/episode numbers
/// ahead of the title (`001 - 0004 - The Title`).
///
/// This override REPLACES the generic name-derived sort name — an episode must
/// sort by its position in the season, never alphabetically by title. Clients
/// build their play queue from the season's episodes in `SortName` order, so a
/// title-derived sort name scrambles the queue: "next episode" points at the
/// wrong item, and at the alphabetically-last episode there is no next at all
/// (a dead Next button and no autoplay).
fn episode_sort_name(parent_index: Option<i64>, index: Option<i64>, name: &str) -> String {
    let season = parent_index.map_or_else(String::new, |n| format!("{n:03} - "));
    let episode = index.map_or_else(String::new, |n| format!("{n:04} - "));
    format!("{season}{episode}{name}")
}

/// Port of C# `Season.CreateSortName`: the zero-padded season number, or the
/// name when the season has no number (so `Specials` (0000) sorts first).
fn season_sort_name(index: Option<i64>, name: &str) -> String {
    index.map_or_else(|| create_sort_name(name), |n| format!("{n:04}"))
}

/// The sort name a row derives from `title`, honouring the per-kind
/// `CreateSortName` overrides (episodes and seasons sort by number, everything
/// else by the name pipeline).
fn derived_sort_name(entity: &BaseItemEntity, title: &str) -> String {
    match entity.type_.rsplit('.').next().unwrap_or(&entity.type_) {
        "Episode" => episode_sort_name(entity.parent_index_number, entity.index_number, title),
        "Season" => season_sort_name(entity.index_number, title),
        _ => create_sort_name(title),
    }
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
        // Music kinds have their own filename tables upstream (an album prefers
        // its folder name + `cdart`; a song takes no local image of its own).
        "MusicAlbum" => ImageItemKind::MusicAlbum,
        "MusicArtist" => ImageItemKind::MusicArtist,
        "AudioBook" => ImageItemKind::AudioBook,
        "Audio" => ImageItemKind::Audio,
        // A photo takes NO local image of its own (C# `LocalImageProvider`
        // returns false for `item is Photo`) — without this, one shared
        // `thumb.jpg` in an album folder attaches to every photo in it. An
        // album uses the music filename table, as upstream routes it.
        "Photo" => ImageItemKind::Photo,
        "PhotoAlbum" => ImageItemKind::PhotoAlbum,
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
    // Port check against C# `Episode.CreateSortName` /
    // `Season.CreateSortName`: episodes sort by season/episode number ahead of
    // the title, seasons by their zero-padded number.
    #[test]
    fn episode_and_season_sort_names_match_jellyfin() {
        assert_eq!(
            super::episode_sort_name(Some(1), Some(4), "The One Where..."),
            "001 - 0004 - The One Where..."
        );
        assert_eq!(
            super::episode_sort_name(Some(10), Some(123), "Title"),
            "010 - 0123 - Title"
        );
        // A null number renders as nothing at all (the C# ternaries).
        assert_eq!(super::episode_sort_name(None, Some(2), "T"), "0002 - T");
        assert_eq!(super::episode_sort_name(Some(1), None, "T"), "001 - T");
        assert_eq!(super::episode_sort_name(None, None, "T"), "T");

        // Sorting the same season's episodes by this key is episode order,
        // never alphabetical (the play-queue regression).
        let mut keys: Vec<String> = [(1, 1, "Zebra"), (1, 2, "Alpha"), (1, 10, "Middle")]
            .iter()
            .map(|(s, e, n)| super::episode_sort_name(Some(*s), Some(*e), n))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "001 - 0001 - Zebra",
                "001 - 0002 - Alpha",
                "001 - 0010 - Middle"
            ]
        );

        assert_eq!(super::season_sort_name(Some(1), "Season 1"), "0001");
        // Specials (season 0) sort ahead of season 1.
        assert!(
            super::season_sort_name(Some(0), "Specials")
                < super::season_sort_name(Some(1), "Season 1")
        );
        // No number → the name pipeline.
        assert_eq!(
            super::season_sort_name(None, "Season Unknown"),
            super::create_sort_name("Season Unknown")
        );
    }

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

    // PhotoResolver.IsImageFile: the extension set, minus the artwork prefixes.
    #[test]
    fn photo_files_exclude_artwork_and_non_images() {
        assert!(super::is_photo_file("/p/DSC_0001.JPG"));
        assert!(super::is_photo_file("/p/holiday.png"));
        assert!(super::is_photo_file("/p/raw.cr2"));
        // Artwork prefixes are never photos, whatever their case.
        for artwork in [
            "folder.jpg",
            "Thumb.png",
            "landscape.jpg",
            "fanart.jpg",
            "backdrop.jpg",
            "poster.png",
            "cover.jpg",
            "logo.png",
            "default.jpg",
        ] {
            assert!(
                !super::is_photo_file(&format!("/p/{artwork}")),
                "{artwork} is artwork, not a photo"
            );
        }
        // A prefix match, as upstream does it: "poster-2.jpg" is still artwork.
        assert!(!super::is_photo_file("/p/poster-2.jpg"));
        assert!(!super::is_photo_file("/p/clip.mkv"));
        assert!(!super::is_photo_file("/p/no-extension"));
    }

    // PhotoResolver.IsOwnedByMedia: an image whose stem starts with a sibling
    // video's stem is that video's artwork, not a photo of its own.
    #[test]
    fn images_owned_by_a_sibling_video_are_not_photos() {
        let naming = super::NamingOptions::new();
        let siblings = ["/m/Movie.mkv", "/m/Movie-thumb.jpg", "/m/Sunset.jpg"];
        assert!(super::is_owned_by_media(
            &siblings,
            "/m/Movie-thumb.jpg",
            &naming
        ));
        assert!(!super::is_owned_by_media(
            &siblings,
            "/m/Sunset.jpg",
            &naming
        ));
    }

    // The EXIF fields round-trip through the `Data` blob under Jellyfin's own
    // property names, so an adopted database keeps its photo metadata.
    #[test]
    fn photo_exif_fields_use_jellyfins_data_keys() {
        let exif = ferrofin_drawing::photo_provider::PhotoExif {
            camera_make: Some("ACME".into()),
            aperture: Some(2.8),
            orientation: Some(ferrofin_model::drawing::ImageOrientation::RightTop),
            iso_speed_rating: Some(400),
            ..Default::default()
        };
        let fields = super::photo_exif_fields(&exif);
        let keys: Vec<_> = fields.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            crate::item_data::PHOTO_EXIF_KEYS,
            "the Data keys must match the ones Jellyfin serializes"
        );
        let data = crate::item_data::merge_data_fields(None, &fields).expect("data");
        let parsed = crate::item_data::parse_data(Some(&data));
        assert_eq!(
            crate::item_data::read_data_string(&parsed, "CameraMake").as_deref(),
            Some("ACME")
        );
        assert_eq!(
            crate::item_data::read_data_f64(&parsed, "Aperture"),
            Some(2.8)
        );
        assert_eq!(
            crate::item_data::read_data_string(&parsed, "Orientation").as_deref(),
            Some("RightTop")
        );
        assert_eq!(
            crate::item_data::read_data_i32(&parsed, "IsoSpeedRating"),
            Some(400)
        );
        // A field the file has no value for is absent, not null.
        assert!(!parsed.contains_key("CameraModel"));
    }

    // A re-scan of a photo whose EXIF was stripped must clear the stale values
    // rather than leave the old ones behind.
    #[test]
    fn re_scanning_a_stripped_photo_clears_its_exif_keys() {
        let with_exif = crate::item_data::merge_data_fields(
            None,
            &super::photo_exif_fields(&ferrofin_drawing::photo_provider::PhotoExif {
                camera_make: Some("ACME".into()),
                ..Default::default()
            }),
        )
        .expect("data");
        let cleared = crate::item_data::merge_data_fields(
            Some(&with_exif),
            &super::photo_exif_fields(&ferrofin_drawing::photo_provider::PhotoExif::default()),
        )
        .expect("data changed");
        assert!(!crate::item_data::parse_data(Some(&cleared)).contains_key("CameraMake"));
    }

    // Unrelated Data keys (playlist membership, VideoType, …) survive the merge.
    #[test]
    fn merging_exif_preserves_other_data_keys() {
        let existing = crate::item_data::set_data_field(None, "VideoType", "VideoFile");
        let merged = crate::item_data::merge_data_fields(
            existing.as_deref(),
            &super::photo_exif_fields(&ferrofin_drawing::photo_provider::PhotoExif {
                software: Some("Darktable".into()),
                ..Default::default()
            }),
        )
        .expect("data");
        let parsed = crate::item_data::parse_data(Some(&merged));
        assert_eq!(
            crate::item_data::read_data_string(&parsed, "VideoType").as_deref(),
            Some("VideoFile")
        );
        assert_eq!(
            crate::item_data::read_data_string(&parsed, "Software").as_deref(),
            Some("Darktable")
        );
    }

    // A sub-directory holding photos is a PhotoAlbum the photos hang off; the
    // library root is the collection folder itself and never an album.
    #[tokio::test]
    async fn photo_albums_are_the_sub_directories_not_the_library_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("loose.jpg"), b"x").expect("write");
        std::fs::create_dir(root.join("Holiday")).expect("mkdir");
        std::fs::write(root.join("Holiday").join("DSC_0001.jpg"), b"x").expect("write");
        // Artwork prefixes are still excluded inside an album folder.
        std::fs::write(root.join("Holiday").join("folder.jpg"), b"x").expect("write");

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(dir.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence);
        let mut out = Vec::new();
        let root_str = root.to_string_lossy().into_owned();
        let cf = uuid::Uuid::from_u128(0x7000);
        scanner.plan_photos(
            &root_str,
            &root_str,
            cf,
            cf,
            &super::NamingOptions::new(),
            &mut out,
        );

        let kinds: Vec<(&str, String)> = out
            .iter()
            .map(|p| {
                (
                    p.entity.type_.rsplit('.').next().unwrap_or_default(),
                    p.entity.name.clone().unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            kinds.contains(&("Photo", "loose".to_owned())),
            "a root photo is parented to the library: {kinds:?}"
        );
        assert!(
            kinds.contains(&("PhotoAlbum", "Holiday".to_owned())),
            "the sub-directory became an album: {kinds:?}"
        );
        assert!(
            kinds.contains(&("Photo", "DSC_0001".to_owned())),
            "the album's photo was resolved: {kinds:?}"
        );
        assert!(
            !kinds.iter().any(|(_, name)| name == "folder"),
            "artwork inside an album folder is not a photo: {kinds:?}"
        );
        // The album's photo is parented to the album, not the library.
        let album = out
            .iter()
            .find(|p| p.entity.type_.ends_with(".PhotoAlbum"))
            .expect("album");
        let child = out
            .iter()
            .find(|p| p.entity.name.as_deref() == Some("DSC_0001"))
            .expect("child");
        assert!(child.ancestors.contains(&album.id));
    }

    // BookResolver's extension set, matched case-insensitively.
    #[test]
    fn book_files_are_recognized_by_extension() {
        for book in [
            "Dune.epub",
            "Dune.EPUB",
            "batman.cbz",
            "manual.pdf",
            "x.azw3",
        ] {
            assert!(super::is_book_file(&format!("/b/{book}")), "{book}");
        }
        for other in ["cover.jpg", "notes.txt", "movie.mkv", "no-extension"] {
            assert!(!super::is_book_file(&format!("/b/{other}")), "{other}");
        }
    }

    // Embedded book metadata fills only what the row still lacks, but the title
    // is authoritative (as it is for NFO).
    #[test]
    fn apply_book_fills_empty_fields_and_takes_the_title() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let book = ferrofin_providers::BookMetadata {
            name: Some("The Killing Joke".into()),
            overview: Some("One bad day.".into()),
            production_year: Some(1988),
            genres: vec!["Superhero".into()],
            studios: vec!["DC Comics".into()],
            tags: vec!["classic".into()],
            index_number: Some(1),
            ..Default::default()
        };
        let mut entity = BaseItemEntity {
            name: Some("batman - the killing joke".into()),
            overview: Some("from a sidecar".into()),
            ..Default::default()
        };
        super::apply_book(&mut entity, &book);
        assert_eq!(entity.name.as_deref(), Some("The Killing Joke"));
        assert_eq!(entity.overview.as_deref(), Some("from a sidecar")); // kept
        assert_eq!(entity.production_year, Some(1988));
        assert_eq!(entity.genres.as_deref(), Some("Superhero"));
        assert_eq!(entity.studios.as_deref(), Some("DC Comics"));
        assert_eq!(entity.tags.as_deref(), Some("classic"));
        assert_eq!(entity.index_number, Some(1));
        assert!(
            entity.sort_name.is_some(),
            "the sort name follows the title"
        );
    }

    #[test]
    fn book_credits_become_people_rows() {
        let book = ferrofin_providers::BookMetadata {
            people: vec![
                ("Alan Moore".to_owned(), "Author".to_owned()),
                ("Brian Bolland".to_owned(), "Penciller".to_owned()),
            ],
            ..Default::default()
        };
        let people = super::book_people(&book);
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].name, "Alan Moore");
        assert_eq!(people[0].person_type.as_deref(), Some("Author"));
        assert!(people[1].provider_id.is_none());
    }

    /// An OMDb record parsed from a body shaped like the real API's.
    fn omdb_item(json: &str) -> ferrofin_providers::OmdbItem {
        serde_json::from_str(json).expect("parse omdb body")
    }

    // OMDb fills only what is still empty, exactly like the TMDB/NFO appliers.
    #[test]
    fn apply_omdb_fills_only_empty_fields() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let item = omdb_item(
            r#"{"Plot":"from omdb","Year":"2010","Rated":"PG-13","Runtime":"148 min",
                "Genre":"Action, Sci-Fi","imdbRating":"8.8",
                "Ratings":[{"Source":"Rotten Tomatoes","Value":"87%"}]}"#,
        );
        let mut e = BaseItemEntity {
            overview: Some("already set".into()),
            community_rating: Some(1.0),
            ..Default::default()
        };
        super::apply_omdb(&mut e, &item, true, true);
        assert_eq!(e.overview.as_deref(), Some("already set")); // not overwritten
        assert_eq!(e.community_rating, Some(1.0)); // not overwritten
        assert_eq!(e.critic_rating, Some(87.0)); // filled
        assert_eq!(e.production_year, Some(2010));
        assert_eq!(e.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(e.run_time_ticks, Some(148 * 60 * 10_000_000));
        assert_eq!(e.genres.as_deref(), Some("Action|Sci-Fi"));
    }

    // OMDb serves English data only, so C# skips the genres and the certificate
    // for a library set to any other metadata language.
    #[test]
    fn apply_omdb_skips_localized_fields_for_a_non_english_library() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let item = omdb_item(r#"{"Plot":"plot","Rated":"PG-13","Genre":"Action"}"#);
        let mut e = BaseItemEntity::default();
        super::apply_omdb(&mut e, &item, false, true);
        assert_eq!(e.overview.as_deref(), Some("plot")); // language-neutral
        assert_eq!(e.official_rating, None);
        assert_eq!(e.genres, None);
    }

    // The certificate is a US rating; C# only takes it for a US library.
    #[test]
    fn apply_omdb_skips_the_certificate_outside_the_us() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let item = omdb_item(r#"{"Rated":"PG-13","Genre":"Action"}"#);
        let mut e = BaseItemEntity::default();
        super::apply_omdb(&mut e, &item, true, false);
        assert_eq!(e.official_rating, None);
        assert_eq!(e.genres.as_deref(), Some("Action")); // genres are not US-gated
    }

    // Credits land as Director, Writer, then one row per actor.
    #[test]
    fn omdb_people_map_to_director_writer_then_actors() {
        let item = omdb_item(
            r#"{"Director":"Christopher Nolan","Writer":"Jonathan Nolan",
                "Actors":"Leonardo DiCaprio, Elliot Page"}"#,
        );
        let people = super::omdb_people(&item);
        let kinds: Vec<_> = people
            .iter()
            .map(|p| p.person_type.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(kinds, ["Director", "Writer", "Actor", "Actor"]);
        assert_eq!(people[3].name, "Elliot Page");
        assert!(people.iter().all(|p| p.provider_id.is_none()));
    }

    // A library that saved no TypeOptions gets Jellyfin's own defaults, so the
    // English/US gates above are open unless the admin changed them.
    #[test]
    fn fetcher_policy_defaults_to_english_us() {
        let policy = super::FetcherPolicy::default();
        assert_eq!(policy.metadata_language(), "en");
        assert_eq!(policy.country_code(), "us");
    }

    #[test]
    fn fetcher_policy_reads_the_librarys_language_and_country() {
        let options = ferrofin_model::configuration::LibraryOptions {
            preferred_metadata_language: Some("DE".to_owned()),
            metadata_country_code: Some("de".to_owned()),
            ..Default::default()
        };
        let policy = super::FetcherPolicy {
            options: Some(&options),
        };
        assert_eq!(policy.metadata_language(), "de");
        assert_eq!(policy.country_code(), "de");
    }

    // OMDb's poster is appended last so the dedup keeps it only as a last
    // resort, and never at all when the library disabled the OMDb image fetcher.
    #[test]
    fn omdb_poster_is_the_last_resort_primary() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let entity = BaseItemEntity {
            id: "item-1".to_owned(),
            ..Default::default()
        };
        let mut cache = super::ArtworkCache::default();
        cache
            .omdb_poster
            .insert("item-1".to_owned(), "https://omdb.test/p.jpg".to_owned());

        // Nothing else supplied a Primary: OMDb's poster is used.
        let mut images = Vec::new();
        super::append_omdb_poster(
            &mut images,
            &entity,
            &cache,
            super::FetcherPolicy::default(),
            "Movie",
        );
        let deduped = super::dedup_images_by_type(images);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].url, "https://omdb.test/p.jpg");

        // A better provider already supplied one: OMDb's is dropped.
        let mut images = vec![ferrofin_providers::RemoteImage {
            image_type: ferrofin_model::entities::ImageType::Primary,
            url: "https://tmdb.test/p.jpg".to_owned(),
        }];
        super::append_omdb_poster(
            &mut images,
            &entity,
            &cache,
            super::FetcherPolicy::default(),
            "Movie",
        );
        let deduped = super::dedup_images_by_type(images);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].url, "https://tmdb.test/p.jpg");
    }

    #[test]
    fn omdb_poster_is_skipped_when_the_library_disabled_the_fetcher() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let entity = BaseItemEntity {
            id: "item-1".to_owned(),
            ..Default::default()
        };
        let mut cache = super::ArtworkCache::default();
        cache
            .omdb_poster
            .insert("item-1".to_owned(), "https://omdb.test/p.jpg".to_owned());
        // A saved TypeOptions listing only TMDB means OMDb's checkbox is off.
        let options = ferrofin_model::configuration::LibraryOptions {
            type_options: vec![ferrofin_model::configuration::TypeOptions {
                type_: Some("Movie".to_owned()),
                image_fetchers: vec!["TheMovieDb".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut images = Vec::new();
        super::append_omdb_poster(
            &mut images,
            &entity,
            &cache,
            super::FetcherPolicy {
                options: Some(&options),
            },
            "Movie",
        );
        assert!(images.is_empty());
    }

    // The post-scan music pass resolves each album's + artist's MusicBrainz ids
    // from the embedded ids on its tracks (no network when they're all present),
    // and aggregates the album-artist onto the album. Seeds through the seams.
    /// Serves `manifest` for `thumbs.txt` requests and `image` for everything
    /// else — enough HTTP for the studios client (blocking, own thread).
    fn spawn_art_server(manifest: &'static str, image: &'static [u8]) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body: Vec<u8> = if req.contains("thumbs.txt") {
                    manifest.as_bytes().to_vec()
                } else {
                    image.to_vec()
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    /// A person's downloaded profile art must be persisted with its real pixel
    /// dimensions (and blurhash), not the `0x0` placeholder `download_images`
    /// returns.
    ///
    /// Discriminating: the asserted `48x64` comes from the real image processor
    /// probing the real JPEG on disk, never from a fake — drop the
    /// `fill_image_metadata` call from `enrich_people` and the row is stored
    /// `0x0` with no hash and both assertions fail.
    ///
    /// It matters on the read path: `DtoService::primary_aspect_ratio` probes
    /// the file itself whenever the stored dimensions are `0`, and nothing
    /// memoizes that probe, so a `0x0` row costs an open-and-parse of the JPEG
    /// header on *every* request for `PrimaryImageAspectRatio` over that person.
    #[tokio::test(flavor = "multi_thread")]
    async fn enrich_people_stores_probed_dimensions_for_person_art() {
        use crate::item_persistence_service::FerrofinItemPersistenceService;
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
        use crate::people_repository::FerrofinPeopleRepository;
        use crate::test_support::test_db;
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_drawing::ImageCrateEncoder;
        use ferrofin_drawing::ImageProcessor;
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, WrittenPerson};

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let items: Arc<dyn ItemRepository> =
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup));

        let person_id = uuid::Uuid::new_v4();
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(person_id),
                type_: stored_type_name(BaseItemKind::Person).unwrap().to_owned(),
                name: Some("Ada Lovelace".into()),
                ..Default::default()
            }])
            .await
            .expect("seed person");

        let tmp = tempfile::tempdir().unwrap();
        let meta_root = tmp.path().join("People");
        // Pre-place the profile art under the name `download_images` looks for,
        // so it reuses the on-disk file and the test never needs the network.
        let art_dir = meta_root.join(person_id.to_string());
        std::fs::create_dir_all(&art_dir).unwrap();
        let mut profile = image::RgbImage::new(48, 64);
        for (x, _y, px) in profile.enumerate_pixels_mut() {
            *px = if x < 24 {
                image::Rgb([200, 40, 40])
            } else {
                image::Rgb([40, 40, 200])
            };
        }
        profile.save(art_dir.join("primary.jpg")).unwrap();

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
            ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
        );
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_image_processor(processor)
                .with_metadata(
                    Arc::new(ferrofin_providers::TmdbClient::new()),
                    meta_root.clone(),
                );

        let people = FerrofinPeopleRepository::new(db.clone());
        scanner
            .enrich_people(
                &people,
                vec![WrittenPerson {
                    id: person_id,
                    // No biography lookup: that branch is the only one that
                    // would reach the network.
                    needs_details: false,
                    image_url: Some("https://image.tmdb.invalid/ada.jpg".into()),
                    provider_id: None,
                }],
            )
            .await;

        let images = items.get_image_infos(person_id).await.expect("images");
        assert_eq!(images.len(), 1, "the person's primary art must be stored");
        assert_eq!(
            (images[0].width, images[0].height),
            (48, 64),
            "person art must be stored with its probed dimensions, not 0x0"
        );
        assert!(
            images[0]
                .blur_hash
                .as_deref()
                .is_some_and(|h| !h.is_empty()),
            "person art must carry the blurhash the same probe produces"
        );
    }

    // The post-scan studio pass downloads the artwork-repository thumb for a
    // materialized Studio row without images — and skips it once it has one.
    #[tokio::test(flavor = "multi_thread")]
    async fn enrich_studio_images_downloads_thumbs_for_bare_studios() {
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

        // A movie crediting the studio; saving its item values materializes
        // the Studio by-name row.
        let movie_id = uuid::Uuid::new_v4();
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(movie_id),
                type_: stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
                name: Some("Solaris".into()),
                ..Default::default()
            }])
            .await
            .expect("seed movie");
        persistence
            .save_item_values(movie_id, &[(3, "Mosfilm".into())])
            .await
            .expect("materialize studio");
        let studios = items
            .get_studios(&ferrofin_traits::options::InternalItemsQuery::default())
            .await
            .expect("studios");
        assert_eq!(studios.items.len(), 1);
        let studio_id = uuid::Uuid::parse_str(&studios.items[0].item.id).unwrap();

        let base = spawn_art_server("Mosfilm", b"JPEGDATA");
        let tmp = tempfile::tempdir().unwrap();
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_items(Arc::clone(&items))
                .with_metadata_dir(tmp.path().join("metadata"))
                .with_studio_images(Arc::new(ferrofin_providers::StudiosClient::with_repo_url(
                    &base,
                )));

        scanner.enrich_studio_images().await.expect("enrich");

        let images = items.get_image_infos(studio_id).await.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].image_type,
            ferrofin_model::entities::ImageType::Thumb
        );
        assert_eq!(std::fs::read(&images[0].path).unwrap(), b"JPEGDATA");

        // Idempotent: a second pass leaves the single image row in place.
        scanner.enrich_studio_images().await.expect("re-enrich");
        let images = items.get_image_infos(studio_id).await.expect("images");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn album_name_consensus_needs_agreement() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let track = |album: Option<&str>| BaseItemEntity {
            album: album.map(ToOwned::to_owned),
            ..Default::default()
        };
        // All tagged the same → that name wins over the folder stem.
        assert_eq!(
            super::album_name_consensus(&[
                track(Some("Californication")),
                track(Some("Californication")),
            ]),
            Some("Californication".to_owned())
        );
        // Untagged tracks don't veto the consensus.
        assert_eq!(
            super::album_name_consensus(&[track(Some("Californication")), track(None)]),
            Some("Californication".to_owned())
        );
        // Disagreement (or nothing tagged) leaves the folder-derived name.
        assert_eq!(
            super::album_name_consensus(&[track(Some("A")), track(Some("B"))]),
            None
        );
        assert_eq!(super::album_name_consensus(&[track(None)]), None);
    }

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

        scanner
            .enrich_music(&std::collections::HashMap::new())
            .await
            .expect("enrich");

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

    // The music pass honors the per-library MusicBrainz checkbox: rows whose
    // owning library disabled it are skipped entirely (resolved per row via
    // TopParentId).
    #[tokio::test]
    async fn enrich_music_skips_libraries_that_disabled_musicbrainz() {
        use crate::item_persistence_service::FerrofinItemPersistenceService;
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
        use crate::test_support::test_db;
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_model::configuration::TypeOptions;
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};

        let db = test_db().await;
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let items: Arc<dyn ItemRepository> =
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup));

        let library_id = uuid::Uuid::new_v4();
        let album_id = uuid::Uuid::new_v4();
        let track_id = uuid::Uuid::new_v4();
        let stored = |k| stored_type_name(k).unwrap().to_owned();
        persistence
            .save_items(&[
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(album_id),
                    type_: stored(BaseItemKind::MusicAlbum),
                    name: Some("Kind of Blue".into()),
                    top_parent_id: Some(ferrofin_db::store::guid_to_db(library_id)),
                    ..Default::default()
                },
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(track_id),
                    type_: stored(BaseItemKind::Audio),
                    name: Some("So What".into()),
                    parent_id: Some(ferrofin_db::store::guid_to_db(album_id)),
                    ..Default::default()
                },
            ])
            .await
            .expect("seed");
        persistence
            .save_provider_id(track_id, "MusicBrainzAlbum", "rel-x")
            .await
            .unwrap();

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

        // The owning library saved a MusicAlbum entry WITHOUT MusicBrainz.
        let options = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("MusicAlbum".to_owned()),
                metadata_fetchers: vec!["TheAudioDB".to_owned()],
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        };
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            library_id,
            super::FetcherPolicy {
                options: Some(&options),
            },
        );
        scanner.enrich_music(&policies).await.expect("enrich");

        let album_rel = items
            .get_items_with_provider_id("MusicBrainzAlbum")
            .await
            .unwrap();
        assert!(
            !album_rel.contains(&(album_id, "rel-x".to_owned())),
            "a library that disabled MusicBrainz must not gain mb ids: {album_rel:?}"
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
            // A real Episode row: the type drives the per-kind sort-name rule.
            type_: crate::item_type_lookup::stored_type_name(
                ferrofin_model::data::BaseItemKind::Episode,
            )
            .unwrap()
            .to_owned(),
            name: Some("GoT.S01E01.1080p.Bluray".into()),
            sort_name: Some("got.s01e01.1080p.bluray".into()),
            path: Some("/tv/GoT/Season 1/GoT.S01E01.1080p.Bluray.mkv".into()),
            parent_index_number: Some(1),
            index_number: Some(1),
            ..Default::default()
        };
        super::apply_tvdb_episode(&mut placeholder, &ep);
        assert_eq!(placeholder.name.as_deref(), Some("Winter Is Coming"));
        // The new title must NOT become an alphabetical sort name: an episode
        // sorts by its position, or the client's play queue scrambles.
        assert_eq!(
            placeholder.sort_name.as_deref(),
            Some("001 - 0001 - Winter Is Coming")
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

    // An episode's Cast & Crew is the EPISODE's credits. With no TMDB id for
    // the series (or nothing from TMDB), the episode's own TVDB credits stand —
    // the series' regular cast is NOT merged in, which is what made every
    // episode page show the series list verbatim.
    #[tokio::test]
    async fn episode_people_are_the_episodes_own_credits() {
        use ferrofin_providers::{TvdbEpisodeDetails, TvdbPerson};

        let ep = TvdbEpisodeDetails {
            people: vec![
                TvdbPerson {
                    name: "Guest Star".into(),
                    person_type: "GuestStar".into(),
                    role: Some("Villain".into()),
                    image_url: None,
                },
                TvdbPerson {
                    name: "Ep Director".into(),
                    person_type: "Director".into(),
                    role: None,
                    image_url: None,
                },
            ],
            ..TvdbEpisodeDetails::default()
        };

        let tmp = tempfile::tempdir().unwrap();
        let db = crate::test_support::test_db().await;
        let persistence = Arc::new(
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone()),
        );
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence);

        // No TMDB client wired → the episode's TVDB credits are used as-is.
        let people = scanner.episode_people(Some(1399), 1, 1, &ep).await;
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Guest Star", "Ep Director"]);
        assert_eq!(people[0].person_type.as_deref(), Some("GuestStar"));

        // Nor does a series without a TMDB id reach for TMDB.
        let people = scanner.episode_people(None, 1, 1, &ep).await;
        assert_eq!(people.len(), 2);
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

    // A track's embedded cover becomes its Primary image, stored in the
    // metadata dir (never next to the user's media), and the album inherits it.
    #[tokio::test]
    async fn embedded_cover_art_lands_on_the_track_and_its_album() {
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

        let tmp = tempfile::tempdir().unwrap();
        let track_path = tmp.path().join("01 - So What.flac");
        std::fs::write(&track_path, b"audio").unwrap();

        let album_id = uuid::Uuid::new_v4();
        let track_id = uuid::Uuid::new_v4();
        persistence
            .save_items(&[
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(album_id),
                    type_: stored_type_name(BaseItemKind::MusicAlbum)
                        .unwrap()
                        .to_owned(),
                    name: Some("Kind of Blue".into()),
                    ..Default::default()
                },
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(track_id),
                    type_: stored_type_name(BaseItemKind::Audio).unwrap().to_owned(),
                    name: Some("So What".into()),
                    parent_id: Some(ferrofin_db::store::guid_to_db(album_id)),
                    path: Some(track_path.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            ])
            .await
            .expect("seed");
        let track = items.retrieve_item(track_id).await.unwrap().unwrap();

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let meta = tmp.path().join("metadata");
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_probe(
                    Arc::new(FakeProbe),
                    Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
                    Arc::new(crate::chapter_repository::FerrofinChapterRepository::new(
                        db.clone(),
                    )),
                )
                .with_items(Arc::clone(&items))
                .with_metadata_dir(meta.clone());

        // The probe's stream rows: an audio stream plus the attached picture.
        let streams = vec![
            super::MediaStreamInfoEntity {
                item_id: ferrofin_db::store::guid_to_db(track_id),
                stream_index: 0,
                stream_type: 1,
                ..Default::default()
            },
            super::MediaStreamInfoEntity {
                item_id: ferrofin_db::store::guid_to_db(track_id),
                stream_index: 1,
                stream_type: super::EMBEDDED_IMAGE_STREAM_TYPE,
                ..Default::default()
            },
        ];
        let images = scanner
            .extract_embedded_cover(track_id, &track, &streams)
            .await;
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].image_type,
            ferrofin_model::entities::ImageType::Primary
        );
        let stored = std::path::Path::new(&images[0].path);
        assert!(
            stored.starts_with(&meta),
            "cover lives under the metadata dir, not the library: {stored:?}"
        );
        assert_eq!(std::fs::read(stored).unwrap(), b"COVER");
        // ffmpeg's scratch file next to the media is cleaned up.
        assert!(!tmp.path().join("01 - So What.flac.image.jpg").exists());

        // Persist it, then the album inherits it (upstream AlbumImageProvider).
        persistence
            .save_item_images(track_id, &images)
            .await
            .expect("save track image");
        scanner.inherit_album_cover(album_id, items.as_ref()).await;
        let album_images = items.get_image_infos(album_id).await.unwrap();
        assert_eq!(album_images.len(), 1);
        assert_eq!(album_images[0].path, images[0].path);

        // A track with no image stream extracts nothing.
        let none = scanner
            .extract_embedded_cover(track_id, &track, &streams[..1])
            .await;
        assert!(none.is_empty());
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
            path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            // Mirrors the real encoder: ffmpeg writes `{path}.image.jpg` next
            // to the media file, and the caller relocates it.
            let out = format!("{path}.image.jpg");
            std::fs::write(&out, b"COVER").expect("write cover");
            Ok(out)
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

    /// A probe that answers with a duration derived from the file it was given
    /// and records how many probes were in flight at once.
    ///
    /// Both halves are load-bearing: the per-path duration is what catches a
    /// pipelined result being handed to the wrong item, and the high-water mark
    /// is what catches the pipeline silently degrading to serial.
    struct TracingProbe {
        /// Probes currently running.
        live: Arc<std::sync::atomic::AtomicUsize>,
        /// The most probes ever running at the same time.
        peak: Arc<std::sync::atomic::AtomicUsize>,
        /// When set, a probe blocks until this many probes are inside it at
        /// once. That makes the overlap assertion deterministic instead of a
        /// race against the scheduler: on a serial pipeline the rendezvous can
        /// never complete, the probe falls out on [`RENDEZVOUS_TIMEOUT`], and
        /// the peak stays at 1.
        rendezvous: Option<Arc<tokio::sync::Barrier>>,
        /// Set once a rendezvous has completed. One is all the assertion needs,
        /// and retiring it keeps a genuinely serial pipeline to a single
        /// timeout instead of one per probe.
        met: Arc<std::sync::atomic::AtomicBool>,
    }

    /// How long the rendezvous probe waits for its peers before giving up. Only
    /// reached when the pipeline failed to overlap — the assertion then fails
    /// on the peak, rather than hanging the test.
    const RENDEZVOUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// How many probes must meet inside the fake for the wide-window test.
    /// Below the window under test (8) and well below the file count, so a
    /// working pipeline always reaches it.
    const RENDEZVOUS_WIDTH: usize = 4;

    impl TracingProbe {
        /// The duration a file at `path` probes as: `<n> * 1_000_000` ticks for
        /// `... <n>.mkv`, so every fixture file has a distinct, checkable value.
        fn ticks_for(path: &str) -> i64 {
            let stem = path.rsplit('/').next().unwrap_or_default();
            let digits: String = stem.chars().filter(char::is_ascii_digit).collect();
            digits.parse::<i64>().unwrap_or(0) * 1_000_000
        }
    }

    #[async_trait]
    impl MediaEncoder for TracingProbe {
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
            request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            use std::sync::atomic::Ordering;
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            if let Some(barrier) = &self.rendezvous
                && !self.met.load(Ordering::SeqCst)
            {
                let met = tokio::time::timeout(RENDEZVOUS_TIMEOUT, barrier.wait())
                    .await
                    .is_ok();
                self.met.store(met, Ordering::SeqCst);
            }
            tokio::task::yield_now().await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            let path = request.media_source.path.clone().unwrap_or_default();
            Ok(MediaSourceInfo {
                run_time_ticks: Some(Self::ticks_for(&path)),
                path: Some(path),
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

    /// Builds a movies+tv library over `tmp`, scans it with `concurrency`
    /// probes in flight, and returns the tracing probe's peak concurrency plus
    /// the persistence handle to read rows back through.
    async fn scan_with_probe_concurrency(
        tmp: &std::path::Path,
        movies: usize,
        concurrency: usize,
        rendezvous: Option<usize>,
    ) -> (
        usize,
        Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        Vec<(ferrofin_model::data::BaseItemKind, String)>,
    ) {
        let media = tmp.join("movies");
        let tv = tmp.join("tv").join("Series 01").join("Season 01");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::create_dir_all(&tv).unwrap();
        let mut expected: Vec<(ferrofin_model::data::BaseItemKind, String)> = Vec::new();
        for i in 1..=movies {
            let path = media.join(format!("Movie {i:03}.mkv"));
            std::fs::write(&path, b"").unwrap();
            expected.push((
                ferrofin_model::data::BaseItemKind::Movie,
                path.to_string_lossy().into_owned(),
            ));
        }
        // Episodes under a Series/Season folder pair: the folder rows are NOT
        // probe-eligible, so they are exactly what a pipeline that failed to
        // skip non-media items would misalign on.
        for e in 1..=movies {
            let path = tv.join(format!("Series 01 S01E{e:03}.mkv"));
            std::fs::write(&path, b"").unwrap();
            expected.push((
                ferrofin_model::data::BaseItemKind::Episode,
                path.to_string_lossy().into_owned(),
            ));
        }

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, kind, path) in [
            ("Movies", CollectionTypeOptions::movies, media),
            ("Shows", CollectionTypeOptions::tvshows, tmp.join("tv")),
        ] {
            vf.add_virtual_folder(
                name,
                Some(kind),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: path.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        }

        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe = Arc::new(TracingProbe {
            live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::clone(&peak),
            rendezvous: rendezvous.map(|n| Arc::new(tokio::sync::Barrier::new(n))),
            met: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let scanner = LibraryScanner::new(
            Arc::clone(&vf),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_probe_concurrency(concurrency)
        .with_probe(
            probe,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            Arc::new(crate::chapter_repository::FerrofinChapterRepository::new(
                db.clone(),
            )),
        );
        scanner.scan_all().await.unwrap();
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(
            crate::item_repository::FerrofinItemRepository::new(db, lookup),
        );
        (
            peak.load(std::sync::atomic::Ordering::SeqCst),
            items,
            expected,
        )
    }

    // The probe runs ahead of the persistence loop, so the one thing that can
    // silently break is a result landing on the WRONG item. Every file probes
    // as a duration derived from its own name, so a single misaligned handoff
    // (or a mis-skipped folder row) shows up as a wrong RunTimeTicks.
    #[tokio::test(flavor = "multi_thread")]
    async fn pipelined_probe_results_stay_with_their_own_items() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, items, expected) = scan_with_probe_concurrency(tmp.path(), 12, 8, None).await;
        for (kind, path) in &expected {
            let id = crate::item_type_lookup::derive_item_id(*kind, path).expect("derivable id");
            let row = items
                .retrieve_item(id)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{path} was not scanned"));
            assert_eq!(
                row.run_time_ticks,
                Some(TracingProbe::ticks_for(path)),
                "{path} got another item's probe result"
            );
        }
    }

    // ...and the pipeline must actually overlap probes: a window of 8 keeps
    // more than one ffprobe in flight, while a window of 1 is exactly the old
    // strictly-serial scan. Without this, a regression to `window = 1` would
    // still pass every correctness test in this file.
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_window_bounds_how_many_probes_overlap() {
        let wide = tempfile::tempdir().unwrap();
        let (peak_wide, _, _) =
            scan_with_probe_concurrency(wide.path(), 12, 8, Some(RENDEZVOUS_WIDTH)).await;
        assert!(
            peak_wide >= RENDEZVOUS_WIDTH,
            "a window of 8 must run {RENDEZVOUS_WIDTH} probes at once; peak was {peak_wide}"
        );
        assert!(
            peak_wide <= 8,
            "the window must bound in-flight probes; peak was {peak_wide}"
        );

        let serial = tempfile::tempdir().unwrap();
        let (peak_serial, _, _) = scan_with_probe_concurrency(serial.path(), 12, 1, None).await;
        assert_eq!(
            peak_serial, 1,
            "a window of 1 must stay strictly serial (the pre-pipeline behaviour)"
        );
    }

    // `0` is not a legal window — it would deadlock the scan on an empty
    // pipeline — so the builder clamps it to the serial case.
    #[tokio::test]
    async fn probe_concurrency_zero_clamps_to_serial() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_probe_concurrency(0);
        assert_eq!(scanner.probe_concurrency, 1);
    }

    /// Reads back the scanned movies' `(Name, SortName, ProductionYear)` rows,
    /// name-ordered — the one shared movie read-back, so each test doesn't add
    /// its own raw query (the ferrofin-db sql_boundary ratchet counts them).
    /// One movie's enrichment columns + its provider ids (concatenated as
    /// `Key=Value,...`) in a single query — the ONE raw-SQL site shared by
    /// the metadata tests (the sql_boundary ratchet counts call sites).
    async fn movie_detail_row(
        db: &Database,
        name: &str,
    ) -> (
        Option<String>, // Overview
        Option<i64>,    // ProductionYear
        Option<f64>,    // CommunityRating
        Option<String>, // Genres
        Option<String>, // Studios
        Option<String>, // provider ids as "Key=Value,Key=Value"
    ) {
        sqlx::query_as(
            r#"SELECT b."Overview", b."ProductionYear", b."CommunityRating",
                      b."Genres", b."Studios",
                      (SELECT group_concat(p."ProviderId" || '=' || p."ProviderValue")
                         FROM "BaseItemProviders" p WHERE p."ItemId" = b."Id")
               FROM "BaseItems" b
               WHERE b."Type" LIKE '%Movies.Movie' AND b."Name" = ?1"#,
        )
        .bind(name)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

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

    /// A named (library-gated) dynamic provider whose contribution is its
    /// own name — so which provider "won" a supplement-only field is
    /// observable in the row.
    struct NamedProvider(&'static str);
    #[async_trait::async_trait]
    impl ferrofin_traits::providers::DynamicMetadataProvider for NamedProvider {
        fn name(&self) -> &str {
            self.0
        }
        fn library_gated(&self) -> bool {
            true
        }
        async fn lookup(
            &self,
            _item: &ferrofin_traits::providers::DynamicMetadataLookup,
        ) -> Result<
            Option<ferrofin_traits::providers::DynamicMetadataResult>,
            ferrofin_traits::error::ServiceError,
        > {
            Ok(Some(ferrofin_traits::providers::DynamicMetadataResult {
                overview: Some(self.0.to_owned()),
                ..Default::default()
            }))
        }
    }

    /// Builds a movie library whose `LibraryOptions` are the test's to
    /// shape, returning (scanner-less) parts: media dir populated with one
    /// bare movie + optional NFO.
    async fn gated_library(
        tmp: &std::path::Path,
        options: LibraryOptions,
        with_nfo: bool,
    ) -> (
        Arc<FerrofinItemPersistenceService>,
        Arc<dyn VirtualFolderManager>,
        Database,
    ) {
        let media = tmp.join("movies");
        let folder = media.join("Movie 0001 (2020)");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").unwrap();
        if with_nfo {
            std::fs::write(
                folder.join("movie.nfo"),
                b"<movie><title>Movie 0001</title><year>2020</year></movie>",
            )
            .unwrap();
        }
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.join("default"))
                .with_item_store(persistence.clone()),
        );
        let mut options = options;
        options.path_infos = vec![MediaPathInfo {
            path: media.to_string_lossy().into_owned(),
        }];
        vf.add_virtual_folder("Movies", Some(CollectionTypeOptions::movies), &options)
            .await
            .unwrap();
        (persistence, vf, db)
    }

    // The per-library gate is REAL: a named dynamic provider a library's
    // TypeOptions leaves unchecked never runs for that library's items,
    // and the saved fetcher order decides which enabled provider wins a
    // supplement-only field. The flat local-reader list gates the NFO
    // reader the same way.
    #[tokio::test]
    async fn library_options_gate_and_order_fetchers_per_library() {
        use ferrofin_model::configuration::TypeOptions;

        // Library A: only "beta" checked → alpha must not run, beta's
        // overview lands. NFO disabled → the NFO year must NOT land.
        let tmp = tempfile::tempdir().unwrap();
        let (persistence, vf, db) = gated_library(
            tmp.path(),
            LibraryOptions {
                disabled_local_metadata_readers: vec!["Nfo".to_owned()],
                type_options: vec![TypeOptions {
                    type_: Some("Movie".to_owned()),
                    metadata_fetchers: vec!["beta".to_owned()],
                    ..TypeOptions::default()
                }],
                ..LibraryOptions::default()
            },
            true,
        )
        .await;
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_dynamic_providers(vec![
                Arc::new(NamedProvider("alpha")),
                Arc::new(NamedProvider("beta")),
            ]);
        scanner.scan_all().await.unwrap();
        // The NFO reader is disabled, so its <title> never lands: the row
        // keeps the folder-derived name (incl. year) — the successful
        // lookup below IS the NFO-gate assertion.
        let (overview, ..) = movie_detail_row(&db, "Movie 0001 (2020)").await;
        assert_eq!(
            overview.as_deref(),
            Some("beta"),
            "only the checked fetcher may run"
        );

        // Library B: both checked, order says beta first → beta wins the
        // supplement-only overview even though alpha registered first.
        let tmp = tempfile::tempdir().unwrap();
        let (persistence, vf, db) = gated_library(
            tmp.path(),
            LibraryOptions {
                type_options: vec![TypeOptions {
                    type_: Some("Movie".to_owned()),
                    metadata_fetchers: vec!["alpha".to_owned(), "beta".to_owned()],
                    metadata_fetcher_order: vec!["beta".to_owned(), "alpha".to_owned()],
                    ..TypeOptions::default()
                }],
                ..LibraryOptions::default()
            },
            false,
        )
        .await;
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_dynamic_providers(vec![
                Arc::new(NamedProvider("alpha")),
                Arc::new(NamedProvider("beta")),
            ]);
        scanner.scan_all().await.unwrap();
        let (overview, ..) = movie_detail_row(&db, "Movie 0001 (2020)").await;
        assert_eq!(
            overview.as_deref(),
            Some("beta"),
            "the saved fetcher order picks the winner"
        );

        // Library C: no TypeOptions entry at all → everything enabled in
        // registration order; NFO enabled → the year lands.
        let tmp = tempfile::tempdir().unwrap();
        let (persistence, vf, db) =
            gated_library(tmp.path(), LibraryOptions::default(), true).await;
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_dynamic_providers(vec![
                Arc::new(NamedProvider("alpha")),
                Arc::new(NamedProvider("beta")),
            ]);
        scanner.scan_all().await.unwrap();
        // The NFO ran (default policy): its <title> renamed the row.
        let (overview, ..) = movie_detail_row(&db, "Movie 0001").await;
        assert_eq!(overview.as_deref(), Some("alpha"));
    }

    // The dynamic artwork pass fills only the slots the built-in chain left
    // empty, writes bytes as {metadata}/{id}/{stem}.jpg + a persisted image
    // row, and never overwrites art already on disk (earlier scan or user
    // upload) — a re-scan offering different bytes must keep the original.
    #[tokio::test]
    // Full scan harness + on-disk and DB assertions; the sequence is the point.
    #[allow(clippy::items_after_statements, clippy::too_many_lines)]
    async fn dynamic_provider_supplies_missing_artwork_but_never_overwrites() {
        use ferrofin_model::entities::ImageType;

        // PNG magic + a distinguishable payload: the artwork pass sniffs the
        // format before persisting, so plain b"FIRST" would be rejected.
        const PNG_FIRST: &[u8] = b"\x89PNG\r\n\x1a\nFIRST";
        const PNG_SECOND: &[u8] = b"\x89PNG\r\n\x1a\nSECOND";

        struct ArtDb {
            calls: std::sync::Mutex<u32>,
        }
        #[async_trait::async_trait]
        impl ferrofin_traits::providers::DynamicMetadataProvider for ArtDb {
            fn name(&self) -> &'static str {
                "art-db"
            }
            fn library_gated(&self) -> bool {
                // The artwork pass only asks named (provider-info) plugins.
                true
            }
            async fn lookup(
                &self,
                _item: &ferrofin_traits::providers::DynamicMetadataLookup,
            ) -> Result<
                Option<ferrofin_traits::providers::DynamicMetadataResult>,
                ferrofin_traits::error::ServiceError,
            > {
                Ok(None)
            }
            async fn images(
                &self,
                _item: &ferrofin_traits::providers::DynamicMetadataLookup,
                wanted: &[ImageType],
            ) -> Result<Vec<(ImageType, Vec<u8>)>, ferrofin_traits::error::ServiceError>
            {
                let call = {
                    let mut calls = self.calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                assert!(
                    wanted.contains(&ImageType::Primary) || call > 1,
                    "first pass must ask for the missing Primary"
                );
                // Different bytes per call: a re-download would be visible.
                // The Backdrop is garbage (no image magic) — the sniff must
                // drop it instead of persisting a permanent 0×0 row.
                Ok(vec![
                    (
                        ImageType::Primary,
                        if call == 1 {
                            PNG_FIRST.to_vec()
                        } else {
                            PNG_SECOND.to_vec()
                        },
                    ),
                    (ImageType::Backdrop, b"not an image".to_vec()),
                ])
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let folder = media.join("Movie 0001 (2020)");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").unwrap();

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

        let meta_dir = tmp.path().join("metadata");
        let provider = Arc::new(ArtDb {
            calls: std::sync::Mutex::new(0),
        });
        let scanner = LibraryScanner::new(
            vf.clone(),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_metadata_dir(meta_dir.clone())
        .with_dynamic_providers(vec![provider.clone()]);
        scanner.scan_all().await.unwrap();

        // The bytes landed under the SNIFFED extension — PNG magic ⇒
        // primary.png, not a mislabeled primary.jpg.
        let primary = std::fs::read_dir(&meta_dir)
            .expect("metadata dir exists")
            .map(|e| e.unwrap().path().join("primary.png"))
            .find(|p| p.exists())
            .expect("dynamic Primary written to disk as .png");
        assert!(
            !primary.with_file_name("primary.jpg").exists(),
            "PNG bytes must not be stored under a .jpg name"
        );
        assert_eq!(std::fs::read(&primary).unwrap(), PNG_FIRST);
        // …and as a persisted Primary image row pointing at that file
        // (read back through the repository, not raw SQL — boundary rule).
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> =
            Arc::new(crate::item_repository::FerrofinItemRepository::new(
                db.clone(),
                Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
            ));
        let item_id = uuid::Uuid::parse_str(
            &primary
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
        )
        .expect("art dir is the item id");
        let rows = items.get_image_infos(item_id).await.unwrap();
        assert!(
            rows.iter()
                .any(|i| i.image_type == ImageType::Primary && i.path == primary.to_string_lossy()),
            "Primary image row persists; got {rows:?}"
        );
        assert!(
            !rows.iter().any(|i| i.image_type == ImageType::Backdrop),
            "the contributed Backdrop was not a real image — the sniff must \
             reject it, on disk and in the DB; got {rows:?}"
        );
        assert!(
            !primary.with_file_name("backdrop.jpg").exists(),
            "rejected bytes must never reach disk"
        );

        // Re-scan: art on disk wins; the provider's new bytes are ignored.
        scanner.scan_all().await.unwrap();
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            PNG_FIRST,
            "existing artwork must never be overwritten by a re-scan"
        );
    }

    // The dynamic (Tier-1b WASM) metadata pass is SUPPLEMENT-ONLY: it fills
    // fields the built-in chain left empty and merges its provider ids, but a
    // value already present (here: the NFO year) must never be overwritten.
    #[tokio::test]
    // Three provider structs + full scan harness + layered assertions; the
    // sequence is the point.
    #[allow(clippy::too_many_lines)]
    async fn dynamic_provider_supplements_but_never_overwrites() {
        struct HelloDb;
        #[async_trait::async_trait]
        impl ferrofin_traits::providers::DynamicMetadataProvider for HelloDb {
            fn name(&self) -> &'static str {
                "hello-db"
            }
            async fn lookup(
                &self,
                item: &ferrofin_traits::providers::DynamicMetadataLookup,
            ) -> Result<
                Option<ferrofin_traits::providers::DynamicMetadataResult>,
                ferrofin_traits::error::ServiceError,
            > {
                assert_eq!(item.kind, "Movie");
                Ok(Some(ferrofin_traits::providers::DynamicMetadataResult {
                    tagline: None,
                    studios: Vec::new(),
                    tags: Vec::new(),
                    official_rating: None,
                    end_date: None,
                    overview: Some("From the dynamic provider".to_owned()),
                    production_year: Some(1980), // must NOT overwrite the NFO's 2020
                    community_rating: Some(6.5),
                    genres: vec!["Docufiction".to_owned()],
                    provider_ids: vec![("HelloDb".to_owned(), "x1".to_owned())],
                }))
            }
        }
        /// A later source trying to steal an id an earlier one recorded
        /// (case-insensitively) — supplement-only applies to ids too.
        struct IdThief;
        #[async_trait::async_trait]
        impl ferrofin_traits::providers::DynamicMetadataProvider for IdThief {
            fn name(&self) -> &'static str {
                "id-thief"
            }
            async fn lookup(
                &self,
                _item: &ferrofin_traits::providers::DynamicMetadataLookup,
            ) -> Result<
                Option<ferrofin_traits::providers::DynamicMetadataResult>,
                ferrofin_traits::error::ServiceError,
            > {
                Ok(Some(ferrofin_traits::providers::DynamicMetadataResult {
                    tagline: None,
                    studios: Vec::new(),
                    tags: Vec::new(),
                    official_rating: None,
                    end_date: None,
                    provider_ids: vec![("helloDB".to_owned(), "stolen".to_owned())],
                    ..Default::default()
                }))
            }
        }
        /// A second source whose failure must not fail the scan.
        struct Broken;
        #[async_trait::async_trait]
        impl ferrofin_traits::providers::DynamicMetadataProvider for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            async fn lookup(
                &self,
                _item: &ferrofin_traits::providers::DynamicMetadataLookup,
            ) -> Result<
                Option<ferrofin_traits::providers::DynamicMetadataResult>,
                ferrofin_traits::error::ServiceError,
            > {
                Err(ferrofin_traits::error::ServiceError::backend("boom"))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let folder = media.join("Movie 0001 (2020)");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").unwrap();
        // The NFO supplies the year (2020) but neither overview nor genres.
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
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_dynamic_providers(vec![
                    Arc::new(Broken),
                    Arc::new(HelloDb),
                    Arc::new(IdThief),
                ]);
        scanner.scan_all().await.unwrap();

        let (overview, year, rating, genres, _studios, ids) =
            movie_detail_row(&db, "Movie 0001").await;
        assert_eq!(overview.as_deref(), Some("From the dynamic provider"));
        assert_eq!(year, Some(2020), "the NFO year is never overwritten");
        assert_eq!(rating, Some(6.5));
        assert_eq!(genres.as_deref(), Some("Docufiction"));
        let ids = ids.unwrap_or_default();
        assert!(ids.contains("HelloDb=x1"), "dynamic provider ids persist");
        assert!(
            !ids.contains("stolen"),
            "a later source cannot replace an earlier id (got {ids})"
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

        let (overview, _year, _rating, genres, studios, _ids) =
            movie_detail_row(&db, "The Matrix").await;
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

    // Extras (suffix- and directory-classified) become OWNED rows attached to
    // their movie, never Movie rows — feeding /LocalTrailers and the
    // hasTrailer/… filters while staying out of the library grid.
    #[tokio::test]
    async fn scan_attaches_extras_to_their_movie() {
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::options::InternalItemsQuery;
        use ferrofin_traits::persistence::ItemRepository as _;
        use uuid::Uuid;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let folder = media.join("Heat (1995)");
        std::fs::create_dir_all(folder.join("trailers")).unwrap();
        std::fs::write(folder.join("Heat (1995).mkv"), b"").unwrap();
        std::fs::write(folder.join("Heat (1995)-trailer.mkv"), b"").unwrap();
        std::fs::write(folder.join("trailers").join("alt.mkv"), b"").unwrap();

        let (db, _cf) = scan_one(CollectionTypeOptions::movies, "Movies", &media).await;

        // Assert through the repository seams (the same queries the
        // /LocalTrailers endpoint and browse run).
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let movies = repo
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                ..Default::default()
            })
            .await
            .expect("movies");
        assert_eq!(movies.len(), 1, "extras never become Movie rows");
        let movie_id = Uuid::parse_str(&movies[0].id).expect("movie id");

        let trailers = repo
            .get_item_list(&InternalItemsQuery {
                owner_ids: vec![movie_id],
                extra_types: vec![ferrofin_model::entities::ExtraType::Trailer],
                ..Default::default()
            })
            .await
            .expect("trailers");
        assert_eq!(
            trailers.len(),
            2,
            "both trailer spellings attach to the movie"
        );
    }

    // A disc rip is one item for its folder (never one per .vob/.m2ts), and
    // its VideoType lands in Data — what the browse `videoTypes` filter reads.
    #[tokio::test]
    async fn disc_rips_resolve_to_one_movie_with_a_video_type() {
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        let bluray = media.join("Avatar (2009)").join("BDMV").join("STREAM");
        std::fs::create_dir_all(&bluray).unwrap();
        std::fs::write(bluray.join("00001.m2ts"), b"").unwrap();
        let dvd = media.join("Alien (1979)").join("VIDEO_TS");
        std::fs::create_dir_all(&dvd).unwrap();
        std::fs::write(dvd.join("VTS_01_1.VOB"), b"").unwrap();
        // A plain file alongside them stays a normal VideoFile movie.
        let flat = media.join("Heat (1995)");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join("Heat (1995).mkv"), b"").unwrap();

        let (db, _cf) = scan_one(CollectionTypeOptions::movies, "Movies", &media).await;

        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let movies = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::Movie],
                ..Default::default()
            })
            .await
            .expect("movies");
        assert_eq!(movies.len(), 3, "one item per disc folder, not per stream");

        let data_of = |name: &str| {
            movies
                .iter()
                .find(|m| m.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} resolved"))
                .data
                .clone()
                .unwrap_or_default()
        };
        assert!(data_of("Avatar (2009)").contains(r#""VideoType":"BluRay""#));
        assert!(data_of("Alien (1979)").contains(r#""VideoType":"Dvd""#));
        assert!(data_of("Heat (1995)").contains(r#""VideoType":"VideoFile""#));
    }

    #[test]
    fn series_match_prefers_the_folder_year() {
        use ferrofin_providers::TvdbSearchHit;
        let hit = |name: &str, year: i32| TvdbSearchHit {
            tvdb_id: i64::from(year),
            name: name.to_owned(),
            year: Some(year),
            image_url: None,
            overview: None,
        };
        // TVDB ranks the 2005 revival first; the folder says 1963.
        let hits = vec![hit("Doctor Who", 2005), hit("Doctor Who", 1963)];
        assert_eq!(
            super::pick_series_hit(hits.clone(), Some(1963))
                .unwrap()
                .year,
            Some(1963)
        );
        // No year, or no candidate matching it → the API's own ordering stands.
        assert_eq!(
            super::pick_series_hit(hits.clone(), None).unwrap().year,
            Some(2005)
        );
        assert_eq!(
            super::pick_series_hit(hits, Some(1999)).unwrap().year,
            Some(2005)
        );
        assert!(super::pick_series_hit(Vec::new(), Some(1963)).is_none());
    }

    #[test]
    fn iso_files_carry_their_disc_type() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_model::entities::VideoType;
        assert_eq!(super::file_video_type("/m/Movie.iso"), VideoType::Iso);
        assert_eq!(super::file_video_type("/m/Movie.img"), VideoType::Iso);
        assert_eq!(super::file_video_type("/m/Movie.mkv"), VideoType::VideoFile);

        // Upstream's path-substring heuristic fills IsoType.
        let mut entity = BaseItemEntity {
            path: Some("/m/bluray/Movie.iso".to_owned()),
            ..Default::default()
        };
        super::set_video_type(&mut entity, VideoType::Iso);
        let data = entity.data.clone().unwrap();
        assert!(data.contains(r#""VideoType":"Iso""#));
        assert!(data.contains(r#""IsoType":"BluRay""#));
    }

    // A season's episodes must come back in EPISODE order, not alphabetical
    // order by title. Clients build the play queue from the season sorted by
    // SortName, so a title-derived key gives them a scrambled queue: the Next
    // button lands on the wrong episode, and on the alphabetically-last one it
    // has nowhere to go (dead button, no autoplay). File stems here are
    // deliberately inverted against their episode numbers.
    // The library tile samples the collection type's own kinds — a music
    // library's tile comes from its ALBUMS (whose covers exist), not from leaf
    // tracks (which usually have none), and never from the wrong kind.
    #[test]
    fn collage_samples_the_collection_types_kinds() {
        use ferrofin_model::data::BaseItemKind;
        let folder = |ct| ferrofin_model::entities_media::VirtualFolderInfo {
            collection_type: ct,
            ..Default::default()
        };
        assert_eq!(
            super::collage_item_kinds(&folder(Some(CollectionTypeOptions::music))),
            vec![BaseItemKind::MusicAlbum]
        );
        assert_eq!(
            super::collage_item_kinds(&folder(Some(CollectionTypeOptions::tvshows))),
            vec![BaseItemKind::Series]
        );
        assert_eq!(
            super::collage_item_kinds(&folder(Some(CollectionTypeOptions::movies))),
            vec![BaseItemKind::Movie]
        );
        // An untyped (mixed) library samples the mixed set, as upstream does.
        assert!(
            super::collage_item_kinds(&folder(None)).contains(&BaseItemKind::Movie)
                && super::collage_item_kinds(&folder(None)).contains(&BaseItemKind::Audio)
        );
    }

    #[tokio::test]
    async fn season_episodes_sort_by_number_not_title() {
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("tv");
        let season = media.join("Show").join("Season 01");
        std::fs::create_dir_all(&season).unwrap();
        for (stem, _) in [
            ("Zebra S01E01", 1),
            ("Alpha S01E02", 2),
            ("Middle S01E03", 3),
        ] {
            std::fs::write(season.join(format!("{stem}.mkv")), b"").unwrap();
        }

        let (db, _cf) = scan_one(CollectionTypeOptions::tvshows, "TV", &media).await;

        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let by_kind = async |kind| {
            repo.get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![kind],
                recursive: true,
                ..Default::default()
            })
            .await
            .expect("items")
        };

        let mut episodes = by_kind(ferrofin_model::data::BaseItemKind::Episode).await;
        episodes.sort_by(|a, b| a.sort_name.cmp(&b.sort_name));
        assert_eq!(
            episodes.iter().map(|e| e.index_number).collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)],
            "SortName order must be episode order: {:?}",
            episodes.iter().map(|e| &e.sort_name).collect::<Vec<_>>()
        );
        assert!(
            episodes[0]
                .sort_name
                .as_deref()
                .unwrap()
                .starts_with("001 - 0001 - "),
            "padded position prefix (Episode.CreateSortName): {:?}",
            episodes[0].sort_name
        );

        // The season row keys on its number too (Specials first, then 1..N).
        let seasons = by_kind(ferrofin_model::data::BaseItemKind::Season).await;
        assert_eq!(seasons.len(), 1);
        assert_eq!(seasons[0].sort_name.as_deref(), Some("0001"));
    }

    #[tokio::test]
    async fn scan_builds_music_album_with_tracks() {
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("music");
        // Artist/Album layout — the album folder (with audio) is the MusicAlbum.
        let album = media.join("Pink Floyd").join("The Wall");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 In the Flesh.flac"), b"").unwrap();
        std::fs::write(album.join("02 The Thin Ice.flac"), b"").unwrap();

        // A second disc folds into the same album rather than becoming one.
        let disc2 = album.join("CD2");
        std::fs::create_dir_all(&disc2).unwrap();
        std::fs::write(disc2.join("03 Mother.flac"), b"").unwrap();

        let (db, cf) = scan_one(CollectionTypeOptions::music, "Music", &media).await;

        // The artist folder is walked through, not turned into a row of its
        // own (the browsable MusicArtist comes from the by-name materializer,
        // so a path-keyed row here would duplicate every artist).
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let artists = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::MusicArtist],
                ..Default::default()
            })
            .await
            .expect("artists");
        assert!(artists.is_empty(), "no path-keyed artist rows: {artists:?}");

        // Exactly one album — CD2 folds in rather than becoming its own.
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
        assert_eq!(tracks.len(), 3, "both discs' tracks join the album");
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

    /// Writes an empty file at `dir/file`, creating `dir`. Shared by the books
    /// scan tests, which only care that a path exists with the right name.
    fn touch(dir: &std::path::Path, file: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), b"").unwrap();
    }

    /// Every `BaseItems` row of one kind, ordered by name.
    async fn scanned_by_kind(
        db: &Database,
        kind: ferrofin_model::data::BaseItemKind,
    ) -> Vec<ferrofin_db::entities::base_items::BaseItemEntity> {
        use ferrofin_traits::persistence::ItemRepository as _;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let mut items = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![kind],
                recursive: true,
                ..Default::default()
            })
            .await
            .expect("items");
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    // Documents in a books library resolve to `Book` across every structural
    // shape upstream handles: a loose file, a folder that IS one book, and a
    // folder holding several (which flattens to one row per file).
    #[tokio::test]
    async fn scan_builds_books_from_documents() {
        use ferrofin_model::data::BaseItemKind;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("books");

        // A loose document names and dates itself from its own filename.
        touch(
            &media,
            "A Study in Scarlet (Sherlock Holmes, #1) (1887).epub",
        );
        // A folder holding exactly one document IS that book, named after the
        // folder — the file inside can be called anything.
        touch(&media.join("Dracula"), "dracula-text-final.epub");
        // Two documents in one folder are two books; the folder name stands in
        // for a series the filename does not name.
        let holmes = media.join("Sherlock Holmes");
        touch(&holmes, "Sherlock Holmes #2.epub");
        touch(&holmes, "The Hound of the Baskervilles.epub");
        // Extension matching is case-insensitive, and the comic volume/chapter
        // pattern fills ParentIndexNumber/IndexNumber. At the root, so it takes
        // the file arm — in its own folder it would be the folder-is-a-book
        // shape and parse the folder name instead.
        touch(&media, "Saga v02 c07.CBZ");
        // Not a document — never an item.
        touch(&media, "README.txt");

        let (db, cf) = scan_one(CollectionTypeOptions::books, "Books", &media).await;
        let books = scanned_by_kind(&db, BaseItemKind::Book).await;

        assert_eq!(
            books.iter().map(|b| b.name.as_deref()).collect::<Vec<_>>(),
            vec![
                Some("A Study in Scarlet"),
                Some("Dracula"),
                // The comic regex feeds volume/chapter but does NOT trim them
                // off the name: upstream assigns the whole `name` group, not
                // the comic sub-capture.
                Some("Saga v02 c07"),
                // The series/index pattern captures no title of its own, so
                // upstream's empty `Name` falls back to the file stem
                // (`ResolverHelper.EnsureName`) rather than to the series.
                Some("Sherlock Holmes #2"),
                Some("The Hound of the Baskervilles"),
            ],
            "README.txt must not resolve, and each shape must name itself"
        );

        let study = &books[0];
        assert_eq!(study.series_name.as_deref(), Some("Sherlock Holmes"));
        assert_eq!(study.index_number, Some(1));
        assert_eq!(study.production_year, Some(1887));
        assert_eq!(study.media_type.as_deref(), Some("Book"));
        // Upstream's `Book()` gives every book a one-second runtime.
        assert_eq!(study.run_time_ticks, Some(super::BOOK_RUN_TIME_TICKS));
        assert!(!study.is_folder);

        // The single-document folder points at the file, not the folder.
        assert!(
            books[1]
                .path
                .as_deref()
                .unwrap()
                .ends_with("dracula-text-final.epub"),
            "path is the document: {:?}",
            books[1].path
        );
        // Upstream's `GetBook` sets `SeriesName = result.SeriesName ?? ""`, and
        // serializes the empty string rather than omitting the key.
        assert_eq!(books[1].series_name.as_deref(), Some(""));

        // `.CBZ` matched case-insensitively, and the comic `vNN cNN` pattern
        // reached ParentIndexNumber/IndexNumber through the row.
        let saga = &books[2];
        assert_eq!(saga.parent_index_number, Some(2), "comic volume");
        assert_eq!(saga.index_number, Some(7), "comic chapter");
        // A root-level file with no parseable series falls back to the name of
        // its containing directory — which for the root IS the library folder.
        // Surprising, but it is exactly `BookResolver.Resolve`'s
        // `Path.GetFileName(Path.GetDirectoryName(args.Path))`.
        assert_eq!(saga.series_name.as_deref(), Some("books"));

        // "Sherlock Holmes #2" parses its own series; its neighbour does not,
        // so the containing folder name is the fallback.
        assert_eq!(books[3].series_name.as_deref(), Some("Sherlock Holmes"));
        assert_eq!(books[3].index_number, Some(2));
        assert_eq!(books[4].series_name.as_deref(), Some("Sherlock Holmes"));

        // Everything hangs off the library, so `?ParentId=<library>` lists it.
        assert!(
            books
                .iter()
                .all(|i| i.top_parent_id.as_deref() == Some(cf.as_str()))
        );
    }

    // Audio in a books library resolves to `AudioBook`: a folder holding one
    // audio file IS that audiobook, while the shapes upstream refuses to stack
    // fall back to a row per file.
    #[tokio::test]
    async fn scan_builds_audiobooks_from_audio() {
        use ferrofin_model::data::BaseItemKind;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("books");

        // One audio file in a folder is one audiobook, named AND dated after
        // the folder (upstream carries `resolvedItem.Year` onto the item).
        touch(&media.join("The Hobbit (1937)"), "hobbit.mp3");
        // A folder whose extra audio is classified as an EXTRA (a name that
        // does not contain the audiobook's own) fails the single-audiobook
        // guard and falls back to a row per file — upstream's
        // `Extras.Count > 0` continue.
        let dune = media.join("Dune");
        touch(&dune, "Dune.mp3");
        touch(&dune, "interview-with-the-author.mp3");
        // A multi-part audiobook is a row per file (upstream skips the stack
        // "until multi-part books are handled").
        let neuromancer = media.join("Neuromancer");
        touch(&neuromancer, "Neuromancer Part 1.mp3");
        touch(&neuromancer, "Neuromancer Part 2.mp3");
        // The same book in two containers and no part numbers: the list
        // resolver keeps one and files the other as an ALTERNATE VERSION, which
        // is the third of `ResolveMultipleAudio`'s guards and the only one the
        // shapes above never reach. Both files fall through to a row each.
        let foundation = media.join("Foundation");
        touch(&foundation, "Foundation.mp3");
        touch(&foundation, "Foundation.m4b");
        // A cue sheet counts as audio by extension but is never an item — it
        // describes the rip beside it (`AudioResolver.Resolve` bails on .cue).
        let dracula = media.join("Dracula");
        touch(&dracula, "dracula.m4b");
        touch(&dracula, "dracula.cue");

        let (db, cf) = scan_one(CollectionTypeOptions::books, "Books", &media).await;
        let audiobooks = scanned_by_kind(&db, BaseItemKind::AudioBook).await;

        assert_eq!(
            audiobooks
                .iter()
                .map(|b| b.name.as_deref())
                .collect::<Vec<_>>(),
            vec![
                // The extras guard rejected the folder, so both files stand
                // alone under their own stems.
                Some("Dune"),
                // The alternate-version guard rejected this folder, likewise.
                Some("Foundation"),
                Some("Foundation"),
                Some("Neuromancer Part 1"),
                Some("Neuromancer Part 2"),
                // The RAW folder name, year and all: `FindAudioBook` overrides
                // the parsed name with `Path.GetFileName(ContainingFolderPath)`
                // while keeping the parsed year on ProductionYear.
                Some("The Hobbit (1937)"),
                // The .cue beside it produced no row of its own.
                Some("dracula"),
                Some("interview-with-the-author"),
            ],
        );
        // MediaType=Audio is what makes the scan ffprobe them for a runtime.
        assert!(
            audiobooks
                .iter()
                .all(|a| a.media_type.as_deref() == Some("Audio"))
        );
        // Only the folder-is-an-audiobook shape carries a year, off the folder
        // name — the per-file fallback rows carry none, as upstream does.
        let hobbit = audiobooks
            .iter()
            .find(|a| a.name.as_deref() == Some("The Hobbit (1937)"))
            .expect("hobbit");
        assert_eq!(hobbit.production_year, Some(1937));
        assert!(
            audiobooks
                .iter()
                .filter(|a| a.name.as_deref() != Some("The Hobbit (1937)"))
                .all(|a| a.production_year.is_none()),
            "per-file fallback rows are undated"
        );
        assert!(
            audiobooks
                .iter()
                .all(|i| i.top_parent_id.as_deref() == Some(cf.as_str()))
        );
    }

    // A books library takes part in the deleted-item prune like any other; it
    // used to be excluded, which would leave removed books in the library
    // forever now that the scan produces them.
    #[tokio::test]
    async fn books_library_prunes_deleted_items() {
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("books");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("Dune.epub"), b"").unwrap();
        std::fs::write(media.join("Emma.epub"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join(".views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Books",
            Some(CollectionTypeOptions::books),
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
        let repo = Arc::new(crate::FerrofinItemRepository::new(db.clone(), lookup));
        let scanner = LibraryScanner::new(
            vf.clone(),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_items(repo.clone());

        scanner.scan_all().await.unwrap();
        let books = async || {
            let mut items = repo
                .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Book],
                    recursive: true,
                    ..Default::default()
                })
                .await
                .expect("items");
            items.sort_by(|a, b| a.name.cmp(&b.name));
            items
        };
        let first = books().await;
        assert_eq!(first.len(), 2);
        let dune_id = first[0].id.clone();

        std::fs::remove_file(media.join("Emma.epub")).unwrap();
        scanner.scan_all().await.unwrap();
        let second = books().await;
        assert_eq!(second.len(), 1, "the deleted book must be pruned");
        // The id is derived from (kind, path), so a survivor keeps it across
        // rescans — favourites, play-state and client deep links all key on it,
        // and a Jellyfin-identical derivation is what makes the DB drop-in safe.
        assert_eq!(second[0].id, dune_id, "the survivor keeps its id");
    }
}
