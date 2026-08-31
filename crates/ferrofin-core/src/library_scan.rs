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

use chrono::{DateTime, Datelike as _, Utc};
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
use ferrofin_traits::persistence::{
    ItemPersistenceService, ItemRepository, MediaStreamRepository, StoredImageMetadata,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::item_type_lookup;
use crate::media_info_resolver::ExternalStreamResolvers;
use crate::media_source_manager::{attachment_dto_to_entity, stream_dto_to_entity};

/// Per-scan artwork lookup state, so a series is matched against TMDB once and
/// its seasons/episodes reuse that match (and its per-season episode stills).
#[derive(Default)]
struct ArtworkCache {
    /// Series item id → matched TMDB series id.
    series_tmdb: std::collections::HashMap<String, i64>,
    /// (series item id, season number) → the season's TMDB details, fetched
    /// once per scan. `/tv/{id}/season/{n}` carries the season poster, every
    /// episode's still URL, AND every episode's name/overview, so the metadata
    /// pass and the image pass share this one response instead of each paying
    /// for it. `None` records a season TMDB could not resolve, so a miss is
    /// not re-requested once per episode.
    season_details:
        std::collections::HashMap<(String, i32), Option<ferrofin_providers::tmdb::SeasonDetails>>,
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
    /// Image file path → what a previous scan already probed for that file
    /// (dimensions, blurhash, and the mtime they were computed from), read
    /// once for the whole scan. Keyed by PATH rather than by item because
    /// dimensions and a blurhash are functions of the file alone, so two rows
    /// sharing one `folder.jpg` share the answer.
    stored_images: std::collections::HashMap<String, StoredImageMetadata>,
}

/// What a remote metadata fetch yields for one item: the cast/crew to persist
/// and the external provider ids (`Tmdb`/`Imdb`/`Tvdb`) to write once the row
/// exists. Ids are persisted after `save_items` so id-dependent providers
/// (fanart) can key off them on later passes and re-scans.
#[derive(Default)]
struct RemoteMetadata {
    people: Vec<PeopleEntity>,
    provider_ids: Vec<(String, String)>,
    /// Whether a credits fetch actually completed for this item.
    ///
    /// `true` makes [`people`](Self::people) authoritative **even when empty**,
    /// so credits stored by an earlier, wronger scan are cleared. `false` — a
    /// provider that did not run, was gated off by the library, missed, or
    /// errored — leaves whatever is stored alone, so a network outage or an
    /// unticked fetcher never wipes a library's cast.
    people_fetched: bool,
}

impl RemoteMetadata {
    /// People only (no external ids to persist), from a completed fetch.
    fn just_people(people: Vec<PeopleEntity>) -> Self {
        Self {
            people,
            provider_ids: Vec::new(),
            people_fetched: true,
        }
    }
}

/// The handful of stored values a provider's re-scan gate needs, lifted out of
/// the row so the scan loop does not carry a whole `BaseItemEntity` across
/// every await in it (that alone puts the scan future over clippy's
/// `large_futures` ceiling).
struct StoredText {
    name: Option<String>,
    overview: Option<String>,
    /// Whether the stored `name` is a real title rather than the resolver's
    /// file-stem placeholder. Computed at read time, while the row's path is
    /// still in hand.
    titled: bool,
}

impl StoredText {
    /// Takes the row by value: the caller owns it and drops it immediately, so
    /// cloning three `String`s per episode is pure waste at library scale.
    /// `titled` is computed first, while `path` is still in hand.
    fn from_row(row: ferrofin_db::entities::base_items::ItemTextRow) -> Self {
        Self {
            titled: !name_is_placeholder(row.name.as_deref(), row.path.as_deref()),
            name: row.name,
            overview: row.overview,
        }
    }

    /// Whether a previous scan already gave this row both a real title and a
    /// synopsis — the condition for skipping a provider fetch entirely.
    fn is_complete(&self) -> bool {
        self.titled && self.overview.as_deref().is_some_and(|o| !o.is_empty())
    }
}

/// What a TMDB episode-credits lookup yielded.
///
/// The three states must stay distinct all the way to the persistence guard:
/// credits are saved by REPLACEMENT, so treating a failure — or a lookup that
/// never ran — as "this episode has no cast" deletes what is stored.
enum TmdbCredits {
    /// TMDB answered. The list is authoritative even when empty.
    Fetched(Vec<PeopleEntity>),
    /// TMDB was asked and did not answer (network, 429, 5xx, bad JSON).
    Failed,
    /// TMDB was never asked: no client wired, or the series has no TMDB id.
    NotAttempted,
}

impl TmdbCredits {
    /// The credits when TMDB answered, else `None`.
    fn fetched(self) -> Option<Vec<PeopleEntity>> {
        match self {
            Self::Fetched(people) => Some(people),
            Self::Failed | Self::NotAttempted => None,
        }
    }
}

/// A series' TMDB id as resolved so far this scan: the direct TMDB match if
/// there was one, else the `Tmdb` id TheTVDB carries on a series it matched.
///
/// A library can rank TVDB first for `Series` while leaving `Episode` to TMDB,
/// which resolves the series through TVDB and never populates `series_tmdb`.
/// Without the fallback every episode under such a series silently gets
/// nothing. The TVDB arm already reaches for the same id.
fn series_tmdb_id_of(cache: &ArtworkCache, series_id: &str) -> Option<i64> {
    cache.series_tmdb.get(series_id).copied().or_else(|| {
        cache
            .series_tvdb
            .get(series_id)?
            .tmdb_id
            .as_deref()?
            .parse()
            .ok()
    })
}

/// One episode's entry in a cached season response, by episode number.
fn episode_in_season(
    details: &ferrofin_providers::tmdb::SeasonDetails,
    number: i32,
) -> Option<&ferrofin_providers::tmdb::EpisodeDetails> {
    details.episodes.iter().find(|e| e.episode_number == number)
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
    ///
    /// Delegates to the shared gate so the scan and the on-demand refresh in
    /// `ferrofin-providers` answer this question with one implementation —
    /// C# has exactly one (`BaseItemManager.IsMetadataFetcherEnabled`).
    fn metadata_enabled(self, kind: &str, name: &str) -> bool {
        ferrofin_providers::library_options::metadata_fetcher_enabled(self.options, kind, name)
    }

    /// The fetcher's admin-order position for `kind` (lower = higher
    /// authority); a fetcher absent from the order list sorts last, which
    /// preserves the default chain among unordered fetchers.
    ///
    /// Delegates to the shared order for the same reason
    /// [`metadata_enabled`](Self::metadata_enabled) does: C# has one
    /// `GetConfiguredOrder`, and the remote-search path in `ferrofin-providers`
    /// ranks its fetchers with it too.
    fn metadata_rank(self, kind: &str, name: &str) -> usize {
        ferrofin_providers::library_options::metadata_fetcher_rank(self.options, kind, name)
    }

    /// Whether the library enabled image fetcher `name` for `kind`.
    ///
    /// Delegates to the shared gate — see [`Self::metadata_enabled`].
    fn image_enabled(self, kind: &str, name: &str) -> bool {
        ferrofin_providers::library_options::image_fetcher_enabled(self.options, kind, name)
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
/// The probe dominates scan wall time — ffprobe child processes burn 82.2 s of
/// CPU against the server's own 13.3 s, ~86% of everything a scan costs — and
/// it is a pure per-file read, so a narrow window leaves cores idle waiting on
/// I/O. Measured on a local SSD over a 1,100-item library:
///
/// | window | items/s | ffprobe wait as share of the scan loop |
/// |---|---|---|
/// | 4 | 133 | 64.1% |
/// | 8 | 239 | — |
/// | 16 | 365 | — |
/// | 32 | 405 | 1.4% |
///
/// 4 -> 8 is the largest marginal step (+80%); the curve flattens after 16.
///
/// Still deliberately modest rather than core-count-wide. The win is close to
/// linear on a local SSD, but a library on a spinning disk or a network mount
/// turns a wide window into seek thrash, and a scan must never starve playback.
/// Note what bounds the risk: [`default_probe_concurrency`] clamps this to the
/// visible cores, so raising it from 4 to 8 changes NOTHING on a 1-, 2- or
/// 4-core NAS — the machines the conservative value was protecting. It only
/// widens the window where there are already 8+ cores to fill.
///
/// Honest limit on the evidence: the seek-thrash concern is reasoned, not
/// measured, and it was equally unmeasured when this was 4. Operators who know
/// their storage tune it with `FERROFIN_SCAN_PROBE_CONCURRENCY` /
/// `scan_probe_concurrency`; a spinning-disk or NFS library is the case to
/// lower it for.
pub const DEFAULT_SCAN_PROBE_CONCURRENCY: usize = 8;

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

/// The external-stream half of one video's probe: the sidecar resolvers plus
/// the item's internal metadata folder (the second place upstream looks for
/// sidecars — where an upload against a read-only library was written).
struct ExternalProbe {
    resolvers: Arc<ExternalStreamResolvers>,
    internal_metadata_path: Option<String>,
}

/// The scan-wide external-stream seam handed to the probe pipeline: the
/// resolver pair and the metadata root the per-item folder derives from.
struct ExternalProbeSeam {
    resolvers: Arc<ExternalStreamResolvers>,
    metadata_dir: Option<PathBuf>,
}

impl ExternalProbeSeam {
    /// The per-item half for `id`'s probe: the item's internal metadata
    /// folder is `{metadata}/library/{id2}/{id}` (the same derivation the
    /// subtitle manager writes its read-only-library fallback to).
    fn for_item(&self, id: Uuid) -> ExternalProbe {
        let dashless = id.simple().to_string();
        ExternalProbe {
            resolvers: Arc::clone(&self.resolvers),
            internal_metadata_path: self
                .metadata_dir
                .as_ref()
                .map(|root| root.join(&dashless[..2]).join(&dashless))
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Runs `request` on the encoder as a detached task, so the caller can keep
/// several probes in flight while it works through the scan in order.
///
/// For a video (`external` is `Some`) the task then resolves the sidecar
/// subtitle/audio files next to it — each one its own ffprobe — and prepends
/// them to the embedded streams in upstream's order (external subtitles,
/// external audio, then the file's own streams, renumbered from 0). That
/// keeps the sidecar probes off the scan loop like the main probe.
///
/// A probe failure is logged and reported as `None` — exactly what the
/// inline probe did — so one unreadable file never aborts a scan. The
/// sidecars are then not looked for either: the item's stored streams are
/// kept as they were, rather than replaced by externals alone.
fn spawn_probe(
    encoder: Arc<dyn MediaEncoder>,
    request: MediaInfoRequest,
    external: Option<ExternalProbe>,
) -> tokio::task::JoinHandle<Option<MediaInfo>> {
    tokio::task::spawn(async move {
        let mut probed = match encoder.get_media_info_full(&request).await {
            Ok(probed) => probed,
            Err(e) => {
                let path = request.media_source.path.as_deref();
                tracing::warn!(error = %e, ?path, "media probe failed; item left unprobed");
                return None;
            }
        };
        if let (Some(external), Some(path)) = (external, request.media_source.path.as_deref()) {
            let target = external
                .resolvers
                .target_for(path, external.internal_metadata_path);
            let externals = external.resolvers.external_streams(&target, 0).await;
            if !externals.is_empty() {
                tracing::debug!(
                    path,
                    count = externals.len(),
                    "external media streams resolved"
                );
                let embedded = std::mem::take(&mut probed.media_source.media_streams);
                probed.media_source.media_streams =
                    ExternalStreamResolvers::merge_with_embedded(externals, embedded);
            }
        }
        Some(probed)
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
    /// The sidecar subtitle/audio resolvers, run inside each **video**'s
    /// probe task. `None` when no probe is wired.
    externals: Option<ExternalProbeSeam>,
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
    fn new(
        encoder: Option<Arc<dyn MediaEncoder>>,
        externals: Option<ExternalProbeSeam>,
        planned: &'a [Planned],
        window: usize,
    ) -> Self {
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
            externals,
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
                // Sidecar streams are a video concern (`FFProbeVideoInfo`);
                // an audio item's probe is the embedded-tag path only.
                let external = self
                    .externals
                    .as_ref()
                    .filter(|_| !request.media_is_audio)
                    .map(|seam| seam.for_item(self.planned[index].id));
                self.inflight
                    .push_back((index, spawn_probe(Arc::clone(encoder), request, external)));
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
    /// The `Year` by-name provisioner for the post-scan year pass (one `Year`
    /// row per distinct scanned `ProductionYear`, so `/Years` lists every
    /// year without a write on the read path). Absent → no year pass.
    years: Option<crate::years::YearStore>,
    /// The `Genre`/`MusicGenre`/`Studio`/`MusicArtist` by-name provisioner. The
    /// scan uses it only to backfill the metadata `Path` of the by-name rows it
    /// materialized itself — Jellyfin's carry one and the DTO emits it
    /// unconditionally. Absent → those rows keep a `NULL` `Path`.
    by_name: Option<crate::by_name_store::ByNameStore>,
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
    /// Where probed attachments (embedded fonts, cover art streams) are persisted —
    /// C# `FFProbeVideoInfo` saves them right after the streams.
    media_attachments: Option<Arc<dyn ferrofin_traits::persistence::MediaAttachmentRepository>>,
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
            years: None,
            by_name: None,
            studios_client: None,
            metadata_dir: None,
            people: None,
            chapters: None,
            media_attachments: None,
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

    /// Attaches the `Year` provisioner so every scan ends by materializing a
    /// `Year` item per distinct production year (needs
    /// [`with_items`](Self::with_items) for the distinct-year query).
    #[must_use]
    pub fn with_years(mut self, years: crate::years::YearStore) -> Self {
        self.years = Some(years);
        self
    }

    /// Attaches the by-name provisioner so every scan ends by filling in the
    /// metadata `Path` of the `Genre`/`MusicGenre`/`Studio`/`MusicArtist` rows
    /// the item-values step materialized without one.
    #[must_use]
    pub fn with_by_name_store(mut self, store: crate::by_name_store::ByNameStore) -> Self {
        self.by_name = Some(store);
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

    /// Attaches the attachment repository so each probe's embedded attachments
    /// (fonts, attached pictures) are persisted with the item, as
    /// `FFProbeVideoInfo.SaveMediaAttachments` does after the streams.
    #[must_use]
    pub fn with_attachments(
        mut self,
        media_attachments: Arc<dyn ferrofin_traits::persistence::MediaAttachmentRepository>,
    ) -> Self {
        self.media_attachments = Some(media_attachments);
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
        // `LibraryManager.PerformLibraryValidation` opens with
        // `ValidateTopLibraryFolders`, whose tail deletes the library rows whose
        // directory no longer exists. Do the same here so a library removed
        // behind the API's back (or an adopted database carrying a stale row)
        // converges on the next scan instead of haunting `/UserViews` forever.
        match self.virtual_folders.prune_orphan_collection_folders().await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "pruned libraries whose directory is gone"),
            // Never fail a scan over the convergence pass.
            Err(err) => tracing::warn!(%err, "failed to prune orphan library rows"),
        }
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
        let (mut art_cache, locked_items, stored_text) = self.scan_prereads(&planned).await;
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
            let locked = locked_items.contains(&item.id);
            let policy = policy_for(item, &fetcher_policies);
            // What a previous scan already achieved for this row, from the same
            // read-once-per-scan batch the lock set uses.
            let stored = stored_text.get(&item.id);
            // Probe first so the item row is saved already carrying its duration and
            // size (the streams themselves are saved after, since they FK the row).
            let mut entity = item.entity.clone();
            let (media_info, is_audio) = probes.take(scanned).await;
            let rows = Self::apply_probe(&mut entity, media_info.as_ref(), is_audio);
            let tag_provider_ids = rows.provider_ids.clone();
            // Local Kodi/XBMC NFO sidecar first — this is Jellyfin's default local
            // metadata reader, which runs before any remote fetch. It fills
            // genres/studios/tags/overview/ratings/year from `movie.nfo` /
            // `tvshow.nfo` / `<episode>.nfo`, and yields the credited cast/crew
            // plus the external ids the file pins.
            //
            // OPEN WORK ITEM — the metadata CHANGE MONITOR is not ported, so this
            // read is unconditional where upstream's is gated. C# runs the local
            // readers only when one reports a change:
            // `BaseNfoProvider.HasChanged` is
            // `nfoLastWriteTimeUtc - item.DateLastSaved > TimeSpan.FromMinutes(1)`
            // (v10.11.8 MediaBrowser.XbmcMetadata/Providers/BaseNfoProvider.cs),
            // and `MetadataService.GetProviders` returns an EMPTY provider list
            // when nothing changed, so a scan over an item saved more recently
            // than its sidecar merges nothing. Ferrofin re-derives every unlocked
            // row from disk on every pass, which is why a library scan reverts
            // what the Identify dialog's Apply just wrote — measured on the lab
            // pair and recorded as the `unlocked_after_scan` red on
            // `POST /Items/RemoteSearch/Apply/{itemId}`.
            //
            // UN-DEFER PATH (all three together, or the fix loses data — the
            // full argument is on that row in suite/parity/classifications.json):
            //   1. write `BaseItems.DateLastSaved` on every save
            //      (`FerrofinItemPersistenceService::save_items` binds the column
            //      but only ever from an entity carrying `None`). This alone
            //      changes `Etag` (dto_service.rs hashes DateLastSaved) and the
            //      `minDateLastSaved` query filter on every DTO, so it needs its
            //      own batch and its own perf run;
            //   2. gate this call and `fetch_remote_metadata` on the
            //      sidecar-mtime-vs-DateLastSaved comparison;
            //   3. a scan-upsert variant that PRESERVES the provider-supplied
            //      columns when no provider ran — without it, skipping the read
            //      wipes the very fields the gate exists to protect.
            let (mut people, nfo_ids) = if locked {
                (Vec::new(), Vec::new())
            } else {
                self.fetch_local_nfo(&mut entity, policy).await
            };
            // Sidecar ids first, then any this row already carries: both are
            // `info.GetProviderId`, which the fetchers resolve by before they
            // ever search by title.
            let known_ids = known_provider_ids(&nfo_ids, &art_cache, &entity.id);
            // Then enrich from TMDB (overview/tagline/genres/studios/ratings +
            // cast/crew) to fill any gaps the NFO left, so a bare file with no NFO
            // shows the same detail page Jellyfin does. Best-effort: failures don't
            // abort, and NFO-provided people take precedence.
            let remote = if locked {
                RemoteMetadata::default()
            } else {
                // Boxed: the provider chain below this is the deepest branch of
                // the per-item future, and inlining it puts the whole scan
                // future over clippy's `large_futures` ceiling — the scan loop
                // is held across every await in it.
                Box::pin(self.fetch_remote_metadata(
                    &mut entity,
                    &mut art_cache,
                    policy,
                    stored,
                    &known_ids,
                ))
                .await
            };
            let people_fetched = remote.people_fetched;
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
            let built_in_ids = merge_provider_ids(nfo_ids, remote.provider_ids);
            let all_provider_ids = self
                .apply_dynamic_metadata(
                    &mut entity,
                    &built_in_ids,
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
            self.persist_people(item.id, people, people_fetched).await;
            self.persist_item_media(
                item.id,
                &entity,
                ItemMedia {
                    probe: &rows,
                    embedded_images,
                    policy,
                    locked,
                },
                &mut art_cache,
            )
            .await?;
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
        // Boxed: the post-scan passes (music, years, studio/library/dynamic
        // images) run once per scan, and inlining their state kept the scan
        // future at clippy's `large_futures` ceiling.
        Box::pin(self.post_scan_passes(folders)).await;
        Ok(planned.len())
    }

    /// Writes an item's cast/crew, if there is anything authoritative to write.
    ///
    /// A completed credits fetch is authoritative even when it found nobody:
    /// `update_people` replaces the item's rows, so an empty authoritative
    /// result is what clears credits an earlier, wronger scan wrote (episodes
    /// carrying their whole series' cast). A fetch that never ran — locked
    /// item, gated-off fetcher, network failure, provider miss — leaves the
    /// stored rows alone.
    ///
    /// Note the asymmetry: only REMOTE fetches carry the flag.
    /// `fetch_local_nfo` returns a bare vec, so an NFO that credits nobody
    /// cannot clear a stale cast — an NFO-authoritative library keeps the rows
    /// this behaviour exists to correct.
    async fn persist_people(&self, item_id: Uuid, people: Vec<PeopleEntity>, fetched: bool) {
        if people.is_empty() && !fetched {
            return;
        }
        let Some(repo) = &self.people else {
            return;
        };
        match repo.update_people(item_id, &people).await {
            Ok(written) => self.enrich_people(repo.as_ref(), written).await,
            Err(err) => {
                tracing::warn!(%err, item = %item_id, "failed to persist cast/crew");
            }
        }
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

    /// What one scanned item contributes beyond its own row: its streams,
    /// chapters and artwork.
    /// Writes a scanned item's streams, chapters and artwork.
    ///
    /// Split out of the walk to keep it under the line ceiling; none of this
    /// holds the provider chain's future, so it does not affect the boxing that
    /// keeps that future small.
    ///
    /// # Errors
    ///
    /// Propagates a storage failure from the stream or chapter write. Artwork
    /// is best-effort and logs its own failures.
    async fn persist_item_media(
        &self,
        item_id: Uuid,
        entity: &BaseItemEntity,
        media: ItemMedia<'_>,
        art_cache: &mut ArtworkCache,
    ) -> Result<(), ServiceError> {
        let probe = media.probe;
        if let (false, Some(repo)) = (probe.streams.is_empty(), &self.media_streams) {
            repo.save_media_streams(item_id, &probe.streams).await?;
        }
        // Attachments are replaced whenever a video probe ran (an empty set clears
        // the rows of a re-muxed file); without a probe the stored rows stay.
        if let (true, Some(repo)) = (probe.save_attachments, &self.media_attachments) {
            repo.save_media_attachments(item_id, &probe.attachments)
                .await?;
        }
        self.save_chapters(item_id, &probe.chapters).await?;
        // Artwork. TODO(parity, open work item — NOT an accepted divergence):
        // upstream does NOT skip the whole pass for a locked row. v10.11.8
        // `MediaBrowser.Providers/Manager/ProviderManager.cs:412` returns true
        // for `provider is ILocalImageProvider` BEFORE the `item.IsLocked`
        // check, so only the REMOTE image providers are gated — Jellyfin keeps
        // re-discovering a sidecar `poster.jpg` on a locked item. Ferrofin
        // skips everything, so a locked row's Primary freezes at whatever was
        // stored (measured on the parity lab: a metadata-dir download stayed
        // Primary where Jellyfin held the sidecar).
        //
        // It cannot be fixed here alone: `save_item_images` REPLACES the rows,
        // so un-gating this while `item_update.rs` still auto-locks on any edit
        // would let a scan overwrite a user-chosen image. The pair is
        // (a) port `MetadataService`'s per-field merge rules and drop the
        // auto-lock in `item_update.rs`, (b) narrow this gate to the remote arm
        // (`if images.is_empty() && !locked` around `fetch_remote_images`).
        if !media.locked {
            let art = ArtworkPass {
                entity,
                streams: &probe.streams,
                policy: media.policy,
                embedded_images: media.embedded_images,
            };
            self.persist_artwork(item_id, art, art_cache).await;
        }
        Ok(())
    }

    /// The batches every item in the walk reads from, gathered once up front.
    ///
    /// Each of these replaced a per-item query, so they are the difference
    /// between one read per scan and one per row. They run BEFORE the loop, so
    /// keeping them out of it also keeps them out of the future the loop holds
    /// across its awaits.
    async fn scan_prereads(
        &self,
        planned: &[Planned],
    ) -> (
        Box<ArtworkCache>,
        std::collections::HashSet<Uuid>,
        std::collections::HashMap<Uuid, StoredText>,
    ) {
        // Carries matched series' TMDB ids + their episode-still URLs across
        // the scan so seasons/episodes resolve against the same series lookup,
        // pre-seeded with the external ids previous scans recorded.
        // Boxed: the scan loop holds this cache across every item's awaits, so
        // keeping its six maps out of that future keeps the per-scan future
        // small (clippy's `large_futures` ceiling is the tripwire).
        let mut art_cache = Box::new(ArtworkCache::default());
        self.preload_provider_ids(planned, &mut art_cache).await;
        self.preload_image_metadata(planned, &mut art_cache).await;
        // One read of the locked-item set for the whole scan, replacing the
        // per-item row hydration the loop used to pay (see `locked_items`).
        let locked_items = self.locked_items(planned).await;
        // Same one-read-per-scan shape, for the episode providers' gate.
        let stored_text = self.stored_episode_text(planned).await;
        (art_cache, locked_items, stored_text)
    }

    /// Seeds the scan's provider-id cache with the ids previous scans recorded,
    /// in ONE query for the whole run.
    ///
    /// C# resolves each item through `info.GetProviderId` before it ever
    /// searches by title; without this a re-scan re-guesses every title from
    /// scratch, and could match a different record than last time.
    async fn preload_provider_ids(&self, planned: &[Planned], art_cache: &mut ArtworkCache) {
        let ids: Vec<Uuid> = planned.iter().map(|p| p.id).collect();
        match self.persistence.provider_ids_for_items(&ids).await {
            Ok(existing) => {
                for (id, ids) in existing {
                    art_cache.item_provider_ids.insert(guid_to_db(id), ids);
                }
            }
            Err(err) => tracing::warn!(%err, "could not preload recorded provider ids"),
        }
    }

    /// Seeds the scan's image cache with what previous scans already probed,
    /// in ONE query for the whole run.
    ///
    /// This is what makes [`image_metadata_is_current`] answerable without a
    /// read per item. C# gets the same information for free — its
    /// `BaseItem.ImageInfos` are already in memory when
    /// `LibraryManager.UpdateImagesAsync` runs `ImageNeedsRefresh` — whereas
    /// Ferrofin's image rows live only in the database, so the scan reads them
    /// back up front, in the same one-read-per-scan shape as the locked-item
    /// and episode-text prereads.
    async fn preload_image_metadata(&self, planned: &[Planned], art_cache: &mut ArtworkCache) {
        if planned.is_empty() {
            return;
        }
        let ids: Vec<Uuid> = planned.iter().map(|p| p.id).collect();
        match self.persistence.image_metadata_for_items(&ids).await {
            Ok(stored) => {
                for image in stored {
                    art_cache.stored_images.insert(image.path.clone(), image);
                }
            }
            Err(err) => tracing::warn!(%err, "could not preload stored image metadata"),
        }
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
        // One `Year` item per distinct ProductionYear now in the library
        // (Jellyfin creates them lazily from `/Years`; doing it here keeps
        // that read write-free and lists every year on first request).
        if let Err(err) = self.materialize_years().await {
            tracing::warn!(%err, "year pass failed");
        }
        // Retire the parentless `MusicArtist` rows a PREVIOUS scan left behind
        // (before `MusicArtistResolver` was ported) now that the same artist is
        // resolved from its directory — otherwise /Artists lists each one twice.
        if let Err(err) = self.retire_accessed_by_name_artists().await {
            tracing::warn!(%err, "by-name artist retirement pass failed");
        }
        // Cumulative runtime for the folder kinds that support it, once every
        // track has been probed — an album/artist reports the summed runtime of
        // its children, which is a stored column, not a per-request rollup.
        if let Err(err) = self.update_cumulative_run_time_ticks().await {
            tracing::warn!(%err, "cumulative run time ticks pass failed");
        }
        // The metadata `Path` of the by-name rows the item-values step wrote
        // (`{metadata}/Genre/Action`, …). Jellyfin's `CreateItemByName` sets it
        // at insert time; here the row is a by-product of `save_item_values`,
        // which has no notion of the metadata root, so it is filled once here.
        if let Some(store) = &self.by_name
            && let Err(err) = store.backfill_paths().await
        {
            tracing::warn!(%err, "by-name path backfill failed");
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
        // Genre / music-genre / playlist / photo-album Primaries (upstream's
        // `BaseDynamicImageProvider` family, run by the by-name validators at
        // the end of library validation). Same ordering reason as the library
        // tiles: the collages sample the artwork the passes above produced.
        if let (Some(items), Some(processor), Some(meta_root)) = (
            &self.item_repository,
            &self.image_processor,
            &self.metadata_dir,
        ) {
            let providers = crate::dynamic_images::DynamicImageProviders::new(
                Arc::clone(items),
                Arc::clone(&self.persistence),
                Arc::clone(processor),
                meta_root.clone(),
            );
            if let Err(err) = providers.refresh_all().await {
                tracing::warn!(%err, "dynamic image pass failed");
            }
        }
    }

    /// Drops the `IsAccessedByName` `MusicArtist` rows that a folder-resolved
    /// artist of the same name has superseded.
    ///
    /// Before `MusicArtistResolver` was ported, an artist directory was walked
    /// through without emitting a row and the only browsable `MusicArtist` was
    /// the one `item_persistence_service::save_item_values` materialized off the
    /// track's `AlbumArtist` `ItemValues` row — parentless, pathed at the
    /// METADATA directory, and invisible to every user-scoped query (no
    /// `TopParentId`). New writes can no longer create that twin (the by-name
    /// insert is guarded on `(Type, CleanName)`), but an EXISTING database — a
    /// Ferrofin one, or an adopted Jellyfin one whose artists were only ever
    /// by-name — still holds it, and `item_repository::push_by_name_join` joins
    /// by `CleanName`, so both rows would answer /Artists.
    ///
    /// Only a row that has a folder-backed twin is dropped: an artist that is
    /// genuinely accessed-by-name (a compilation's `AlbumArtist` with no
    /// directory of its own) keeps its row, exactly as it does upstream.
    ///
    /// `BaseItems.ParentId` cascades, but a retired row is parentless AND
    /// childless — every album now hangs off the resolved artist — so nothing
    /// cascades with it. Its user data is lost, which is the honest cost: the
    /// row it was keyed to no longer exists.
    async fn retire_accessed_by_name_artists(&self) -> Result<(), ServiceError> {
        let Some(items) = &self.item_repository else {
            return Ok(());
        };
        let artists = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicArtist],
                ..InternalItemsQuery::default()
            })
            .await?;
        // A row counts as folder-backed when it carries a TopParentId — that is
        // exactly what `scope_to_user_libraries` (`AddUserToQuery`) requires and
        // what the resolver now sets.
        let resolved: std::collections::HashSet<String> = artists
            .iter()
            .filter(|a| a.top_parent_id.as_ref().is_some_and(|t| !t.is_empty()))
            .filter_map(|a| a.clean_name.clone())
            .collect();
        let stale: Vec<Uuid> = artists
            .iter()
            .filter(|a| a.top_parent_id.as_ref().is_none_or(String::is_empty))
            .filter(|a| a.clean_name.as_ref().is_some_and(|c| resolved.contains(c)))
            .filter_map(|a| Uuid::parse_str(&a.id).ok())
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        tracing::info!(
            retired = stale.len(),
            "retiring by-name MusicArtist rows superseded by resolved artist folders"
        );
        self.persistence.delete_items(&stale).await
    }

    /// Port of `MetadataService.UpdateCumulativeRunTimeTicks`
    /// (`MediaBrowser.Providers/Manager/MetadataService.cs:451`): a folder whose
    /// `SupportsCumulativeRunTimeTicks` is true stores the summed runtime of its
    /// **non-folder recursive children** in its own `RunTimeTicks` column —
    /// `foreach (child) if (!child.IsFolder) ticks += child.RunTimeTicks ?? 0;`,
    /// written even when the sum is zero (`Folder.cs:97` makes it false by
    /// default; `MusicAlbum.cs:54` and `MusicArtist.cs:39` override it to true).
    ///
    /// That column is what `DtoService` emits as both `RunTimeTicks`
    /// (`DtoService.cs:1111`, ungated) and `CumulativeRunTimeTicks`
    /// (`DtoService.cs:594`), so with it `NULL` an album reported no runtime at
    /// all while Jellyfin reported the sum of its tracks.
    ///
    /// The children come from the base
    /// `MetadataService.GetChildrenForMetadataUpdates` — `GetRecursiveChildren()`
    /// over the item hierarchy — which, now that `MusicArtistResolver` is
    /// ported, is the artist's albums and their tracks.
    ///
    /// A real 10.11.8 stores `0` on a first-scanned artist. That is a race, not
    /// the intended value: its own `DateLastRefreshed` shows the artist
    /// refreshed BEFORE its tracks were probed, so the identical C# summed a set
    /// of `NULL`s, while the album — same code, refreshed after — got the right
    /// number. Ferrofin sums the probed ticks, so an artist reports its real
    /// runtime; the divergence is recorded in `suite/parity/classifications.json`.
    ///
    /// `Playlist` is the third `SupportsCumulativeRunTimeTicks` kind
    /// (`Playlist.cs:79`); its children are `LinkedChildren`, not scanned
    /// descendants, so it belongs to the playlist write path rather than this
    /// scan pass — an open work item, not a skipped one.
    async fn update_cumulative_run_time_ticks(&self) -> Result<(), ServiceError> {
        let Some(items) = &self.item_repository else {
            return Ok(());
        };
        for kind in [BaseItemKind::MusicArtist, BaseItemKind::MusicAlbum] {
            let folders = items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..InternalItemsQuery::default()
                })
                .await?;
            for folder in folders {
                let Ok(id) = Uuid::parse_str(&folder.id) else {
                    continue;
                };
                let children = items
                    .get_item_list(&InternalItemsQuery {
                        ancestor_ids: vec![id],
                        recursive: true,
                        ..InternalItemsQuery::default()
                    })
                    .await?;
                let ticks: i64 = children
                    .iter()
                    .filter(|child| !child.is_folder)
                    .map(|child| child.run_time_ticks.unwrap_or(0))
                    .sum();
                // `if (!folder.RunTimeTicks.HasValue || folder.RunTimeTicks.Value != ticks)`
                if folder.run_time_ticks == Some(ticks) {
                    continue;
                }
                let mut row = folder;
                row.run_time_ticks = Some(ticks);
                self.persistence
                    .save_items(std::slice::from_ref(&row))
                    .await?;
            }
        }
        Ok(())
    }

    /// The post-scan year pass: reads the distinct `ProductionYear`s across
    /// the whole library and creates the `Year` rows that are missing. No-op
    /// unless both the item repository and the year provisioner are wired.
    async fn materialize_years(&self) -> Result<(), ServiceError> {
        let (Some(items), Some(years)) = (&self.item_repository, &self.years) else {
            return Ok(());
        };
        let distinct = items
            .get_distinct_years(&InternalItemsQuery {
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await?;
        let created = years.ensure_missing(&distinct).await?;
        if !created.is_empty() {
            tracing::info!(created = created.len(), "materialized year items");
        }
        Ok(())
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
        // `LibraryChangedNotifier`: the id in Jellyfin's guid spelling (N form) —
        // jellyfin-web compares it with the card's `data-id` as a plain string.
        let payload = serde_json::json!({
            "ItemId": library.simple().to_string(),
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

        // Every id as `ToString("N")` (`LibraryChangedNotifier`): jellyfin-web's
        // itemsrefresher matches these strings against item ids.
        let n = |id: &Uuid| id.simple().to_string();
        let mut update = ferrofin_model::entities_media::LibraryUpdateInfo {
            folders_added_to: folders_added.iter().map(n).collect(),
            folders_removed_from: folders_removed.iter().map(n).collect(),
            items_added: added.iter().map(|p| n(&p.id)).collect(),
            items_removed: removed
                .iter()
                .flat_map(|(_, ids)| ids.iter().map(n))
                .collect(),
            collection_folders: collection_folders.iter().map(n).collect(),
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
        // No adoption here: the post-scan music pass runs outside the item walk
        // and so has no `ArtworkCache`. Re-probing a handful of album/artist
        // covers is not the scan's cost centre; the adoption belongs here too
        // once the passes share the walk's cache.
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
            // C# `AlbumMetadataService.GetChildrenForMetadataUpdates` is
            // `item.GetRecursiveChildren(i => i is Audio)` — recursive, so a
            // multi-disc album (`Album/Disc 1/*.flac`) still aggregates.
            let tracks = items
                .get_item_list(&InternalItemsQuery {
                    parent_id: album_uuid,
                    include_item_types: vec![BaseItemKind::Audio],
                    recursive: true,
                    ..Default::default()
                })
                .await?;

            let (mut updated, changed) = apply_album_child_metadata(album, &tracks);
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
            // `ReplaceAlbumName` (AudioDB settings page, off by default): the
            // client only carries a name when the admin turned it on, and
            // upstream OVERWRITES — `item.Name = result.strAlbum` — rather than
            // filling a gap, so this is not gated on the existing name.
            if let Some(name) = a.name
                && updated.name.as_deref() != Some(name.as_str())
            {
                updated.name = Some(name);
                changed = true;
            }
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
            // `ReplaceArtistName` (MusicBrainz settings page, off by default)
            // renames the artist to MusicBrainz's spelling — but ONLY on the
            // branch that RESOLVED the id by searching. C#
            // `MusicBrainzArtistProvider.cs:135-150` puts the rename inside
            // `if (string.IsNullOrWhiteSpace(musicBrainzId))`, so an artist
            // whose mbid came from its own tags is never renamed.
            let mut searched_name = None;
            let mbid = match artist_mbid.get(name) {
                Some(id) => Some(id.clone()),
                None if mb_enabled => match mb.search_artist_match(name).await {
                    Some(hit) => {
                        searched_name = hit.name;
                        Some(hit.id)
                    }
                    None => None,
                },
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
            if let Some(mb_name) = searched_name
                && mb_enabled
                && updated.name.as_deref() != Some(mb_name.as_str())
                && mb.replace_artist_name().await
            {
                updated.name = Some(mb_name);
                changed = true;
            }
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
            // Already answered — either by a caller that adopted a previous
            // scan's values (`adopt_stored_image_metadata`) or by an earlier
            // pass in this one. Re-probing would decode the same file twice.
            if image_metadata_is_complete(image) {
                continue;
            }
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
    ///
    /// When a probe is wired, every video's probe also resolves its sidecar
    /// subtitle/audio files (`SubtitleResolver` + `AudioResolver`), which
    /// need the naming options' extension/flag tables, the language lookup
    /// (the configured localization, else the default culture set — the
    /// lookup itself is culture-table-only), the encoder, and the filesystem.
    fn probe_pipeline<'a>(&self, planned: &'a [Planned]) -> ProbePipeline<'a> {
        let externals = self.media_encoder.as_ref().map(|encoder| {
            let localization = self.localization.clone().unwrap_or_else(|| {
                Arc::new(crate::localization_manager::LocalizationManager::new(""))
            });
            ExternalProbeSeam {
                resolvers: Arc::new(ExternalStreamResolvers::new(
                    Arc::new(NamingOptions::new()),
                    localization,
                    Arc::clone(encoder),
                    Arc::clone(&self.file_system),
                )),
                metadata_dir: self.metadata_dir.clone(),
            }
        });
        ProbePipeline::new(
            self.media_encoder.clone(),
            externals,
            planned,
            self.probe_concurrency,
        )
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
    ) -> ProbeRows {
        let Some(probed) = probed else {
            return ProbeRows::default();
        };
        let source = &probed.media_source;
        entity.run_time_ticks = source.run_time_ticks.or(entity.run_time_ticks);
        entity.size = source.size.or(entity.size);
        // Both probers persist the container bitrate onto the item row
        // (`FFProbeVideoInfo.cs:216` / `AudioFileProber.cs:133`, both
        // `TotalBitrate = mediaInfo.Bitrate`). `BaseItem.GetVersionInfo` seeds
        // the media source from it, and `ItemSortBy.VideoBitRate` orders on it,
        // so leaving it NULL silently breaks that sort.
        entity.total_bitrate = source.bitrate.map(i64::from).or(entity.total_bitrate);
        // The item row's own Width/Height are the PRIMARY VIDEO STREAM's, written
        // by the video prober on every probe (`FFProbeVideoInfo.Fetch`,
        // FFProbeVideoInfo.cs:265-266: `video.Height = videoStream?.Height ?? 0;
        // video.Width = videoStream?.Width ?? 0;`). `AudioFileProber` never
        // touches them, so an audio item keeps whatever the row had. These are
        // what `DtoService` emits as `Width`/`Height` — not the poster's size.
        if !media_is_audio {
            let video = source
                .media_streams
                .iter()
                .find(|s| s.stream_type == ferrofin_model::entities::MediaStreamType::Video);
            entity.width = video.and_then(|s| s.width).map(i64::from);
            entity.height = video.and_then(|s| s.height).map(i64::from);
        }
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
        // Only the video prober saves attachments (`FFProbeVideoInfo`); the audio
        // prober (`AudioFileProber`) never touches the attachment table, so an
        // embedded cover art "attached_pic" stream stays out of it.
        let attachments = if media_is_audio {
            Vec::new()
        } else {
            source
                .media_attachments
                .iter()
                .map(|a| attachment_dto_to_entity(&entity.id, a))
                .collect()
        };
        ProbeRows {
            streams,
            chapters,
            attachments,
            provider_ids,
            save_attachments: !media_is_audio,
        }
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
    ) -> (Vec<PeopleEntity>, Vec<(String, String)>) {
        use ferrofin_providers::xbmc::{
            self, base_parser::NoDirectoryService, config::NfoConfiguration, item::NfoItemKind,
        };
        if !policy.local_reader_enabled(fetcher_names::NFO) {
            return (Vec::new(), Vec::new());
        }
        let short = entity.type_.rsplit('.').next().unwrap_or(&entity.type_);
        let kind = match short {
            "Movie" => NfoItemKind::Movie,
            "Series" => NfoItemKind::Series,
            "Season" => NfoItemKind::Season,
            "Episode" => NfoItemKind::Episode,
            "MusicAlbum" => NfoItemKind::MusicAlbum,
            "MusicArtist" => NfoItemKind::MusicArtist,
            _ => return (Vec::new(), Vec::new()),
        };
        let Some(path) = entity.path.as_deref() else {
            return (Vec::new(), Vec::new());
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
            return (Vec::new(), Vec::new());
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
            _ => return (Vec::new(), Vec::new()),
        };
        if parsed.is_err() {
            return (Vec::new(), Vec::new());
        }
        apply_nfo(entity, &result.item);
        // The `<imdbid>`/`<tmdbid>`/`<musicbrainzalbumid>` the sidecar declares
        // are the ids the USER pinned. They are what the remote fetchers should
        // resolve by — C#'s local provider populates `item.ProviderIds` before
        // any remote provider runs — and they must be persisted, or a re-scan
        // searches by title again and the pin is silently ignored.
        let ids = result
            .item
            .provider_ids
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()));
        (
            result
                .people
                .unwrap_or_default()
                .into_iter()
                .map(person_to_entity)
                .collect(),
            ids.filter(|(_, v)| !v.trim().is_empty()).collect(),
        )
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
        stored: Option<&StoredText>,
        known_ids: &[(String, String)],
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
        if tvdb_on
            && (tvdb_first || !tmdb_on)
            && let Some(result) = self.fetch_tvdb_metadata(entity, &short, cache).await
        {
            // A TVDB hit is authoritative. A miss falls through to TMDB — for
            // an episode as well as a series.
            return result;
        }
        let omdb_on = policy.metadata_enabled(&short, fetcher_names::OMDB);
        if !tmdb_on {
            // Each fetcher's checkbox gates only itself: unchecking TheMovieDb
            // must not silently disable OMDb as well.
            return if omdb_on {
                self.fetch_omdb_metadata(entity, &short, cache, policy, known_ids)
                    .await
            } else {
                RemoteMetadata::default()
            };
        }
        if let Some(result) = self
            .fetch_tmdb_metadata(entity, &short, omdb_on, cache, stored, known_ids)
            .await
        {
            return result;
        }
        // The library ranked TMDB above TVDB and TMDB missed: TVDB is the
        // fallback. Episodes are included — before this change TMDB had no
        // Episode branch at all, so the gap was unreachable; now an episode
        // TMDB has nothing for would otherwise never reach the fetcher the
        // library ranked second.
        if tvdb_on
            && matches!(short.as_str(), "Series" | "Episode")
            && !tvdb_first
            && let Some(result) = self.fetch_tvdb_metadata(entity, &short, cache).await
        {
            return result;
        }
        // Nothing upstream matched: OMDb closes the chain, matching its C#
        // `Order = 2` (behind TMDB and TVDB, ahead of nothing).
        if omdb_on {
            return self
                .fetch_omdb_metadata(entity, &short, cache, policy, known_ids)
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
    /// item is skipped entirely — C# `RefreshMetadata` returns before any
    /// provider runs when `item.IsLocked` — so nothing here, not even the
    /// dimensions or the `Data` blob, touches a locked row.
    async fn enrich_photo(&self, entity: &mut BaseItemEntity, locked: bool) -> Vec<ItemImageInfo> {
        if !entity.type_.ends_with(".Photo") || locked {
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
            // A field-level `MetadataField.Name` lock; the item-level lock
            // already returned above.
            name_locked: false,
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
        if let Some(name) = photo.name.filter(|n| !n.is_empty()) {
            // The sort name follows the title, or the album keeps sorting
            // by filename while displaying the EXIF title.
            entity.sort_name = Some(derived_sort_name(entity, &name));
            entity.name = Some(name);
        }
        // C# assigns both straight from the tag, so a photo whose comment or
        // rating was removed has the field cleared. That only holds when the
        // file was READ — one that failed to open says nothing about either
        // field and must not wipe them.
        if photo.exif_was_read {
            entity.overview = photo.overview;
            entity.community_rating = photo.community_rating;
        }
        if let Some(taken) = photo.date_taken {
            // C# sets all three from DateTaken. `DateCreated` is what the
            // client sorts "Date Added" by, so a scanned photo album orders
            // by when the shots were taken, not when the files were copied.
            entity.date_created = Some(taken);
            entity.premiere_date = Some(taken);
            entity.production_year = photo.production_year.map(i64::from);
        }
        // Same rule as the overview/rating above: these keys are written from
        // the tags, so a `None` CLEARS the stored value — but only when the
        // file was actually read. A photo whose EXIF failed to open says
        // nothing about its camera or GPS and must keep what it had.
        if photo.exif_was_read
            && let Some(data) = crate::item_data::merge_data_fields(
                entity.data.as_deref(),
                &photo_exif_fields(&photo.exif),
            )
        {
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
        known_ids: &[(String, String)],
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
        // The id this item already carries — from its NFO sidecar, or from the
        // fetcher that ran before OMDb in this same pass.
        let own_imdb = imdb_id_in(known_ids);
        let item = match short {
            "Movie" | "Series" => {
                let kind = if short == "Movie" {
                    ferrofin_providers::OmdbKind::Movie
                } else {
                    ferrofin_providers::OmdbKind::Series
                };
                // C# `OmdbItemProvider.GetResult` reads `info.GetProviderId(Imdb)`
                // first and only searches by title when there is none: a user who
                // pinned an id in an NFO must not be re-matched by fuzzy title.
                match (own_imdb.as_deref(), name) {
                    (Some(imdb), _) => omdb.item(imdb).await,
                    (None, Some(name)) => omdb.find_by_title(kind, name, year).await,
                    (None, None) => None,
                }
            }
            "Episode" => {
                // C# `FetchEpisodeData` matches the season listing on the
                // EPISODE's own IMDb id first and only falls back to the
                // episode number — a season whose numbering disagrees with
                // OMDb's resolves correctly only when that id is supplied.
                let series_imdb = entity
                    .series_id
                    .as_deref()
                    .and_then(|series| imdb_id_of(cache.item_provider_ids.get(series)));
                match (
                    series_imdb,
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) {
                    (Some(series), Some(season), Some(number)) => {
                        omdb.episode(&series, season, number, own_imdb.as_deref())
                            .await
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
        // A completed OMDb fetch, so its credits are authoritative even when
        // empty — same rule as the TMDB and TVDB arms.
        RemoteMetadata {
            // C# `ParseAdditionalMetadata` returns before adding the
            // director/writer/actors unless the OMDb plugin's `CastAndCrew`
            // flag is set, and that bool has no initializer — so upstream's
            // default is OFF and Ferrofin matches it.
            people: if omdb.cast_and_crew().await {
                omdb_people(&item)
            } else {
                Vec::new()
            },
            provider_ids,
            people_fetched: true,
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
        cache: &mut ArtworkCache,
        stored: Option<&StoredText>,
        known_ids: &[(String, String)],
    ) -> Option<RemoteMetadata> {
        let tmdb = self.tmdb.as_ref()?;
        // TMDB is Jellyfin's default episode provider, and on a library
        // migrated from Jellyfin it is usually the ONLY metadata fetcher saved
        // for `Episode` (stock Jellyfin never lists TheTVDB). Its episode half
        // reads the season response rather than `/movie|tv/{id}`, so it
        // branches before the `TmdbKind` split below.
        if short == "Episode" {
            return self.fetch_tmdb_episode(entity, cache, stored).await;
        }
        let kind = match short {
            "Movie" => TmdbKind::Movie,
            "Series" => TmdbKind::Series,
            _ => return None,
        };
        let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
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
            // A series that needs no metadata of its own STILL has to publish
            // its TMDB id: every season and episode beneath it resolves through
            // `cache.series_tmdb`. Returning here without it is why an
            // already-enriched series would leave its whole episode tree with no
            // provider at all — silently, and only on re-scans.
            // ponytail: re-searches by name to recover the id, because there
            // is no scan-time reader for the `Tmdb` id already persisted to
            // BaseItemProviders when this series was first matched. One extra
            // search per already-enriched series per scan, and in principle a
            // renamed row could resolve to a different id and re-point its
            // episode tree. Read the stored id back instead if either bites.
            if matches!(kind, TmdbKind::Series)
                && !cache.series_tmdb.contains_key(&entity.id)
                && let Some(name) = entity.name.clone().filter(|n| !n.is_empty())
                && let Some(hit) = tmdb
                    .search(kind, &name, year, None)
                    .await
                    .into_iter()
                    .next()
            {
                cache.series_tmdb.insert(entity.id.clone(), hit.tmdb_id);
            }
            return Some(RemoteMetadata::default());
        }
        // C# `TmdbMovieProvider.GetMetadata` reads `info.GetProviderId(Tmdb)`
        // first and only searches by title when there is none, so a `<tmdbid>`
        // the user pinned in an NFO is honoured rather than re-guessed.
        let pinned = known_ids
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("Tmdb"))
            .and_then(|(_, value)| value.trim().parse::<i64>().ok());
        let tmdb_id = if let Some(id) = pinned {
            id
        } else {
            let name = entity.name.clone().filter(|n| !n.is_empty())?;
            tmdb.search(kind, name.as_str(), year, None)
                .await
                .into_iter()
                .next()
                .map(|h| h.tmdb_id)?
        };
        // Same reason as the short-circuit above: the seasons/episodes below
        // this series key off the cached id. Cached however the id was
        // resolved — a pinned series id serves its children just as well, and
        // caching only the searched one would leave them looking it up again.
        if matches!(kind, TmdbKind::Series) {
            cache.series_tmdb.insert(entity.id.clone(), tmdb_id);
        }
        let details = tmdb.details(kind, tmdb_id, None).await?;
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
            people_fetched: true,
        })
    }

    /// The TMDB **episode** metadata pass — port of `TmdbEpisodeProvider`.
    ///
    /// An episode's title and synopsis come from its entry in the series'
    /// cached `/tv/{id}/season/{n}` response (see
    /// [`season_details_cached`](Self::season_details_cached)), so a whole
    /// season's episodes cost one request between them — the same request the
    /// image pass already makes for the season poster and episode stills.
    ///
    /// `None` means TMDB had nothing for this episode (the caller may fall back
    /// to another fetcher); `Some(default)` means the row needed no fetch.
    async fn fetch_tmdb_episode(
        &self,
        entity: &mut BaseItemEntity,
        cache: &mut ArtworkCache,
        stored: Option<&StoredText>,
    ) -> Option<RemoteMetadata> {
        let (Some(series_id), Some(season), Some(number)) = (
            entity.series_id.clone(),
            entity
                .parent_index_number
                .and_then(|n| i32::try_from(n).ok()),
            entity.index_number.and_then(|n| i32::try_from(n).ok()),
        ) else {
            return None;
        };
        // Re-scan gate. It reads the STORED row, not the planned entity: the
        // entity is rebuilt from the filesystem every scan, so its name is
        // always the file stem and its overview always `None`. Gating on it
        // would never fire — every episode would re-request its credits on
        // every nightly scan.
        //
        // Skipping also has to carry the stored values forward. The scan upsert
        // writes `excluded` for unlocked rows, so returning here without them
        // would overwrite a good title with the placeholder — one scan during a
        // TMDB outage would revert the whole library to filenames.
        //
        // An NFO wins either way: `apply_nfo` already ran and moved the name
        // off the stem, which is what the placeholder check reads.
        //
        // ponytail: an episode TMDB has a title but no overview for never
        // satisfies this gate, so it re-fetches its credits every scan (the
        // season response itself is cached, so that half is free). Same
        // unbounded-refetch shape as the trailers backfill above. Store a
        // "checked" marker if it ever costs real time.
        if let Some(stored) = stored.filter(|s| s.is_complete()) {
            if name_is_file_stem_placeholder(entity) {
                // Recomputed from the stored title, not copied from the stored
                // key: a key derived by an older algorithm would otherwise
                // survive every rescan and sort next to freshly derived ones
                // (the play queue reads this). `BaseItem.SortName` prefers a
                // `ForcedSortName` when the item has one, so this does too.
                entity.name.clone_from(&stored.name);
                entity.sort_name = match entity.forced_sort_name.as_deref() {
                    Some(forced) => Some(ferrofin_util::sort_name::forced_sort_key(forced)),
                    None => stored
                        .name
                        .as_deref()
                        .map(|name| derived_sort_name(entity, name)),
                };
            }
            if entity.overview.is_none() {
                entity.overview.clone_from(&stored.overview);
            }
            return Some(RemoteMetadata::default());
        }
        let ep = self
            .season_details_cached(cache, &series_id, season)
            .await
            .and_then(|d| episode_in_season(d, number))?
            .clone();
        apply_tmdb_episode(entity, &ep);
        let people = self
            .tmdb_episode_people(series_tmdb_id_of(cache, &series_id), season, number)
            .await
            .fetched();
        // `None` is a FAILED credits request (network, 429, 5xx, bad JSON), not
        // an episode with no cast. Reporting it as a completed fetch would let
        // one rate-limited request during a large scan delete the episode's
        // stored credits — and the re-scan gate above would then skip that
        // episode forever, because its title and overview are now set.
        // Upstream `TmdbEpisodeProvider` sets the episode's own Tmdb id; the
        // client's Identify and external-links surface on an episode page reads
        // it.
        let provider_ids = ep
            .tmdb_id
            .map(|id| vec![("Tmdb".to_owned(), id.to_string())])
            .unwrap_or_default();
        Some(match people {
            Some(people) => RemoteMetadata {
                people,
                provider_ids,
                people_fetched: true,
            },
            None => RemoteMetadata {
                people: Vec::new(),
                provider_ids,
                people_fetched: false,
            },
        })
    }

    /// The people TMDB credits on ONE episode — the regulars credited in *that*
    /// episode in billing order, then its guest stars, then its crew.
    ///
    /// Port of `TmdbEpisodeProvider`'s credits handling. The series' full
    /// regular cast is deliberately NOT merged in: doing that made every
    /// episode page show the series list verbatim, burying the guest stars and
    /// the director the page exists to show.
    ///
    /// `None` means no credits were fetched — TMDB is unwired, the series has
    /// no TMDB id, or the request failed. `Some(vec![])` means TMDB answered
    /// and credits nobody. Credits are persisted by replacement, so the caller
    /// must not conflate the two.
    async fn tmdb_episode_people(
        &self,
        series_tmdb_id: Option<i64>,
        season: i32,
        number: i32,
    ) -> TmdbCredits {
        let (Some(tmdb), Some(series_id)) = (&self.tmdb, series_tmdb_id) else {
            return TmdbCredits::NotAttempted;
        };
        match tmdb.episode_credits(series_id, season, number).await {
            Some(credits) => TmdbCredits::Fetched(tmdb_people(&credits)),
            None => TmdbCredits::Failed,
        }
    }

    /// The people credited on ONE episode: TMDB's per-episode credits when the
    /// series carries a TMDB id, else TVDB's episode credits.
    ///
    /// TVDB's episode credits are the fallback when no TMDB credits were
    /// fetched (no id, or the request failed) and when TMDB credited nobody.
    /// `tmdb_people` filters nothing, so "TMDB returned an empty list" and
    /// "the mapping produced nobody" are the same condition — the pre-existing
    /// behaviour is preserved either way it is spelled.
    ///
    /// `None` means nothing authoritative was learned, and only one case
    /// produces it: TMDB's request **failed** and TVDB's record credits nobody.
    /// Reporting that as a completed fetch would let one TMDB 429 clear the
    /// episode's stored cast.
    ///
    /// A TMDB fetch that was never *attempted* — no client wired, or the series
    /// has no TMDB id — is different: TVDB is then the only source, so its
    /// empty record is authoritative and does clear. Conflating the two would
    /// leave a TVDB-only server unable to correct a stale cast, which is the
    /// bug this change exists to fix, on the other provider.
    async fn episode_people(
        &self,
        series_tmdb_id: Option<i64>,
        season: i32,
        number: i32,
        ep: &ferrofin_providers::TvdbEpisodeDetails,
    ) -> Option<Vec<PeopleEntity>> {
        let tvdb = || tvdb_people(&ep.people);
        match self
            .tmdb_episode_people(series_tmdb_id, season, number)
            .await
        {
            // TMDB answered with credits: they win.
            TmdbCredits::Fetched(credits) if !credits.is_empty() => Some(credits),
            // TMDB answered with nobody. Prefer TVDB's record if it has anyone;
            // otherwise both sources agree, and that is authoritative.
            TmdbCredits::Fetched(empty) => Some(match tvdb() {
                fallback if fallback.is_empty() => empty,
                fallback => fallback,
            }),
            // Nothing was asked of TMDB, so TVDB is the only source and its
            // answer stands even when empty.
            TmdbCredits::NotAttempted => Some(tvdb()),
            // TMDB was asked and failed: only a non-empty TVDB record says
            // anything about this episode.
            TmdbCredits::Failed => Some(tvdb()).filter(|p| !p.is_empty()),
        }
    }

    /// The TheTVDB metadata pass — the TV authority. For a **series** it searches
    /// by name/year, applies the matched series' fields, and caches the details
    /// (its `tvdb_id` lets episodes resolve, its artwork feeds the image pass).
    /// For an **episode** it resolves the episode by (season, number) against the
    /// cached series id and applies its name/overview/air date. Returns the cast
    /// to persist.
    ///
    /// `None` is a MISS — TVDB is unwired, could not resolve the title, or has
    /// nothing for this episode — and the caller falls through to the next
    /// fetcher. A series miss was previously detectable from
    /// `cache.series_tvdb`, but an episode miss had no such signal, so the
    /// caller returned unconditionally for episodes: with no saved
    /// `TypeOptions` both fetcher ranks are `usize::MAX`, making `tvdb_first`
    /// true by default, so a library on default ordering never offered TMDB the
    /// episodes TVDB could not resolve (alternate numbering, specials, very new
    /// episodes). Both kinds now report a miss the same way.
    async fn fetch_tvdb_metadata(
        &self,
        entity: &mut BaseItemEntity,
        short: &str,
        cache: &mut ArtworkCache,
    ) -> Option<RemoteMetadata> {
        let tvdb = self.tvdb.as_ref()?;
        match short {
            "Series" => {
                let name = entity.name.as_deref().filter(|n| !n.is_empty())?;
                let year = entity.production_year.and_then(|y| i32::try_from(y).ok());
                let hit = pick_series_hit(tvdb.search(name, year).await, year)?;
                let details = tvdb.series_details(hit.tvdb_id, METADATA_COUNTRY).await?;
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
                Some(RemoteMetadata {
                    people,
                    provider_ids,
                    people_fetched: true,
                })
            }
            "Episode" => {
                let (Some(series_id), Some(season), Some(number)) = (
                    entity.series_id.clone(),
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) else {
                    return None;
                };
                // The parent series must have matched TVDB earlier this scan.
                let tvdb_id = cache.series_tvdb.get(&series_id).map(|d| d.tvdb_id)?;
                let ep = tvdb
                    .episode_by_number(
                        tvdb_id,
                        ferrofin_providers::tvdb::DEFAULT_SEASON_TYPE,
                        season,
                        number,
                    )
                    .await?;
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
                // A hit either way: the episode's text was applied above. Only
                // the CAST may be unknown, which `people_fetched: false` says.
                Some(
                    match self
                        .episode_people(series_tmdb_id_of(cache, &series_id), season, number, &ep)
                        .await
                    {
                        Some(people) => RemoteMetadata::just_people(people),
                        // Nothing authoritative about this episode's cast —
                        // leave whatever is stored alone rather than clearing it.
                        None => RemoteMetadata::default(),
                    },
                )
            }
            _ => None,
        }
    }

    /// What previous scans already probed for the profile images of `written`,
    /// keyed by image path — one query for the whole credit list.
    ///
    /// Built here rather than carried on the scan's `ArtworkCache`: person ids
    /// are only known once `update_people` has written them, which is after the
    /// per-scan prereads have run.
    async fn stored_person_image_metadata(
        &self,
        written: &[ferrofin_traits::persistence::WrittenPerson],
    ) -> std::collections::HashMap<String, StoredImageMetadata> {
        let with_art: Vec<Uuid> = written
            .iter()
            .filter(|p| p.image_url.is_some())
            .map(|p| p.id)
            .collect();
        if with_art.is_empty() {
            return std::collections::HashMap::new();
        }
        match self.persistence.image_metadata_for_items(&with_art).await {
            Ok(rows) => rows
                .into_iter()
                .map(|row| (row.path.clone(), row))
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "could not read stored person image metadata");
                std::collections::HashMap::new()
            }
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
        // The same `ImageNeedsRefresh` gate the item walk applies, batched over
        // this item's whole credit list: without it every rescan re-decodes and
        // re-BlurHash-encodes every cast member's profile photo, which on a
        // library with thousands of credited people is more image work than the
        // items themselves. One read per credited cast, not one per person.
        let stored = self.stored_person_image_metadata(&written).await;
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
                adopt_stored_image_metadata(&mut infos, &stored);
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
            // Local discovery is NEVER gated on the ImageFetchers list: upstream's
            // `ProviderManager.CanRefreshImages` short-circuits `provider is
            // ILocalImageProvider` to true before consulting `TypeOptions` — and the
            // dashboard checkbox list only ever contains remote fetchers, so gating
            // here silently killed all sidecar artwork once library options were saved.
            let mut images = discover_local_images(entity);
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
            adopt_stored_image_metadata(&mut images, &art_cache.stored_images);
            self.fill_image_metadata(&mut images).await;
            if let Err(err) = self.persistence.save_item_images(item_id, &images).await {
                tracing::warn!(%err, item = %item_id, "failed to persist embedded artwork");
            }
            return;
        }
        // "Local Images" is the media-adjacent discovery (poster.jpg next to the
        // file). Like the metadata art dir below, it is never gated: upstream's
        // `CanRefreshImages` short-circuits `ILocalImageProvider` to enabled before
        // the `TypeOptions.ImageFetchers` check, and that checkbox list only ever
        // names remote fetchers — gating on it silently dropped all sidecar art.
        let mut images = discover_local_images(entity);
        if images.is_empty() && policy.image_enabled(short, fetcher_names::EMBEDDED_IMAGES) {
            images = self.extract_embedded_cover(item_id, entity, streams).await;
        }
        if images.is_empty() {
            images = self.fetch_remote_images(entity, art_cache, policy).await;
        }
        self.append_art_dir_images(entity, &mut images);
        self.apply_dynamic_images(entity, &mut images, policy).await;
        adopt_stored_image_metadata(&mut images, &art_cache.stored_images);
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

    /// Loads the ids of every metadata-locked item (`IsLocked`, the metadata
    /// editor's "lock this item"), once per scan. Absent repository
    /// (unit-test builds) or a read failure → nothing is treated as locked.
    ///
    /// This used to be a per-item `retrieve_item` — a full `SELECT *`
    /// hydration of all 72 `BaseItems` columns to read one boolean, paid for
    /// every planned item on every scan (O(items) queries). Locked items are
    /// rare (usually none), so the whole answer is one small indexed read and
    /// the loop looks the id up in the returned set.
    ///
    /// The index is what makes that true: `FerrofinIX_BaseItems_IsLocked`
    /// (migration 0017) is PARTIAL (`WHERE "IsLocked" = 1`), so it holds only
    /// the locked rows. Without it this is `SCAN BaseItems` — 26-53 ms warm on
    /// a 100k-item table — which is tolerable amortized over a full scan but
    /// not on `scan_paths`, the library-monitor path that runs for one or two
    /// items on every filesystem event.
    ///
    /// A lock applied *while* this scan is running is not seen by it, exactly
    /// as a lock applied one item too late was never seen before. The locked
    /// item's stored metadata is protected regardless: the scan upsert guards
    /// every user-owned column with `CASE WHEN "IsLocked" = 1 THEN "<col>"`,
    /// evaluated against the row's committed value at write time.
    async fn locked_items(&self, planned: &[Planned]) -> std::collections::HashSet<Uuid> {
        // Nothing to scan, nothing to look up. `run_scan` is shared with
        // `scan_paths`, and a noisy directory (`.partial` files, stray
        // subtitles) produces watcher events that plan zero items — which must
        // not cost a database read at all.
        if planned.is_empty() {
            return std::collections::HashSet::new();
        }
        let Some(repo) = &self.item_repository else {
            return std::collections::HashSet::new();
        };
        match repo.locked_item_ids().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                tracing::warn!(%err, "failed to read locked items; treating none as locked");
                std::collections::HashSet::new()
            }
        }
    }

    /// The stored text of every episode, read once per scan.
    ///
    /// The same shape and the same reason as
    /// [`locked_items`](Self::locked_items): the episode providers' re-scan
    /// gate needs what a previous scan achieved, and asking per item would
    /// reinstate the `SELECT *`-per-item cost that read was introduced to
    /// remove. Episodes only — they are the sole consumer.
    async fn stored_episode_text(
        &self,
        planned: &[Planned],
    ) -> std::collections::HashMap<Uuid, StoredText> {
        let Some(repo) = &self.item_repository else {
            return std::collections::HashMap::new();
        };
        // Only the episodes this scan planned. Reading every episode row
        // instead costs ~113 ms and ~30 MB on a 60k-episode library — paid in
        // full by `scan_paths`, which the library monitor runs for one changed
        // file, and paid by libraries with no episodes in them at all.
        let episode_type =
            item_type_lookup::stored_type_name(ferrofin_model::data::BaseItemKind::Episode);
        let ids: Vec<Uuid> = planned
            .iter()
            .filter(|p| Some(p.entity.type_.as_str()) == episode_type)
            .map(|p| p.id)
            .collect();
        if ids.is_empty() {
            return std::collections::HashMap::new();
        }
        match repo
            .item_text_rows(ferrofin_model::data::BaseItemKind::Episode, &ids)
            .await
        {
            Ok(rows) => {
                let mut dropped = 0_usize;
                let map: std::collections::HashMap<Uuid, StoredText> = rows
                    .into_iter()
                    .filter_map(|row| {
                        // Parse before the move: `from_row` takes the row by
                        // value, so the borrow of `row.id` must end first.
                        let parsed = Uuid::parse_str(&row.id);
                        if let Ok(id) = parsed {
                            Some((id, StoredText::from_row(row)))
                        } else {
                            dropped += 1;
                            None
                        }
                    })
                    .collect();
                if dropped > 0 {
                    // Silently dropping rows here would look like "no previous
                    // scan" and re-fetch forever, so say it happened.
                    tracing::debug!(dropped, "stored episode rows with unparseable ids");
                }
                map
            }
            Err(err) => {
                // A closed gate re-fetches; it does NOT protect the stored
                // title, because the scan upsert writes `excluded."Name"` for
                // an unlocked row. A fetch that then misses would put the
                // file-stem placeholder back.
                tracing::warn!(%err, "failed to read stored episode text; re-fetching all");
                std::collections::HashMap::new()
            }
        }
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

    /// The `/tv/{id}/season/{n}` response for one season, fetched at most once
    /// per scan.
    ///
    /// That single response carries the season poster, every episode's still
    /// URL, and every episode's name/overview — so the metadata pass (episode
    /// titles and synopses) and the image pass (season poster, episode stills)
    /// both come through here rather than each paying for the request. A
    /// resolved miss is cached as `None` so a season TMDB does not have is not
    /// re-requested once per episode.
    ///
    /// `None` when TMDB is unwired, the series never matched TMDB, or the
    /// request failed. A series with no cached TMDB id is NOT recorded as a
    /// miss: nothing was asked, and the id may still arrive.
    async fn season_details_cached<'a>(
        &self,
        cache: &'a mut ArtworkCache,
        series_id: &str,
        season: i32,
    ) -> Option<&'a ferrofin_providers::tmdb::SeasonDetails> {
        let key = (series_id.to_owned(), season);
        if !cache.season_details.contains_key(&key) {
            let tmdb = self.tmdb.as_ref()?;
            // No series match yet → no request to make, and no miss to record.
            let tmdb_id = series_tmdb_id_of(cache, series_id)?;
            let details = tmdb.season_details(tmdb_id, season).await;
            cache.season_details.insert(key.clone(), details);
        }
        cache.season_details.get(&key)?.as_ref()
    }

    /// The season-poster / episode-still image pass, split out of
    /// [`fetch_remote_images`](Self::fetch_remote_images). A **season** takes
    /// its poster from the cached `/tv/{id}/season/{n}` response; an
    /// **episode** downloads the still cached earlier this scan (TVDB's, else
    /// that same season response). Both go through
    /// [`season_details_cached`](Self::season_details_cached), so the season
    /// request is made at most once per scan no matter which pass asks first.
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
            // Season posters are TMDB's.
            if !policy.image_enabled(short, fetcher_names::TMDB) {
                return Vec::new();
            }
            let (Some(series_id), Some(season_num)) = (
                entity.series_id.clone(),
                entity.index_number.and_then(|n| i32::try_from(n).ok()),
            ) else {
                return Vec::new();
            };
            let poster = self
                .season_details_cached(cache, &series_id, season_num)
                .await
                .and_then(|d| d.poster.clone());
            let images = poster
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
        // fall back to this episode's entry in the cached season response, and
        // to OMDb's poster last (C# `OmdbImageProvider.Supports` covers Movie,
        // Trailer and Episode).
        let tvdb_still = policy
            .image_enabled(short, fetcher_names::TVDB)
            .then(|| cache.episode_tvdb_still.get(&entity.id).cloned())
            .flatten();
        let url = match tvdb_still {
            Some(url) => Some(url),
            None if policy.image_enabled(short, fetcher_names::TMDB) => {
                match (
                    entity.series_id.clone(),
                    entity
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    entity.index_number.and_then(|n| i32::try_from(n).ok()),
                ) {
                    (Some(series_id), Some(season_num), Some(ep_num)) => self
                        .season_details_cached(cache, &series_id, season_num)
                        .await
                        .and_then(|d| episode_in_season(d, ep_num))
                        .and_then(|ep| ep.still_url.clone()),
                    _ => None,
                }
            }
            None => None,
        };
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
        // One stat per file feeds both dates (a second `getattr` round-trip
        // per item adds up on an NFS-backed library); `std::fs::metadata`
        // follows symlinks, matching upstream's `LinkTarget` resolution. A
        // path that cannot be stat'ed at all is stamped with the scan time
        // (upstream's `dateCreated == MinValue` guard). A folder is never
        // stat'ed for its dates at all (see below).
        let times = if is_folder {
            None
        } else {
            std::fs::metadata(path).ok().map(|m| FileTimes::of(&m))
        };
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
            // Port of `ResolverHelper.SetDateCreated` (+ `EnsureDates`): with
            // `UseFileCreationTimeForDateAdded` (the default) a FILE's "Date
            // Added" is its creation time and `DateModified` its mtime — scan
            // wall-clock would order a first scan by directory traversal. A
            // DIRECTORY is different: `ManagedFileSystem.GetFileSystemMetadata`
            // only fills `CreationTimeUtc`/`LastWriteTimeUtc` for a `FileInfo`,
            // so every folder item (Series, Season, MusicAlbum, PhotoAlbum, a
            // disc-rip Movie whose path is the directory) resolves with
            // `MinValue` dates → `DateCreated = DateTime.UtcNow` at FIRST
            // resolve and `DateModified` unset (stored NULL). The scan upsert's
            // `coalesce("DateCreated", excluded."DateCreated")` is what keeps
            // that first-resolve stamp stable across rescans.
            date_created: Some(match &times {
                Some(times) => creation_time_from(times).into(),
                None => Utc::now(),
            }),
            date_modified: times.map(|t| t.mtime.into()),
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
            // The extra's item KIND comes from its extra type, per
            // `ExtraResolver.GetResolversForExtraType` (v10.11.8), which is a
            // three-way switch, not a two-way one:
            //   ExtraType.Trailer   => _trailerResolvers  (GenericVideoResolver<Trailer>)
            //   ExtraType.ThemeSong => null               ("For audio we'll have
            //                          to rely on the AudioResolver, which is a
            //                          'built-in'") — so a theme song is an
            //                          AUDIO item, not a video one
            //   _                   => _videoResolvers
            // A `Trailer` is what makes a `-trailer.*` file visible to
            // `GET /Trailers` and `/Items?includeItemTypes=Trailer`; an `Audio`
            // theme song is what makes `/Items/{id}/ThemeMedia` return it under
            // `ThemeSongsResult` rather than as a stray Video row.
            let (kind, media_type) = match extra_type {
                ferrofin_model::entities::ExtraType::Trailer => (BaseItemKind::Trailer, "Video"),
                ferrofin_model::entities::ExtraType::ThemeSong => (BaseItemKind::Audio, "Audio"),
                _ => (BaseItemKind::Video, "Video"),
            };
            let Some((id, mut entity)) =
                self.base_item(kind, cf, cf, file_stem(&path), &path, false)
            else {
                continue;
            };
            entity.media_type = Some(media_type.to_owned());
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
            // Upstream resolves EXTRAS first, over every file in the folder —
            // `ExtraRuleResolver` has audio rules as well as video ones
            // (`theme.mp3` is `ExtraRuleType.Filename` + `MediaType.Audio`), and
            // `ExtraResolver.GetResolversForExtraType` sends a ThemeSong to the
            // AudioResolver. Gating on `is_video_file` alone dropped every audio
            // extra on the floor, so a movie's theme song was never ingested at
            // all.
            let is_video = video_resolver::is_video_file(&entry.path, naming);
            let is_audio = is_audio_file(&entry.path, naming);
            if !is_video && !is_audio {
                continue;
            }
            let extra = ferrofin_naming::video::extra_rule_resolver::get_extra_info(
                &entry.path,
                naming,
                Some(root),
            );
            if let Some(extra_type) = extra.extra_type {
                extras.push((entry.path.clone(), extra_type));
                continue;
            }
            // An audio file that matched no extra rule is not a movie: a
            // soundtrack sitting beside the film belongs to a music library.
            if !is_video {
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

    /// Music library: an artist directory is a `MusicArtist`
    /// (`MusicArtistResolver`), a folder that directly contains audio files is a
    /// `MusicAlbum` (`MusicAlbumResolver`, its audio files become `Audio`
    /// tracks); anything else is walked so a deeper layout still yields rows.
    fn plan_music(&self, dir: &str, cf: Uuid, naming: &NamingOptions, out: &mut Vec<Planned>) {
        // The library root itself is the CollectionFolder — never an artist or
        // an album (upstream's `args.Parent.IsRoot` guard), so recurse into it.
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_music_node(&entry.path, cf, cf, false, naming, out);
            }
        }
        // Loose audio directly in the library root still becomes an album, so
        // stray files are browsable rather than invisible.
        self.plan_music_album(dir, cf, cf, naming, out);
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
        let read = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || ferrofin_providers::read_book_metadata(&path)).await
        };
        let people = match read.ok().flatten() {
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
        // Inflating a comic archive can run to hundreds of megabytes; that is
        // blocking work and must not sit on an async worker thread.
        let owned = path.to_owned();
        let cover =
            tokio::task::spawn_blocking(move || ferrofin_providers::read_book_cover(&owned)).await;
        let Some((name, bytes)) = cover.ok().flatten() else {
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
            // A directory name is not a filename: `file_stem` would truncate
            // "Trip 2024. Iceland" at the dot.
            let name = folder_name(dir).unwrap_or_else(|| file_stem(dir));
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

    /// Resolves one directory beneath a music library: a `MusicArtist` (port of
    /// `MusicArtistResolver`), a `MusicAlbum` (port of `MusicAlbumResolver`), or
    /// a container to walk through — one of an artist's release subfolders
    /// (`albums`, `live`, …) or an intermediate grouping directory.
    ///
    /// The order is upstream's resolver priority: `MusicArtistResolver` is
    /// `ResolverPriority.Second` and `MusicAlbumResolver` `Third`, so a folder
    /// that holds BOTH loose audio and an album subfolder is the artist.
    ///
    /// `in_artist` is upstream's `args.HasParent<MusicArtist>()` guard ("don't
    /// allow nested artists"); its `HasParent<MusicAlbum>()` half is structural
    /// here, because this function returns as soon as a directory resolves as an
    /// album and so never recurses beneath one.
    fn plan_music_node(
        &self,
        dir: &str,
        cf: Uuid,
        parent: Uuid,
        in_artist: bool,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        if !in_artist
            && self.is_music_artist(dir, naming)
            && let Some((artist_id, artist)) = self.base_item(
                BaseItemKind::MusicArtist,
                cf,
                parent,
                file_stem(dir),
                dir,
                true,
            )
        {
            out.push(Planned {
                id: artist_id,
                entity: artist,
                ancestors: vec![cf],
            });
            for entry in self.file_system.get_file_system_entries(dir) {
                if entry.type_ == FileSystemEntryType::Directory {
                    self.plan_music_node(&entry.path, cf, artist_id, true, naming, out);
                }
            }
            // Loose audio sitting DIRECTLY in a resolved artist folder is not
            // an album, and used to be wrapped in one here. Upstream never
            // wraps it: once the directory resolves as a `MusicArtist`, its
            // child FILES go through the ordinary resolver chain, and
            // `MusicAlbumResolver` cannot claim them — it resolves a
            // DIRECTORY (`MusicAlbumResolver.Resolve` returns null unless
            // `args.IsDirectory`), so an audio file becomes an `Audio` whose
            // parent is the artist. Wrapping them invented a `MusicAlbum` row
            // Jellyfin has no row for and stamped the artist's folder name
            // into every such track's `Album`.
            self.plan_loose_artist_audio(dir, cf, artist_id, naming, out);
            return;
        }
        if self.is_music_album(dir, naming, true) {
            self.plan_music_album(dir, cf, parent, naming, out);
            return;
        }
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                self.plan_music_node(&entry.path, cf, parent, in_artist, naming, out);
            }
        }
    }

    /// Whether `dir` resolves as a `MusicArtist` — port of
    /// `MusicArtistResolver.Resolve`
    /// (`Emby.Server.Implementations/Library/Resolvers/Audio/MusicArtistResolver.cs:54`).
    ///
    /// Upstream's other guards are structural here and have no test of their
    /// own: `!args.IsDirectory` (only directories reach this), the
    /// `CollectionType.music` check (only the music arm calls it), the
    /// nested-artist guard (the caller's `in_artist`), and `args.Parent.IsRoot`
    /// — that one guards the AGGREGATE root, and a music library's artist
    /// folders sit under the library folder, never under it. It must NOT be
    /// translated into "skip depth 1", which would suppress every artist.
    fn is_music_artist(&self, dir: &str, naming: &NamingOptions) -> bool {
        let entries = self.file_system.get_file_system_entries(dir);
        // `args.ContainsFileSystemEntryByName("artist.nfo")` short-circuits
        // before every other test, and matches on the entry NAME regardless of
        // whether the entry is a file or a directory.
        if entries
            .iter()
            .any(|e| entry_name(&e.path).eq_ignore_ascii_case("artist.nfo"))
        {
            return true;
        }
        let parser = ferrofin_naming::audio::AlbumParser::new(naming);
        // Upstream's `Parallel.ForEach` + `state.Stop()` is an early-exit `any`.
        for entry in &entries {
            if entry.type_ != FileSystemEntryType::Directory {
                continue;
            }
            // A named artist subfolder ("albums", "live", …) says artist.
            if naming
                .artist_subfolders
                .iter()
                .any(|s| s.eq_ignore_ascii_case(entry_name(&entry.path)))
            {
                return true;
            }
            // A multi-disc folder is part of an ALBUM, never an artist signal.
            if parser.is_multi_part(&entry.path) {
                continue;
            }
            // `MusicAlbumResolver.IsMusicAlbum(path, dirService)` is
            // `ContainsMusic(entries, allowSubfolders: true)`.
            if self.is_music_album(&entry.path, naming, true) {
                return true;
            }
        }
        false
    }

    /// Emits the `MusicAlbum` row for `dir` plus its tracks — the audio files
    /// directly inside, and those in any multi-disc subfolder (`CD1`, `Disc 2`,
    /// …), which fold into the same album rather than becoming albums of their
    /// own.
    fn plan_music_album(
        &self,
        dir: &str,
        cf: Uuid,
        parent: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        let tracks = self.collect_album_tracks(dir, naming);
        if tracks.is_empty() {
            return;
        }
        let album_name = file_stem(dir);
        let Some((album_id, album)) = self.base_item(
            BaseItemKind::MusicAlbum,
            cf,
            parent,
            album_name.clone(),
            dir,
            true,
        ) else {
            return;
        };
        // The presentation key is NOT the album name: a real 10.11.8 stores
        // the album's own id in `N` form here, the same as every other media
        // row ("{AlbumArtist}-{Name}" is its *user data* key,
        // `MusicAlbum.cs:106`). Writing the name grouped every "Greatest Hits"
        // in the library into one row. The writer derives it — see
        // `kinds::presentation_unique_key`.
        // AncestorIds is what the recursive-child count and every
        // `ancestorIds=` filter read, so a resolved artist must appear in its
        // albums' and tracks' chains.
        let album_ancestors = if parent == cf {
            vec![cf]
        } else {
            vec![cf, parent]
        };
        out.push(Planned {
            id: album_id,
            entity: album,
            ancestors: album_ancestors.clone(),
        });
        let mut ancestors = album_ancestors;
        ancestors.push(album_id);
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

    /// The audio files sitting directly in a resolved artist directory, as
    /// `Audio` rows parented straight to the `MusicArtist` — no album.
    ///
    /// Subdirectories are deliberately NOT walked: `plan_music_node` has
    /// already recursed into every child directory of the artist, so reusing
    /// [`collect_album_tracks`](Self::collect_album_tracks) here (which also
    /// pulls a multi-disc subfolder's tracks in) would plan those tracks
    /// twice — once under their own album and once under the artist.
    fn plan_loose_artist_audio(
        &self,
        dir: &str,
        cf: Uuid,
        artist_id: Uuid,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory || !is_audio_file(&entry.path, naming)
            {
                continue;
            }
            let Some((id, mut entity)) = self.base_item(
                BaseItemKind::Audio,
                cf,
                artist_id,
                file_stem(&entry.path),
                &entry.path,
                false,
            ) else {
                continue;
            };
            entity.media_type = Some("Audio".to_owned());
            // No `album` placeholder: there is no album. A file that carries an
            // ALBUM tag still gets it from `apply_audio_metadata`; one that does
            // not is albumless on both servers.
            out.push(Planned {
                id,
                entity,
                ancestors: vec![cf, artist_id],
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
    // An OPF's `file-as` / `calibre:title_sort` is C#'s `ForcedSortName`: it is
    // the whole point of a Calibre library's "Tolkien, J.R.R." ordering, and
    // outranks the name-derived sort key set just above.
    if let Some(sort) = book
        .sort_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        entity.sort_name = Some(sort.to_owned());
    }
    if entity.original_title.is_none() {
        entity.original_title.clone_from(&book.original_title);
    }
    // C# `BookMetadataService.MergeData` assigns SeriesName when `replaceData`
    // OR the target is empty, and a default scan passes `replaceData: true`
    // (`MetadataService.cs` `shouldReplace`). So the embedded value WINS over
    // the resolver's parent-folder guess — which `push_book` always sets, and
    // which an empty-only guard would therefore never let through.
    if let Some(series) = book.series_name.as_deref().filter(|s| !s.is_empty()) {
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

/// The three stat timestamps the creation-time rule reads.
#[derive(Debug, Clone, Copy)]
struct FileTimes {
    /// The statx birth time, when the filesystem reports one.
    birth: Option<std::time::SystemTime>,
    /// The inode change time (`st_ctime`).
    ctime: std::time::SystemTime,
    /// The content modification time (`st_mtime`).
    mtime: std::time::SystemTime,
}

impl FileTimes {
    /// Reads the timestamps off a stat result. `Metadata::created()` is
    /// `ErrorKind::Unsupported` exactly when statx reports no `STATX_BTIME` —
    /// the same condition as .NET's `HasBirthTime`.
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        Self {
            birth: meta.created().ok(),
            ctime: ctime_of(meta).unwrap_or(mtime),
            mtime,
        }
    }
}

/// `st_ctime` as a [`SystemTime`](std::time::SystemTime) (Unix only; other
/// targets have no change time and fall back to the mtime).
#[cfg(unix)]
fn ctime_of(meta: &std::fs::Metadata) -> Option<std::time::SystemTime> {
    use std::os::unix::fs::MetadataExt as _;
    let secs = u64::try_from(meta.ctime()).ok()?;
    let nanos = u32::try_from(meta.ctime_nsec()).ok()?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos))
}

#[cfg(not(unix))]
fn ctime_of(_meta: &std::fs::Metadata) -> Option<std::time::SystemTime> {
    None
}

/// .NET's `FileSystemInfo.CreationTimeUtc` on Unix (`FileStatus.Unix.cs`
/// `GetCreationTime`): the statx birth time when the filesystem has one,
/// otherwise "the oldest time we have in between change and modify time" —
/// the older of `ctime` and `mtime`. Ported for fidelity rather than effect:
/// `min(ctime, mtime)` only differs from the mtime alone when the mtime lies
/// in the future of the inode change (clock skew, a future-dated copy) — a
/// `cp -p`/rsync-preserved file still dates by its preserved mtime, exactly
/// as it does on Jellyfin.
fn creation_time_from(times: &FileTimes) -> std::time::SystemTime {
    times.birth.unwrap_or_else(|| times.ctime.min(times.mtime))
}

/// The final path component, extension included — upstream's
/// `FileSystemMetadata.Name`, which resolvers match entry names against.
fn entry_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
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
    // `<dateadded>` overrides the resolver's stamp (`BaseNfoParser`:
    // `item.DateCreated = dateCreated`, and `MetadataService.MergeData` copies
    // a non-MinValue `DateCreated` from the provider result onto the item).
    // Accepted divergence: this lands on FIRST import only — the scan upsert
    // (`item_persistence_service::scan_upsert_sql`) coalesces `DateCreated`,
    // so a `<dateadded>` added or edited later is not re-applied on rescan or
    // refresh, where upstream's `MergeData` re-stamps it on every refresh.
    // Letting an NFO-sourced `DateCreated` win that coalesce is the
    // persistence-side follow-up.
    if n.date_created.is_some() {
        entity.date_created = n.date_created;
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

/// The ids a fetcher may resolve this row by: the sidecar's first, then any the
/// row already carries from a previous scan.
///
/// Both are `info.GetProviderId` as far as the fetchers are concerned; the
/// sidecar leads because it is what the user pinned.
fn known_provider_ids(
    nfo_ids: &[(String, String)],
    art_cache: &ArtworkCache,
    entity_id: &str,
) -> Vec<(String, String)> {
    merge_provider_ids(
        nfo_ids.to_vec(),
        art_cache
            .item_provider_ids
            .get(entity_id)
            .cloned()
            .unwrap_or_default(),
    )
}

/// Everything one ffprobe result contributes beyond the item row itself.
#[derive(Default)]
struct ProbeRows {
    /// The stream rows (`MediaStreamInfos`).
    streams: Vec<MediaStreamInfoEntity>,
    /// The chapter rows.
    chapters: Vec<ChapterEntity>,
    /// The attachment rows (`AttachmentStreamInfos`).
    attachments: Vec<ferrofin_db::entities::base_items::AttachmentStreamInfoEntity>,
    /// Provider ids read from embedded audio tags.
    provider_ids: Vec<(String, String)>,
    /// Whether the attachment rows are authoritative: a probe ran and the item
    /// is a video (`false` — no probe, or an audio file — leaves stored rows alone).
    save_attachments: bool,
}

/// What one scanned item contributes beyond its own row — the arguments
/// [`LibraryScanner::persist_item_media`] would otherwise take positionally.
struct ItemMedia<'a> {
    /// What the probe yielded (streams, chapters, attachments).
    probe: &'a ProbeRows,
    /// Images the file itself yielded (a photo, or a book's cover).
    embedded_images: Vec<ItemImageInfo>,
    /// The library's fetcher policy for this item.
    policy: FetcherPolicy<'a>,
    /// Whether the item is locked, in which case artwork is left alone.
    locked: bool,
}

/// `local` followed by the entries of `remote` whose key `local` does not
/// already hold.
///
/// NFO ids lead: `save_provider_id` is INSERT OR REPLACE, so a user's pinned
/// id must not be overwritten by one a remote search guessed for the same key.
fn merge_provider_ids(
    mut local: Vec<(String, String)>,
    remote: Vec<(String, String)>,
) -> Vec<(String, String)> {
    for (key, value) in remote {
        if !local.iter().any(|(k, _)| k.eq_ignore_ascii_case(&key)) {
            local.push((key, value));
        }
    }
    local
}

/// The IMDb id among a scanned row's provider-id pairs, if it recorded one.
fn imdb_id_of(ids: Option<&Vec<(String, String)>>) -> Option<String> {
    imdb_id_in(ids?)
}

/// The IMDb id among a list of provider-id pairs.
fn imdb_id_in(ids: &[(String, String)]) -> Option<String> {
    ids.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Imdb"))
        .map(|(_, v)| v.clone())
}

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
    // Deliberately NOT `Runtime`: C# `OmdbProvider` deserializes it but never
    // assigns it. The media file's probed duration is authoritative, and OMDb's
    // rounded minutes would otherwise become a Series' runtime out of nowhere.
    if english {
        if us && entity.official_rating.is_none() {
            entity.official_rating.clone_from(&item.rated);
        }
        // `ParseAdditionalMetadata` clears the genres on OMDb's OWN fresh
        // `MetadataResult`, not on the accumulated one: `ExecuteRemoteProviders`
        // then folds each provider's result in with `replaceData: false`, so
        // OMDb's list only lands where nothing has one yet. The "IMDb data is
        // better than TVDB" comment is about beating the OTHER remote fetchers
        // in the same pass, not about displacing an NFO.
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

/// Whether the row's name is still the resolver's file-stem placeholder rather
/// than a real title.
///
/// A provider title outranks the placeholder (upstream
/// `MetadataService.MergeBaseItemData` runs with `replaceData=true` on a
/// standard scan, so the provider replaces the stem the resolver stamped), but
/// an NFO `<title>` still wins: `apply_nfo` ran first and changed the name away
/// from the stem, which is exactly what this detects.
///
/// Shared by every episode provider so the two cannot disagree about what a
/// placeholder is — a disagreement would show up as one provider silently
/// refusing to title an episode.
///
/// One deliberate change from the inline version this replaces: a row with a
/// blank name AND a path used to fall through to `"" == file_stem(path)`
/// (false) and keep its blank name. A blank name is a placeholder by any
/// reading, so it now matches first.
fn name_is_file_stem_placeholder(entity: &BaseItemEntity) -> bool {
    name_is_placeholder(entity.name.as_deref(), entity.path.as_deref())
}

/// [`name_is_file_stem_placeholder`] over the two fields it reads, so the
/// narrow `ItemTextRow` projection can use the identical rule.
fn name_is_placeholder(name: Option<&str>, path: Option<&str>) -> bool {
    match (name, path) {
        (None | Some(""), _) => true,
        (Some(name), Some(path)) => name == file_stem(path),
        _ => false,
    }
}

/// Applies a provider's episode title to the row, replacing the resolver's
/// file-stem placeholder. The derived sort name follows the new title, as
/// `apply_nfo` does. A blank title, or a row already carrying a real title,
/// is left alone.
fn apply_episode_title(entity: &mut BaseItemEntity, title: Option<&str>) {
    if !name_is_file_stem_placeholder(entity) {
        return;
    }
    if let Some(title) = title.map(str::trim).filter(|s| !s.is_empty()) {
        entity.name = Some(title.to_owned());
        entity.sort_name = Some(derived_sort_name(entity, title));
    }
}

/// Applies matched TheTVDB **episode** fields to the row (fill-if-empty for
/// everything except the title, where the provider outranks the resolver's
/// filename placeholder).
fn apply_tvdb_episode(entity: &mut BaseItemEntity, d: &ferrofin_providers::TvdbEpisodeDetails) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&d.overview);
    }
    apply_episode_title(entity, d.name.as_deref());
    if entity.production_year.is_none() {
        entity.production_year = d.production_year.map(i64::from);
    }
    if entity.premiere_date.is_none() {
        entity.premiere_date = d.aired.as_deref().and_then(parse_ymd);
    }
}

/// Applies a matched TMDB **episode** entry from the season response to the row
/// — the same fill-if-empty overview and placeholder-replacing title as
/// [`apply_tvdb_episode`].
///
/// The air date and community rating come from the same season response — with
/// TVDB unticked for `Episode` (the common shape on a Jellyfin-migrated
/// library) this is the only fetcher that fills them, and a client shows an
/// episode with no date and no rating where Jellyfin shows both.
fn apply_tmdb_episode(entity: &mut BaseItemEntity, d: &ferrofin_providers::tmdb::EpisodeDetails) {
    if entity.overview.is_none() {
        entity.overview.clone_from(&d.overview);
    }
    apply_episode_title(entity, d.name.as_deref());
    let aired = d.air_date.as_deref().and_then(parse_ymd);
    if entity.premiere_date.is_none() {
        entity.premiere_date = aired;
    }
    if entity.production_year.is_none() {
        entity.production_year = aired.map(|d| i64::from(d.year()));
    }
    if entity.community_rating.is_none() {
        entity.community_rating = d.vote_average.map(f64::from);
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
    // The sort key is recomputed LAST, because `Audio.CreateSortName` reads the
    // disc/track numbers this pass just filled in
    // (`{ParentIndexNumber:0000 - }{IndexNumber:0000 - }Name`). Deriving it
    // earlier — as the resolver's `base_item` does for every row — leaves a
    // track sorting by the alphanumeric name key, which puts it in a different
    // place than Jellyfin puts it in every album and every search-hint page.
    // A `ForcedSortName` (an NFO `<sortname>`) still wins, as `BaseItem.SortName`
    // does.
    if entity.forced_sort_name.is_none()
        && let Some(name) = entity.name.clone()
    {
        entity.sort_name = Some(derived_sort_name(entity, &name));
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

/// Applies C# `AlbumMetadataService`'s child-metadata aggregation to one album,
/// returning the updated row and whether anything changed.
///
/// `AlbumMetadataService` turns on `EnableUpdatingPremiereDateFromChildren`,
/// `EnableUpdatingGenresFromChildren` and `EnableUpdatingStudiosFromChildren`
/// over `GetChildrenForMetadataUpdates => GetRecursiveChildren(i => i is
/// Audio)`, so an album's genres, studios and premiere date are the
/// union/minimum of its tracks'. Ferrofin used to aggregate only the album
/// artist and the year, which left every scanned album with `Genres: []` —
/// invisible to a `Genres=` query, and therefore unmatchable by
/// `/Albums/{id}/Similar`, which is exactly such a query.
fn apply_album_child_metadata(
    album: &BaseItemEntity,
    tracks: &[BaseItemEntity],
) -> (BaseItemEntity, bool) {
    // Aggregate the tracks' metadata onto the album row.
    let mut updated = album.clone();
    let mut changed = false;
    // `MetadataService.UpdateCumulativeRunTimeTicks` (v10.11.8
    // `MediaBrowser.Providers/Manager/MetadataService.cs`), which the base
    // `UpdateMetadataFromChildren` runs for any `Folder` with
    // `SupportsCumulativeRunTimeTicks` — `MusicAlbum.cs` sets it. It runs BEFORE
    // the `IsLocked` early-return in `AlbumMetadataService`, so a locked album
    // still gets its runtime. The sum is assigned even when it is 0, which is
    // why Jellyfin emits `RunTimeTicks: 0` and never omits the field.
    let ticks: i64 = tracks
        .iter()
        .filter(|t| !t.is_folder)
        .map(|t| t.run_time_ticks.unwrap_or(0))
        .sum();
    if updated.run_time_ticks != Some(ticks) {
        updated.run_time_ticks = Some(ticks);
        changed = true;
    }
    // The item-level lock stands in for C#'s per-field
    // `LockedFields.Contains(MetadataField.Genres|Studios)`: Ferrofin
    // does not model field-level locks anywhere (see the photo pass's
    // `name_locked`), and the item lock is the stricter guard.
    if !album.is_locked {
        // `MetadataService.UpdateGenres`: `children.SelectMany(i =>
        // i.Genres).Distinct(OrdinalIgnoreCase)` — an unconditional
        // assignment, so an album whose tracks lost their genre tags
        // loses the genres too.
        changed |= assign_from_children(
            &mut updated.genres,
            &distinct_ignoring_case(tracks.iter().map(|t| t.genres.as_deref())),
        );
        // `MetadataService.UpdateStudios`, same shape.
        changed |= assign_from_children(
            &mut updated.studios,
            &distinct_ignoring_case(tracks.iter().map(|t| t.studios.as_deref())),
        );
        // `AlbumMetadataService.SetArtistsFromSongs` — the tracks' performer
        // credits, most-frequent first. Without this the album row's `Artists`
        // column stays NULL and the DTO omits both `Artists` and `ArtistItems`.
        changed |= assign_ordered(
            &mut updated.artists,
            &frequency_ordered_distinct(tracks.iter().map(|t| t.artists.as_deref())),
        );
        // `AlbumMetadataService.SetAlbumArtistFromSongs` — the same shape over
        // the tracks' album-artist tags. C# assigns unconditionally; the old
        // "fill only when the column is empty, from the first non-empty track"
        // rule appears nowhere upstream.
        changed |= assign_ordered(
            &mut updated.album_artists,
            &frequency_ordered_distinct(tracks.iter().map(|t| t.album_artists.as_deref())),
        );
    }
    // `MetadataService.UpdatePremiereDate`: the earliest child premiere
    // date wins and re-derives the production year; only when NO child
    // carries one does it fall back to the minimum child production
    // year (and `Select(i => i.ProductionYear ?? 0).Min()` means one
    // undated track suppresses that fallback entirely). This overwrites
    // — C# does not merely fill a null.
    if !tracks.is_empty() {
        if let Some(date) = tracks.iter().filter_map(|t| t.premiere_date).min() {
            let year = i64::from(date.year());
            if updated.premiere_date != Some(date) || updated.production_year != Some(year) {
                updated.premiere_date = Some(date);
                updated.production_year = Some(year);
                changed = true;
            }
        } else {
            let year = tracks
                .iter()
                .map(|t| t.production_year.unwrap_or(0))
                .min()
                .unwrap_or(0);
            if year > 0 && updated.production_year != Some(year) {
                updated.production_year = Some(year);
                changed = true;
            }
        }
    }
    // The album row's name starts as the folder stem, which is usually
    // release noise ("RHCP - Californication (1999) FLAC"). The tracks'
    // ALBUM tag is authoritative when they agree — upstream's album
    // metadata comes from the tags, not the directory.
    if let Some(tagged) = album_name_consensus(tracks)
        && updated.name.as_deref() != Some(tagged.as_str())
    {
        updated.sort_name = Some(create_sort_name(&tagged));
        updated.name = Some(tagged);
        changed = true;
    }
    (updated, changed)
}

/// The distinct values of a `|`-joined column across a set of children, deduped
/// case-insensitively with the first-seen casing kept.
///
/// Port of the `children.SelectMany(i => i.<Field>).Distinct(StringComparer.
/// OrdinalIgnoreCase)` that `MetadataService.UpdateGenres`/`UpdateStudios` run.
fn distinct_ignoring_case<'a>(columns: impl Iterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for value in columns
        .flatten()
        .flat_map(|column| column.split('|'))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if seen.insert(value.to_lowercase()) {
            out.push(value.to_owned());
        }
    }
    out
}

/// The grouping key of .NET's `StringComparer.OrdinalIgnoreCase`.
///
/// Ordinal-ignore-case is *invariant simple* case folding: .NET upper-cases one
/// char to exactly one char and leaves a char whose mapping is not 1:1 alone.
/// Rust's `str::to_lowercase` is the *full* Unicode mapping, which is a
/// different comparer — under it `\u{3c2}` (final sigma) and `\u{3c3}` are
/// distinct, where .NET folds both to `\u{3a3}` and calls them equal. Folding
/// per char and keeping only the 1:1 results reproduces .NET exactly, including
/// its non-folds (`\u{df}` stays, because `ToUpperInvariant` cannot expand it
/// to `SS`).
fn ordinal_ignore_case_key(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(one), None) => one,
                _ => c,
            }
        })
        .collect()
}

/// The distinct values of a `|`-joined column across a set of children, ordered
/// by descending frequency with ties keeping first-appearance order.
///
/// Port of the LINQ `children.SelectMany(...).GroupBy(i, OrdinalIgnoreCase)
/// .OrderByDescending(g => g.Count()).Select(g => g.Key)` that
/// `AlbumMetadataService.SetArtistsFromSongs` / `SetAlbumArtistFromSongs` run.
/// `GroupBy` yields groups in first-appearance order and `g.Key` is the
/// first-seen casing; `OrderByDescending` is a stable sort, so `sort_by` on an
/// insertion-ordered vector reproduces it exactly.
fn frequency_ordered_distinct<'a>(columns: impl Iterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for value in columns
        .flatten()
        .flat_map(|column| column.split('|'))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let key = ordinal_ignore_case_key(value);
        if let Some(&at) = index.get(&key) {
            order[at].1 += 1;
        } else {
            index.insert(key, order.len());
            order.push((value.to_owned(), 1));
        }
    }
    // Stable, so ties keep first-appearance order — C# `OrderByDescending`.
    order.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    order.into_iter().map(|(name, _)| name).collect()
}

/// Writes an aggregated child value list onto a parent's `|`-joined column when
/// the ORDERED, case-sensitive sequence differs, reporting whether it changed.
///
/// This is the comparison `Set*FromSongs` uses — a plain
/// `SequenceEqual(..., StringComparer.Ordinal)` — as opposed to the sorted,
/// case-insensitive set comparison of `UpdateGenres`/`UpdateStudios`. A pure
/// re-ordering therefore counts as a change and rewrites the column.
fn assign_ordered(column: &mut Option<String>, values: &[String]) -> bool {
    let current: Vec<&str> = column
        .as_deref()
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if current.len() == values.len() && current.iter().zip(values).all(|(c, v)| *c == v.as_str()) {
        return false;
    }
    *column = (!values.is_empty()).then(|| values.join("|"));
    true
}

/// Writes an aggregated child value list onto a parent's `|`-joined column,
/// reporting whether it changed.
///
/// The comparison is C#'s: same length, and the same members ignoring case and
/// order (`currentList.Order().SequenceEqual(item.Genres.Order(), Ordinal
/// IgnoreCase)`). An empty aggregate clears the column, because the C#
/// assignment is unconditional.
fn assign_from_children(column: &mut Option<String>, values: &[String]) -> bool {
    let normalize = |v: &[String]| {
        let mut n: Vec<String> = v.iter().map(|s| s.to_lowercase()).collect();
        n.sort_unstable();
        n
    };
    let current: Vec<String> = column
        .as_deref()
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if normalize(&current) == normalize(values) {
        return false;
    }
    *column = (!values.is_empty()).then(|| values.join("|"));
    true
}

/// Collects an item's genres/studios/tags/artists as `(ItemValueType
/// discriminant, value)` pairs for the `ItemValues` filter tables.
///
/// Re-exported from `ferrofin-db` so `ferrofin-providers` — which may not
/// depend on `ferrofin-core` — can re-index an item it refreshed with exactly
/// the same rule the scanner uses.
pub(crate) use ferrofin_db::entities::base_items::item_values_of;

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
        "Audio" | "AudioBook" => {
            audio_sort_name(entity.parent_index_number, entity.index_number, title)
        }
        _ => create_sort_name(title),
    }
}

/// Port of C# `Audio.CreateSortName` (v10.11.8
/// `MediaBrowser.Controller/Entities/Audio/Audio.cs`):
/// `ParentIndexNumber.ToString("0000 - ") + IndexNumber.ToString("0000 - ") + Name`,
/// each prefix omitted when its number is absent, and the **raw** name appended
/// — `Audio` overrides `CreateSortName` outright, so the alphanumeric
/// lowercase/pad pipeline never runs on a track.
///
/// A track stored with the alphanumeric key instead sorts in a different place
/// than Jellyfin puts it, which reorders every album and every search-hint page
/// that contains one.
fn audio_sort_name(parent_index: Option<i64>, index: Option<i64>, name: &str) -> String {
    let disc = parent_index.map_or_else(String::new, |n| format!("{n:04} - "));
    let track = index.map_or_else(String::new, |n| format!("{n:04} - "));
    format!("{disc}{track}{name}")
}

/// Port of C# `BaseItem.CreateSortName` + `ModifySortChunks`; the shared
/// implementation lives in `ferrofin-util` so the metadata providers (Identify
/// renames) and the Live TV guide compute the same sort key as the scanner.
pub(crate) use ferrofin_util::sort_name::create_sort_name;

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

/// Whether an image row already carries everything the DTO layer needs, so
/// nothing has to decode the file.
fn image_metadata_is_complete(image: &ItemImageInfo) -> bool {
    image.width > 0 && image.height > 0 && image.blur_hash.as_ref().is_some_and(|b| !b.is_empty())
}

/// Copies a previous scan's dimensions and blurhash onto each discovered image
/// whose file has not changed since, so the image pass has nothing left to
/// decode.
///
/// This is the Ferrofin shape of C#'s `LibraryManager.UpdateImagesAsync`
/// selecting only the `ImageNeedsRefresh` images: upstream's `BaseItem` already
/// holds the previous values in memory, so it simply leaves the untouched ones
/// alone. Here they come from the scan's one-query image preread.
fn adopt_stored_image_metadata(
    images: &mut [ItemImageInfo],
    stored: &std::collections::HashMap<String, StoredImageMetadata>,
) {
    if stored.is_empty() {
        return;
    }
    for image in images.iter_mut() {
        if let Some(prev) = stored.get(&image.path)
            && image_metadata_is_current(prev, image.date_modified)
        {
            image.width = prev.width;
            image.height = prev.height;
            image.blur_hash.clone_from(&prev.blur_hash);
        }
    }
}

/// Whether `stored` still describes the file, so its dimensions and blurhash
/// can be reused instead of re-probing.
///
/// Verbatim port of the local-file half of C#
/// `LibraryManager.ImageNeedsRefresh`: a refresh is needed when the stored
/// width, height or blurhash is missing, or when the stored `DateModified`
/// differs from the file's current mtime by **more than one second**. The
/// one-second tolerance is upstream's, and it is load-bearing — a filesystem
/// that stores mtimes at whole-second resolution would otherwise never agree
/// with a sub-second timestamp Ferrofin wrote.
fn image_metadata_is_current(
    stored: &StoredImageMetadata,
    file_modified: chrono::DateTime<Utc>,
) -> bool {
    stored.width > 0
        && stored.height > 0
        && stored.blur_hash.as_ref().is_some_and(|b| !b.is_empty())
        && stored
            .date_modified
            .signed_duration_since(file_modified)
            .num_milliseconds()
            .abs()
            <= 1000
}

/// Discovers an item's local artwork (poster/backdrop/logo/…) by scanning its
/// folder with the local-image providers, returning rows ready to persist.
///
/// Dimensions are left `0` (unknown) here; they are filled in afterwards, either
/// by adopting a previous scan's probe or by decoding the file. Episodes use the
/// episode provider; everything the generic provider supports uses it;
/// unsupported kinds yield nothing.
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
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::ItemPersistenceService as _;

    /// `AlbumMetadataService` groups the songs' artists with
    /// `StringComparer.OrdinalIgnoreCase`, which folds one char to one char and
    /// leaves the non-1:1 mappings alone. `str::to_lowercase` (the full Unicode
    /// mapping) is a different comparer, and these are the pairs where they part.
    #[test]
    fn ordinal_ignore_case_matches_dotnet_not_full_case_mapping() {
        let key = super::ordinal_ignore_case_key;
        assert_eq!(key("Artist 01"), key("ARTIST 01"));
        // Greek final sigma: .NET folds \u{3c2}/\u{3c3} together, `to_lowercase` does not.
        assert_eq!(key("\u{3c2}"), key("\u{3c3}"));
        assert_ne!("\u{3c2}".to_lowercase(), "\u{3c3}".to_lowercase());
        // Turkish dotted capital I stays distinct from ASCII `i`, as in .NET.
        assert_ne!(key("\u{130}"), key("i"));
        // Sharp s does not expand: `ToUpperInvariant` cannot map it 1:1 to `SS`.
        assert_ne!(key("\u{df}"), key("ss"));
        // Frequency ordering itself is unaffected by the change.
        let rows = [Some("B|a"), Some("A"), Some("b")];
        assert_eq!(
            super::frequency_ordered_distinct(rows.iter().copied()),
            vec!["B".to_owned(), "a".to_owned()]
        );
    }

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

    /// .NET `GetCreationTime` on Unix: the birth time wins; without one the
    /// OLDER of ctime and mtime (not the mtime alone).
    #[rstest::rstest]
    #[case(Some(10), 20, 30, 10)]
    #[case(Some(40), 20, 30, 40)]
    #[case(None, 20, 30, 20)]
    #[case(None, 30, 20, 20)]
    #[case(None, 25, 25, 25)]
    fn creation_time_is_birth_time_or_oldest_of_ctime_mtime(
        #[case] birth: Option<u64>,
        #[case] ctime: u64,
        #[case] mtime: u64,
        #[case] expected: u64,
    ) {
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let times = super::FileTimes {
            birth: birth.map(at),
            ctime: at(ctime),
            mtime: at(mtime),
        };
        assert_eq!(super::creation_time_from(&times), at(expected));
    }

    /// On a real file the stat rule agrees with the filesystem: the statx
    /// birth time when there is one, else the older of ctime/mtime.
    #[test]
    fn file_times_read_the_birth_time_when_the_filesystem_has_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("movie.mkv");
        std::fs::write(&file, b"x").expect("write");
        let meta = std::fs::metadata(&file).expect("stat");
        let times = super::FileTimes::of(&meta);
        let expected = meta
            .created()
            .ok()
            .unwrap_or_else(|| times.ctime.min(times.mtime));
        assert_eq!(super::creation_time_from(&times), expected);
    }

    /// `ResolverHelper.SetDateCreated` + the `ManagedFileSystem` directory
    /// quirk: a FILE row carries its creation time and mtime; a FOLDER row is
    /// stamped with the resolve time and no `DateModified` (upstream only
    /// fills the dates for a `FileInfo`, so directories resolve with
    /// `MinValue` → `UtcNow`).
    #[tokio::test]
    async fn base_item_stamps_folders_with_now_and_files_with_file_times() {
        let dir = tempfile::tempdir().expect("tempdir");
        let series_dir = dir.path().join("Series 01");
        std::fs::create_dir(&series_dir).expect("mkdir");
        let file = series_dir.join("S01E01.mkv");
        std::fs::write(&file, b"x").expect("write");
        // Push the directory's own mtime into the past so "now" is
        // distinguishable from the directory's timestamps. On a filesystem
        // with birth times the just-created directory's btime is also "now",
        // so the `created > past` check only bites the old behaviour on
        // btime-less filesystems; the `date_modified == None` assertion is
        // the discriminating one everywhere.
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::open(&series_dir)
            .expect("open dir")
            .set_modified(past)
            .expect("set dir mtime");

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(dir.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence);
        let cf = uuid::Uuid::from_u128(0x7100);

        let before = chrono::Utc::now() - chrono::Duration::seconds(5);
        let (_, folder) = scanner
            .base_item(
                BaseItemKind::Series,
                cf,
                cf,
                "Series 01".into(),
                &series_dir.to_string_lossy(),
                true,
            )
            .expect("series row");
        let created = folder.date_created.expect("folders are stamped");
        assert!(
            created >= before,
            "a folder is stamped with the resolve time"
        );
        assert!(
            created > chrono::DateTime::<chrono::Utc>::from(past),
            "not the directory's own timestamp"
        );
        assert_eq!(folder.date_modified, None, "a folder has no DateModified");

        let (_, episode) = scanner
            .base_item(
                BaseItemKind::Episode,
                cf,
                cf,
                "S01E01".into(),
                &file.to_string_lossy(),
                false,
            )
            .expect("episode row");
        let meta = std::fs::metadata(&file).expect("stat");
        assert_eq!(
            episode.date_created,
            Some(chrono::DateTime::<chrono::Utc>::from(
                super::creation_time_from(&super::FileTimes::of(&meta))
            )),
            "a file's DateCreated is its creation time"
        );
        assert_eq!(
            episode.date_modified,
            Some(chrono::DateTime::<chrono::Utc>::from(
                meta.modified().expect("mtime")
            )),
            "a file's DateModified is its mtime"
        );

        // A path that cannot be stat'ed is stamped with the scan time —
        // upstream's `MinValue → UtcNow` guard.
        let (_, ghost) = scanner
            .base_item(
                BaseItemKind::Movie,
                cf,
                cf,
                "Ghost".into(),
                &dir.path().join("nope.mkv").to_string_lossy(),
                false,
            )
            .expect("ghost row");
        let created = ghost.date_created.expect("stamped");
        assert!((chrono::Utc::now() - created).num_seconds().abs() < 60);
        assert_eq!(ghost.date_modified, None);
    }

    /// A folder's first-resolve stamp survives a rescan: the scan upsert
    /// coalesces `DateCreated`, so the `now` of a later scan never replaces
    /// the `now` of the first (what makes the folder's "Date Added" stable).
    #[tokio::test]
    async fn rescan_preserves_folder_date_created() {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let id = uuid::Uuid::from_u128(0x7200);
        let first = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut series = BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Series)
                .unwrap()
                .to_owned(),
            name: Some("Series 01".into()),
            is_folder: true,
            date_created: Some(first),
            ..Default::default()
        };
        persistence
            .save_scanned_items(std::slice::from_ref(&series))
            .await
            .expect("first scan");
        series.date_created = Some(chrono::Utc::now());
        persistence
            .save_scanned_items(std::slice::from_ref(&series))
            .await
            .expect("rescan");
        let stored = crate::test_support::fetch_item(&db, id).await;
        assert_eq!(stored.date_created, Some(first));
    }

    /// An NFO `<dateadded>` overrides the resolver's stamp (`BaseNfoParser`
    /// sets `item.DateCreated`; `MergeData` copies a non-MinValue value).
    #[test]
    fn apply_nfo_applies_dateadded_as_date_created() {
        let resolver_stamp = chrono::Utc::now();
        let added = chrono::DateTime::parse_from_rfc3339("2020-05-06T07:08:09Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut entity = BaseItemEntity {
            date_created: Some(resolver_stamp),
            ..Default::default()
        };
        let nfo = ferrofin_providers::xbmc::item::NfoBaseItem {
            date_created: Some(added),
            ..Default::default()
        };
        super::apply_nfo(&mut entity, &nfo);
        assert_eq!(entity.date_created, Some(added));

        // No `<dateadded>` leaves the resolver's stamp alone.
        let mut entity = BaseItemEntity {
            date_created: Some(resolver_stamp),
            ..Default::default()
        };
        super::apply_nfo(
            &mut entity,
            &ferrofin_providers::xbmc::item::NfoBaseItem::default(),
        );
        assert_eq!(entity.date_created, Some(resolver_stamp));
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
        // OMDb's `Runtime` is never mapped, matching upstream.
        assert_eq!(e.run_time_ticks, None);
        assert_eq!(e.genres.as_deref(), Some("Action|Sci-Fi"));
    }

    // OMDb serves English data only, so C# skips the genres and the certificate
    // for a library set to any other metadata language.
    #[test]
    fn omdb_genres_join_the_rows_existing_ones() {
        // `ParseAdditionalMetadata` clears the genres on OMDb's OWN fresh
        // MetadataResult; `ExecuteRemoteProviders` then folds that result into
        // the accumulated one with `replaceData: false`, so OMDb's list is
        // fill-if-empty. Replacing here would discard an NFO's genres.
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let item: ferrofin_providers::OmdbItem =
            serde_json::from_str(r#"{"Genre":"Action, Sci-Fi","Response":"True"}"#).unwrap();
        let mut e = BaseItemEntity {
            genres: Some("Drama".to_owned()),
            ..BaseItemEntity::default()
        };
        super::apply_omdb(&mut e, &item, true, true);
        assert_eq!(e.genres.as_deref(), Some("Drama|Action|Sci-Fi"));

        // A row with none takes OMDb's outright.
        let mut blank = BaseItemEntity::default();
        super::apply_omdb(&mut blank, &item, true, true);
        assert_eq!(blank.genres.as_deref(), Some("Action|Sci-Fi"));

        // An OMDb record with no Genre never touches what is there.
        let bare: ferrofin_providers::OmdbItem =
            serde_json::from_str(r#"{"Response":"True"}"#).unwrap();
        let mut kept = BaseItemEntity {
            genres: Some("Drama".to_owned()),
            ..BaseItemEntity::default()
        };
        super::apply_omdb(&mut kept, &bare, true, true);
        assert_eq!(kept.genres.as_deref(), Some("Drama"));
    }

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

    // Every scan re-runs the credits fetch, so before the `ImageNeedsRefresh`
    // gate reached the cast, every scan also re-decoded and re-BlurHash-encoded
    // every credited person's profile photo. A 2-movie live library already
    // pulls 24 person images; a real one pulls thousands.
    #[tokio::test(flavor = "multi_thread")]
    async fn enrich_people_reuses_stored_person_image_metadata_on_a_rescan() {
        use crate::item_persistence_service::FerrofinItemPersistenceService;
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
        use crate::people_repository::FerrofinPeopleRepository;
        use crate::test_support::test_db;
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, WrittenPerson};
        use std::sync::atomic::Ordering;

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
        // Pre-place the profile art where `download_images` looks, so the test
        // never reaches the network.
        let art_dir = meta_root.join(person_id.to_string());
        std::fs::create_dir_all(&art_dir).unwrap();
        write_poster(&art_dir.join("primary.jpg"), 48, 64);

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let dimensions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blur_hashes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> =
            Arc::new(CountingProcessor {
                inner: Arc::new(ImageProcessor::new(
                    Arc::new(ImageCrateEncoder::new()),
                    tmp.path().join("cache"),
                )),
                dimensions: Arc::clone(&dimensions),
                blur_hashes: Arc::clone(&blur_hashes),
            });
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_image_processor(processor)
                .with_metadata(
                    Arc::new(ferrofin_providers::TmdbClient::new()),
                    meta_root.clone(),
                );

        let people = FerrofinPeopleRepository::new(db.clone());
        let credit = || WrittenPerson {
            id: person_id,
            // No biography lookup: that branch is the only one that would
            // reach the network.
            needs_details: false,
            image_url: Some("https://image.tmdb.invalid/ada.jpg".into()),
            provider_id: None,
        };

        scanner.enrich_people(&people, vec![credit()]).await;
        let first = items.get_image_infos(person_id).await.expect("images");
        assert_eq!((first[0].width, first[0].height), (48, 64));
        assert!(dimensions.swap(0, Ordering::Relaxed) >= 1);
        assert!(blur_hashes.swap(0, Ordering::Relaxed) >= 1);

        scanner.enrich_people(&people, vec![credit()]).await;
        assert_eq!(
            (
                dimensions.load(Ordering::Relaxed),
                blur_hashes.load(Ordering::Relaxed)
            ),
            (0, 0),
            "an unchanged profile photo must not be decoded again"
        );
        let second = items.get_image_infos(person_id).await.expect("images");
        assert_eq!(
            (
                second[0].width,
                second[0].height,
                second[0].blur_hash.clone()
            ),
            (first[0].width, first[0].height, first[0].blur_hash.clone()),
            "the reused row is identical to the probed one"
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
    async fn an_album_inherits_genres_studios_and_premiere_date_from_its_tracks() {
        // C# `AlbumMetadataService`: `EnableUpdatingGenresFromChildren`,
        // `EnableUpdatingStudiosFromChildren` and
        // `EnableUpdatingPremiereDateFromChildren` are all `true` over
        // `GetRecursiveChildren(i => i is Audio)`, and
        // `MetadataService.UpdateGenres`/`UpdateStudios` take the
        // case-insensitive distinct union while `UpdatePremiereDate` takes the
        // minimum child date and re-derives the year from it.
        //
        // Ferrofin aggregated only the album artist and the year, so every
        // scanned album had `Genres: []` — which made an album unreachable by
        // the `Genres=` query behind `/Albums/{id}/Similar`.
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
        let track_ids = [
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        ];
        let stored = |k| stored_type_name(k).unwrap().to_owned();
        let track = |id: uuid::Uuid, name: &str, genres: &str, month: u32| BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: stored(BaseItemKind::Audio),
            name: Some(name.to_owned()),
            parent_id: Some(ferrofin_db::store::guid_to_db(album_id)),
            genres: Some(genres.to_owned()),
            studios: Some("Blue Note".to_owned()),
            premiere_date: chrono::DateTime::parse_from_rfc3339(&format!(
                "2020-{month:02}-01T00:00:00Z"
            ))
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc)),
            production_year: Some(2020),
            ..Default::default()
        };
        persistence
            .save_items(&[
                BaseItemEntity {
                    id: ferrofin_db::store::guid_to_db(album_id),
                    type_: stored(BaseItemKind::MusicAlbum),
                    name: Some("Kind of Blue".into()),
                    ..Default::default()
                },
                // Two tracks share a genre with different casing (C# dedupes
                // `OrdinalIgnoreCase`), a third adds a second genre.
                track(track_ids[0], "So What", "Jazz", 6),
                track(track_ids[1], "Blue in Green", "jazz", 3),
                track(track_ids[2], "Flamenco Sketches", "Jazz|Modal", 9),
            ])
            .await
            .expect("seed");
        // The scan records the album as each track's ancestor; the aggregation
        // reads the tracks through the recursive (`GetRecursiveChildren`) query,
        // so a multi-disc album still aggregates.
        for id in track_ids {
            persistence
                .set_ancestors(id, &[album_id])
                .await
                .expect("ancestors");
        }

        let tmp = tempfile::tempdir().unwrap();
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_music(
                Arc::new(ferrofin_providers::MusicBrainzClient::new("", "test")),
                Arc::clone(&items),
            )
            .enrich_music(&std::collections::HashMap::new())
            .await
            .expect("enrich");

        let album = items.retrieve_item(album_id).await.unwrap().unwrap();
        // Deduped case-insensitively (`Jazz` and `jazz` are one genre), with
        // whichever casing the first child carried — as C#'s
        // `Distinct(StringComparer.OrdinalIgnoreCase)` keeps it.
        let mut genres: Vec<String> = album
            .genres
            .as_deref()
            .unwrap_or_default()
            .split('|')
            .map(str::to_lowercase)
            .collect();
        genres.sort();
        assert_eq!(genres, vec!["jazz".to_owned(), "modal".to_owned()]);
        assert_eq!(album.studios.as_deref(), Some("Blue Note"));
        // The EARLIEST track date, and the year re-derived from it.
        assert_eq!(
            album.premiere_date.map(|d| d.to_rfc3339()),
            Some("2020-03-01T00:00:00+00:00".to_owned())
        );
        assert_eq!(album.production_year, Some(2020));

        // The genres must also reach `ItemValues`, or the `Genres=` query
        // behind `/Albums/{id}/Similar` still cannot find the album — the row
        // column alone is not what that query reads.
        let by_genre = items
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicAlbum],
                genres: vec!["Modal".to_owned()],
                ..Default::default()
            })
            .await
            .expect("genre query");
        assert_eq!(
            by_genre.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![ferrofin_db::store::guid_to_db(album_id)]
        );
    }

    /// `Audio.CreateSortName` (v10.11.8
    /// `MediaBrowser.Controller/Entities/Audio/Audio.cs`):
    /// `{ParentIndexNumber:0000 - }{IndexNumber:0000 - }Name`, with the RAW
    /// name — `Audio` overrides `CreateSortName`, so the alphanumeric pipeline
    /// never runs on a track.
    #[rstest::rstest]
    #[case(
        None,
        Some(1),
        "Artist 01 Album 01 Track 01",
        "0001 - Artist 01 Album 01 Track 01"
    )]
    #[case(Some(2), Some(13), "Blue in Green", "0002 - 0013 - Blue in Green")]
    #[case(None, None, "So What", "So What")]
    #[case(Some(1), None, "So What", "0001 - So What")]
    fn audio_sort_name_matches_the_csharp_format(
        #[case] disc: Option<i64>,
        #[case] track: Option<i64>,
        #[case] name: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(super::audio_sort_name(disc, track, name), expected);
    }

    /// `derived_sort_name` routes `Audio`/`AudioBook` to that override and
    /// leaves every other kind on the alphanumeric pipeline.
    #[test]
    fn only_the_audio_kinds_take_the_audio_sort_override() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let row = |type_: &str| BaseItemEntity {
            type_: type_.to_owned(),
            index_number: Some(1),
            ..Default::default()
        };
        assert_eq!(
            super::derived_sort_name(
                &row("MediaBrowser.Controller.Entities.Audio.Audio"),
                "Track 01"
            ),
            "0001 - Track 01"
        );
        assert_eq!(
            super::derived_sort_name(
                &row("MediaBrowser.Controller.Entities.Movies.Movie"),
                "The A"
            ),
            ferrofin_util::sort_name::create_sort_name("The A")
        );
    }

    /// `AlbumMetadataService.SetArtistsFromSongs` / `SetAlbumArtistFromSongs`
    /// and the base `UpdateCumulativeRunTimeTicks`, on the shape the parity
    /// fixture has: three tracks by one artist, 2 s each.
    #[test]
    fn album_aggregates_artists_and_cumulative_runtime_from_tracks() {
        use super::apply_album_child_metadata;
        use ferrofin_db::entities::base_items::BaseItemEntity;

        let track = |artists: &str, ticks: i64| BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
            artists: Some(artists.to_owned()),
            album_artists: Some("Artist 03".to_owned()),
            run_time_ticks: Some(ticks),
            ..Default::default()
        };
        let album = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
            name: Some("Album 01".to_owned()),
            is_folder: true,
            ..Default::default()
        };

        let tracks = [
            track("Artist 03", 20_000_000),
            track("Artist 03", 20_000_000),
            track("Artist 03", 20_000_000),
        ];
        let (updated, changed) = apply_album_child_metadata(&album, &tracks);
        assert!(changed);
        assert_eq!(updated.artists.as_deref(), Some("Artist 03"));
        assert_eq!(updated.album_artists.as_deref(), Some("Artist 03"));
        assert_eq!(updated.run_time_ticks, Some(60_000_000));

        // Zero-length tracks still assign 0 — Jellyfin emits `RunTimeTicks: 0`,
        // it never omits the field.
        let silent = [track("Artist 03", 0)];
        let (updated, _) = apply_album_child_metadata(&album, &silent);
        assert_eq!(updated.run_time_ticks, Some(0));
    }

    /// `GroupBy(...).OrderByDescending(g => g.Count())`: the most-credited
    /// artist leads, ties keep first-appearance order, and the casing is the
    /// first-seen one.
    #[test]
    fn album_artists_are_frequency_ordered() {
        use super::apply_album_child_metadata;
        use ferrofin_db::entities::base_items::BaseItemEntity;

        let track = |artists: &str| BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
            artists: Some(artists.to_owned()),
            ..Default::default()
        };
        let album = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
            is_folder: true,
            ..Default::default()
        };
        let tracks = [track("A|B"), track("b"), track("B")];
        let (updated, _) = apply_album_child_metadata(&album, &tracks);
        assert_eq!(updated.artists.as_deref(), Some("B|A"));
    }

    /// The C# order is: base `UpdateMetadataFromChildren` (cumulative runtime)
    /// FIRST, then `if (item.IsLocked) return;`. So a locked album keeps its
    /// user-owned artist columns but still gets its runtime updated.
    #[test]
    fn locked_album_keeps_artists_but_still_gets_runtime() {
        use super::apply_album_child_metadata;
        use ferrofin_db::entities::base_items::BaseItemEntity;

        let album = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
            is_folder: true,
            is_locked: true,
            artists: Some("Hand Edited".to_owned()),
            ..Default::default()
        };
        let tracks = [BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
            artists: Some("Artist 03".to_owned()),
            run_time_ticks: Some(20_000_000),
            ..Default::default()
        }];
        let (updated, changed) = apply_album_child_metadata(&album, &tracks);
        assert!(changed);
        assert_eq!(updated.artists.as_deref(), Some("Hand Edited"));
        assert_eq!(updated.run_time_ticks, Some(20_000_000));
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
        persistence
            .set_ancestors(track_id, &[album_id])
            .await
            .expect("ancestors");
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
            .set_ancestors(track_id, &[album_id])
            .await
            .expect("ancestors");
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
        let people = scanner
            .episode_people(Some(1399), 1, 1, &ep)
            .await
            .expect("TVDB credits are authoritative");
        let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Guest Star", "Ep Director"]);
        assert_eq!(people[0].person_type.as_deref(), Some("GuestStar"));

        // Nor does a series without a TMDB id reach for TMDB.
        let people = scanner
            .episode_people(None, 1, 1, &ep)
            .await
            .expect("TVDB credits are authoritative");
        assert_eq!(people.len(), 2);

        // No TMDB client is wired here, so TMDB was never ATTEMPTED and TVDB is
        // the only source: its empty record is authoritative and does clear a
        // stale cast. (A request that was attempted and FAILED is the case that
        // must not clear — covered by
        // `a_failed_credits_request_does_not_wipe_a_stored_cast`.)
        let bare = ferrofin_providers::TvdbEpisodeDetails::default();
        assert_eq!(
            scanner.episode_people(Some(1399), 1, 1, &bare).await,
            Some(Vec::new()),
            "a TVDB-only server must still be able to correct a stale cast"
        );
    }

    // ---------------------------------------------------------------------
    // TMDB episode provider
    // ---------------------------------------------------------------------

    /// A minimal TMDB stand-in serving `/tv/{id}/season/{n}`, counting every
    /// request it receives. Returns `(base_url, hit_counter)`.
    ///
    /// The counter is the point: the season response carries the episode text
    /// AND the artwork, so both scan passes must share one request per season.
    /// A regression that re-fetches per episode still produces correct data —
    /// only the count catches it.
    fn spawn_tmdb_server(
        body: Option<&'static str>,
        credits: Option<&'static str>,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{Read as _, Write as _};
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Every request, not just season fetches: a series lookup never asks
        // for a season, so a season-only counter cannot prove TMDB stayed out.
        let all = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::clone(&hits);
        let requests = Arc::clone(&all);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                // `/credits` first: its path contains `/season/` too. A `None`
                // body is a MISS — a real non-2xx, not an empty 200, so the
                // caller's failure branch is the one under test.
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (status, payload) = if req.contains("/credits") {
                    credits.map_or(("500 Internal Server Error", "{}"), |b| ("200 OK", b))
                } else if req.contains("/season/") {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    body.map_or(("404 Not Found", "{}"), |b| ("200 OK", b))
                } else if req.contains("/search/tv") {
                    ("200 OK", SERIES_SEARCH_JSON)
                } else if req.contains("/tv/") {
                    // `/tv/{id}` — a series detail fetch. Answering it (rather
                    // than 404ing) is what lets a test prove the TVDB chain
                    // stopped: if it did not, THIS overview wins.
                    ("200 OK", SERIES_DETAILS_JSON)
                } else {
                    ("404 Not Found", "{}")
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        (format!("http://{addr}"), hits, all)
    }

    /// The series search that gives the scan a TMDB id to hang seasons off.
    const SERIES_SEARCH_JSON: &str = r#"{"results": [{"id": 1399, "name": "GoT"}]}"#;

    /// `/tv/{id}` — deliberately a DIFFERENT overview from the TVDB fake's, so
    /// a test can tell which provider wrote the row.
    const SERIES_DETAILS_JSON: &str = r#"{"overview": "From TMDB.", "genres": []}"#;

    /// One episode's credits: two billed regulars, a guest star, a director,
    /// and a job Jellyfin does not map to a person type.
    const CREDITS_JSON: &str = r#"{
        "cast": [
            {"id": 1, "name": "Sean Bean", "character": "Ned Stark"},
            {"id": 2, "name": "Maisie Williams", "character": "Arya Stark"}
        ],
        "guest_stars": [
            {"id": 3, "name": "Jason Momoa", "character": "Khal Drogo"}
        ],
        "crew": [
            {"id": 4, "name": "Tim Van Patten", "job": "Director"},
            {"id": 5, "name": "A Gaffer", "job": "Gaffer"}
        ]
    }"#;

    const SEASON_JSON: &str = r#"{
        "name": "Season 1",
        "overview": "The first season.",
        "poster_path": "/poster.jpg",
        "episodes": [
            {"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
             "overview": "Ned is summoned south.", "still_path": "/e1.jpg",
             "air_date": "2011-04-17", "vote_average": 8.3},
            {"id": 63057, "episode_number": 2, "name": "The Kingsroad",
             "overview": "The party rides north.", "still_path": "/e2.jpg",
             "air_date": "2011-04-24", "vote_average": 0.0}
        ]
    }"#;

    /// Builds a scanner wired to a fake TMDB, plus the episode row the scan
    /// would have planned (name = the file stem, no overview).
    async fn tmdb_episode_fixture(
        base_url: &str,
        tmp: &std::path::Path,
    ) -> (
        LibraryScanner,
        super::ArtworkCache,
        ferrofin_db::entities::base_items::BaseItemEntity,
    ) {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.join("default"))
                .with_item_store(persistence.clone()),
        );
        let tmdb = Arc::new(ferrofin_providers::TmdbClient::new().with_base_url(base_url));
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_metadata(tmdb, tmp.join("metadata"));

        let mut cache = super::ArtworkCache::default();
        cache.series_tmdb.insert("SERIES".to_owned(), 1399);

        let episode = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
            name: Some("GoT.S01E01.1080p.Bluray".into()),
            path: Some("/tv/GoT/Season 1/GoT.S01E01.1080p.Bluray.mkv".into()),
            series_id: Some("SERIES".into()),
            parent_index_number: Some(1),
            index_number: Some(1),
            ..Default::default()
        };
        (scanner, cache, episode)
    }

    // The regression this whole change exists for: on a library migrated from
    // Jellyfin, TheMovieDb is the only metadata fetcher saved for `Episode`, so
    // TMDB must title the episode itself. Before the fix TMDB had no Episode
    // branch at all and every episode kept its filename forever.
    #[tokio::test]
    async fn tmdb_episode_title_replaces_the_filename_placeholder() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;

        let result = scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("applied");
        assert_eq!(episode.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(episode.overview.as_deref(), Some("Ned is summoned south."));
        // An episode sorts by position, never alphabetically by the new title,
        // or the client's play queue scrambles.
        assert_eq!(
            episode.sort_name.as_deref(),
            Some("001 - 0001 - Winter Is Coming")
        );
        // The same response carries the air date and rating. With TheTVDB
        // unticked for Episode — the shape that caused this bug — nothing else
        // fills them, and the client shows a dateless, unrated episode.
        assert_eq!(
            episode
                .premiere_date
                .map(|d| d.format("%Y-%m-%d").to_string()),
            Some("2011-04-17".to_owned())
        );
        assert_eq!(episode.production_year, Some(2011));
        assert!((episode.community_rating.expect("rating") - 8.3).abs() < 1e-6);
        // Upstream persists the episode's own Tmdb id.
        assert_eq!(
            result.provider_ids,
            vec![("Tmdb".to_owned(), "63056".to_owned())]
        );
    }

    // TMDB reports 0 for "nobody has rated this yet". Storing it as a rating of
    // zero would show an episode rated 0/10 rather than unrated, and — because
    // every fill here is fill-if-empty — permanently, since a later real rating
    // could never replace it.
    #[tokio::test]
    async fn a_zero_tmdb_vote_is_unrated_not_a_rating_of_zero() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        episode.name = Some("GoT.S01E02.1080p.Bluray".into());
        episode.path = Some("/tv/GoT/Season 1/GoT.S01E02.1080p.Bluray.mkv".into());
        episode.index_number = Some(2);

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("applied");
        assert_eq!(episode.name.as_deref(), Some("The Kingsroad"));
        assert_eq!(episode.community_rating, None);
        // The air date still lands — only the rating was absent.
        assert_eq!(episode.production_year, Some(2011));
    }

    // A library can rank TheTVDB first for `Series` while leaving `Episode` to
    // TMDB. The series then resolves through TVDB and never populates
    // `series_tmdb`, so without the fallback to the Tmdb id TVDB carries, every
    // episode beneath it silently gets nothing.
    #[tokio::test]
    async fn an_episode_resolves_through_the_tmdb_id_tvdb_carries() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        // Only TVDB matched the series.
        cache.series_tmdb.clear();
        cache.series_tvdb.insert(
            "SERIES".to_owned(),
            ferrofin_providers::TvdbSeriesDetails {
                tvdb_id: 121_361,
                tmdb_id: Some("1399".to_owned()),
                ..Default::default()
            },
        );

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("resolves through TVDB's Tmdb id");
        assert_eq!(episode.name.as_deref(), Some("Winter Is Coming"));
    }

    // The episode page exists to show who was in THAT episode. Its credits are
    // the regulars billed in it, then its guest stars, then its crew — never
    // the series' regular cast, which is what every episode page listed before
    // ca2f22c and again after 659b62e gated the only branch that fetched them.
    #[tokio::test]
    async fn tmdb_episode_credits_are_the_episodes_own() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;

        let result = scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("applied");

        let names: Vec<&str> = result.people.iter().map(|p| p.name.as_str()).collect();
        // Billing order, then the guest star, then the crew. The unmapped
        // "Gaffer" job is dropped, as upstream drops it.
        assert_eq!(
            names,
            vec![
                "Sean Bean",
                "Maisie Williams",
                "Jason Momoa",
                "Tim Van Patten"
            ]
        );
        assert_eq!(result.people[0].person_type.as_deref(), Some("Actor"));
        assert_eq!(result.people[0].role.as_deref(), Some("Ned Stark"));
        assert_eq!(result.people[2].person_type.as_deref(), Some("GuestStar"));
        assert_eq!(result.people[3].person_type.as_deref(), Some("Director"));
    }

    // A series with no TMDB id yields "not fetched" rather than reaching for
    // the network with a missing id — and "not fetched" is what stops the
    // caller from clearing the episode's stored cast.
    #[tokio::test]
    async fn tmdb_episode_people_need_a_series_id() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, _cache, _ep) = tmdb_episode_fixture(&base, tmp.path()).await;

        assert!(matches!(
            scanner.tmdb_episode_people(None, 1, 1).await,
            super::TmdbCredits::NotAttempted
        ));
        assert_eq!(
            scanner
                .tmdb_episode_people(Some(1399), 1, 1)
                .await
                .fetched()
                .expect("credits fetched")
                .len(),
            4
        );
    }

    // A failed credits request is `None` — distinct from an episode TMDB
    // credits nobody on, which is `Some(vec![])`. Conflating them is what lets
    // one 429 delete a cast.
    #[tokio::test]
    async fn a_failed_credits_request_is_not_an_empty_cast() {
        let tmp = tempfile::tempdir().unwrap();

        let (failing, _, _) = spawn_tmdb_server(Some(SEASON_JSON), None);
        let (scanner, _cache, _ep) = tmdb_episode_fixture(&failing, tmp.path()).await;
        assert!(
            matches!(
                scanner.tmdb_episode_people(Some(1399), 1, 1).await,
                super::TmdbCredits::Failed
            ),
            "a 500 is not an authoritative empty cast"
        );

        let (empty, _, _) = spawn_tmdb_server(Some(SEASON_JSON), Some(r#"{"cast":[]}"#));
        let (scanner, _cache, _ep) = tmdb_episode_fixture(&empty, tmp.path()).await;
        assert_eq!(
            scanner
                .tmdb_episode_people(Some(1399), 1, 1)
                .await
                .fetched(),
            Some(Vec::new()),
            "an answered request with no credits IS authoritative"
        );
    }

    // An NFO <title> outranks the provider: `apply_nfo` ran first and moved the
    // name off the file stem, which is exactly what the placeholder check reads.
    // The overview is still filled, because it was empty.
    #[tokio::test]
    async fn tmdb_episode_keeps_an_nfo_title_but_fills_the_overview() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        episode.name = Some("A Title From The NFO".into());

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("applied");
        assert_eq!(episode.name.as_deref(), Some("A Title From The NFO"));
        assert_eq!(episode.overview.as_deref(), Some("Ned is summoned south."));
    }

    // `/tv/{id}/season/{n}` carries the episode text AND the season poster AND
    // every episode still. One request must serve the whole season across both
    // the metadata and the image pass — otherwise a 10k-episode library pays
    // 10k requests for data it already had.
    #[tokio::test]
    async fn season_details_are_fetched_once_per_season() {
        let (base, hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut ep1) = tmdb_episode_fixture(&base, tmp.path()).await;

        let mut ep2 = ep1.clone();
        ep2.name = Some("GoT.S01E02.1080p.Bluray".into());
        ep2.path = Some("/tv/GoT/Season 1/GoT.S01E02.1080p.Bluray.mkv".into());
        ep2.index_number = Some(2);

        scanner.fetch_tmdb_episode(&mut ep1, &mut cache, None).await;
        scanner.fetch_tmdb_episode(&mut ep2, &mut cache, None).await;
        // The image pass asks for the same season.
        let poster = scanner
            .season_details_cached(&mut cache, "SERIES", 1)
            .await
            .and_then(|d| d.poster.clone());

        assert_eq!(ep1.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(ep2.name.as_deref(), Some("The Kingsroad"));
        assert!(poster.is_some());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // Re-scan gate. It has to read the STORED row: the planned entity is
    // rebuilt from the filesystem every scan, so its name is always the file
    // stem and gating on it would never fire — every episode in the library
    // would re-request its credits on every nightly scan.
    //
    // Skipping must also carry the stored title forward, because the scan
    // upsert writes the entity's values: returning early without them would
    // overwrite a good title with the placeholder, so one scan during a TMDB
    // outage would revert the whole library to filenames.
    #[tokio::test]
    async fn a_previously_titled_episode_is_neither_re_requested_nor_reverted() {
        let (base, hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;

        // What an earlier scan achieved. `episode` is the freshly-planned row,
        // still carrying the file stem and no overview.
        let stored = super::StoredText::from_row(ferrofin_db::entities::base_items::ItemTextRow {
            id: String::new(),
            name: Some("Winter Is Coming".into()),
            sort_name: Some("001 - 0001 - Winter Is Coming".into()),
            overview: Some("Ned is summoned south.".into()),
            path: episode.path.clone(),
        });

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, Some(&stored))
            .await
            .expect("nothing to do");

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(episode.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(episode.overview.as_deref(), Some("Ned is summoned south."));
        assert_eq!(
            episode.sort_name.as_deref(),
            Some("001 - 0001 - Winter Is Coming")
        );
    }

    // A key an older algorithm produced must not survive the rescan: the row
    // carries the stored TITLE forward and derives the key from it again, so a
    // library never ends up half-sorted by two different rules.
    #[tokio::test]
    async fn a_carried_forward_title_re_derives_its_sort_key() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;

        // A stale key: what the pre-convergence scanner wrote for this title.
        let stored = super::StoredText::from_row(ferrofin_db::entities::base_items::ItemTextRow {
            id: String::new(),
            name: Some("Winter Is Coming".into()),
            sort_name: Some("winter is coming".into()),
            overview: Some("Ned is summoned south.".into()),
            path: episode.path.clone(),
        });

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, Some(&stored))
            .await
            .expect("nothing to do");

        assert_eq!(episode.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(
            episode.sort_name.as_deref(),
            Some("001 - 0001 - Winter Is Coming"),
            "the key is re-derived from the carried-forward title, not copied"
        );
    }

    // `BaseItem.SortName` prefers a `ForcedSortName` (`<sorttitle>`); the
    // re-derivation above must not overwrite one.
    #[tokio::test]
    async fn a_forced_sort_name_survives_the_re_derivation() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        episode.forced_sort_name = Some("Thrones 01".into());

        let stored = super::StoredText::from_row(ferrofin_db::entities::base_items::ItemTextRow {
            id: String::new(),
            name: Some("Winter Is Coming".into()),
            sort_name: Some("thrones 0000000001".into()),
            overview: Some("Ned is summoned south.".into()),
            path: episode.path.clone(),
        });

        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, Some(&stored))
            .await
            .expect("nothing to do");

        assert_eq!(
            episode.sort_name.as_deref(),
            Some("thrones 0000000001"),
            "ModifySortChunks(ForcedSortName).ToLowerInvariant()"
        );
    }

    // A season TMDB has nothing for is recorded as a miss, so a 24-episode
    // season costs ONE failed request rather than 24. Without the negative
    // caching every episode under an unmatched season re-asks.
    #[tokio::test]
    async fn a_season_miss_is_cached_and_not_re_requested() {
        // `/season/` answers 404 — a real miss, not an empty 200.
        let (base, hits, _all) = spawn_tmdb_server(None, Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut ep1) = tmdb_episode_fixture(&base, tmp.path()).await;

        assert!(
            scanner
                .season_details_cached(&mut cache, "SERIES", 1)
                .await
                .is_none(),
            "a non-2xx season response is a miss"
        );
        // The miss is remembered, so neither a second lookup nor an episode
        // fetch spends another request.
        assert!(
            scanner
                .season_details_cached(&mut cache, "SERIES", 1)
                .await
                .is_none()
        );
        assert!(
            scanner
                .fetch_tmdb_episode(&mut ep1, &mut cache, None)
                .await
                .is_none()
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The row keeps its placeholder for another fetcher to fill.
        assert_eq!(ep1.name.as_deref(), Some("GoT.S01E01.1080p.Bluray"));
    }

    // A series that needs no metadata of its own returns before the search
    // runs — and used to return without publishing its TMDB id, leaving every
    // episode beneath it with no provider. That is the re-scan case: the shape
    // a library is in on the second and every later scan, and the one a
    // first-scan test cannot reach.
    #[tokio::test]
    async fn an_already_enriched_series_still_publishes_its_tmdb_id() {
        use ferrofin_db::entities::base_items::BaseItemEntity;

        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        // Nothing pre-seeded: the series must publish its own id.
        cache.series_tmdb.clear();

        // Overview set AND trailers already stored → every `wants_*` is false,
        // so `fetch_tmdb_metadata` takes the short-circuit.
        let mut series = BaseItemEntity {
            id: "SERIES".into(),
            type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
            name: Some("GoT".into()),
            overview: Some("A series that was enriched on an earlier scan.".into()),
            data: Some(r#"{"RemoteTrailers":[{"Url":"https://y/t","Name":"Trailer"}]}"#.into()),
            ..Default::default()
        };
        let result = scanner
            .fetch_tmdb_metadata(&mut series, "Series", false, &mut cache, None, &[])
            .await;

        let result = result.expect("the series needed no fetch");
        assert!(
            result.people.is_empty() && !result.people_fetched,
            "the short-circuit fetches no credits, so it must not claim to have"
        );
        assert_eq!(
            series.overview.as_deref(),
            Some("A series that was enriched on an earlier scan."),
            "the short-circuit must not rewrite the row"
        );
        assert_eq!(
            cache.series_tmdb.get("SERIES").copied(),
            Some(1399),
            "the id its episodes resolve through must be published anyway"
        );

        // And the episodes below it now resolve.
        scanner
            .fetch_tmdb_episode(&mut episode, &mut cache, None)
            .await
            .expect("episode resolves through the published id");
        assert_eq!(episode.name.as_deref(), Some("Winter Is Coming"));
    }

    // A series with no cached TMDB id makes NO request and records NO miss —
    // the id may still arrive (the series row is scanned before its episodes,
    // but a gated-off Series fetcher leaves the cache empty).
    #[tokio::test]
    async fn an_unmatched_series_asks_tmdb_nothing() {
        let (base, hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        cache.series_tmdb.clear();

        assert!(
            scanner
                .fetch_tmdb_episode(&mut episode, &mut cache, None)
                .await
                .is_none()
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(cache.season_details.is_empty());
        // Untouched: the row keeps its placeholder for a later fetcher.
        assert_eq!(episode.name.as_deref(), Some("GoT.S01E01.1080p.Bluray"));
    }

    /// A TVDB stand-in: `/login` issues a token, `/search` answers from
    /// `search`, and `/series/…`/`/episodes/…` from `details`. A `None` body
    /// 404s, which is how a miss is expressed.
    ///
    /// Exists because every TVDB **hit** path was unreachable from these tests
    /// while `TvdbClient::with_base_url` was crate-private — the "a TVDB hit is
    /// authoritative, do not also run TMDB" branches could be deleted with the
    /// whole suite green.
    fn spawn_tvdb_server(search: Option<&'static str>, details: Option<&'static str>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (status, payload) = if req.contains("/login") {
                    ("200 OK", r#"{"data":{"token":"tok"}}"#)
                } else if req.contains("/search") {
                    search.map_or(("404 Not Found", "{}"), |b| ("200 OK", b))
                } else if req.contains("/series/") || req.contains("/episodes/") {
                    details.map_or(("404 Not Found", "{}"), |b| ("200 OK", b))
                } else {
                    ("404 Not Found", "{}")
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        format!("http://{addr}")
    }

    // A TVDB series hit is authoritative: TMDB must not also run and overwrite
    // it. Nothing covered this branch — a full TVDB hit could be made to report
    // a miss and every test still passed.
    #[tokio::test]
    async fn a_tvdb_series_hit_stops_the_chain() {
        use ferrofin_db::entities::base_items::BaseItemEntity;

        // `/search` → Envelope<Vec<SearchItem>>; `/series/{id}/extended` →
        // Envelope<SeriesExtendedWire>. One fake serves both; the search item
        // carries the id the details call is then made with.
        const SEARCH: &str = r#"{"data":[{"tvdb_id":"121361","name":"GoT","year":"2011"}]}"#;
        // No overview: `apply_tvdb_series` fills only what is empty, so if the
        // chain fails to stop, TMDB's fill-if-empty genuinely writes
        // "From TMDB." here. With an overview already set the assertion below
        // could not fail for the reason it claims.
        const DETAILS: &str = r#"{"data":{"id":121361,"name":"GoT"}}"#;
        let the_tvdb = spawn_tvdb_server(Some(SEARCH), Some(DETAILS));
        // TMDB answers a series fetch too, with a different overview — so if
        // the chain fails to stop, the row ends up saying "From TMDB."
        let (the_moviedb, _season_hits, tmdb_requests) =
            spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));

        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, _ep) = tmdb_episode_fixture(&the_moviedb, tmp.path()).await;
        let scanner = scanner.with_tvdb(Arc::new(
            ferrofin_providers::TvdbClient::new().with_base_url(&the_tvdb),
        ));

        let mut series = BaseItemEntity {
            id: "SERIES".into(),
            type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
            name: Some("GoT".into()),
            ..Default::default()
        };
        let result = scanner
            .fetch_remote_metadata(
                &mut series,
                &mut cache,
                super::FetcherPolicy::default(),
                None,
                &[],
            )
            .await;

        assert!(
            cache.series_tvdb.contains_key("SERIES"),
            "the hit must be cached — seasons and episodes key off it"
        );
        assert!(
            result
                .provider_ids
                .iter()
                .any(|(k, v)| k == "Tvdb" && v == "121361"),
            "the TVDB id must be persisted: {:?}",
            result.provider_ids
        );
        assert_eq!(
            series.overview, None,
            "TMDB must not have filled the overview the TVDB hit left empty"
        );
        assert_eq!(
            tmdb_requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "TMDB must not be asked anything after a TVDB hit"
        );
    }

    // ---------------------------------------------------------------------
    // Stale credits
    // ---------------------------------------------------------------------

    /// A one-episode TV library, scanned once with no metadata provider wired,
    /// then given a stale cast — the state a library is left in by a scan that
    /// wrote the series' regulars onto every episode.
    ///
    /// Returns everything a second scan needs, plus the episode's item id.
    async fn library_with_a_stale_episode_cast(
        tmp: &std::path::Path,
    ) -> (
        Arc<dyn VirtualFolderManager>,
        Arc<FerrofinItemPersistenceService>,
        Arc<crate::people_repository::FerrofinPeopleRepository>,
        uuid::Uuid,
    ) {
        use ferrofin_db::entities::base_items::PeopleEntity;
        use ferrofin_traits::persistence::PeopleRepository as _;

        let tv = tmp.join("tv");
        std::fs::create_dir_all(tv.join("GoT/Season 01")).unwrap();
        std::fs::write(tv.join("GoT/Season 01/GoT S01E01.mkv"), b"").unwrap();

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.join("default"))
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

        let people = Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
            db.clone(),
        ));
        // No TMDB wired: the episode row lands with its file-stem name and no
        // overview, exactly as a library scanned before the provider existed.
        LibraryScanner::new(
            vf.clone(),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_people(people.clone() as Arc<dyn ferrofin_traits::persistence::PeopleRepository>)
        .scan(None)
        .await
        .unwrap();

        // The scan derives an item's id from its kind + path, so reproduce it
        // rather than querying for it. The scanner was built with the default
        // `IdDerivation::LegacyLowercase`, which is what `derive_item_id` uses.
        let episode_id = crate::item_type_lookup::derive_item_id(
            ferrofin_model::data::BaseItemKind::Episode,
            &tv.join("GoT/Season 01/GoT S01E01.mkv").to_string_lossy(),
        )
        .expect("episode id");

        // The stale rows: the series' regular cast, stamped on the episode.
        people
            .update_people(
                episode_id,
                &[
                    PeopleEntity {
                        id: String::new(),
                        name: "A Series Regular".into(),
                        person_type: Some("Actor".into()),
                        ..Default::default()
                    },
                    PeopleEntity {
                        id: String::new(),
                        name: "Another Series Regular".into(),
                        person_type: Some("Actor".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(episode_credit_count(&people, episode_id).await, 2);

        (vf, persistence, people, episode_id)
    }

    /// The credits stored against an item, read back through the same
    /// repository the scan writes them with.
    async fn episode_credit_count(
        people: &crate::people_repository::FerrofinPeopleRepository,
        item_id: uuid::Uuid,
    ) -> usize {
        use ferrofin_traits::persistence::PeopleRepository as _;
        people
            .get_people_batch(&[item_id])
            .await
            .expect("read credits")
            .get(&item_id)
            .map_or(0, Vec::len)
    }

    // `update_people` replaces an item's credit rows, but an empty result never
    // reached it — so the series cast written onto every episode by an older
    // scan could never be corrected, no matter how many re-scans ran. A fetch
    // that COMPLETED is authoritative even when it found nobody.
    #[tokio::test]
    async fn a_completed_credits_fetch_clears_a_stale_cast() {
        // Season text so the episode is worth fetching, and credits with nobody
        // in them — a real TMDB answer for an episode it has no cast for.
        let (base, _hits, _all) =
            spawn_tmdb_server(Some(SEASON_JSON), Some(r#"{"cast":[],"crew":[]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let (vf, persistence, people, episode_id) =
            library_with_a_stale_episode_cast(tmp.path()).await;

        let tmdb = Arc::new(ferrofin_providers::TmdbClient::new().with_base_url(&base));
        LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_metadata(tmdb, tmp.path().join("metadata"))
            .with_people(people.clone() as Arc<dyn ferrofin_traits::persistence::PeopleRepository>)
            .scan(None)
            .await
            .unwrap();

        assert_eq!(
            episode_credit_count(&people, episode_id).await,
            0,
            "a completed credits fetch that found nobody must clear the stale rows"
        );
    }

    // The dangerous direction: a credits request that FAILED must not read as
    // "this episode has no cast". TMDB answers 429 under exactly the load a
    // first corrective scan of a large library generates, and the wipe would be
    // permanent — the title and overview are written before the credits call,
    // so the re-scan gate skips that episode from then on.
    #[tokio::test]
    async fn a_failed_credits_request_does_not_wipe_a_stored_cast() {
        // Season text answers; `/credits` 500s.
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), None);
        let tmp = tempfile::tempdir().unwrap();
        let (vf, persistence, people, episode_id) =
            library_with_a_stale_episode_cast(tmp.path()).await;

        let tmdb = Arc::new(ferrofin_providers::TmdbClient::new().with_base_url(&base));
        LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_metadata(tmdb, tmp.path().join("metadata"))
            .with_people(people.clone() as Arc<dyn ferrofin_traits::persistence::PeopleRepository>)
            .scan(None)
            .await
            .unwrap();

        assert_eq!(
            episode_credit_count(&people, episode_id).await,
            2,
            "a failed credits request must leave the stored cast alone"
        );
    }

    // The other direction, and the reason the flag exists rather than always
    // writing: with no provider wired nothing was fetched, so the stored cast
    // is all anyone has. A network outage or an unticked fetcher must never
    // wipe a library's credits.
    #[tokio::test]
    async fn stored_credits_survive_a_scan_that_fetched_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (vf, persistence, people, episode_id) =
            library_with_a_stale_episode_cast(tmp.path()).await;

        LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_people(people.clone() as Arc<dyn ferrofin_traits::persistence::PeopleRepository>)
            .scan(None)
            .await
            .unwrap();

        assert_eq!(episode_credit_count(&people, episode_id).await, 2);
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
        // A miss, not an empty hit: the caller must be able to fall through.
        assert!(
            scanner
                .fetch_tvdb_metadata(&mut nameless, "Series", &mut cache)
                .await
                .is_none()
        );
        assert!(cache.series_tvdb.is_empty());

        // Episode whose series isn't in the TVDB cache → skipped (no network).
        let mut orphan_ep = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
            series_id: Some(uuid::Uuid::new_v4().to_string()),
            parent_index_number: Some(1),
            index_number: Some(1),
            ..Default::default()
        };
        assert!(
            scanner
                .fetch_tvdb_metadata(&mut orphan_ep, "Episode", &mut cache)
                .await
                .is_none()
        );
    }

    // With no saved TypeOptions both fetcher ranks are `usize::MAX`, so
    // `tvdb_first` is true and TVDB runs first. An episode TVDB cannot resolve
    // — alternate numbering, a special, something aired yesterday — must still
    // reach TMDB. It did not: the caller returned unconditionally for episodes,
    // because a series miss was detectable from the cache and an episode miss
    // was not.
    #[tokio::test]
    async fn an_episode_tvdb_misses_still_reaches_tmdb() {
        let (base, _hits, _all) = spawn_tmdb_server(Some(SEASON_JSON), Some(CREDITS_JSON));
        let tmp = tempfile::tempdir().unwrap();
        let (scanner, mut cache, mut episode) = tmdb_episode_fixture(&base, tmp.path()).await;
        // TVDB is wired and ranked first (default policy: both fetcher ranks
        // are `usize::MAX`, so `tvdb_first` is true), and misses — the fixture
        // leaves `cache.series_tvdb` empty, so the episode arm returns at its
        // cache lookup before any request. Point the client at a dead address
        // regardless, so a future change to the fixture cannot turn this into a
        // live TVDB login on every CI run.
        let scanner = scanner.with_tvdb(Arc::new(
            ferrofin_providers::TvdbClient::new().with_base_url("http://127.0.0.1:1"),
        ));

        let result = scanner
            .fetch_remote_metadata(
                &mut episode,
                &mut cache,
                super::FetcherPolicy::default(),
                None,
                &[],
            )
            .await;

        assert_eq!(
            episode.name.as_deref(),
            Some("Winter Is Coming"),
            "TMDB must get the episode TVDB could not resolve"
        );
        assert!(result.people_fetched);
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
    #[allow(clippy::too_many_lines)]
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
        persistence
            .set_ancestors(track_id, &[album_id])
            .await
            .expect("ancestors");
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
                media_attachments: vec![ferrofin_model::entities_media::MediaAttachment {
                    index: 2,
                    codec: Some("ttf".to_owned()),
                    codec_tag: Some("[0][0][0][0]".to_owned()),
                    file_name: Some("font.ttf".to_owned()),
                    mime_type: Some("application/x-truetype-font".to_owned()),
                    ..Default::default()
                }],
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
        use ferrofin_traits::persistence::MediaAttachmentRepository as _;
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

        let attachments = Arc::new(
            crate::media_attachment_repository::FerrofinMediaAttachmentRepository::new(db.clone()),
        );
        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(FerrofinFileSystem::new()), persistence)
                .with_probe(
                    Arc::new(FakeProbe),
                    Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
                    Arc::new(crate::chapter_repository::FerrofinChapterRepository::new(
                        db.clone(),
                    )),
                )
                .with_attachments(attachments.clone());
        scanner.scan_all().await.unwrap();

        // The probed duration + size land on the item row.
        let (ticks, size, movie_id): (Option<i64>, Option<i64>, String) = sqlx::query_as(
            r#"SELECT "RunTimeTicks","Size","Id" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
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

        // The probed attachment is persisted with the item (`SaveMediaAttachments`),
        // under the ffprobe stream index a client later asks /Attachments/{index} for.
        let rows = attachments
            .get_media_attachments(&ferrofin_traits::persistence::MediaAttachmentQuery {
                item_id: uuid::Uuid::parse_str(&movie_id).unwrap(),
                index: None,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, 2);
        assert_eq!(rows[0].filename.as_deref(), Some("font.ttf"));
        assert_eq!(
            rows[0].mime_type.as_deref(),
            Some("application/x-truetype-font")
        );
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
        // Boxed: the scan future is over clippy's `large_futures` ceiling.
        Box::pin(scanner.scan_all()).await.unwrap();
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

    /// Saved library options (any dashboard "Save", the bench harness, an adopted
    /// Jellyfin DB) persist a `TypeOptions` whose `ImageFetchers` list can only ever
    /// name REMOTE fetchers — "Local Images" is a `Cap::LocalImage` provider and never
    /// appears in the checkbox list. Upstream short-circuits `ILocalImageProvider` to
    /// enabled before that check; gating local discovery on the list silently dropped
    /// every sidecar poster once options were saved.
    #[tokio::test]
    async fn saved_type_options_never_disable_local_artwork() {
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;
        use ferrofin_traits::options::InternalItemsQuery;
        use ferrofin_traits::persistence::ItemRepository;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();
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
                type_options: vec![ferrofin_model::configuration::TypeOptions {
                    type_: Some("Movie".to_owned()),
                    image_fetchers: Vec::new(),
                    ..Default::default()
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();

        let scanner = LibraryScanner::new(
            vf.clone(),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        );
        scanner.scan_all().await.unwrap();

        // Assert through the repository seam (the SQL-boundary ratchet forbids new raw
        // queries in this file): the scanned movie carries the poster as its Primary.
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(db, lookup));
        let ids = items
            .get_item_ids(&InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::Movie],
                recursive: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "the movie was scanned in");
        let primaries: Vec<_> = items
            .get_image_infos(ids[0])
            .await
            .unwrap()
            .into_iter()
            .filter(|i| i.image_type == ferrofin_model::entities::ImageType::Primary)
            .collect();
        assert_eq!(
            primaries.len(),
            1,
            "the sidecar poster must survive an empty ImageFetchers list"
        );
        assert!(primaries[0].path.ends_with("poster.jpg"));
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
                    crate::event_manager::consumer_done()
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
<uniqueid type="imdb">tt0133093</uniqueid>
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

        let (overview, _year, _rating, genres, studios, ids) =
            movie_detail_row(&db, "The Matrix").await;
        // The id the USER pinned in the sidecar must be persisted: it is what
        // every later fetch resolves by, and dropping it makes each re-scan
        // fall back to a fuzzy title search.
        assert_eq!(ids.as_deref(), Some("Imdb=tt0133093"));
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
        // …plus the other two arms of `GetResolversForExtraType`: a behind-the-
        // scenes extra (`_videoResolvers` -> Video) and a theme song (`null` ->
        // the AudioResolver -> Audio).
        std::fs::create_dir_all(folder.join("behind the scenes")).unwrap();
        std::fs::write(folder.join("behind the scenes").join("making-of.mkv"), b"").unwrap();
        std::fs::write(folder.join("theme.mp3"), b"").unwrap();

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

        // C# resolves an `ExtraType.Trailer` extra with
        // `GenericVideoResolver<Trailer>`, so the row's item TYPE is Trailer —
        // which is what `GET /Trailers` (and `/Items?includeItemTypes=Trailer`)
        // filters on. Stored as `Video`, both routes returned nothing.
        let by_kind = repo
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Trailer],
                ..Default::default()
            })
            .await
            .expect("trailers by kind");
        assert_eq!(
            by_kind.len(),
            2,
            "both trailer spellings are Trailer-kind items"
        );
        let mut names: Vec<&str> = by_kind
            .iter()
            .map(|i| i.name.as_deref().unwrap_or(""))
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Heat (1995)-trailer", "alt"]);

        // A non-trailer, non-theme extra keeps the `Video` kind
        // (`_videoResolvers`).
        let videos = repo
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Video],
                ..Default::default()
            })
            .await
            .expect("videos");
        assert_eq!(
            videos.len(),
            1,
            "the behind-the-scenes extra is the fixture's only Video-kind row"
        );
        assert_eq!(videos[0].media_type.as_deref(), Some("Video"));

        // `ExtraType.ThemeSong => null` in `GetResolversForExtraType`, with the
        // comment "we'll have to rely on the AudioResolver": a theme song is an
        // AUDIO item. Kinded Video, it never reached `ThemeSongsResult` and sat
        // in the library as a stray video row instead.
        let audio = repo
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Audio],
                ..Default::default()
            })
            .await
            .expect("audio");
        assert_eq!(audio.len(), 1, "the theme song is an Audio row");
        assert_eq!(audio[0].media_type.as_deref(), Some("Audio"));
        assert_eq!(
            audio[0].extra_type,
            Some(ferrofin_model::entities::ExtraType::ThemeSong as i32)
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

    /// Port check for `MetadataService.UpdateCumulativeRunTimeTicks`: the
    /// folder kinds whose `SupportsCumulativeRunTimeTicks` is true store the
    /// summed runtime of their non-folder recursive children — the column both
    /// `RunTimeTicks` and `CumulativeRunTimeTicks` are emitted from.
    #[tokio::test]
    async fn cumulative_run_time_ticks_sums_non_folder_recursive_children() {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let items = crate::test_support::item_repository_over(db.clone());

        let album = uuid::Uuid::from_u128(0x1001);
        let artist = uuid::Uuid::from_u128(0x1002);
        let series = uuid::Uuid::from_u128(0x1003);
        let row =
            |id: uuid::Uuid, kind: BaseItemKind, folder: bool, ticks: Option<i64>| BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(id),
                type_: crate::item_type_lookup::stored_type_name(kind)
                    .unwrap_or_default()
                    .to_owned(),
                name: Some(format!("{kind:?} {id}")),
                is_folder: folder,
                run_time_ticks: ticks,
                ..BaseItemEntity::default()
            };
        let mut rows = vec![
            row(album, BaseItemKind::MusicAlbum, true, None),
            // A by-name artist: no children in the hierarchy, so its sum is 0 —
            // and the C# writes 0 rather than leaving the column NULL.
            row(artist, BaseItemKind::MusicArtist, true, None),
            // Not a `SupportsCumulativeRunTimeTicks` kind (`Folder.cs:97`).
            row(series, BaseItemKind::Series, true, None),
        ];
        for i in 0..3u128 {
            let track = uuid::Uuid::from_u128(0x2000 + i);
            let mut t = row(track, BaseItemKind::Audio, false, Some(20_000_000));
            t.parent_id = Some(ferrofin_db::store::guid_to_db(album));
            rows.push(t);
        }
        // A folder child must NOT be counted, only recursed through.
        let inner = uuid::Uuid::from_u128(0x3001);
        let mut disc = row(inner, BaseItemKind::Folder, true, Some(999));
        disc.parent_id = Some(ferrofin_db::store::guid_to_db(album));
        rows.push(disc);
        persistence.save_items(&rows).await.unwrap();
        for i in 0..3u128 {
            persistence
                .set_ancestors(uuid::Uuid::from_u128(0x2000 + i), &[album])
                .await
                .unwrap();
        }
        persistence.set_ancestors(inner, &[album]).await.unwrap();

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(FerrofinVirtualFolderManager::new(
            std::path::PathBuf::from("/nonexistent"),
        ));
        let scanner = LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        )
        .with_items(Arc::clone(&items));

        scanner.update_cumulative_run_time_ticks().await.unwrap();

        let ticks = |id: uuid::Uuid| {
            let items = Arc::clone(&items);
            async move {
                items
                    .retrieve_item(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .run_time_ticks
            }
        };
        assert_eq!(
            ticks(album).await,
            Some(60_000_000),
            "3 tracks, not the disc folder"
        );
        assert_eq!(ticks(artist).await, Some(0), "written as 0, not left NULL");
        assert_eq!(ticks(series).await, None, "Series does not support it");

        // Idempotent: a second pass finds nothing to change.
        scanner.update_cumulative_run_time_ticks().await.unwrap();
        assert_eq!(ticks(album).await, Some(60_000_000));
    }

    /// The retirement pass drops an `IsAccessedByName` artist ONLY when a
    /// folder-resolved artist of the same `CleanName` has superseded it — a
    /// genuinely by-name artist (a compilation's AlbumArtist with no directory)
    /// keeps its row, exactly as it does upstream.
    #[tokio::test]
    async fn retirement_drops_only_by_name_artists_a_resolved_folder_superseded() {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let items = crate::test_support::item_repository_over(db.clone());

        let library = uuid::Uuid::from_u128(0x4000);
        let row = |id: uuid::Uuid, name: &str, top: Option<uuid::Uuid>| BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::MusicArtist)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name.to_owned()),
            clean_name: Some(crate::text_util::get_clean_value(name)),
            is_folder: true,
            top_parent_id: top.map(ferrofin_db::store::guid_to_db),
            parent_id: top.map(ferrofin_db::store::guid_to_db),
            ..BaseItemEntity::default()
        };
        // The library the resolved artist parents into — `ParentId` is a real
        // FK, so the row has to exist before the artists reference it.
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(library),
                type_: crate::item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder)
                    .unwrap_or_default()
                    .to_owned(),
                name: Some("Music".to_owned()),
                is_folder: true,
                ..BaseItemEntity::default()
            }])
            .await
            .unwrap();
        let resolved = uuid::Uuid::from_u128(0x4001);
        let superseded = uuid::Uuid::from_u128(0x4002);
        let genuine = uuid::Uuid::from_u128(0x4003);
        persistence
            .save_items(&[
                row(resolved, "Artist 01", Some(library)),
                // The pre-port twin: same name, no TopParentId.
                row(superseded, "Artist 01", None),
                // No folder ever resolved this one — it must survive.
                row(genuine, "Various Artists", None),
            ])
            .await
            .unwrap();

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(FerrofinVirtualFolderManager::new(
            std::path::PathBuf::from("/nonexistent"),
        ));
        let scanner = LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            Arc::clone(&persistence),
        )
        .with_items(Arc::clone(&items));

        scanner.retire_accessed_by_name_artists().await.unwrap();

        assert!(
            items.retrieve_item(resolved).await.unwrap().is_some(),
            "the resolved artist survives"
        );
        assert!(
            items.retrieve_item(superseded).await.unwrap().is_none(),
            "the superseded by-name twin is retired"
        );
        assert!(
            items.retrieve_item(genuine).await.unwrap().is_some(),
            "a genuinely accessed-by-name artist keeps its row"
        );

        // Idempotent: a second pass finds nothing left to retire.
        scanner.retire_accessed_by_name_artists().await.unwrap();
        assert!(items.retrieve_item(resolved).await.unwrap().is_some());
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

        // `MusicArtistResolver` (priority Second): the artist folder holds a
        // music album, so it resolves to a MusicArtist row of its own, parented
        // into the library — which is what gives it a TopParentId and makes it
        // reachable from a user-scoped recursive query.
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
        // Exactly ONE row is also the de-duplication assertion: the track's
        // `AlbumArtist` ItemValues row would otherwise materialize a parentless
        // by-name MusicArtist with the same CleanName, and `push_by_name_join`
        // joins on CleanName — so /Artists would list "Pink Floyd" twice. That
        // double-listing is what a previous attempt at this port was rolled
        // back for.
        assert_eq!(artists.len(), 1, "exactly one artist row: {artists:?}");
        let artist = &artists[0];
        assert_eq!(artist.name.as_deref(), Some("Pink Floyd"));
        assert_eq!(
            artist.path.as_deref(),
            Some(media.join("Pink Floyd").to_str().unwrap()),
            "the artist is pathed at its MEDIA directory, not a metadata dir"
        );
        assert_eq!(artist.parent_id.as_deref(), Some(cf.as_str()));
        assert_eq!(artist.top_parent_id.as_deref(), Some(cf.as_str()));
        assert!(artist.is_folder);

        // Exactly one album — CD2 folds in rather than becoming its own — and it
        // hangs off the ARTIST now, the way Jellyfin parents it.
        let album_row: (String, String, Option<String>) = sqlx::query_as(
            r#"SELECT "Id","ParentId","Name" FROM "BaseItems"
               WHERE "Type"='MediaBrowser.Controller.Entities.Audio.MusicAlbum'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(album_row.1, artist.id, "album parents to the artist");
        assert_eq!(album_row.2.as_deref(), Some("The Wall"));

        // AncestorIds is what every recursive count and `ancestorIds=` filter
        // reads, so the artist has to be in the album's and the tracks' chains —
        // asserted through the query path that consumes it.
        let under_artist = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                ancestor_ids: vec![uuid::Uuid::parse_str(&artist.id).unwrap()],
                recursive: true,
                ..Default::default()
            })
            .await
            .expect("recursive children");
        let mut kinds: Vec<_> = under_artist
            .iter()
            .map(|i| i.type_.rsplit('.').next().unwrap_or("").to_owned())
            .collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            vec!["Audio", "Audio", "Audio", "MusicAlbum"],
            "the artist's recursive children are its album and its 3 tracks"
        );

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

    /// `MusicArtistResolver`'s `artist.nfo` shortcut fires before every other
    /// test, so a folder with the sidecar and no album subfolder is still an
    /// artist — and a folder holding a NAMED release container (`albums`,
    /// `live`, …) is one too, with the albums inside it re-parented onto the
    /// artist rather than onto the container.
    #[tokio::test]
    async fn scan_resolves_artists_from_nfo_and_from_release_subfolders() {
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("music");

        // (a) artist.nfo, no album beneath it at all.
        let bare = media.join("Aphex Twin");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("artist.nfo"), b"<artist/>").unwrap();

        // (b) a named artist subfolder holding the actual album.
        let container = media.join("Boards of Canada").join("albums");
        let album = container.join("Music Has the Right to Children");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 Wildlife Analysis.flac"), b"").unwrap();

        let (db, _cf) = scan_one(CollectionTypeOptions::music, "Music", &media).await;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let mut artists = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::MusicArtist],
                ..Default::default()
            })
            .await
            .expect("artists");
        artists.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = artists.iter().map(|a| a.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                Some("Aphex Twin".to_owned()),
                Some("Boards of Canada".to_owned())
            ],
            "both artists resolve"
        );

        // The release container itself is NOT an item; its album parents
        // straight onto the artist.
        let boc = artists
            .iter()
            .find(|a| a.name.as_deref() == Some("Boards of Canada"))
            .expect("boc");
        let albums = repo
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::MusicAlbum],
                ..Default::default()
            })
            .await
            .expect("albums");
        assert_eq!(albums.len(), 1);
        assert_eq!(
            albums[0].parent_id.as_deref(),
            Some(boc.id.as_str()),
            "album skips the `albums` container"
        );
    }

    /// Loose audio directly inside a resolved artist folder is `Audio`
    /// PARENTED TO THE ARTIST — not a synthetic album named after the artist.
    ///
    /// Upstream resolves the artist DIRECTORY as `MusicArtist` and then hands
    /// its child files to the ordinary resolver chain; `MusicAlbumResolver`
    /// only ever resolves a directory, so a stray track never acquires an
    /// album. Wrapping it invented a `MusicAlbum` row Jellyfin has no row for.
    /// The negative control is the second assertion: exactly ONE album exists
    /// (the real one in the subfolder), and the multi-disc subfolder's tracks
    /// are planned once, not twice.
    #[tokio::test]
    async fn loose_audio_in_an_artist_folder_parents_to_the_artist_not_a_fake_album() {
        use ferrofin_traits::persistence::ItemRepository as _;

        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("music");

        // An artist folder: one real album subfolder (which is what makes the
        // directory resolve as an artist at all) plus a stray track beside it.
        let artist = media.join("Burial");
        let album = artist.join("Untrue");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 Archangel.flac"), b"").unwrap();
        std::fs::write(artist.join("Rival Dealer.flac"), b"").unwrap();

        let (db, _cf) = scan_one(CollectionTypeOptions::music, "Music", &media).await;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo = crate::FerrofinItemRepository::new(db.clone(), lookup);
        let list = |kind| {
            let repo = repo.clone();
            async move {
                repo.get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..Default::default()
                })
                .await
                .expect("query")
            }
        };

        let artists = list(ferrofin_model::data::BaseItemKind::MusicArtist).await;
        assert_eq!(artists.len(), 1);
        let artist_id = artists[0].id.clone();

        // Exactly one album — "Untrue". No album named after the artist folder.
        let albums = list(ferrofin_model::data::BaseItemKind::MusicAlbum).await;
        let album_names: Vec<_> = albums.iter().map(|a| a.name.clone()).collect();
        assert_eq!(album_names, vec![Some("Untrue".to_owned())]);

        let mut audio = list(ferrofin_model::data::BaseItemKind::Audio).await;
        audio.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(audio.len(), 2, "each track planned exactly once");
        let stray = audio
            .iter()
            .find(|a| a.name.as_deref() == Some("Rival Dealer"))
            .expect("the stray track");
        assert_eq!(
            stray.parent_id.as_deref(),
            Some(artist_id.as_str()),
            "the stray track parents to the ARTIST"
        );
        assert_eq!(stray.album, None, "and it is given no invented album name");
        // The album's own track still parents to the album, unchanged.
        let inside = audio
            .iter()
            .find(|a| a.name.as_deref() == Some("01 Archangel"))
            .expect("the album track");
        assert_eq!(inside.parent_id.as_deref(), Some(albums[0].id.as_str()));
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

    // ---------------------------------------------------------------------
    // Image metadata is probed ONCE per file, not once per scan.
    // ---------------------------------------------------------------------

    // Port oracle: `LibraryManager.ImageNeedsRefresh` (Emby.Server.Implementations/
    // Library/LibraryManager.cs). A local image is refreshed only when its stored
    // width/height/blurhash is missing, or when the stored DateModified differs
    // from the file's mtime by MORE than one second.
    #[test]
    fn image_metadata_is_current_matches_jellyfins_image_needs_refresh() {
        use ferrofin_traits::persistence::StoredImageMetadata;

        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
        let complete = |dm: chrono::DateTime<chrono::Utc>| StoredImageMetadata {
            path: "/m/poster.jpg".to_owned(),
            width: 1000,
            height: 1500,
            blur_hash: Some("LEHV6nWB2yk8".to_owned()),
            date_modified: dm,
        };

        assert!(
            super::image_metadata_is_current(&complete(base), base),
            "unchanged file with complete metadata is reused"
        );
        assert!(
            super::image_metadata_is_current(&complete(base), base + chrono::Duration::seconds(1)),
            "a one-second skew is inside upstream's tolerance"
        );
        assert!(
            !super::image_metadata_is_current(&complete(base), base + chrono::Duration::seconds(2)),
            "a file newer than the stored stamp forces a refresh"
        );
        assert!(
            !super::image_metadata_is_current(&complete(base), base - chrono::Duration::seconds(2)),
            "the comparison is absolute, as C#'s .Duration() is"
        );

        for (label, broken) in [
            (
                "no width",
                StoredImageMetadata {
                    width: 0,
                    ..complete(base)
                },
            ),
            (
                "no height",
                StoredImageMetadata {
                    height: 0,
                    ..complete(base)
                },
            ),
            (
                "no blurhash",
                StoredImageMetadata {
                    blur_hash: None,
                    ..complete(base)
                },
            ),
            (
                "empty blurhash",
                StoredImageMetadata {
                    blur_hash: Some(String::new()),
                    ..complete(base)
                },
            ),
        ] {
            assert!(
                !super::image_metadata_is_current(&broken, base),
                "{label}: incomplete stored metadata must be recomputed"
            );
        }
    }

    /// An [`ImageProcessor`](ferrofin_traits::drawing::ImageProcessor) that counts
    /// the two calls the scan's image pass makes and delegates everything to a
    /// real processor, so a test can prove a rescan did no pixel work.
    struct CountingProcessor {
        inner: Arc<dyn ferrofin_traits::drawing::ImageProcessor>,
        dimensions: Arc<std::sync::atomic::AtomicUsize>,
        blur_hashes: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ferrofin_traits::drawing::ImageProcessor for CountingProcessor {
        fn supported_input_formats(&self) -> Vec<String> {
            self.inner.supported_input_formats()
        }
        fn supports_image_collage_creation(&self) -> bool {
            self.inner.supports_image_collage_creation()
        }
        fn supported_image_output_formats(&self) -> Vec<ferrofin_model::drawing::ImageFormat> {
            self.inner.supported_image_output_formats()
        }
        async fn get_image_dimensions(
            &self,
            path: &str,
        ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
            self.dimensions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.get_image_dimensions(path).await
        }
        async fn get_item_image_dimensions(
            &self,
            item_id: uuid::Uuid,
            info: &ferrofin_traits::options::ItemImageInfo,
        ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
            self.inner.get_item_image_dimensions(item_id, info).await
        }
        async fn get_image_blur_hash(&self, path: &str) -> Result<String, ServiceError> {
            self.blur_hashes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.get_image_blur_hash(path).await
        }
        async fn get_image_blur_hash_sized(
            &self,
            path: &str,
            image_dimensions: ferrofin_model::drawing::ImageDimensions,
        ) -> Result<String, ServiceError> {
            self.blur_hashes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner
                .get_image_blur_hash_sized(path, image_dimensions)
                .await
        }
        async fn get_image_cache_tag(
            &self,
            item_id: uuid::Uuid,
            image: &ferrofin_traits::options::ItemImageInfo,
        ) -> Result<Option<String>, ServiceError> {
            self.inner.get_image_cache_tag(item_id, image).await
        }
        async fn get_image_cache_tag_for_path(
            &self,
            base_item_path: &str,
            image_date_modified: chrono::DateTime<chrono::Utc>,
        ) -> Result<Option<String>, ServiceError> {
            self.inner
                .get_image_cache_tag_for_path(base_item_path, image_date_modified)
                .await
        }
        async fn process_image(
            &self,
            options: &ferrofin_traits::options::ImageProcessingOptions,
        ) -> Result<ferrofin_traits::drawing::ProcessedImage, ServiceError> {
            self.inner.process_image(options).await
        }
        async fn create_image_collage(
            &self,
            options: &ferrofin_traits::options::ImageCollageOptions,
            library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            self.inner.create_image_collage(options, library_name).await
        }
    }

    /// The scanner + repository + call counters a poster-refresh test drives,
    /// built over a one-movie library with a `poster.jpg` beside the media file.
    struct PosterFixture {
        scanner: LibraryScanner,
        repo: Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        poster: std::path::PathBuf,
        dimensions: Arc<std::sync::atomic::AtomicUsize>,
        blur_hashes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PosterFixture {
        /// The stored Primary image row of the fixture's single movie.
        async fn primary(&self) -> ferrofin_traits::options::ItemImageInfo {
            use ferrofin_model::data::BaseItemKind;
            use ferrofin_model::entities::ImageType;
            use ferrofin_traits::options::InternalItemsQuery;

            let movies = self
                .repo
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Movie],
                    recursive: true,
                    ..Default::default()
                })
                .await
                .expect("movie rows");
            let id = uuid::Uuid::parse_str(&movies[0].id).expect("movie id");
            self.repo
                .get_image_infos(id)
                .await
                .expect("image rows")
                .into_iter()
                .find(|i| i.image_type == ImageType::Primary)
                .expect("poster row")
        }
    }

    /// Builds [`PosterFixture`] under `root`, with a `width`×`height` poster.
    async fn poster_fixture(root: &std::path::Path, width: u32, height: u32) -> PosterFixture {
        use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
        use ferrofin_traits::persistence::ItemRepository;

        let movies = root.join("movies");
        let media = movies.join("Heat (1995)");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("Heat (1995).mkv"), b"").unwrap();
        let poster_path = media.join("poster.jpg");
        write_poster(&poster_path, width, height);

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("default"))
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

        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let repo: Arc<dyn ItemRepository> =
            Arc::new(crate::FerrofinItemRepository::new(db.clone(), lookup));
        let dimensions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blur_hashes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> =
            Arc::new(CountingProcessor {
                inner: Arc::new(ImageProcessor::new(
                    Arc::new(ImageCrateEncoder::new()),
                    root.join("cache"),
                )),
                dimensions: Arc::clone(&dimensions),
                blur_hashes: Arc::clone(&blur_hashes),
            });
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_image_processor(processor)
            .with_items(Arc::clone(&repo))
            .with_metadata_dir(root.join("meta"));

        PosterFixture {
            scanner,
            repo,
            poster: poster_path,
            dimensions,
            blur_hashes,
        }
    }

    /// Writes a deterministic `width`×`height` JPEG to `path`.
    fn write_poster(path: &std::path::Path, width: u32, height: u32) {
        let mut poster = image::RgbImage::new(width, height);
        for (x, y, px) in poster.enumerate_pixels_mut() {
            *px = image::Rgb([
                u8::try_from(x % 256).unwrap_or(0),
                30,
                u8::try_from(y % 256).unwrap_or(0),
            ]);
        }
        poster.save(path).unwrap();
    }

    // Decoding a poster and BlurHash-encoding it is the largest non-ffprobe slice
    // of a scan (measured: 2.79 s of a 5.76 s 1100-item loop), and it was paid
    // again on EVERY rescan for files that had not changed. Upstream pays it once
    // per file (`LibraryManager.ImageNeedsRefresh`), and so must we — while
    // storing byte-identical rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn rescan_reuses_stored_image_metadata_instead_of_re_probing() {
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let fx = poster_fixture(tmp.path(), 40, 60).await;

        fx.scanner.scan_all().await.unwrap();
        let cold_dimensions = fx.dimensions.swap(0, Ordering::Relaxed);
        let cold_hashes = fx.blur_hashes.swap(0, Ordering::Relaxed);
        assert!(
            cold_dimensions >= 1 && cold_hashes >= 1,
            "the first scan must actually probe the poster (got {cold_dimensions} dimension \
             probes / {cold_hashes} blurhashes)"
        );
        let cold = fx.primary().await;
        assert!(cold.width > 0 && cold.blur_hash.is_some());

        fx.scanner.scan_all().await.unwrap();
        assert_eq!(
            (
                fx.dimensions.load(Ordering::Relaxed),
                fx.blur_hashes.load(Ordering::Relaxed)
            ),
            (0, 0),
            "an unchanged poster must not be decoded again on a rescan"
        );

        // …and the row the rescan wrote is identical to the one the cold scan did.
        let rescanned = fx.primary().await;
        assert_eq!(rescanned.width, cold.width);
        assert_eq!(rescanned.height, cold.height);
        assert_eq!(rescanned.blur_hash, cold.blur_hash);
    }

    // The other half of `ImageNeedsRefresh`: a poster the user replaced has an
    // mtime past the stored stamp, so it IS re-probed and the row picks up the
    // new dimensions. Without this the reuse above would pin stale artwork
    // metadata forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_poster_is_re_probed() {
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let fx = poster_fixture(tmp.path(), 40, 60).await;
        fx.scanner.scan_all().await.unwrap();
        fx.scanner.scan_all().await.unwrap();
        fx.dimensions.store(0, Ordering::Relaxed);
        fx.blur_hashes.store(0, Ordering::Relaxed);

        write_poster(&fx.poster, 80, 20);
        std::fs::File::options()
            .write(true)
            .open(&fx.poster)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_hours(1))
            .unwrap();

        fx.scanner.scan_all().await.unwrap();
        assert!(
            fx.blur_hashes.load(Ordering::Relaxed) >= 1,
            "a changed poster must be re-probed"
        );
        let changed = fx.primary().await;
        assert_eq!((changed.width, changed.height), (80, 20));
    }
}
