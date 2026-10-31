//! The real [`LiveTvManager`] over the SQLite channel/guide cache.
//!
//! Configuration (tuner hosts, listing providers) is stored verbatim as JSON so
//! reads round-trip the DTO. `refresh_guide` fetches each tuner host (M3U) and
//! listing provider (XMLTV), rewrites `FerrofinLiveTvChannels`/`FerrofinLiveTvPrograms`, and
//! binds programmes to channels by the tuner `tvg-id` / XMLTV `channel id`.
//! Channels and programmes are surfaced to clients as `BaseItemDto`s.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db, opt_datetime_to_db};
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::ItemValueType;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{
    BaseItemDto, DayOfWeek, MediaSourceInfo, MediaSourceType, NameIdPair, NameValuePair, SortOrder,
};
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_model::live_tv::{
    ChannelMappingOptionsDto, ChannelType, GuideInfo, ItemSortBy, ListingsProviderInfo, LiveTvInfo,
    LiveTvServiceInfo, LiveTvServiceStatus, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
    TunerChannelMapping, TunerHostInfo,
};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SelfWrite, SelfWriteGuard, UserViewManager};
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::ProviderManager;
use ferrofin_traits::stubs::{LiveStreamFile, LiveTvChannelQuery, LiveTvManager};
use ferrofin_traits::system::LocalApiAccess;
use serde::de::DeserializeOwned;

use crate::dvr::{ActiveRecording, RecorderKind, RecordingInput, TimerRecordingInfo};
use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::guide_repository::PROGRAM_SELECT;
use crate::mapping::{
    EpgChannel, EpgChannelData, TunerChannel, epg_channel_for_tuner_channel,
    eq_ordinal_ignore_case, is_listing_provider_enabled_for_tuner, m3u_channel_id,
    tuner_channel_mapping,
};
use crate::projection::{
    ChannelRow, ProgramImages, ProgramRow as GuideProgramRow, channel_entity,
    channel_images_update, channel_item_update, program_entity, program_images_update,
    program_item_update, remove_fields,
};
use crate::schedules_direct::{SchedulesDirect, SdChannel, SdProgram};
use crate::stream::{LiveStreamHandle, LiveStreamKind, TunerStreamSource};
use crate::xmltv::{Xmltv, XmltvChannel, XmltvProgramme, read_xmltv_where};
use crate::xmltv_cache::XmlTvDocument;

/// SQLite's conservative default bind-parameter limit (`SQLITE_MAX_VARIABLE_NUMBER`
/// is 999 before 3.32, 32766 after); multi-row inserts chunk to stay under it.
const SQLITE_BIND_LIMIT: usize = 999;

/// How many programme item rows one `save_items`/`delete_items`/
/// `save_item_values_for_items` call carries.
///
/// The guide mirror is O(channels x guide-hours): a few hundred rows on a test
/// fixture, tens of thousands on a real seven-day lineup. The first two calls
/// are multi-row statements, so the chunk exists to bound the statement size
/// and the peak memory of one write, not to bound the number of writes. The
/// genre write is one transaction per call, so there it also bounds how long
/// one write holds the single writer.
const PROGRAM_ITEM_WRITE_CHUNK: usize = 500;

/// `GuideManager.MaxCacheDays`: a refresh pre-caches the images of the
/// programmes it wrote that end within this many days. Upstream's hardcoded
/// value; a candidate setting awaiting the owner's decision (gl-367).
const PRE_CACHE_DAYS: i64 = 2;

/// The cap on concurrent pre-cache downloads, below the core count:
/// upstream's `Math.Min(Environment.ProcessorCount, 10)`. A candidate setting
/// awaiting the owner's decision (gl-367).
const PRE_CACHE_PARALLELISM: usize = 10;

/// The C# type name whose MD5 prefixes every Live TV `LiveStreamId`.
///
/// Port of `LiveTvMediaSourceProvider.GetChannelStream`'s
/// `service.GetType().FullName.GetMD5().ToString("N") + "_"` — the built-in
/// service is `DefaultLiveTvService` (the one named "Emby").
const LIVE_TV_SERVICE_TYPE_NAME: &str = "Jellyfin.LiveTv.DefaultLiveTvService";

/// The first key of a channel's `OpenToken`: C# `item.GetType().Name`.
const OPEN_TOKEN_ITEM_TYPE: &str = "LiveTvChannel";

/// The separator between an open token's / live-stream id's keys.
///
/// Port of `LiveTvMediaSourceProvider.StreamIdDelimiter` — deliberately not a
/// pipe, because Roku clients fail on one without an error message.
const STREAM_ID_DELIMITER: char = '_';

/// The buffer a Live TV media source declares when the tuner set none.
///
/// Port of `LiveTvMediaSourceProvider.GetMediaSourcesInternal`'s
/// `source.BufferMs ??= 1500`.
const DEFAULT_LIVE_STREAM_BUFFER_MS: i32 = 1500;

/// The container a shared live stream's buffer is written in — it is literally
/// the `{uniqueId}.ts` MPEG-TS file the copy task appends to. A probe of the
/// opened source refines the rest of the media info; this is what the recorder's
/// direct-vs-encoded choice reads when no probe is possible.
const LIVE_STREAM_BUFFER_CONTAINER: &str = "ts";

/// Days of guide data ingested and advertised when `LiveTvOptions.GuideDays`
/// is unset.
///
/// Port of the `: 7` fallback in `GuideManager.GetGuideDays` (v10.11.8
/// GuideManager.cs:161-168).
const DEFAULT_GUIDE_DAYS: i64 = 7;

/// The ceiling `LiveTvOptions.GuideDays` is clamped to.
///
/// Port of `GuideManager.MaxGuideDays` (v10.11.8 GuideManager.cs:26).
const MAX_GUIDE_DAYS: i64 = 14;

/// How far before "now" the guide window opens, so an airing already in
/// progress when the refresh runs is still ingested.
///
/// Port of `GuideManager.RefreshChannelsInternal`'s
/// `DateTime.UtcNow.AddHours(-1)` (v10.11.8 GuideManager.cs:226).
const GUIDE_WINDOW_LEAD_IN_HOURS: i64 = 1;

/// One airing's recommendation score.
///
/// Port of `LiveTvManager.GetRecommendationScore(program, user, factorChannelWatchCount: true)`
/// (v10.11.8 LiveTvManager.cs:333-372). `channel` is the caller's user-data for
/// the owning channel as `(Likes, IsFavorite, PlayCount)`; upstream returns the
/// programme-only score when the channel item cannot be resolved, which is what
/// `None` stands for here.
fn recommendation_score(
    program: &GuideProgramRow,
    channel: Option<&(Option<bool>, bool, i32)>,
) -> i32 {
    let mut score = 0;
    if program.is_live {
        score += 1;
    }
    if program.is_series && !program.is_repeat {
        score += 1;
    }
    let Some((likes, is_favorite, play_count)) = channel else {
        return score;
    };
    if let Some(likes) = likes {
        score += if *likes { 2 } else { -2 };
    }
    if *is_favorite {
        score += 3;
    }
    score + play_count
}

/// The calendar date part of a stored start instant, for
/// `OrderBy(i => i.StartDate.Date)`.
///
/// The column holds .NET's `yyyy-MM-ddTHH:mm:ss.fffffffZ`, so the date is the
/// leading `yyyy-MM-dd`. Comparing the prefix as text is the same ordering as
/// comparing the dates, and costs no parse.
fn start_day(start_date: &str) -> &str {
    start_date.get(..10).unwrap_or(start_date)
}

/// The `ILiveTvService` name every internal Live TV id is salted with.
///
/// Port of `LiveTvDtoService.ServiceName` (v10.11.8 LiveTvDtoService.cs:30).
const LIVE_TV_SERVICE_NAME: &str = "Emby";

/// The id-scheme version `LiveTvDtoService` appends before hashing.
///
/// Port of `LiveTvDtoService.InternalVersionNumber` (v10.11.8
/// LiveTvDtoService.cs:28). Upstream bumps it to invalidate every derived Live
/// TV id at once; it is part of the hash input, so it must match byte for byte.
const LIVE_TV_INTERNAL_VERSION: &str = "4";

/// A fresh lock key for one HDHomeRun control session.
///
/// `HdHomerunManager.StartStreaming`'s `_lockkey = (uint)Random.Shared.Next()`
/// (v10.11.8 HdHomerunManager.cs:84). It is a mutual-exclusion token between
/// viewers of the same device, not a secret, so any value the next session is
/// unlikely to repeat will do — `Uuid::new_v4`'s first four bytes come from the
/// same OS entropy `Random.Shared` seeds from, without adding a dependency for
/// one `u32`.
fn rand_lockkey() -> u32 {
    let bytes = Uuid::new_v4().into_bytes();
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// `LibraryManager.GetNewItemId(key, type)` for a key that is already relative
/// to the program-data path.
///
/// Port of `LibraryManager.GetNewItemIdInternal` (v10.11.8 LibraryManager.cs:636-658):
/// the program-data prefix strip does not apply to these synthetic keys, the
/// case fold has already been applied by the caller (upstream applies it *before*
/// prepending `type.FullName`, so the type name keeps its original casing), and
/// the hash is `BaseExtensions.GetMD5` — MD5 over UTF-16LE in .NET `Guid` byte
/// order, which [`ferrofin_common::extensions::get_md5`] implements.
fn new_item_id(type_full_name: &str, lowercased_key: &str) -> Uuid {
    ferrofin_common::extensions::get_md5(&format!("{type_full_name}{lowercased_key}"))
}

/// .NET `string.ToLowerInvariant`, which is **not** Rust's
/// [`str::to_lowercase`].
///
/// `ToLowerInvariant` is documented to perform *simple* case mapping — it maps
/// each code point independently and never changes the string's length — while
/// `str::to_lowercase` performs *full*, context-sensitive Unicode lowercasing.
/// They differ on two things that can reach these hash inputs, because the
/// programme half of the key embeds the XMLTV `channel` attribute verbatim and
/// that is arbitrary text:
///
/// - **Final sigma.** `str::to_lowercase` implements the `Final_Sigma`
///   condition, so a trailing `Σ` becomes `ς` (U+03C2); the simple mapping
///   always yields `σ` (U+03C3). A Greek channel id ending in a capital sigma
///   would otherwise derive a programme GUID Jellyfin never mints.
/// - **U+0130** (`İ`). Its *full* lowercase mapping expands to two code points
///   (`i` + U+0307 combining dot above); the simple mapping is the single
///   `i`. `SpecialCasing.txt` lists U+0130 as the only **unconditional**
///   multi-code-point lowercase mapping — every other multi-char entry is
///   conditional (final sigma above, plus the `lt`/`tr`/`az` locale rules,
///   which the invariant culture does not apply) — so special-casing it here
///   makes the rest of the fold exact rather than merely closer.
///
/// This lives next to its callers rather than beside
/// [`ferrofin_common::extensions::get_md5`] (the other .NET string primitive it
/// is always paired with) only because those are the sole callers; move it
/// there the moment a second subsystem needs the same fold.
fn to_lower_invariant(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\u{0130}' {
            out.push('i');
            continue;
        }
        // `char::to_lowercase` is already context-free, so with U+0130 handled
        // above it yields exactly one char for every input; the fallback keeps
        // this total rather than relying on that. The iterator is never empty.
        out.push(c.to_lowercase().next().unwrap_or(c));
    }
    out
}

/// The internal item GUID of a Live TV channel, from its tuner-facing external id.
///
/// Port of `LiveTvDtoService.GetInternalChannelId` (v10.11.8 LiveTvDtoService.cs:403-408).
fn internal_channel_id(external_id: &str) -> Uuid {
    new_item_id(
        crate::projection::CHANNEL_TYPE_NAME,
        &to_lower_invariant(&format!(
            "{LIVE_TV_SERVICE_NAME}{external_id}{LIVE_TV_INTERNAL_VERSION}"
        )),
    )
}

/// The internal item GUID of a Live TV programme, from its listings-facing
/// external id.
///
/// Port of `LiveTvDtoService.GetInternalProgramId` (v10.11.8 LiveTvDtoService.cs:424-429).
fn internal_program_id(external_id: &str) -> Uuid {
    new_item_id(
        crate::projection::PROGRAM_TYPE_NAME,
        &to_lower_invariant(&format!(
            "{LIVE_TV_SERVICE_NAME}{external_id}{LIVE_TV_INTERNAL_VERSION}"
        )),
    )
}

/// A guide document's `<channel>` as the matcher sees it
/// (`XmlTvListingsProvider.GetChannels`).
fn epg_channel(c: &XmltvChannel) -> EpgChannel {
    EpgChannel {
        number: c.listings_number().to_owned(),
        id: c.id.clone(),
        name: c.display_name.clone(),
        image_url: c.icon.clone(),
    }
}

/// Port of `ListingsManager.AddProviderMetadata`/`AddMetadata`: a tuner
/// channel matched to a channel of a listings provider enabled for its tuner
/// takes that guide channel's logo, when the guide has one. Providers are
/// applied in order, so a later one's logo wins. `guides` holds only the
/// providers whose guide was read, as upstream skips one whose
/// `GetEpgChannels` throws: while a guide cannot be read, its channels carry
/// the playlist's logo again, and the guide's returns with it, as upstream.
fn add_provider_metadata(
    lineups: &mut [(String, String, Vec<crate::tuner_host::TunerChannel>)],
    guides: &[(ListingsProviderInfo, GuideSource)],
) {
    for (provider, source) in guides {
        let epg = match source {
            GuideSource::XmlTv { channels, .. } => {
                EpgChannelData::new(&channels.iter().map(epg_channel).collect::<Vec<_>>())
            }
            GuideSource::SchedulesDirect { channels, .. } => EpgChannelData::new(channels),
        };
        for (host_id, _, lineup) in lineups.iter_mut() {
            if !is_listing_provider_enabled_for_tuner(provider, host_id) {
                continue;
            }
            for channel in lineup.iter_mut() {
                let tuner = TunerChannel {
                    id: channel.external_id.clone(),
                    tuner_channel_id: channel.guide_key.clone(),
                    number: channel.number.clone().unwrap_or_default(),
                    name: channel.name.clone(),
                    tuner_host_id: host_id.clone(),
                };
                if let Some(url) =
                    epg_channel_for_tuner_channel(&provider.channel_mappings, &tuner, &epg)
                        .and_then(|matched| matched.image_url.as_deref())
                        .filter(|url| !url.trim().is_empty())
                {
                    channel.image_url = Some(url.to_owned());
                }
            }
        }
    }
}

/// [`PROGRAM_SELECT`]'s `FROM`/`JOIN` counting instead of selecting, so the
/// total-record count runs the identical filter set.
const PROGRAM_COUNT: &str = r#"SELECT COUNT(*)
               FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId""#;

/// The `ON CONFLICT` tail of the channel upsert: a channel already in the
/// lineup keeps its first-seen `DateCreated` (upstream stamps it only on a NEW
/// `LiveTvChannel`; a pre-migration NULL heals to the refresh instant via the
/// COALESCE) and takes the playlist's current word for everything else.
const CHANNEL_UPSERT_CONFLICT: &str = r#" ON CONFLICT("Id") DO UPDATE SET
                    "DateCreated"=COALESCE("FerrofinLiveTvChannels"."DateCreated", excluded."DateCreated"),
                    "TunerHostId"=excluded."TunerHostId","TvgId"=excluded."TvgId",
                    "Name"=excluded."Name","Number"=excluded."Number",
                    "ImageUrl"=excluded."ImageUrl","ChannelType"=excluded."ChannelType",
                    "StreamUrl"=excluded."StreamUrl","SortIndex"=excluded."SortIndex",
                    "ExternalId"=excluded."ExternalId","IsHd"=excluded."IsHd",
                    "VideoCodec"=excluded."VideoCodec","AudioCodec"=excluded."AudioCodec""#;

/// The `ON CONFLICT` tail of the programme upsert: a refreshed airing keeps
/// its first-seen `DateCreated` (upstream stamps it only on a NEW
/// `LiveTvProgram`; a pre-migration NULL heals to the refresh instant via the
/// COALESCE), while everything else is the listing's current word.
const PROGRAM_UPSERT_CONFLICT: &str = r#" ON CONFLICT("Id") DO UPDATE SET
                    "DateCreated"=COALESCE("FerrofinLiveTvPrograms"."DateCreated", excluded."DateCreated"),
                    "ChannelId"=excluded."ChannelId","StartDate"=excluded."StartDate",
                    "EndDate"=excluded."EndDate","Title"=excluded."Title",
                    "EpisodeTitle"=excluded."EpisodeTitle","Overview"=excluded."Overview",
                    "Genres"=excluded."Genres","ImageUrl"=excluded."ImageUrl",
                    "ProductionYear"=excluded."ProductionYear","EpisodeNum"=excluded."EpisodeNum",
                    "IsNew"=excluded."IsNew","IsPremiere"=excluded."IsPremiere",
                    "IsRepeat"=excluded."IsRepeat","OfficialRating"=excluded."OfficialRating",
                    "IsMovie"=excluded."IsMovie","IsSeries"=excluded."IsSeries",
                    "IsNews"=excluded."IsNews","IsKids"=excluded."IsKids",
                    "IsSports"=excluded."IsSports","ExternalId"=excluded."ExternalId",
                    "IsLive"=excluded."IsLive","ExternalSeriesId"=excluded."ExternalSeriesId",
                    "SeasonNumber"=excluded."SeasonNumber","EpisodeNumber"=excluded."EpisodeNumber",
                    "ShowId"=excluded."ShowId""#;

/// The on-disk locations the Live TV engine reads and writes.
///
/// Ports the three application paths the C# Live TV code reaches for:
/// `IConfigurationManager.GetTranscodePath()` (the live-stream buffer),
/// `CommonApplicationPaths.DataPath` (`livetv/recordings`), and the
/// `livetv` named-configuration file that holds `LiveTvOptions`.
#[derive(Debug, Clone, Default)]
pub struct LiveTvPaths {
    /// Where a shared live stream's `{uniqueId}.ts` buffer is written
    /// (C# `GetTranscodePath()`).
    pub transcode_dir: ferrofin_util::directory_path::DirectoryPath,
    /// The server data directory, under which `livetv/recordings` is the
    /// default DVR target (C# `CommonApplicationPaths.DataPath`).
    pub data_dir: PathBuf,
    /// The `named/livetv.json` file holding the dashboard's `LiveTvOptions`.
    pub options_file: PathBuf,
}

/// Concrete Live TV manager backed by [`Database`] and a [`SourceFetcher`].
#[derive(Clone)]
pub struct FerrofinLiveTvManager {
    db: Database,
    server_id: String,
    /// The user manager, for `LiveTvInfo.EnabledUsers` (C# `IUserManager.Users`
    /// filtered by the `EnableLiveTvAccess` permission). Absent in unit tests
    /// that never ask for it.
    users: Option<Arc<dyn ferrofin_traits::library::UserManager>>,
    /// Whether any tuner host is configured, kept current by
    /// [`LiveTvManager::save_tuner_host`]/[`LiveTvManager::delete_tuner_host`]
    /// and every [`LiveTvManager::get_tuner_hosts`] read. Backs the
    /// synchronous [`LiveTvManager::has_tuner_hosts`] the "Refresh Guide"
    /// task's hidden rule polls.
    tuner_flag: Arc<AtomicBool>,
    /// The DTO service the channel/programme projections run through — the C#
    /// `LiveTvManager` holds `IDtoService` the same way. A `OnceLock` because
    /// the composition root has a cycle to break (`DtoService` needs the
    /// media-source manager, which needs this manager): it is set via
    /// [`FerrofinLiveTvManager::set_dto`] once the DTO service exists, the way
    /// C# breaks the same cycle with `Lazy<ILiveTvManager>`.
    dto: OnceLock<Arc<dyn DtoService>>,
    /// Where the live-stream buffer and DVR recordings live.
    paths: LiveTvPaths,
    /// The tuner HTTP seam the live-stream engine opens channels through.
    tuner_source: Arc<dyn TunerStreamSource>,
    /// Where a live stream's buffered file and an in-progress recording are
    /// served from — C# `IServerApplicationHost.GetApiUrlForLocalAccess()`,
    /// asked per stream as upstream does, so it follows the bound listener and
    /// the live network settings. An `Arc<OnceLock<_>>` because the composition
    /// root builds the application host *after* this manager, and every clone
    /// must see it once it lands.
    local_api: Arc<OnceLock<Arc<dyn LocalApiAccess>>>,
    /// The server's local-network check, shared with the M3U tuner host.
    local_network: Arc<OnceLock<crate::stream::LocalNetworkCheck>>,
    /// The open live streams, keyed by `LiveStreamId` (C#
    /// `MediaSourceManager._openStreams`, whose values are the tuner-side
    /// `ILiveStream`s). Guarded by a `std::sync::Mutex`: the guard is always
    /// dropped before an `.await`.
    live_streams: Arc<Mutex<HashMap<String, LiveStreamHandle>>>,
    /// The recordings being captured right now, keyed by the FIRING TIMER's id
    /// (C# `RecordingsManager._activeRecordings`, whose key is `timer.Id`).
    active_recordings: Arc<Mutex<HashMap<String, ActiveRecording>>>,
    /// The timers armed to fire, keyed by timer id (C# `TimerManager._timers`,
    /// a `System.Threading.Timer` each). The value is the flag that cancels a
    /// *pending* fire — see [`FerrofinLiveTvManager::arm_timer`] for why the
    /// task is never aborted.
    armed_timers: Arc<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
    /// How many times each timer has been retried after a failed capture (C#
    /// `TimerInfo.RetryCount`, which upstream persists; a restart re-arms the
    /// timer anyway, so in memory is enough).
    retry_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// How many times in a row each fired timer's row could not be read, kept
    /// apart from the capture retries in `retry_counts` — see
    /// [`FerrofinLiveTvManager::retry_unread_timer`].
    read_retry_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// The timers whose fire is checking for, or starting, their capture —
    /// see [`FiringClaim`].
    firing: Arc<Mutex<HashSet<String>>>,
    /// The stored rows already reported as unparseable, by table — see
    /// [`FerrofinLiveTvManager::parse_stored_row`].
    unparseable_rows: Arc<Mutex<HashMap<&'static str, HashSet<String>>>>,
    /// The Live TV options file last reported as unusable, by path and
    /// modification time — see [`FerrofinLiveTvManager::live_tv_options`].
    options_warned: Arc<Mutex<Option<WarnedFile>>>,
    /// The media encoder, for the encoded recorder's ffmpeg. Absent in tests
    /// and on a server with no ffmpeg, where only the direct recorder runs.
    encoder: Option<Arc<dyn MediaEncoder>>,
    /// Brackets each recording as it is written, so a recording inside a
    /// watched library is not rescanned mid-write (`RecordingsManager`'s
    /// `ReportFileSystemChangeBeginning`/`Complete`). `None` in unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
    /// The NFO options' `ReleaseDateFormat`, for a recording's sidecar NFO
    /// ([`with_release_date_format`](Self::with_release_date_format)).
    release_date_format: crate::recording_metadata::ReleaseDateFormat,
    /// The server's `PreferredMetadataLanguage`, read live
    /// ([`with_metadata_language`](Self::with_metadata_language)).
    metadata_language: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// Serializes "join or open", so two clients tuning the same channel at
    /// once cannot both miss the join and both dial the tuner.
    ///
    /// Port of `MediaSourceManager._liveStreamLocker` (an
    /// `AsyncNonKeyedLocker(1)` around the whole of `OpenLiveStreamInternal`);
    /// a `tokio::sync::Mutex` because this one *is* held across awaits.
    open_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes the WRITE half of a guide refresh, so two passes can never
    /// interleave their inserts and prunes.
    ///
    /// Port of `_taskManager.CancelIfRunningAndQueue<RefreshGuideScheduledTask>()`:
    /// upstream there is exactly ONE guide-refresh task, and queuing it cancels
    /// the running one. Ferrofin reaches `refresh_guide` from the scheduled task
    /// AND from the queued refresh the tuner-host/listings-provider writes
    /// trigger, so without this two passes could overlap and one's
    /// `CleanDatabase` prune would delete the listings the other had just
    /// written.
    ///
    /// It is taken AFTER every fetch has returned, never around one: upstream
    /// cancels the in-flight refresh, we queue behind it (a pass a newer one
    /// supersedes skips its write, see `refresh_generation`), and queuing behind
    /// a lock that is waiting on a third-party HTTP source is how one wedged
    /// tuner URL stalls the whole subsystem. It does cover each guide document's
    /// second, local read (`insert_programs` keeps only the airings of the
    /// channels the stored lineup binds), which is bounded by the document,
    /// not by a remote source.
    guide_lock: Arc<tokio::sync::Mutex<()>>,
    /// Counts guide refreshes, so a newer one supersedes an older one still
    /// fetching: the half of `CancelIfRunningAndQueue` the write lock does not
    /// give.
    ///
    /// Every refresh takes the next generation. A pass that finds a newer
    /// generation stops asking Schedules Direct for more stations and skips its
    /// write half; the newer pass rewrites the guide from the current
    /// configuration. Without it, the two saves of jellyfin-web's SD setup and
    /// every mapping edit would each run a full SD pass, up to three requests
    /// per bound station, against SD's daily request limit.
    refresh_generation: Arc<AtomicU64>,
    /// The Schedules Direct listings provider (and its country list), sharing
    /// the fetcher and caching under the application cache directory.
    schedules_direct: SchedulesDirect,
    /// The XMLTV document cache every guide reader goes through
    /// (`XmlTvListingsProvider.GetXml`).
    xmltv: crate::xmltv_cache::XmlTvCache,
    /// The `BaseItems` seam the guide refresh mirrors the channel lineup
    /// through, and the Live TV `UserView` the rows parent to.
    ///
    /// A `OnceLock` for the same composition-root cycle the `dto` field breaks:
    /// the item store and the user-view manager are both built after this
    /// manager. Absent in unit tests, where the guide refresh simply writes no
    /// item rows — the same state a server has before its first guide refresh.
    item_store: OnceLock<LiveTvItemStore>,
    /// The image services: the remote-image cache a guide refresh warms
    /// (`GuideManager.PreCacheImages`) and the processor that tags a channel's
    /// logo (`ChannelPrimaryImageTag`). A `OnceLock` like `item_store`; absent,
    /// nothing is pre-cached and no programme carries a channel image tag.
    images: OnceLock<LiveTvImages>,
    /// The resolved Live TV `UserView` id, memoized after the first successful
    /// lookup (it is stable for the life of the database).
    live_tv_view: OnceLock<Uuid>,
    /// The registered tuner backends, one per tuner *kind*.
    ///
    /// Port of `TunerHostManager._tunerHosts` (v10.11.8 TunerHostManager.cs:44),
    /// which is the DI-registered `IEnumerable<ITunerHost>` filtered to
    /// `IsSupported`. It is the single source for the advertised type list, the
    /// save-time type lookup, the guide refresh's per-tuner dispatch and the
    /// media-source/stream paths — so a tuner kind is added by registering an
    /// implementation, never by adding a `match`.
    hosts: Vec<Arc<dyn crate::tuner_host::TunerHost>>,
}

/// The item-layer services the guide refresh needs to keep `BaseItems` in step
/// with the channel lineup.
#[derive(Clone)]
struct LiveTvItemStore {
    /// Writes and deletes the channel item rows.
    persistence: Arc<dyn ItemPersistenceService>,
    /// Reads back the channel rows already stored, for the delete half of
    /// `GuideManager.CleanDatabase`.
    items: Arc<dyn ItemRepository>,
    /// Resolves (and provisions) the Live TV `UserView` the rows parent to.
    views: Arc<dyn UserViewManager>,
}

/// The image services the guide uses; see [`FerrofinLiveTvManager::images`].
#[derive(Clone)]
struct LiveTvImages {
    /// Downloads remote images into the remote-image cache.
    providers: Arc<dyn ProviderManager>,
    /// Computes an image's cache tag.
    processor: Arc<dyn ImageProcessor>,
}

impl std::fmt::Debug for FerrofinLiveTvManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLiveTvManager")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// A registered listings-provider backend (`IListingsProvider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingsBackend {
    /// `XmlTvListingsProvider` (`Type` `xmltv`).
    XmlTv,
    /// `SchedulesDirect` (`Type` `SchedulesDirect`).
    SchedulesDirect,
}

impl ListingsBackend {
    /// Port of `ListingsManager.GetProvider`: the backend registered for a
    /// `Type` (ignoring case), or `ResourceNotFoundException` (`404`) when
    /// there is none, a missing `Type` included.
    fn of(provider_type: Option<&str>) -> Result<Self, ServiceError> {
        match provider_type {
            Some(t) if eq_ordinal_ignore_case(t, "xmltv") => Ok(Self::XmlTv),
            Some(t) if eq_ordinal_ignore_case(t, crate::schedules_direct::PROVIDER_TYPE) => {
                Ok(Self::SchedulesDirect)
            }
            other => Err(ServiceError::not_found(format!(
                "Couldn't find provider of type {}",
                other.unwrap_or_default()
            ))),
        }
    }

    /// The backend's display name (`IListingsProvider.Name`).
    fn name(self) -> &'static str {
        match self {
            Self::XmlTv => "XmlTV",
            Self::SchedulesDirect => crate::schedules_direct::PROVIDER_NAME,
        }
    }
}

/// A tuner channel as the match ladder sees it (`ChannelInfo`'s `Id`,
/// `TunerChannelId`, `Number` and `Name`), from its lineup entry: the stream
/// URL, the guide key the tuner stores as `TvgId`, the number and the name.
fn matcher_channel(
    tuner_url: &str,
    tuner_host_id: &str,
    stream_url: &str,
    guide_key: String,
    number: String,
    name: String,
) -> TunerChannel {
    TunerChannel {
        id: m3u_channel_id(tuner_url, stream_url),
        tuner_channel_id: guide_key,
        number,
        name,
        tuner_host_id: tuner_host_id.to_owned(),
    }
}

/// A Schedules Direct station as the match ladder sees it.
fn sd_epg_channel(channel: SdChannel) -> EpgChannel {
    EpgChannel {
        id: channel.id.unwrap_or_default(),
        name: channel.name,
        number: channel.number,
        image_url: channel.image_url,
    }
}

/// What one listings provider supplied to a guide refresh, fetched before the
/// write lock is taken.
enum GuideSource {
    /// An XMLTV document and its `<channel>` list; the airings are read in the
    /// write half (`insert_programs`).
    XmlTv {
        document: XmlTvDocument,
        channels: Vec<XmltvChannel>,
    },
    /// Schedules Direct: the lineup's stations and the airings of each station
    /// a tuner channel binds to.
    SchedulesDirect {
        channels: Vec<EpgChannel>,
        programs: HashMap<String, Vec<SdProgram>>,
    },
}

/// The programme columns a listings provider supplies as they are, borrowed
/// from its own record (an XMLTV `<programme>` or a Schedules Direct
/// `ProgramInfo`).
struct ProgramFields<'a> {
    title: &'a str,
    overview: Option<&'a str>,
    image_url: Option<&'a str>,
    production_year: Option<i32>,
    episode_num: Option<&'a str>,
    is_new: bool,
    is_premiere: bool,
    official_rating: Option<&'a str>,
    is_live: bool,
}

impl<'a> ProgramFields<'a> {
    fn of_xmltv(prog: &'a XmltvProgramme) -> Self {
        Self {
            title: &prog.title,
            overview: prog.desc.as_deref(),
            image_url: prog.icon.as_deref(),
            production_year: prog.year,
            episode_num: prog.episode_num.as_deref(),
            is_new: prog.is_new,
            is_premiere: prog.is_premiere,
            official_rating: prog.rating.as_deref(),
            is_live: prog.is_live,
        }
    }

    /// The columns `GuideManager.GetProgram` takes from a Schedules Direct
    /// `ProgramInfo`. `IsNew` stays unset: the SD provider never sets it
    /// (the line is commented out upstream).
    fn of_schedules_direct(program: &'a SdProgram) -> Self {
        Self {
            title: &program.name,
            overview: program.overview.as_deref(),
            image_url: program.image_url.as_deref(),
            production_year: program.production_year,
            episode_num: None,
            is_new: false,
            is_premiere: program.is_premiere,
            official_rating: program.official_rating.as_deref(),
            is_live: program.is_live,
        }
    }
}

/// The derived classification of a Schedules Direct airing, as
/// `GuideManager.GetProgram` stores it: `IsSeries` widened by a non-empty
/// episode title, the rest as the provider set it.
fn sd_program_class(program: &SdProgram) -> ProgramClass {
    ProgramClass {
        is_movie: program.is_movie,
        is_series: program.is_series
            || program
                .episode_title
                .as_deref()
                .is_some_and(|title| !title.is_empty()),
        is_news: program.is_news,
        is_kids: program.is_kids,
        is_sports: program.is_sports,
        is_repeat: program.is_repeat,
        episode_title: program.episode_title.clone(),
        season_number: program.season_number,
        episode_number: program.episode_number,
        external_id: Some(program.id.clone()),
        external_series_id: program.series_id.clone().filter(|id| !id.trim().is_empty()),
        show_id: program.show_id.clone(),
    }
}

/// The programmes one channel's turn of the guide mirror created or updated,
/// each with its end, for `PreCacheImages`.
type RefreshedAirings = Vec<(Uuid, Option<DateTime<Utc>>)>;

/// A guide write's rows, built as the write draws them.
///
/// Boxed so the iterator's closure types never reach the futures holding it
/// across `write_program_rows`' awaits: as plain types they fail rustc's
/// higher-ranked `Send` check on the `async_trait` refresh future ("not
/// general enough"), with or without annotated closure arguments.
type ProgramRows<'a> = Box<dyn Iterator<Item = ProgramRow<'a>> + Send + 'a>;

/// One `(channel, programme)` binding ready to insert.
struct ProgramRow<'a> {
    /// The programme item's id (`LiveTvDtoService.GetInternalProgramId`).
    id: Uuid,
    channel_id: &'a String,
    /// The provider's programme id suffixed with `_{channelExternalId}`
    /// (`ListingsManager.GetProgramsAsync`) — the listings-facing id the item
    /// GUID is derived from.
    external_id: Option<String>,
    start: String,
    end: Option<String>,
    genres: Option<String>,
    class: ProgramClass,
    fields: ProgramFields<'a>,
    /// The listing's artwork, for the item mirror's `UpdateImages`.
    images: ProgramImages,
}

/// What `XmlTvListingsProvider.GetProgramInfo` derives for one airing beyond
/// the raw XMLTV fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // they are the upstream ProgramInfo flags
struct ProgramClass {
    is_movie: bool,
    is_series: bool,
    is_news: bool,
    is_kids: bool,
    is_sports: bool,
    is_repeat: bool,
    /// `ProgramInfo.EpisodeTitle` — cleared for movies.
    episode_title: Option<String>,
    season_number: Option<i32>,
    /// Cleared for movies.
    episode_number: Option<i32>,
    /// `ProgramInfo.Id`: `{channelId}_{start:O}`.
    external_id: Option<String>,
    /// `ProgramInfo.SeriesId`: the title's MD5 (`N`) when the airing is an episode.
    external_series_id: Option<String>,
    /// `ProgramInfo.ShowId` — the showing-identity key a DVR series rule
    /// matches on, derived from the airing's PRE-movie-reset fields.
    show_id: String,
}

/// The listings provider's category lists (`ListingsProviderInfo`), each
/// falling back to Jellyfin's defaults when the provider carries none.
struct CategoryClasses {
    news: Vec<String>,
    sports: Vec<String>,
    kids: Vec<String>,
    movie: Vec<String>,
}

impl CategoryClasses {
    fn from_provider(provider: &ListingsProviderInfo) -> Self {
        let defaults = ListingsProviderInfo::default();
        let pick = |own: &Option<Vec<String>>, default: &Option<Vec<String>>| {
            own.clone().or_else(|| default.clone()).unwrap_or_default()
        };
        Self {
            news: pick(&provider.news_categories, &defaults.news_categories),
            sports: pick(&provider.sports_categories, &defaults.sports_categories),
            kids: pick(&provider.kids_categories, &defaults.kids_categories),
            movie: pick(&provider.movie_categories, &defaults.movie_categories),
        }
    }

    /// `programCategories.Any(c => list.Contains(c, OrdinalIgnoreCase))` —
    /// per-character case folding, so non-ASCII names compare as .NET does.
    fn any_in(list: &[String], categories: &[String]) -> bool {
        let fold = |s: &str| s.chars().flat_map(char::to_uppercase).collect::<String>();
        categories
            .iter()
            .any(|c| list.iter().any(|l| fold(l) == fold(c)))
    }

    /// `ProgramInfo.ShowId` for one airing, as
    /// `XmlTvListingsProvider.GetProgramInfo` computes it (v10.11.8
    /// XmlTvListingsProvider.cs:185-212).
    ///
    /// The block runs BEFORE the `IsMovie` reset four lines below it, so the
    /// inputs are the RAW `<sub-title>`/episode number even for a movie, and
    /// `IsSeries` is still the provider's own `Episode.Episode is not null`
    /// rather than the widened value `GuideManager.GetProgram` later stores.
    /// That is why the value is computed here, at ingest, and persisted: the
    /// stored row has already had a movie's episode fields cleared and can no
    /// longer reproduce it.
    fn show_id(
        prog: &crate::xmltv::XmltvProgramme,
        season_number: Option<i32>,
        episode_number: Option<i32>,
    ) -> String {
        derive_show_id(
            &prog.title,
            prog.sub_title.as_deref(),
            season_number,
            episode_number,
            // `programInfo.IsSeries` at this point in the C# is the provider's
            // `program.Episode.Episode is not null`.
            episode_number.is_some(),
            prog.is_previously_shown && !prog.is_new,
            prog.start.map(|start| start.with_timezone(&Utc)),
        )
    }

    /// Port of the derived part of `XmlTvListingsProvider.GetProgramInfo`.
    fn classify(&self, prog: &crate::xmltv::XmltvProgramme) -> ProgramClass {
        let categories: Vec<String> = prog
            .categories
            .iter()
            .filter(|c| !c.trim().is_empty())
            .cloned()
            .collect();
        let (season_number, episode_number) = (prog.season_number, prog.episode_number);
        let is_movie = Self::any_in(&self.movie, &categories);
        // The provider's `IsSeries = Episode is not null` is widened by
        // `GuideManager.GetProgram` to `|| !IsNullOrEmpty(EpisodeTitle)`; a movie
        // clears the episode number and title first, so it stays false.
        let episode_title = if is_movie {
            None
        } else {
            prog.sub_title.clone()
        };
        let is_series = !is_movie && (episode_number.is_some() || episode_title.is_some());
        // `program.Title?.GetMD5()`: no title, no series id.
        let external_series_id = episode_number.filter(|_| !prog.title.is_empty()).map(|_| {
            ferrofin_common::extensions::get_md5(&prog.title)
                .simple()
                .to_string()
        });
        ProgramClass {
            show_id: Self::show_id(prog, season_number, episode_number),
            is_movie,
            is_series,
            is_news: Self::any_in(&self.news, &categories),
            is_kids: Self::any_in(&self.kids, &categories),
            is_sports: Self::any_in(&self.sports, &categories),
            is_repeat: prog.is_previously_shown && !prog.is_new,
            episode_title,
            season_number,
            episode_number: if is_movie { None } else { episode_number },
            // `XmlTvListingsProvider.GetProgramInfo`: `$"{channelId}_{start:O}"`
            // (v10.11.8 XmlTvListingsProvider.cs:216). `{0:O}` on a
            // `DateTimeOffset` is `yyyy-MM-ddTHH:mm:ss.fffffff` plus the source
            // offset as `+HH:mm` — never `Z`, and never normalised to UTC, so a
            // `+0100` guide really does render `+01:00`. `ListingsManager`
            // appends `"_" + channel.Id` afterwards (ListingsManager.cs:151-155);
            // that half needs the owning channel, so it is added at the
            // insert site.
            external_id: prog.start.map(|start| {
                format!(
                    "{}_{}.{:07}{}",
                    prog.channel_id,
                    start.format("%Y-%m-%dT%H:%M:%S"),
                    start.timestamp_subsec_nanos() / 100,
                    start.offset()
                )
            }),
            external_series_id,
        }
    }
}

/// A fired timer's claim on its capture, held from the in-progress check
/// until `record_stream` returns, which is after the whole capture (hours,
/// for a long showing) or its failure. A second fire of the same timer
/// meanwhile finds the claim taken and stands down. Dropping the claim, a
/// panic's unwind included, releases the timer.
struct FiringClaim {
    firing: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl FiringClaim {
    /// Claims timer `id`, unless a fire of it already holds the claim.
    fn take(firing: &Arc<Mutex<HashSet<String>>>, id: &str) -> Option<Self> {
        firing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned())
            .then(|| Self {
                firing: Arc::clone(firing),
                id: id.to_owned(),
            })
    }
}

impl Drop for FiringClaim {
    fn drop(&mut self) {
        self.firing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// Reads an XMLTV document from its file on the blocking pool, keeping the
/// programmes `keep` accepts, and hands the document back with the result.
///
/// A large IPTV guide is hundreds of megabytes and takes seconds to parse, and
/// a guide refresh reads each document twice (channels, then the bound
/// channels' airings), so neither read runs on a runtime worker. The file is
/// read as a stream, as `XmlTvReader` reads it (Jellyfin.XmlTv
/// `XmlTvReader.GetChannels`/`GetProgrammes`), so neither read holds it whole.
async fn parse_guide(
    document: XmlTvDocument,
    language: Option<String>,
    keep: impl FnMut(&XmltvProgramme) -> bool + Send + 'static,
) -> Result<(XmlTvDocument, Xmltv), ServiceError> {
    tokio::task::spawn_blocking(move || {
        let guide = match document.reader() {
            Ok(reader) => read_xmltv_where(reader, language.as_deref(), keep),
            Err(e) => Xmltv {
                error: Some(format!("the XMLTV document could not be read: {e}")),
                ..Xmltv::default()
            },
        };
        (document, guide)
    })
    .await
    .map_err(|e| ServiceError::Backend(format!("XMLTV parse task failed: {e}")))
}

impl FerrofinLiveTvManager {
    /// Replaces the registered tuner hosts.
    ///
    /// Test-only seam. The real set is upstream's fixed DI registration
    /// (`LiveTvServiceCollectionExtensions.AddLiveTvServices`), so there is no
    /// production reason to swap it; a test needs one to drive
    /// `discover_tuners` without broadcasting on the host's network.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_tuner_hosts(
        mut self,
        hosts: Vec<Arc<dyn crate::tuner_host::TunerHost>>,
    ) -> Self {
        self.hosts = hosts;
        self
    }

    /// Creates the manager over the given database and source fetcher, caching
    /// Schedules Direct documents under `cache_dir` (the application cache
    /// path — `IApplicationPaths.CachePath` upstream).
    #[must_use]
    pub fn new(
        db: Database,
        fetcher: Arc<dyn SourceFetcher>,
        server_id: String,
        cache_dir: impl Into<ferrofin_util::directory_path::DirectoryPath>,
    ) -> Self {
        let cache_dir: ferrofin_util::directory_path::DirectoryPath = cache_dir.into();
        let xmltv = crate::xmltv_cache::XmlTvCache::new(Arc::clone(&fetcher), cache_dir.clone());
        // The same two hosts, in the same order, that
        // `LiveTvServiceCollectionExtensions.AddLiveTvServices` registers
        // (v10.11.8 LiveTvServiceCollectionExtensions.cs:41-42): HDHomeRun
        // then M3U. The ORDER of registration is not the advertised order —
        // `GetTunerHostTypes` sorts by name — but keeping it identical means a
        // future first-match rule cannot silently diverge.
        let local_network = Arc::new(OnceLock::new());
        let hosts: Vec<Arc<dyn crate::tuner_host::TunerHost>> = vec![
            Arc::new(crate::hdhomerun::HdHomerunHost::new(Arc::clone(&fetcher))),
            Arc::new(
                crate::tuner_host::M3uTunerHost::new(Arc::clone(&fetcher))
                    .sharing_local_network(Arc::clone(&local_network)),
            ),
        ];
        let schedules_direct = SchedulesDirect::new(fetcher, cache_dir);
        Self {
            db,
            hosts,
            users: None,
            server_id,
            item_store: OnceLock::new(),
            images: OnceLock::new(),
            live_tv_view: OnceLock::new(),
            tuner_flag: Arc::new(AtomicBool::new(false)),
            dto: OnceLock::new(),
            paths: LiveTvPaths::default(),
            tuner_source: Arc::new(crate::stream::ReqwestTunerSource::new()),
            local_api: Arc::new(OnceLock::new()),
            local_network,
            live_streams: Arc::new(Mutex::new(HashMap::new())),
            active_recordings: Arc::new(Mutex::new(HashMap::new())),
            armed_timers: Arc::new(Mutex::new(HashMap::new())),
            retry_counts: Arc::new(Mutex::new(HashMap::new())),
            read_retry_counts: Arc::new(Mutex::new(HashMap::new())),
            firing: Arc::new(Mutex::new(HashSet::new())),
            unparseable_rows: Arc::new(Mutex::new(HashMap::new())),
            options_warned: Arc::new(Mutex::new(None)),
            encoder: None,
            self_writes: None,
            release_date_format: crate::recording_metadata::default_release_date_format(),
            metadata_language: None,
            open_lock: Arc::new(tokio::sync::Mutex::new(())),
            guide_lock: Arc::new(tokio::sync::Mutex::new(())),
            refresh_generation: Arc::new(AtomicU64::new(0)),
            schedules_direct,
            xmltv,
        }
    }

    /// Brackets each recording on `guard` from the moment its file is chosen
    /// until it is settled, as `RecordingsManager.RecordStream` does. Called
    /// once by the composition root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// Gives the M3U tuner host the server's local-network check
    /// (`NetworkManager.IsInLocalNetwork(string)`), which decides whether a
    /// channel's source is remote. Without it, only private IP literals count
    /// as local.
    #[must_use]
    pub fn with_local_network(self, check: crate::stream::LocalNetworkCheck) -> Self {
        // Set once, before the manager is shared; a second call keeps the first.
        let _ = self.local_network.set(check);
        self
    }

    /// Attaches the media encoder the encoded recorder runs ffmpeg through.
    ///
    /// Without one, a source that needs remuxing cannot be recorded at all —
    /// which is also true of upstream on a server with no ffmpeg.
    #[must_use]
    pub fn with_encoder(mut self, encoder: Arc<dyn MediaEncoder>) -> Self {
        self.encoder = Some(encoder);
        self
    }

    /// Writes a recording NFO's dates in the NFO options' `ReleaseDateFormat`
    /// (the `xbmcmetadata` named configuration). Without one they are written
    /// `yyyy-MM-dd`, that option's default.
    #[must_use]
    pub fn with_release_date_format(
        mut self,
        format: crate::recording_metadata::ReleaseDateFormat,
    ) -> Self {
        self.release_date_format = format;
        self
    }

    /// Reads the server's `PreferredMetadataLanguage` through `language`, each
    /// time a guide is read, so a changed setting applies to the next read.
    /// It is the language an XMLTV guide is read in when its provider names
    /// none ([`guide_language`](Self::guide_language)). Without one (tests) a
    /// guide is read with no language.
    #[must_use]
    pub fn with_metadata_language(
        mut self,
        language: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        self.metadata_language = Some(language);
        self
    }

    /// The language an XMLTV guide is read in: `XmlTvListingsProvider.GetLanguage`
    /// (v12.2 :53-60), the provider's `PreferredLanguage` unless it is blank,
    /// else the server's `PreferredMetadataLanguage`.
    fn guide_language(&self, provider: &ListingsProviderInfo) -> Option<String> {
        provider
            .preferred_language
            .clone()
            .filter(|language| !language.trim().is_empty())
            .or_else(|| self.metadata_language.as_ref().map(|language| language()))
    }

    /// Sets the on-disk locations the live-stream buffer and DVR recordings use.
    #[must_use]
    pub fn with_paths(mut self, paths: LiveTvPaths) -> Self {
        self.paths = paths;
        self
    }

    /// Replaces the tuner HTTP seam (tests serve an in-memory broadcast).
    #[must_use]
    pub fn with_tuner_source(mut self, source: Arc<dyn TunerStreamSource>) -> Self {
        self.tuner_source = source;
        self
    }

    /// Attaches the host that answers where this server's API is reached
    /// locally (C# `GetApiUrlForLocalAccess()`), which the live stream buffer
    /// and in-progress recordings are served from.
    ///
    /// The composition root calls this once the application host exists; a
    /// second call is ignored.
    pub fn set_local_api_access(&self, access: Arc<dyn LocalApiAccess>) {
        let _ = self.local_api.set(access);
    }

    /// Attaches the user manager so `GET /LiveTv/Info` can list the users who may
    /// use Live TV (the composition root wires it; tests may leave it off).
    #[must_use]
    pub fn with_users(mut self, users: Arc<dyn ferrofin_traits::library::UserManager>) -> Self {
        self.users = Some(users);
        self
    }

    /// Attaches the DTO service the channel/programme projections run through
    /// (builder form, for tests and simple wiring).
    #[must_use]
    pub fn with_dto(self, dto: Arc<dyn DtoService>) -> Self {
        let _ = self.dto.set(dto);
        self
    }

    /// Attaches the DTO service after construction — the composition root's
    /// form, because the DTO service is built later than this manager (see the
    /// field doc). A second call is ignored.
    pub fn set_dto(&self, dto: Arc<dyn DtoService>) {
        let _ = self.dto.set(dto);
    }

    /// Attaches the item layer the guide refresh mirrors channels into: the
    /// persistence service that writes the rows, the repository that reads back
    /// the ones already stored, and the user-view manager that resolves the
    /// Live TV `UserView` they parent to.
    ///
    /// The composition-root form (a second call is ignored), for the same
    /// reason [`set_dto`](Self::set_dto) exists: all three are constructed
    /// after this manager.
    pub fn set_item_store(
        &self,
        persistence: Arc<dyn ItemPersistenceService>,
        items: Arc<dyn ItemRepository>,
        views: Arc<dyn UserViewManager>,
    ) {
        let _ = self.item_store.set(LiveTvItemStore {
            persistence,
            items,
            views,
        });
    }

    /// Attaches the image services (see the `images` field). The
    /// composition-root form; a second call is ignored.
    pub fn set_images(
        &self,
        providers: Arc<dyn ProviderManager>,
        processor: Arc<dyn ImageProcessor>,
    ) {
        let _ = self.images.set(LiveTvImages {
            providers,
            processor,
        });
    }

    /// Port of `GuideManager.PreCacheImages` (v12.2 GuideManager.cs:750-805):
    /// the remote images of the programmes this refresh created or updated
    /// and that end within [`PRE_CACHE_DAYS`] are downloaded ahead of the
    /// first request, at most [`PRE_CACHE_PARALLELISM`] (or the core count,
    /// when lower) at a time. A failed download only means the image is
    /// fetched on its first request instead. The Schedules Direct daily
    /// image limit upstream checks here has no provider to come from yet
    /// (gl-413).
    ///
    /// `programs` come one group per channel, and each group's images are
    /// read when its turn comes, as upstream pre-caches each channel's
    /// programmes once it has stored them.
    async fn pre_cache_images(&self, programs: &[RefreshedAirings]) {
        let (Some(store), Some(images)) = (self.item_store.get(), self.images.get()) else {
            return;
        };
        let max_cache_date = Utc::now() + chrono::Duration::days(PRE_CACHE_DAYS);
        let parallelism = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(PRE_CACHE_PARALLELISM);
        // Once per URL: many airings share a series' poster, and two downloads
        // of one URL would race.
        let mut seen: HashSet<String> = HashSet::new();
        let mut failed = 0_usize;
        let mut downloads = tokio::task::JoinSet::new();
        for group in programs {
            let ids: Vec<Uuid> = group
                .iter()
                .filter(|(_, end)| end.is_some_and(|end| end < max_cache_date))
                .map(|(id, _)| *id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let rows = match store.items.get_image_infos_batch(&ids).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, "live tv: could not read the programme images to pre-cache");
                    break;
                }
            };
            let paths = rows
                .into_values()
                .flatten()
                .filter(|image| !image.is_local_file())
                .map(|image| image.path);
            for path in paths {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if downloads.len() >= parallelism
                    && let Some(Ok(false)) = downloads.join_next().await
                {
                    failed += 1;
                }
                let providers = Arc::clone(&images.providers);
                downloads.spawn(async move {
                    providers
                        .cached_remote_image(&path)
                        .await
                        .inspect_err(|error| {
                            tracing::debug!(%error, "live tv: could not pre-cache a programme image");
                        })
                        .is_ok()
                });
            }
        }
        while let Some(done) = downloads.join_next().await {
            if matches!(done, Ok(false)) {
                failed += 1;
            }
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                "live tv: some programme images could not be pre-cached"
            );
        }
    }

    /// The `ChannelPrimaryImageTag` of each channel among `rows` that has a
    /// Primary image (`LiveTvDtoService.GetImageTag`), by channel id.
    async fn channel_image_tags(&self, rows: &[&GuideProgramRow]) -> HashMap<Uuid, String> {
        let mut tags = HashMap::new();
        let (Some(store), Some(images)) = (self.item_store.get(), self.images.get()) else {
            return tags;
        };
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.channel_id).ok())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let stored = match store.items.get_image_infos_batch(&ids).await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::debug!(%error, "live tv: could not read the channel images to tag");
                return tags;
            }
        };
        for (id, channel_images) in stored {
            let Some(primary) = channel_images
                .iter()
                .find(|image| image.image_type == ImageType::Primary)
            else {
                continue;
            };
            // `GetImageTag` logs and answers null on a failure. Debug, not
            // upstream's error: it would repeat on every guide page. The tag
            // hashes the channel item's path, and a channel item has none
            // (`GuideManager.GetChannel` sets no `Path`; nor does
            // `projection::channel_entity`), so its row is not read for it.
            match images.processor.get_image_cache_tag(None, primary).await {
                Ok(Some(tag)) => {
                    tags.insert(id, tag);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, item_id = %id, "live tv: could not tag a channel image");
                }
            }
        }
        tags
    }

    /// The Live TV `UserView` every channel item parents to (C#
    /// `GetInternalLiveTvFolder()`), memoized after the first resolution.
    ///
    /// `None` means the item layer is not wired or the view could not be
    /// provisioned; a channel then simply carries no `ParentId`, which is what
    /// it carried before this path existed.
    async fn live_tv_view_id(&self) -> Option<Uuid> {
        if let Some(id) = self.live_tv_view.get() {
            return Some(*id);
        }
        let store = self.item_store.get()?;
        let id = store
            .views
            .get_internal_live_tv_folder_id()
            .await
            .inspect_err(|error| {
                tracing::warn!(%error, "live tv: could not resolve the Live TV view folder");
            })
            .ok()
            .flatten()?;
        let _ = self.live_tv_view.set(id);
        Some(id)
    }

    /// Mirrors the current channel lineup into `BaseItems`.
    ///
    /// Port of `GuideManager.GetChannel`'s `CreateItem`/`UpdateItemAsync`
    /// (v10.11.8 src/Jellyfin.LiveTv/Guide/GuideManager.cs:375-468) and the
    /// `CleanDatabase(newChannelIdList, [BaseItemKind.LiveTvChannel], …)` that
    /// follows the refresh (:147): every channel in the lineup is a real item
    /// row parented to the Live TV view, and a row whose channel has left every
    /// tuner's lineup is deleted.
    ///
    /// This is what makes a channel reachable through the ordinary item
    /// universe. Without it a channel lives only in `FerrofinLiveTvChannels`,
    /// and every route that resolves an item through `BaseItems` — starting
    /// with `GET /Items/{itemId}` — answers 404 for an id
    /// `GET /LiveTv/Channels` has just handed the client.
    ///
    /// The two tables are written from THIS one path, so they cannot drift
    /// apart: `FerrofinLiveTvChannels` stays the tuner-facing record (stream
    /// URL, tuner host, codecs) and `BaseItems` its item-universe mirror.
    async fn sync_channel_items(&self) -> Result<(), ServiceError> {
        let Some(store) = self.item_store.get() else {
            return Ok(());
        };
        let view_id = self.live_tv_view_id().await;
        let rows = crate::guide_repository::channel_rows(&self.db).await?;
        // The channel items already stored, read ONCE: the merge below needs
        // them (the C# loads each item before assigning to it) and so does
        // `CleanDatabase`.
        let stored = store
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::LiveTvChannel],
                ..InternalItemsQuery::default()
            })
            .await?;
        let by_id: std::collections::HashMap<String, &BaseItemEntity> = stored
            .iter()
            .map(|row| (row.id.to_uppercase(), row))
            .collect();
        let mut created: Vec<BaseItemEntity> = Vec::new();
        let mut updated: Vec<BaseItemEntity> = Vec::new();
        for row in &rows {
            let fresh = channel_entity(row, parse_dt, view_id);
            match by_id.get(&fresh.id.to_uppercase()) {
                // `isNew` — the item did not exist, so `CreateItem` stores it
                // whole.
                None => created.push(fresh),
                // `forceUpdate` — the item exists, so only the fields the
                // refresh owns are assigned onto the STORED row, and it is
                // written back only if one of them actually changed. Writing
                // `fresh` here instead would be a full-column upsert that
                // reverts everything the metadata editor put on a channel.
                Some(prev) => updated.extend(channel_item_update(prev, &fresh)),
            }
        }
        for batch in [&created, &updated] {
            if !batch.is_empty() {
                store.persistence.save_items(batch).await?;
            }
        }
        Self::reattach_channel_user_data(store, &created).await;
        // The channel's Primary image (`GetChannel`), once its item exists.
        let channels: Vec<(Uuid, &ChannelRow)> = rows
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect();
        let ids: Vec<Uuid> = channels.iter().map(|(id, _)| *id).collect();
        let images = store.items.get_image_infos_batch(&ids).await?;
        let changed: Vec<(Uuid, Vec<ItemImageInfo>)> = channels
            .into_iter()
            .filter_map(|(id, row)| {
                let current = images.get(&id).map_or(&[][..], Vec::as_slice);
                channel_images_update(current, row.image_url.as_deref()).map(|list| (id, list))
            })
            .collect();
        store.persistence.save_items_images(&changed).await?;
        // `CleanDatabase(newChannelIdList, [BaseItemKind.LiveTvChannel], …)`
        // (:145-148). The upstream guard is the `cleanDatabase` flag, which
        // ONLY a thrown `RefreshChannelsInternal` clears (:130-134) — an
        // empty-but-successful lineup still cleans, and takes every channel
        // item with it. That is deliberate: it is how the last tuner's removal
        // stops its channels haunting `GET /Items/{id}` and every recursive
        // query for ever.
        //
        // Ferrofin has no empty-because-failed case to protect against.
        // `refresh_guide` never reaches `replace_channels` on a failed lineup
        // fetch, so the cached `FerrofinLiveTvChannels` rows survive and
        // `channel_rows` still returns them — the same cache fallback
        // `BaseTunerHost.GetChannels` performs upstream. An empty `rows` here
        // therefore means the lineup is genuinely empty, which is precisely
        // when the C# deletes.
        let keep: std::collections::HashSet<String> =
            rows.iter().map(|row| row.id.to_uppercase()).collect();
        let stale: Vec<Uuid> = stored
            .iter()
            .filter(|row| !keep.contains(&row.id.to_uppercase()))
            .filter_map(|row| Uuid::parse_str(&row.id).ok())
            .collect();
        if !stale.is_empty() {
            tracing::info!(
                count = stale.len(),
                "live tv: removing channel items that left the lineup"
            );
            store.persistence.delete_items(&stale).await?;
        }
        Ok(())
    }

    /// Gives each channel item this guide refresh `created` back the user
    /// data it parked on the deletion placeholder when it left the lineup.
    ///
    /// Upstream refreshes every channel (`GuideManager.cs:317`), and a created
    /// item's first refresh re-attaches (`MetadataService.SaveItemAsync` with
    /// `isFirstRefresh` → `ReattachUserDataAsync`). Ferrofin refreshes no
    /// channel's metadata and never stamps its `DateLastRefreshed`, so the
    /// item's creation stands in for that first refresh. Re-attaching every
    /// unstamped channel would run on every refresh for all of them. A channel
    /// that fails is skipped, as upstream catches each one; the failures are
    /// logged once for the pass.
    async fn reattach_channel_user_data(store: &LiveTvItemStore, created: &[BaseItemEntity]) {
        let mut failed = 0_usize;
        let mut first_error = None;
        for entity in created {
            if let Err(err) = store.persistence.reattach_user_data(entity).await {
                tracing::debug!(item_id = %entity.id, %err, "live tv: could not re-attach a channel's user data");
                failed += 1;
                first_error.get_or_insert(err);
            }
        }
        if let Some(err) = first_error {
            tracing::warn!(
                failed,
                %err,
                "live tv: could not re-attach the user data of some returning channels; the first error"
            );
        }
    }

    /// Mirrors the ingested guide into `BaseItems`.
    ///
    /// The programme half of [`Self::sync_channel_items`], and deliberately the
    /// same mechanism rather than a second one: upstream stores a programme
    /// exactly as it stores a channel — `CreateItems(newPrograms, currentChannel,
    /// …)` / `UpdateItemsAsync(updatedPrograms, …)` (v10.11.8
    /// src/Jellyfin.LiveTv/Guide/GuideManager.cs:277-291) — and prunes it with
    /// the `CleanDatabase(newProgramIdList, [BaseItemKind.LiveTvProgram], …)`
    /// that sits one line below the channel one (:145-148).
    ///
    /// Without it the recursive item universe holds no airing at all:
    /// `GET /Items?recursive=true&includeItemTypes=LiveTvProgram` is empty,
    /// `GET /Items/{programId}` 404s for an id `GET /LiveTv/Programs` just
    /// handed the client, and `GET /Genres` cannot list a genre that only the
    /// guide carries.
    ///
    /// Called once, AFTER the listings-provider loop, not from inside
    /// `insert_programs`: `insert_programs` runs once per provider and prunes
    /// only the channels that provider's document covered, so a per-provider
    /// clean here would delete the other providers' rows. Upstream's programme
    /// `CleanDatabase` is likewise global, taken over every service's
    /// accumulated id list. A provider whose fetch failed never reaches
    /// `insert_programs` at all, so its cached rows are still in the table and
    /// the mirror keeps them — the same "an unreachable source must not wipe the
    /// guide" property the channel half gets from its own cache fallback.
    ///
    /// One channel at a time, as `RefreshChannelsInternal` stores them (v12.4
    /// GuideManager.cs:232-339): a channel's guide rows and the items already
    /// stored for them are read, merged, written and dropped before the next
    /// channel's are read, so the mirror holds one channel's airings, not the
    /// whole guide's. What builds up is per airing and small: the ids of the
    /// airings the guide holds (upstream's `newProgramIdList`), for the
    /// `CleanDatabase` at the end, which reads the stored ids alone
    /// (`GetItemIdsList`, :346), and the id and end of each one written, for
    /// `PreCacheImages`.
    ///
    /// `listings` are the airings this pass wrote, with their listing's
    /// artwork: `GuideManager.UpdateImages` runs on those, as upstream runs it
    /// on the programmes the listings returned, and each channel takes its own
    /// out of the map. Returned are the programmes created or updated, with
    /// their end, one group per channel, for `PreCacheImages`.
    async fn sync_program_items(
        &self,
        mut listings: HashMap<Uuid, ProgramImages>,
    ) -> Result<Vec<RefreshedAirings>, ServiceError> {
        let Some(store) = self.item_store.get() else {
            return Ok(Vec::new());
        };
        let view_id = self.live_tv_view_id().await;
        // Upstream's `newProgramIdList`: every airing the guide still holds.
        let mut kept: HashSet<Uuid> = HashSet::new();
        let mut refreshed = Vec::new();
        for channel_id in crate::guide_repository::all_channel_ids(&self.db).await? {
            let rows = crate::guide_repository::channel_program_rows(&self.db, &channel_id).await?;
            if rows.is_empty() {
                continue;
            }
            kept.extend(rows.iter().filter_map(|row| Uuid::parse_str(&row.id).ok()));
            let written =
                Self::sync_channel_program_items(store, &rows, view_id, &mut listings).await?;
            if !written.is_empty() {
                refreshed.push(written);
            }
        }
        // What is left was written this pass and replaced since (a later
        // provider rewrote its channel's guide): it has no guide row, so its
        // item goes in the clean below.
        drop(listings);
        // `CleanDatabase(newProgramIdList, [BaseItemKind.LiveTvProgram], …)`
        // (:147), guarded upstream only by the `cleanDatabase` flag a THROWN
        // refresh clears — see the channel half for why an empty set here is a
        // genuine empty set and not a swallowed failure.
        let stale: Vec<Uuid> = store
            .items
            .get_item_ids(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::LiveTvProgram],
                ..InternalItemsQuery::default()
            })
            .await?
            .into_iter()
            .filter(|id| !kept.contains(id))
            .collect();
        if !stale.is_empty() {
            tracing::info!(
                count = stale.len(),
                "live tv: removing programme items that left the guide"
            );
            for chunk in stale.chunks(PROGRAM_ITEM_WRITE_CHUNK) {
                store.persistence.delete_items(chunk).await?;
            }
        }
        Ok(refreshed)
    }

    /// One channel's turn of [`Self::sync_program_items`]: `GetProgram` over
    /// the channel's guide `rows` against the items already stored for them,
    /// then `CreateItems` / `UpdateItemsAsync` and `UpdateImages` for what
    /// changed. Returns the programmes it created or updated, with their end.
    async fn sync_channel_program_items(
        store: &LiveTvItemStore,
        rows: &[GuideProgramRow],
        view_id: Option<Uuid>,
        listings: &mut HashMap<Uuid, ProgramImages>,
    ) -> Result<RefreshedAirings, ServiceError> {
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok())
            .collect();
        // The programme items already stored for these airings: the merge
        // needs them (the C# loads `existingPrograms` before assigning). They
        // are read by id, so an airing finds its item whichever channel the
        // item was stored under.
        let stored: HashMap<Uuid, BaseItemEntity> = store
            .items
            .retrieve_items(&ids)
            .await?
            .into_iter()
            .filter(|row| row.type_ == crate::projection::PROGRAM_TYPE_NAME)
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect();
        let mut save: Vec<BaseItemEntity> = Vec::new();
        // The genres to (re)write as `ItemValues`, for the rows whose genre list
        // actually moved. A guide's genres are what make `GET /Genres` list a
        // by-name row a library never produced ("News"), and re-writing all of
        // them every refresh would be several statements per airing on a table
        // that is O(channels x guide-hours).
        let mut genres_to_write: Vec<(Uuid, Vec<(i32, String)>)> = Vec::new();
        // The programmes `CreateItems`/`UpdateItemsAsync` would write, with
        // their end.
        let mut written: HashSet<Uuid> = HashSet::new();
        let mut ends: HashMap<Uuid, Option<DateTime<Utc>>> = HashMap::new();
        for row in rows {
            let fresh = program_entity(row, parse_dt, view_id);
            let id = Uuid::parse_str(&fresh.id).ok();
            let stored_row = id.and_then(|id| stored.get(&id));
            // A new airing without genres has no links to replace and none to
            // write; on a first import that is most of the guide.
            let genres_changed = stored_row.map_or_else(
                || fresh.genres.as_deref().is_some_and(|g| !g.is_empty()),
                |prev| prev.genres != fresh.genres,
            );
            if let Some(id) = id {
                ends.insert(id, fresh.end_date);
                if genres_changed {
                    genres_to_write.push((
                        id,
                        fresh
                            .genres
                            .as_deref()
                            .map(|g| {
                                g.split('|')
                                    .map(|genre| {
                                        (i32::from(ItemValueType::Genre), genre.to_owned())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    ));
                }
            }
            let item = match stored_row {
                // `isNew` — `CreateItems` stores the item whole.
                None => Some(fresh),
                // `isUpdated` — only the fields the listing owns are assigned
                // onto the STORED row, and it is written back only when one of
                // them actually changed.
                Some(prev) => program_item_update(prev, &fresh),
            };
            if let Some(item) = item {
                if let Some(id) = id {
                    written.insert(id);
                }
                save.push(item);
            }
        }
        drop(stored);
        for chunk in save.chunks(PROGRAM_ITEM_WRITE_CHUNK) {
            store.persistence.save_items(chunk).await?;
        }
        drop(save);
        for chunk in genres_to_write.chunks(PROGRAM_ITEM_WRITE_CHUNK) {
            store.persistence.save_item_values_for_items(chunk).await?;
        }
        // `UpdateImages`, for the airings the listings returned. An image
        // change alone also makes the programme "updated" upstream.
        let listed: Vec<(Uuid, ProgramImages)> = ids
            .iter()
            .filter_map(|id| listings.remove(id).map(|images| (*id, images)))
            .collect();
        Self::sync_program_images(store, &listed, &mut written).await?;
        Ok(written
            .into_iter()
            .map(|id| (id, ends.get(&id).copied().flatten()))
            .collect())
    }

    /// `GuideManager.UpdateImages` over the airings in `listed`, writing the
    /// image lists that change and adding their programmes to `written` (the
    /// programmes created or updated this pass, which a removal alone needs
    /// to be written).
    async fn sync_program_images(
        store: &LiveTvItemStore,
        listed: &[(Uuid, ProgramImages)],
        written: &mut HashSet<Uuid>,
    ) -> Result<(), ServiceError> {
        if listed.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = listed.iter().map(|(id, _)| *id).collect();
        let current = store.items.get_image_infos_batch(&ids).await?;
        let mut images_to_write: Vec<(Uuid, Vec<ItemImageInfo>)> = Vec::new();
        for (id, listing) in listed {
            let stored = current.get(id).map_or(&[][..], Vec::as_slice);
            if let Some(images) = program_images_update(stored, listing, written.contains(id)) {
                images_to_write.push((*id, images));
                written.insert(*id);
            }
        }
        store.persistence.save_items_images(&images_to_write).await
    }

    /// Re-expands every series timer over the guide that was just ingested.
    ///
    /// Port of `DefaultLiveTvService.RefreshSeriesTimers` (v10.11.8
    /// src/Jellyfin.LiveTv/DefaultLiveTvService.cs:78-86): every stored series
    /// timer runs through `UpdateTimersForSeriesTimer(timer, false, true)` —
    /// `updateTimerSettings: false`, so a timer a user hand-edited keeps its
    /// padding, and `deleteInvalidTimers: true`, so a pending timer whose airing
    /// has left the guide goes.
    ///
    /// It is called from `RefreshGuide` and nowhere else (GuideManager.cs:150-156),
    /// and that is the whole point: the guide window rolls forward every day, so
    /// a series timer created on Monday only keeps recording Thursday's showings
    /// if something re-expands it against the airings the refresh has just
    /// brought in. Ferrofin ran the fan-out ONLY when a series timer was created
    /// or edited, so a rule went stale the moment the guide moved past the
    /// airings it was created against — measured on the parity pair, whose one
    /// Ferrofin series timer had produced zero timers.
    ///
    /// One series timer's failure does not fail the refresh, mirroring how the
    /// C# guide refresh treats a per-service error: the remaining rules still
    /// get their pass.
    async fn refresh_series_timers(&self) -> Result<(), ServiceError> {
        let timers = self
            .get_series_timers(&ferrofin_model::live_tv::SeriesTimerQuery::default())
            .await?;
        for timer in timers {
            let Some(id) = timer.base.id.as_deref() else {
                continue;
            };
            let Some(stored) = self.stored_series_timer(id).await? else {
                continue;
            };
            if let Err(e) = self
                .update_timers_for_series_timer(&stored, false, true)
                .await
            {
                tracing::warn!(
                    series_timer = %id,
                    error = %e,
                    "live tv: could not re-expand a series timer over the refreshed guide"
                );
            }
        }
        Ok(())
    }

    /// Port of `DefaultLiveTvService.RefreshTimers` (v12.2
    /// `DefaultLiveTvService.cs:88-117`), run at the end of every guide
    /// refresh, over every timer but the `Completed` ones (`GetTimersAsync`):
    ///
    /// - one that has ended and is not recording is deleted;
    /// - one that names no programme or no channel is left as it is;
    /// - one whose programme has left the guide is deleted;
    /// - the rest take the refreshed programme's facts
    ///   (`CopyProgramInfoToTimerInfo`), are saved and re-armed
    ///   (`TimerManager.Update`).
    async fn refresh_timers(&self) -> Result<(), ServiceError> {
        for timer in self.all_timers().await? {
            if timer.status == RecordingStatus::Completed {
                continue;
            }
            let Some(id) = timer.base.id.filter(|id| !id.is_empty()) else {
                continue;
            };
            // One timer's failure does not leave the rest stale, as in
            // `refresh_series_timers`.
            if let Err(e) = self.refresh_timer(&id).await {
                tracing::warn!(
                    timer_id = %id,
                    error = %e,
                    "live tv: could not refresh a recording timer"
                );
            }
        }
        Ok(())
    }

    /// One timer's turn of [`refresh_timers`](Self::refresh_timers), decided
    /// on the row as it is now: the timer may have fired, or its capture
    /// settled, since the list was taken, and a timer that has become
    /// `Completed` is not touched.
    ///
    /// Upstream's service-level `TimerInfo.ProgramId` is the listings
    /// provider's id (`GetProgramInfoFromCache` maps it to the item); the wire
    /// DTO carries it as `external_program_id` beside the item's own
    /// `program_id`, so a timer names no programme only when both are blank.
    async fn refresh_timer(&self, id: &str) -> Result<(), ServiceError> {
        let Some(mut timer) = self.get_timer(id).await? else {
            return Ok(());
        };
        if timer.status == RecordingStatus::Completed {
            return Ok(());
        }
        if Utc::now() > timer.base.end_date && !self.active_recordings_lock().contains_key(id) {
            tracing::info!(
                timer_id = id,
                reason = "ended",
                "live tv: recording timer removed"
            );
            self.disarm_timer(id);
            return self.delete_by_id(DELETE_TIMER_SQL, id).await;
        }
        let names_program = [&timer.base.program_id, &timer.base.external_program_id]
            .into_iter()
            .any(|program| program.as_deref().is_some_and(|p| !p.trim().is_empty()));
        if !names_program || timer.base.channel_id.is_nil() {
            return Ok(());
        }
        let Some(program) = self.program_row_for_timer(&timer.base).await? else {
            tracing::info!(
                timer_id = id,
                reason = "programme left the guide",
                "live tv: recording timer removed"
            );
            self.disarm_timer(id);
            return self.delete_by_id(DELETE_TIMER_SQL, id).await;
        };
        let before = timer.clone();
        copy_program_into_timer(&program, &mut timer);
        let changed = timer != before;
        if changed {
            self.persist_timer(&timer).await?;
        }
        // `_timerManager.Update(timer)` → `AddOrUpdateSystemTimer`, for every
        // live timer whether or not the guide changed it: re-arming is the
        // backstop for an arm that was lost, and a timer whose showing is under
        // way fires again at once, which `on_timer_fired` turns away when it is
        // already recording. An unchanged timer that still has an arm keeps
        // it: re-arming would aim at the same moment, and it keeps a read
        // retry's backoff (`retry_unread_timer`) instead of firing at once.
        if changed || !self.armed_timers_lock().contains_key(id) {
            self.arm_timer(&timer);
        }
        Ok(())
    }

    /// The wired DTO service, or the honest error when the composition root
    /// (or a test) never attached one.
    fn dto_service(&self) -> Result<&Arc<dyn DtoService>, ServiceError> {
        self.dto
            .get()
            .ok_or_else(|| ServiceError::Backend("live tv dto service not wired".to_owned()))
    }

    /// Refreshes the channel lineup for one tuner host from the lineup its
    /// [`TunerHost`](crate::tuner_host::TunerHost) produced, returning the ids
    /// it wrote for the caller's `CleanDatabase` pass.
    ///
    /// The lineup arrives already built, so the identity of a channel is the
    /// HOST's business — `m3u_{md5}{md5}` for a playlist, `hdhr_{GuideNumber}`
    /// for an HDHomeRun — exactly as `ChannelInfo.Id` is upstream.
    ///
    /// An UPSERT, not a delete-and-reinsert: `GuideManager.RefreshChannelsInternal`
    /// saves each channel it found and only removes the ones the pass did not
    /// re-emit, at the very end. Deleting first would cascade every programme of
    /// the tuner away (`FK_LiveTvPrograms_Channels_ChannelId ON DELETE
    /// CASCADE`), so a client reading the guide mid-refresh would see it empty
    /// and briefly wrong.
    async fn replace_channels(
        &self,
        tuner_id: &str,
        channels: &[crate::tuner_host::TunerChannel],
    ) -> Result<Vec<String>, ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        // `DateCreated` is bound for every row, but only a NEW channel keeps
        // it: the upsert's COALESCE holds an existing channel's first-seen
        // instant, the way `GuideManager.GetChannel` only stamps
        // `DateCreated = DateTime.UtcNow` on a NEW item.
        let now = datetime_to_db(Utc::now());
        let mut written = Vec::with_capacity(channels.len());

        // 14 columns per row; chunked multi-row insert instead of one round-trip
        // per channel.
        for (chunk_index, chunk) in channels.chunks(SQLITE_BIND_LIMIT / 14).enumerate() {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                r#"INSERT INTO "FerrofinLiveTvChannels"
                   ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType","StreamUrl","SortIndex","DateCreated","ExternalId","IsHd","VideoCodec","AudioCodec") "#,
            );
            let base = chunk_index * (SQLITE_BIND_LIMIT / 14);
            qb.push_values(chunk.iter().enumerate(), |mut b, (offset, ch)| {
                // `GuideManager.GetChannel` stores the item under
                // `GetInternalChannelId(serviceName, channelInfo.Id)`, where
                // `channelInfo.Id` is the tuner's external id — a pure function
                // of the tuner and this entry. Nothing per-instance goes in, so
                // two servers reading the same lineup agree, and re-adding the
                // same tuner keeps every channel's user data, timers and
                // favourites attached.
                let id = guid_to_db(internal_channel_id(&ch.external_id));
                let channel_type = if ch.is_radio { "Radio" } else { "Tv" };
                let date_created = now.clone();
                written.push(id.clone());
                b.push_bind(id)
                    .push_bind(tuner_id)
                    .push_bind(&ch.guide_key)
                    .push_bind(&ch.name)
                    .push_bind(&ch.number)
                    .push_bind(&ch.image_url)
                    .push_bind(channel_type)
                    .push_bind(&ch.url)
                    .push_bind(i64::try_from(base + offset).unwrap_or(i64::MAX))
                    .push_bind(date_created)
                    .push_bind(&ch.external_id)
                    .push_bind(ch.is_hd.map(i64::from))
                    .push_bind(&ch.video_codec)
                    .push_bind(&ch.audio_codec);
            });
            qb.push(CHANNEL_UPSERT_CONFLICT);
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(written)
    }

    /// The configured listings provider with this id.
    ///
    /// Port of the `ListingProviders.FirstOrDefault(... OrdinalIgnoreCase)`
    /// lookup every `ListingsManager` entry point opens with. Upstream spells
    /// two of them `.First(...)`, which throws `InvalidOperationException`
    /// (HTTP `500`) on no match; the honest answer for an unknown id is the
    /// `ResourceNotFoundException` → `404` the third one already gives.
    async fn listing_provider_by_id(&self, id: &str) -> Result<ListingsProviderInfo, ServiceError> {
        self.get_listing_providers()
            .await?
            .into_iter()
            .find(|p| {
                p.id.as_deref()
                    .is_some_and(|pid| eq_ordinal_ignore_case(pid, id))
            })
            .ok_or_else(|| ServiceError::not_found(format!("listings provider {id}")))
    }

    /// A listings provider's own channel list.
    ///
    /// Port of `IListingsProvider.GetChannels` for each backend:
    /// - XMLTV (`XmlTvListingsProvider.GetChannels`): the guide document's
    ///   `<channel>` elements, numbered from their display-names and falling
    ///   back to the id when none is numeric ([`crate::xmltv::XmltvChannel::listings_number`]).
    /// - Schedules Direct: the lineup's stations ([`SchedulesDirect::get_channels`]);
    ///   an unavailable SD has none, as upstream's `[]`.
    ///
    /// The number feeds the by-number arm of the match ladder.
    async fn provider_channels(
        &self,
        info: &ListingsProviderInfo,
    ) -> Result<Vec<EpgChannel>, ServiceError> {
        if ListingsBackend::of(info.type_.as_deref())? == ListingsBackend::SchedulesDirect {
            return Ok(self
                .schedules_direct
                .get_channels(info)
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(sd_epg_channel)
                .collect());
        }
        let document = self.xmltv.get_xml(info).await?;
        // The channels only: every airing is dropped as soon as it is read.
        let (_, guide) = parse_guide(document, self.guide_language(info), |_| false).await?;
        // Upstream's `GetChannels` throws on a bad document, so the mapping
        // dialog shows an error instead of "no channels".
        if let Some(e) = guide.error {
            let path = info.path.as_deref().unwrap_or_default();
            return Err(ServiceError::Backend(format!("{path}: {e}")));
        }
        Ok(guide
            .channels
            .into_iter()
            .map(|c| epg_channel(&c))
            .collect())
    }

    /// Port of `XmlTvListingsProvider.Validate`: a save is an explicit retry,
    /// so the path's download backoff is dropped; a local path (anything not
    /// starting with `http`) must name an existing file, or the save is
    /// `FileNotFoundException` (`404`). The test reads the raw string, so an
    /// empty or blank `Path` is that `404` too: no file has that name.
    ///
    /// A missing `Path` is a `400` here; upstream dereferences it and answers
    /// a `NullReferenceException` `500`, a Jellyfin bug not ported.
    async fn validate_xmltv(&self, info: &ListingsProviderInfo) -> Result<(), ServiceError> {
        let Some(path) = info.path.as_deref() else {
            return Err(ServiceError::InvalidInput(
                "listing provider Path is required".into(),
            ));
        };
        self.xmltv.forget_failure(path);
        // `File.Exists`: false for a directory, and for anything unreadable.
        if !crate::fetch::is_http(path)
            && !tokio::fs::metadata(path)
                .await
                .is_ok_and(|meta| meta.is_file())
        {
            return Err(ServiceError::not_found(format!(
                "Could not find the XmlTv file specified: {path}"
            )));
        }
        Ok(())
    }

    /// Stores a listings provider, the configuration half of
    /// `ListingsManager.SaveListingProvider` (and of `SetChannelMapping`,
    /// which saves without validating).
    ///
    /// The id rule is upstream's: a posted id that matches a configured
    /// provider ignoring case replaces it, keeping the posted spelling
    /// (`config.ListingProviders[index] = info`); a blank one, or one that
    /// names no provider, is replaced by a fresh `Guid.NewGuid().ToString("N")`
    /// — 32 lowercase hex digits, no dashes, which is the form that goes out on
    /// `GET /System/Configuration/livetv`.
    async fn store_listing_provider(
        &self,
        mut info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        let posted = info.id.clone().filter(|id| !id.trim().is_empty());
        let stored = match posted.as_deref() {
            Some(wanted) => {
                crate::guide_repository::existing_config_id(
                    &self.db,
                    crate::guide_repository::LISTING_PROVIDERS_TABLE,
                    wanted,
                )
                .await?
            }
            None => None,
        };
        let id = match (&posted, &stored) {
            (Some(wanted), Some(_)) => wanted.clone(),
            _ => Uuid::new_v4().simple().to_string(),
        };
        info.id = Some(id.clone());
        let data = serde_json::to_string(&info)
            .map_err(|e| LiveTvError::serialize("serialize listing provider", e))?;

        crate::guide_repository::store_listing_provider(
            &self.db,
            stored.as_deref(),
            &id,
            info.type_.as_deref().unwrap_or("xmltv"),
            // A Schedules Direct provider has no path.
            info.path.as_deref().unwrap_or_default(),
            &data,
        )
        .await?;

        // `InvalidateListingsProviderCache(info.Id)`: a changed Path must not
        // keep serving the old document.
        if let Some(stored) = stored.as_deref().filter(|stored| *stored != id) {
            self.xmltv.invalidate(stored).await;
        }
        self.xmltv.invalidate(&id).await;
        Ok(info)
    }

    /// Every tuner channel this listings provider supplies listings for, with
    /// its stored key alongside the external `ChannelInfo.Id` clients see.
    ///
    /// Port of `ListingsManager.GetChannelsForListingsProvider`: each tuner
    /// host's lineup, filtered by `IsListingProviderEnabledForTuner`.
    async fn channels_for_listings_provider(
        &self,
        info: &ListingsProviderInfo,
    ) -> Result<Vec<(String, TunerChannel)>, ServiceError> {
        let mut out = Vec::new();
        for tuner in self.get_tuner_hosts().await? {
            let (Some(host_id), Some(url)) = (tuner.id.as_deref(), tuner.url.as_deref()) else {
                continue;
            };
            if !is_listing_provider_enabled_for_tuner(info, host_id) {
                continue;
            }
            for row in crate::guide_repository::tuner_lineup(&self.db, host_id).await? {
                out.push((
                    row.id,
                    matcher_channel(
                        url,
                        host_id,
                        &row.stream_url,
                        row.tvg_id,
                        row.number,
                        row.name,
                    ),
                ));
            }
        }
        Ok(out)
    }

    /// Drops every stored airing on the given channels, returning the
    /// `Id → DateCreated` map they had.
    ///
    /// This is `GuideManager.CleanDatabase` (v10.11.8 GuideManager.cs:147-148)
    /// expressed as a replace: a refresh is authoritative for the channels it
    /// covers, so anything it does not re-emit must go, and re-inserting only
    /// what it did emit is the same end state. The old creation instants come
    /// back with the caller because upstream *updates* a surviving programme
    /// item rather than recreating it, and `DateCreated` is a field the
    /// programme DTO surfaces.
    async fn clean_programs(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        channel_ids: &[String],
    ) -> Result<HashMap<String, Option<String>>, ServiceError> {
        let mut existing: HashMap<String, Option<String>> = HashMap::new();
        for chunk in channel_ids.chunks(SQLITE_BIND_LIMIT) {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                r#"SELECT "Id","DateCreated" FROM "FerrofinLiveTvPrograms" WHERE "ChannelId" IN ("#,
            );
            let mut sep = qb.separated(",");
            for id in chunk {
                sep.push_bind(id);
            }
            qb.push(")");
            for row in qb.build().fetch_all(&mut **tx).await.map_err(db_err)? {
                existing.insert(row.get("Id"), row.get("DateCreated"));
            }

            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new(r#"DELETE FROM "FerrofinLiveTvPrograms" WHERE "ChannelId" IN ("#);
            let mut sep = qb.separated(",");
            for id in chunk {
                sep.push_bind(id);
            }
            qb.push(")");
            qb.build().execute(&mut **tx).await.map_err(db_err)?;
        }
        Ok(existing)
    }

    /// Whether an airing falls inside the guide window the refresh asked for.
    ///
    /// `XmlTvListingsProvider.GetProgramsAsync` hands the window straight to
    /// `XmlTvReader.GetProgrammes(channelId, start, end)`, which keeps a
    /// programme that *overlaps* it — an airing already in progress when the
    /// refresh runs is part of the guide. An airing with no `stop` is treated as
    /// instantaneous at its start.
    ///
    /// The boundary form (`end >= start`, `start < end`) is the one that
    /// reproduces the oracle: on the lab fixture Jellyfin retains exactly the
    /// airings from the hour containing `now - 1h` through `+7d` inclusive of
    /// the far endpoint (338 rows = 2 channels x 169 hourly airings), which no
    /// start-only or fully-exclusive filter yields.
    fn in_guide_window(
        prog: &crate::xmltv::XmltvProgramme,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> bool {
        let Some(prog_start) = prog.start.map(|d| d.with_timezone(&Utc)) else {
            // No start attribute: upstream's reader cannot place it either, and
            // the row is already unaddressable. Keep it rather than silently
            // shrinking the lineup.
            return true;
        };
        let prog_end = prog.stop.map_or(prog_start, |d| d.with_timezone(&Utc));
        prog_end >= start && prog_start < end
    }

    /// The number of days of guide data a refresh ingests and
    /// `GET /LiveTv/GuideInfo` advertises.
    ///
    /// Port of `GuideManager.GetGuideDays` (v10.11.8 GuideManager.cs:161-168):
    /// the dashboard's `LiveTvOptions.GuideDays` clamped to
    /// `1..=`[`MAX_GUIDE_DAYS`], or [`DEFAULT_GUIDE_DAYS`] when unset.
    async fn guide_days(&self) -> i64 {
        self.live_tv_options()
            .await
            .guide_days
            .map_or(DEFAULT_GUIDE_DAYS, |d| {
                i64::from(d).clamp(1, MAX_GUIDE_DAYS)
            })
    }

    /// Which stored channel takes its listings from which guide channel, with
    /// the tuner's `ChannelInfo.Id` alongside each stored GUID.
    ///
    /// The match ladder is `ListingsManager.GetEpgChannelFromTunerChannel` —
    /// the same one the channel-mapping dialog renders — so the provider's
    /// manual `ChannelMappings` actually move listings and the binding is NOT
    /// the raw `tvg-id`.
    ///
    /// The external id rides along because upstream builds a programme's id
    /// from the CHANNEL's external id (`ListingsManager.GetProgramsAsync`,
    /// v10.11.8 ListingsManager.cs:151-155), not from its item GUID, and one
    /// guide channel can legitimately feed more than one tuner channel.
    async fn bind_guide_channels(
        &self,
        epg_channels: &[EpgChannel],
        provider: &ListingsProviderInfo,
    ) -> Result<HashMap<String, Vec<(String, String)>>, ServiceError> {
        let epg = EpgChannelData::new(epg_channels);
        let mut by_epg_channel: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (stored_id, tuner_channel) in self.channels_for_listings_provider(provider).await? {
            if let Some(matched) =
                epg_channel_for_tuner_channel(&provider.channel_mappings, &tuner_channel, &epg)
            {
                by_epg_channel
                    .entry(matched.id.clone())
                    .or_default()
                    .push((stored_id, tuner_channel.id));
            }
        }
        Ok(by_epg_channel)
    }

    /// Inserts programmes from an XMLTV document, binding each to every channel that
    /// takes its listings from the programme's guide channel, classified against
    /// the listings provider's category lists as
    /// `XmlTvListingsProvider.GetProgramInfo` does. Returns the ids it wrote, for
    /// the caller's `CleanDatabase` pass, with each airing's artwork.
    ///
    /// The tuner-channel → guide-channel binding is
    /// `ListingsManager.GetEpgChannelFromTunerChannel`, so the provider's manual
    /// `ChannelMappings` actually move listings — the join is not the raw
    /// `tvg-id`.
    ///
    /// `channels` are the document's `<channel>` elements, read beforehand:
    /// XMLTV may put them after the programmes, and the binding has to be
    /// known before the programmes are read, so that only the bound channels'
    /// airings inside the window are kept. A large IPTV guide carries
    /// thousands of channels nobody mapped, and parsing them all (upstream
    /// streams its cached file one channel at a time instead) held the whole
    /// guide in memory. `document` is dropped once its airings are read.
    async fn insert_programs(
        &self,
        document: XmlTvDocument,
        channels: &[XmltvChannel],
        provider: &ListingsProviderInfo,
        window: (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<Vec<(Uuid, ProgramImages)>, ServiceError> {
        let classes = CategoryClasses::from_provider(provider);
        let (window_start, window_end) = window;

        let epg_channels: Vec<EpgChannel> = channels.iter().map(epg_channel).collect();
        let by_epg_channel = self.bind_guide_channels(&epg_channels, provider).await?;
        // The channels this document is authoritative for — the ones the ladder
        // actually bound, so `clean_programs` replaces only those and a second
        // listings provider's airings survive this pass.
        let touched: Vec<String> = by_epg_channel
            .values()
            .flatten()
            .map(|(stored, _)| stored.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        // Only an airing that can become a row survives the read: one of a
        // bound channel, inside the window. The fetch half already found the
        // document readable; were this read to disagree, writing it would let
        // `clean_programs` replace the bound channels' airings with a partial
        // set, so the refresh stops before any prune instead.
        let bound: HashSet<String> = by_epg_channel.keys().cloned().collect();
        let (document, guide) = parse_guide(document, self.guide_language(provider), move |prog| {
            bound.contains(&prog.channel_id)
                && Self::in_guide_window(prog, window_start, window_end)
        })
        .await?;
        drop(document);
        if let Some(e) = guide.error {
            return Err(ServiceError::Backend(format!(
                "the guide document became unreadable on its second read: {e}"
            )));
        }

        // One (channel, programme) row per binding, built as the write takes
        // it: `write_program_rows` inserts them in chunked multi-row
        // statements (27 columns per row) instead of one round-trip per
        // programme, and holds one statement's rows at a time.
        let rows: ProgramRows<'_> = Box::new(guide.programmes.iter().flat_map(|prog| {
            let channels = by_epg_channel
                .get(&prog.channel_id)
                .map_or(&[][..], Vec::as_slice);
            let start =
                opt_datetime_to_db(prog.start.map(|d| d.with_timezone(&Utc))).unwrap_or_default();
            let end = opt_datetime_to_db(prog.stop.map(|d| d.with_timezone(&Utc)));
            let genres = if prog.categories.is_empty() {
                None
            } else {
                serde_json::to_string(&prog.categories).ok()
            };
            let class = classes.classify(prog);
            channels.iter().map(move |(channel_id, channel_external)| {
                // `ListingsManager.GetProgramsAsync` (v10.11.8
                // ListingsManager.cs:151-155) suffixes the provider's id with
                // `"_" + channel.Id` — the tuner's external channel id — and
                // `LiveTvDtoService.GetInternalProgramId` hashes the result.
                let external_id = class
                    .external_id
                    .as_ref()
                    .map(|base| format!("{base}_{channel_external}"));
                let id = external_id.as_deref().map_or_else(
                    // No start attribute means upstream has no id to build
                    // either; keep the row addressable rather than dropping
                    // it, keyed on what it does have.
                    || internal_program_id(&format!("{channel_external}_{}", prog.title)),
                    internal_program_id,
                );
                ProgramRow {
                    id,
                    channel_id,
                    external_id,
                    start: start.clone(),
                    end: end.clone(),
                    genres: genres.clone(),
                    class: class.clone(),
                    fields: ProgramFields::of_xmltv(prog),
                    // `XmlTvListingsProvider.GetProgramInfo`'s `ImageUrl`,
                    // `ThumbImageUrl` and `BackdropImageUrl`.
                    images: ProgramImages {
                        primary: prog.icon.clone(),
                        thumb: prog.still_image.clone(),
                        backdrop: prog.backdrop_image.clone(),
                    },
                }
            })
        }));

        self.write_program_rows(rows, &touched).await
    }

    /// Inserts the Schedules Direct airings fetched for one provider, binding
    /// each station's airings to every channel that takes its listings from
    /// that station (the same ladder as the XMLTV path). Returns the ids it
    /// wrote, for the caller's `CleanDatabase` pass, with each airing's artwork.
    ///
    /// Only the channels bound to a station whose airings were fetched are
    /// replaced: a station the refresh could not read keeps its stored guide.
    async fn insert_schedules_direct_programs(
        &self,
        channels: &[EpgChannel],
        programs: &HashMap<String, Vec<SdProgram>>,
        provider: &ListingsProviderInfo,
    ) -> Result<Vec<(Uuid, ProgramImages)>, ServiceError> {
        let by_station = self.bind_guide_channels(channels, provider).await?;
        let touched: Vec<String> = by_station
            .iter()
            .filter(|(station, _)| programs.contains_key(*station))
            .flat_map(|(_, bound)| bound.iter().map(|(stored, _)| stored.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let rows: ProgramRows<'_> = Box::new(programs.iter().flat_map(|(station, airings)| {
            let channels = by_station.get(station).map_or(&[][..], Vec::as_slice);
            airings.iter().flat_map(move |program| {
                let start = datetime_to_db(program.start);
                let end = Some(datetime_to_db(program.end));
                let genres = if program.genres.is_empty() {
                    None
                } else {
                    serde_json::to_string(&program.genres).ok()
                };
                let class = sd_program_class(program);
                channels.iter().map(move |(channel_id, channel_external)| {
                    // `ListingsManager.GetProgramsAsync`: `program.Id += "_" + channel.Id`.
                    let external_id = format!("{}_{channel_external}", program.id);
                    ProgramRow {
                        id: internal_program_id(&external_id),
                        channel_id,
                        external_id: Some(external_id),
                        start: start.clone(),
                        end: end.clone(),
                        genres: genres.clone(),
                        class: class.clone(),
                        fields: ProgramFields::of_schedules_direct(program),
                        // `SchedulesDirect.GetProgram`'s `ImageUrl` and
                        // `ThumbImageUrl`; it sets no backdrop.
                        images: ProgramImages {
                            primary: program.image_url.clone(),
                            thumb: program.thumb_image_url.clone(),
                            backdrop: None,
                        },
                    }
                })
            })
        }));

        self.write_program_rows(rows, &touched).await
    }

    /// Whether a refresh newer than `generation` has started.
    fn superseded(&self, generation: u64) -> bool {
        self.refresh_generation.load(Ordering::SeqCst) != generation
    }

    /// The fetch half of a guide refresh for one listings provider: what it
    /// supplied, if anything, and whether it was read in full.
    ///
    /// Jellyfin 12.1's `hasErrors` (`cleanDatabase &= !hasErrors`): a provider
    /// that could not be read, in full or in part, keeps the refresh from
    /// dropping the stored guide it did not re-emit.
    async fn fetch_guide_source(
        &self,
        provider: &ListingsProviderInfo,
        tuner_lineups: &[(String, String, Vec<crate::tuner_host::TunerChannel>)],
        window: (DateTime<Utc>, DateTime<Utc>),
        generation: u64,
    ) -> Result<(Option<GuideSource>, bool), ServiceError> {
        // `ListingsManager.GetListingProviders` leaves out a provider whose type
        // has no registered backend: it supplies nothing and is no error.
        let Ok(backend) = ListingsBackend::of(provider.type_.as_deref()) else {
            return Ok((None, true));
        };
        if backend == ListingsBackend::SchedulesDirect {
            return Ok(
                match self
                    .fetch_schedules_direct_guide(provider, tuner_lineups, window, generation)
                    .await
                {
                    Ok(Some((source, complete))) => (Some(source), complete),
                    Ok(None) => {
                        tracing::warn!(
                            "live tv: Schedules Direct is unavailable; keeping the stored guide"
                        );
                        (None, false)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "live tv: Schedules Direct guide fetch failed");
                        (None, false)
                    }
                },
            );
        }
        let Some(path) = provider.path.as_deref() else {
            return Ok((None, false));
        };
        match self.xmltv.get_xml(provider).await {
            // An unreadable document is a listings error like a failed fetch.
            // The provider writes nothing, so `clean_programs` cannot replace its
            // channels' airings with a partial read either. For a saved provider
            // an empty download never gets here: `get_xml` treats it as a failed
            // download and answers with the cached copy, if there is one, or the
            // `Err` below.
            Ok(document) => {
                // The channels and whether the document is readable; every
                // airing is dropped as it is read.
                let (document, probe) =
                    parse_guide(document, self.guide_language(provider), |_| false).await?;
                if let Some(e) = &probe.error {
                    tracing::warn!(%path, error = %e, "live tv: guide document unreadable; keeping the stored guide");
                    Ok((None, false))
                } else {
                    Ok((
                        Some(GuideSource::XmlTv {
                            document,
                            channels: probe.channels,
                        }),
                        true,
                    ))
                }
            }
            Err(e) => {
                tracing::warn!(%path, error = %e, "live tv: guide fetch failed");
                Ok((None, false))
            }
        }
    }

    /// The Schedules Direct half of a guide refresh's fetch: the lineup's
    /// stations, then the airings of each station a tuner channel binds to.
    ///
    /// The binding is computed here from the tuner lineups this refresh just
    /// read, by the same ladder the write half applies to the stored lineup,
    /// so SD is asked only for the stations the guide will show (upstream
    /// asks per tuner channel).
    ///
    /// `Ok(None)` is "SD is unavailable" (an account error, the login
    /// back-off, the metadata daily limit): the caller keeps the stored guide.
    /// Upstream reads that as stations without airings and drains them, a
    /// Jellyfin bug not ported. The `bool` is whether every bound station
    /// was read.
    async fn fetch_schedules_direct_guide(
        &self,
        provider: &ListingsProviderInfo,
        tuner_lineups: &[(String, String, Vec<crate::tuner_host::TunerChannel>)],
        window: (DateTime<Utc>, DateTime<Utc>),
        generation: u64,
    ) -> Result<Option<(GuideSource, bool)>, ServiceError> {
        let Some(stations) = self.schedules_direct.get_channels(provider).await? else {
            return Ok(None);
        };
        let channels: Vec<EpgChannel> = stations.into_iter().map(sd_epg_channel).collect();
        let epg = EpgChannelData::new(&channels);

        let mut bound = std::collections::BTreeSet::new();
        for (host_id, url, lineup) in tuner_lineups {
            if !is_listing_provider_enabled_for_tuner(provider, host_id) {
                continue;
            }
            for channel in lineup {
                let tuner_channel = matcher_channel(
                    url,
                    host_id,
                    &channel.url,
                    channel.guide_key.clone(),
                    channel.number.clone().unwrap_or_default(),
                    channel.name.clone(),
                );
                // A station without an id has no schedule to ask for
                // (upstream's lookup throws on its null key).
                if let Some(matched) =
                    epg_channel_for_tuner_channel(&provider.channel_mappings, &tuner_channel, &epg)
                    && !matched.id.is_empty()
                {
                    bound.insert(matched.id.clone());
                }
            }
        }

        let mut programs = HashMap::new();
        let mut complete = true;
        let mut failed = 0_usize;
        for station in bound {
            if self.superseded(generation) {
                complete = false;
                break;
            }
            match self
                .schedules_direct
                .get_programs(provider, &station, window.0, window.1)
                .await
            {
                Ok(Some(airings)) => {
                    programs.insert(station, airings);
                }
                Ok(None) => {
                    // SD became unavailable part-way: the stations read so far
                    // are written, the rest keep their stored guide.
                    complete = false;
                    break;
                }
                Err(error) => {
                    // `RefreshChannelsInternal` catches per channel and goes on.
                    complete = false;
                    failed += 1;
                    tracing::debug!(%station, %error, "live tv: Schedules Direct airings fetch failed");
                }
            }
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                read = programs.len(),
                "live tv: some Schedules Direct stations could not be read; they keep their stored guide"
            );
        }
        if programs.is_empty() && !complete {
            return Ok(None);
        }
        Ok(Some((
            GuideSource::SchedulesDirect { channels, programs },
            complete,
        )))
    }

    /// Replaces the airings on `touched` with `rows`, in one transaction, and
    /// returns each written airing's id with its listing's artwork.
    ///
    /// `clean_programs` first — a refresh is authoritative for the channels it
    /// covers — then the chunked multi-row upsert, which preserves each
    /// surviving airing's first-seen `DateCreated` (see
    /// [`PROGRAM_UPSERT_CONFLICT`]) the way upstream updates a programme item
    /// rather than recreating it.
    ///
    /// `rows` is drawn one statement's worth at a time, so the whole guide's
    /// rows never exist at once; only the id and artwork of each are kept, for
    /// the refresh's `CleanDatabase` and the item mirror's `UpdateImages`.
    async fn write_program_rows(
        &self,
        mut rows: ProgramRows<'_>,
        touched: &[String],
    ) -> Result<Vec<(Uuid, ProgramImages)>, ServiceError> {
        let now = datetime_to_db(Utc::now());
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        let existing_dates = Self::clean_programs(&mut tx, touched).await?;

        let per_statement = SQLITE_BIND_LIMIT / 27;
        let mut written = Vec::new();
        let mut chunk: Vec<ProgramRow<'_>> = Vec::with_capacity(per_statement);
        loop {
            chunk.extend(rows.by_ref().take(per_statement));
            if chunk.is_empty() {
                break;
            }
            // A short chunk is the last: the iterator is not asked again
            // after it has ended.
            let last = chunk.len() < per_statement;
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                r#"INSERT INTO "FerrofinLiveTvPrograms"
                   ("Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview",
                    "Genres","ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere",
                    "IsRepeat","OfficialRating","IsMovie","IsSeries","IsNews","IsKids",
                    "IsSports","IsLive","ExternalId","ExternalSeriesId","SeasonNumber",
                    "EpisodeNumber","DateCreated","ShowId") "#,
            );
            qb.push_values(&chunk, |mut b, row| {
                let prog = &row.fields;
                let class = &row.class;
                let id = guid_to_db(row.id);
                b.push_bind(&id)
                    .push_bind(row.channel_id)
                    .push_bind(&row.start)
                    .push_bind(&row.end)
                    .push_bind(prog.title)
                    .push_bind(class.episode_title.as_deref())
                    .push_bind(prog.overview)
                    .push_bind(&row.genres)
                    .push_bind(prog.image_url)
                    .push_bind(prog.production_year)
                    .push_bind(prog.episode_num)
                    .push_bind(i32::from(prog.is_new))
                    .push_bind(i32::from(prog.is_premiere))
                    .push_bind(i32::from(class.is_repeat))
                    .push_bind(prog.official_rating)
                    .push_bind(i32::from(class.is_movie))
                    .push_bind(i32::from(class.is_series))
                    .push_bind(i32::from(class.is_news))
                    .push_bind(i32::from(class.is_kids))
                    .push_bind(i32::from(class.is_sports))
                    .push_bind(i32::from(prog.is_live))
                    .push_bind(&row.external_id)
                    .push_bind(&class.external_series_id)
                    .push_bind(class.season_number)
                    .push_bind(class.episode_number)
                    .push_bind(
                        existing_dates
                            .get(&id)
                            .and_then(Clone::clone)
                            .unwrap_or_else(|| now.clone()),
                    )
                    .push_bind(&class.show_id);
            });
            qb.push(PROGRAM_UPSERT_CONFLICT);
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
            written.extend(chunk.drain(..).map(|row| (row.id, row.images)));
            if last {
                break;
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(written)
    }
}

#[async_trait]
impl LiveTvManager for FerrofinLiveTvManager {
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        // Port of `LiveTvManager.GetLiveTvInfo`. `Services` is the list of
        // `ILiveTvService`s, which on a stock server is exactly one —
        // `DefaultLiveTvService`, named "Emby" — and `GetServiceInfo` sets only
        // its name (so Status=Ok, IsVisible=false, Tuners=[] are the defaults).
        // Tuner hosts are NOT services; they never add an entry.
        let services = vec![LiveTvServiceInfo {
            name: Some("Emby".to_owned()),
            status: LiveTvServiceStatus::Ok,
            is_visible: false,
            tuners: Some(Vec::new()),
            ..LiveTvServiceInfo::default()
        }];
        // `IsLiveTvEnabled(user)`: the EnableLiveTvAccess permission AND
        // (Services.Count > 1 || TunerHosts.Length > 0) — with one service, a
        // tuner host must exist. Ids are `ToString("N")`.
        let has_tuners = !self.get_tuner_hosts().await?.is_empty();
        let mut enabled_users = Vec::new();
        if has_tuners && let Some(users) = &self.users {
            for user in users.get_users().await? {
                let dto = users.get_user_dto(&user, None).await?;
                if dto.policy.is_some_and(|p| p.enable_live_tv_access)
                    && let Ok(id) = Uuid::parse_str(&user.id)
                {
                    enabled_users.push(id.simple().to_string());
                }
            }
        }
        Ok(LiveTvInfo {
            is_enabled: !services.is_empty(),
            services,
            enabled_users,
        })
    }

    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError> {
        let rows =
            sqlx::query(r#"SELECT "Id", "Data" FROM "FerrofinLiveTvTunerHosts" ORDER BY "Id""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        // Every read refreshes the synchronous flag the "Refresh Guide" task's
        // hidden rule polls (the composition root does one read at boot to seed
        // it). It counts ROWS, not parsed DTOs: one undeserializable `Data`
        // blob must not make a configured tuner vanish from the rule.
        self.tuner_flag.store(!rows.is_empty(), Ordering::Relaxed);
        Ok(rows
            .iter()
            .filter_map(|r| {
                self.parse_stored_row(
                    crate::guide_repository::TUNER_HOSTS_TABLE,
                    r.get::<String, _>("Id").as_str(),
                    r.get::<String, _>("Data").as_str(),
                )
            })
            .collect())
    }

    async fn save_tuner_host(
        &self,
        mut info: TunerHostInfo,
    ) -> Result<TunerHostInfo, ServiceError> {
        // `TunerHostManager.SaveTunerHost` (v10.11.8 TunerHostManager.cs:60-69)
        // resolves the provider FIRST and throws `ResourceNotFoundException`
        // when nothing matches `info.Type` — a 404. Defaulting an unknown type
        // to "m3u" instead stored a row no host would ever read, so a typo in a
        // client's Type silently produced a tuner that fetched nothing.
        //
        // An ABSENT or EMPTY type is exactly that case, not an M3U shorthand:
        // upstream's lookup is `string.Equals(info.Type, i.Type,
        // OrdinalIgnoreCase)` over hosts whose `Type` is "hdhomerun"/"m3u", so
        // null and "" match nothing and 404 too. An earlier draft carved them
        // out to "m3u" — measured on the parity pair, that carve-out was the
        // one arm still answering 200 (and storing a row) where Jellyfin
        // answered 404.
        let type_id = info.type_.clone().unwrap_or_default();
        let Some(host) = crate::tuner_host::find_host(&self.hosts, &type_id) else {
            return Err(ServiceError::not_found(format!(
                "tuner host type {type_id}"
            )));
        };

        // Then the id: honoured only when it names a host that already exists
        // (`Array.FindIndex(..., OrdinalIgnoreCase)`, on the id as posted);
        // `index == -1 || IsNullOrWhiteSpace(info.Id)` mints a fresh
        // `Guid.NewGuid().ToString("N")` — 32 lowercase hex, no dashes.
        // `guid_to_db`'s uppercase-dashed DB form is a storage detail; these ids
        // go out over the wire on `GET /System/Configuration/livetv`. A match
        // keeps the posted spelling (`config.TunerHosts[index] = info`): the
        // stored row and its channels are renamed to it as the host is written.
        let posted = info.id.clone().filter(|id| !id.trim().is_empty());
        let stored = match posted.as_deref() {
            Some(wanted) => {
                crate::guide_repository::existing_config_id(
                    &self.db,
                    crate::guide_repository::TUNER_HOSTS_TABLE,
                    wanted,
                )
                .await?
            }
            None => None,
        };
        let id = match (posted, &stored) {
            (Some(wanted), Some(_)) => wanted,
            _ => Uuid::new_v4().simple().to_string(),
        };
        info.id = Some(id.clone());
        let url = info.url.clone().unwrap_or_default();
        if url.is_empty() {
            return Err(ServiceError::InvalidInput(
                "tuner host Url is required".into(),
            ));
        }

        // `if (provider is IConfigurableTunerHost configurable) await
        // configurable.Validate(info)` — before the row is written, so a device
        // that does not answer is not saved, and one that does contributes its
        // `DeviceId`.
        host.validate(&mut info).await?;

        let data = serde_json::to_string(&info)
            .map_err(|e| LiveTvError::serialize("serialize tuner host", e))?;
        crate::guide_repository::store_tuner_host(
            &self.db,
            stored.as_deref(),
            &id,
            &url,
            &type_id,
            &data,
        )
        .await?;
        self.tuner_flag.store(true, Ordering::Relaxed);
        Ok(info)
    }

    fn tuner_host_types(&self) -> Vec<ferrofin_model::dto::NameIdPair> {
        crate::tuner_host::tuner_host_types(&self.hosts)
    }

    async fn discover_tuners(
        &self,
        discovery_duration_ms: u64,
        new_devices_only: bool,
    ) -> Result<Vec<TunerHostInfo>, ServiceError> {
        // `TunerHostManager.DiscoverTuners(newDevicesOnly)` (v10.11.8
        // TunerHostManager.cs:102-121): every registered host is asked, and
        // `DiscoverDevices`' own `catch` turns a host that blew up into an
        // empty list rather than failing the request.
        //
        // `newDevicesOnly` filters the answer against the DEVICE IDS already in
        // the Live TV configuration — `config.TunerHosts.Where(i =>
        // !string.IsNullOrWhiteSpace(i.DeviceId)).Select(i => i.DeviceId)`, then
        // `!configuredDeviceIds.Contains(tuner.DeviceId, OrdinalIgnoreCase)`.
        // A configured row with no DeviceId contributes nothing to the filter,
        // and a discovered device with no DeviceId matches nothing, so both are
        // reported — that is upstream's `Contains` semantics, not a shortcut.
        let configured: Vec<String> = if new_devices_only {
            self.get_tuner_hosts()
                .await?
                .into_iter()
                .filter_map(|h| h.device_id)
                .filter(|d| !d.trim().is_empty())
                .collect()
        } else {
            Vec::new()
        };
        let mut found = Vec::new();
        for host in &self.hosts {
            match host.discover_devices(discovery_duration_ms).await {
                Ok(devices) => {
                    for device in devices {
                        if new_devices_only
                            && device.device_id.as_deref().is_some_and(|id| {
                                configured.iter().any(|c| c.eq_ignore_ascii_case(id))
                            })
                        {
                            continue;
                        }
                        tracing::info!(
                            host = host.name(),
                            url = device.url.as_deref().unwrap_or_default(),
                            "live tv: discovered a tuner device"
                        );
                        found.push(device);
                    }
                }
                Err(error) => {
                    tracing::error!(host = host.name(), %error, "live tv: tuner discovery failed");
                }
            }
        }
        Ok(found)
    }

    async fn delete_tuner_host(&self, id: &str) -> Result<(), ServiceError> {
        // `LiveTvController.DeleteTunerHost` filters with
        // `StringComparison.OrdinalIgnoreCase`; the stored key is a
        // BINARY-collated TEXT primary key, so the match must say NOCASE or a
        // case-differing id silently deletes nothing. SQLite's NOCASE folds
        // ASCII only, which is exact here: both servers mint this id as
        // `Guid.NewGuid().ToString("N")`, so the key space is 32 hex digits.
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvTunerHosts" WHERE "Id" = ?1 COLLATE NOCASE"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        // Only the configuration row goes, as upstream edits only the
        // configuration: the host's channels and airings stay listed until the
        // guide refresh the handler queues prunes them (migration 0046 dropped
        // the cascade).
        //
        // Recount so deleting the last host hides the guide-refresh task. This
        // is bookkeeping, not part of the delete's contract: a failed recount
        // must not turn a committed delete into a client-visible error.
        match crate::guide_repository::tuner_hosts_exist(&self.db).await {
            Ok(any) => self.tuner_flag.store(any, Ordering::Relaxed),
            Err(e) => tracing::warn!(error = %e, "live tv: tuner-host recount failed"),
        }
        Ok(())
    }

    fn has_tuner_hosts(&self) -> bool {
        self.tuner_flag.load(Ordering::Relaxed)
    }

    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError> {
        let rows = sqlx::query(
            r#"SELECT "Id", "Data" FROM "FerrofinLiveTvListingProviders" ORDER BY "Id""#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                self.parse_stored_row(
                    crate::guide_repository::LISTING_PROVIDERS_TABLE,
                    r.get::<String, _>("Id").as_str(),
                    r.get::<String, _>("Data").as_str(),
                )
            })
            .collect())
    }

    async fn save_listing_provider(
        &self,
        info: ListingsProviderInfo,
        validate_login: bool,
        validate_listings: bool,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        // `ListingsManager.SaveListingProvider`: the provider registered for
        // the type (a missing or unknown type is 404) validates the info before
        // anything is stored.
        match ListingsBackend::of(info.type_.as_deref())? {
            ListingsBackend::XmlTv => self.validate_xmltv(&info).await?,
            ListingsBackend::SchedulesDirect => {
                self.schedules_direct
                    .validate(&info, validate_login, validate_listings)
                    .await?;
            }
        }
        self.store_listing_provider(info).await
    }

    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError> {
        // `ListingsManager.DeleteListingsProvider` filters OrdinalIgnoreCase.
        sqlx::query(
            r#"DELETE FROM "FerrofinLiveTvListingProviders" WHERE "Id" = ?1 COLLATE NOCASE"#,
        )
        .bind(id)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        self.xmltv.invalidate(id).await;
        Ok(())
    }

    async fn get_lineups(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        country: Option<&str>,
        location: Option<&str>,
    ) -> Result<Vec<NameIdPair>, ServiceError> {
        // Port of `ListingsManager.GetLineups`. With a blank id the C# calls
        // `GetProvider(providerType).GetLineups(null, …)`: an unregistered type
        // is `ResourceNotFoundException` (404), and the xmltv backend
        // dereferences the null `info` inside `GetXml` and 500s. Ferrofin
        // answers 404 to both — there is no provider to read a document from,
        // and a NullReferenceException is not a contract.
        // `IsNullOrWhiteSpace(providerId)` decides "no provider"; the lookup
        // itself compares the id as given.
        let Some(id) = provider_id.filter(|s| !s.trim().is_empty()) else {
            return Err(ServiceError::not_found(format!(
                "Couldn't find provider of type {}",
                provider_type.unwrap_or_default()
            )));
        };
        let info = self.listing_provider_by_id(id).await?;
        // `GetProvider(info.Type).GetLineups(info, country, location)`.
        if ListingsBackend::of(info.type_.as_deref())? == ListingsBackend::SchedulesDirect {
            return self
                .schedules_direct
                .get_lineups(&info, country, location)
                .await;
        }
        // XMLTV has one lineup, the document's channels.
        Ok(self
            .provider_channels(&info)
            .await?
            .into_iter()
            .map(|c| NameIdPair {
                name: Some(c.name),
                id: Some(c.id),
            })
            .collect())
    }

    async fn get_channel_mapping_options(
        &self,
        provider_id: &str,
    ) -> Result<ChannelMappingOptionsDto, ServiceError> {
        // Port of `ListingsManager.GetChannelMappingOptions`.
        let info = self.listing_provider_by_id(provider_id).await?;
        let provider_name = ListingsBackend::of(info.type_.as_deref())?.name();
        let tuner_channels = self.channels_for_listings_provider(&info).await?;
        let provider_channels = self.provider_channels(&info).await?;
        let epg = EpgChannelData::new(&provider_channels);
        Ok(ChannelMappingOptionsDto {
            tuner_channels: tuner_channels
                .iter()
                .map(|(_, c)| tuner_channel_mapping(c, &info.channel_mappings, &epg))
                .collect(),
            provider_channels: provider_channels
                .into_iter()
                .map(|c| NameIdPair {
                    name: Some(c.name),
                    id: Some(c.id),
                })
                .collect(),
            mappings: info.channel_mappings,
            provider_name: Some(provider_name.to_owned()),
        })
    }

    async fn set_channel_mapping(
        &self,
        provider_id: &str,
        tuner_channel_id: &str,
        provider_channel_id: &str,
    ) -> Result<TunerChannelMapping, ServiceError> {
        // Port of `ListingsManager.SetChannelMapping`. The pair list is
        // rebuilt, not upserted: every pair keyed on this tuner channel is
        // removed first, and a replacement is stored only when the two ids
        // differ and the pair is new — so re-posting a channel onto itself is
        // the documented "unmap" gesture.
        let mut info = self.listing_provider_by_id(provider_id).await?;
        let already = info.channel_mappings.iter().any(|pair| {
            pair.name
                .as_deref()
                .is_some_and(|n| eq_ordinal_ignore_case(n, tuner_channel_id))
                && pair
                    .value
                    .as_deref()
                    .is_some_and(|v| eq_ordinal_ignore_case(v, provider_channel_id))
        });
        info.channel_mappings.retain(|pair| {
            !pair
                .name
                .as_deref()
                .is_some_and(|n| eq_ordinal_ignore_case(n, tuner_channel_id))
        });
        if !eq_ordinal_ignore_case(tuner_channel_id, provider_channel_id) && !already {
            info.channel_mappings.push(NameValuePair {
                name: Some(tuner_channel_id.to_owned()),
                value: Some(provider_channel_id.to_owned()),
            });
        }
        // Stored as it is: `SetChannelMapping` saves the configuration
        // without running the provider's `Validate`.
        let info = self.store_listing_provider(info).await?;

        let tuner_channels = self.channels_for_listings_provider(&info).await?;
        let provider_channels = self.provider_channels(&info).await?;
        let epg = EpgChannelData::new(&provider_channels);

        // Upstream ends here with `CancelIfRunningAndQueue<RefreshGuideScheduledTask>()`
        // and returns: the mapping moves listings only once the guide has been
        // rebuilt through it, but that rebuild is the TASK's work, not the
        // request's. Ferrofin queues it the same way, one layer out
        // (`handlers::live_tv::queue_guide_refresh`), so a POST does not block on
        // an M3U/XMLTV fetch. The response below is computed from the saved
        // configuration and the tuner lineup, exactly as the C# computes it —
        // it never depended on the refresh having finished.

        tuner_channels
            .iter()
            .map(|(_, c)| tuner_channel_mapping(c, &info.channel_mappings, &epg))
            .find(|row| {
                row.id
                    .as_deref()
                    .is_some_and(|id| eq_ordinal_ignore_case(id, tuner_channel_id))
            })
            // C# `.First(...)` throws `InvalidOperationException` (500) for a
            // tuner channel that is not in the lineup; 404 is the honest answer
            // for an id that names nothing.
            .ok_or_else(|| ServiceError::not_found(format!("tuner channel {tuner_channel_id}")))
    }

    async fn get_channels(
        &self,
        query: &LiveTvChannelQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        // Port of `LiveTvManager.GetInternalChannels` + the controller's
        // projection. The generic item repository upstream runs this over SQL;
        // the channel lineup is small (a tuner's M3U), so Ferrofin filters and
        // sorts the loaded rows instead — identical results, one query.
        let mut rows: Vec<ChannelRow> = crate::guide_repository::channel_rows(&self.db).await?;

        // Favourite/like filters and favourite-first sorting read the user's
        // channel user-data (C# pushes the same predicates into SQL).
        let user_id = query
            .user
            .as_ref()
            .and_then(|u| Uuid::parse_str(&u.id).ok());
        let needs_user_data = query.enable_favorite_sorting
            || query.is_favorite.is_some()
            || query.is_liked.is_some();
        let user_data = match user_id {
            Some(uid) if needs_user_data => {
                crate::guide_repository::channel_user_data(&self.db, uid).await?
            }
            _ => HashMap::new(),
        };
        crate::projection::filter_channel_rows(&mut rows, query, &user_data);
        crate::projection::sort_channel_rows(&mut rows, query, &user_data);

        let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
        let start_index = query.start_index.unwrap_or(0);
        let skip = usize::try_from(start_index).unwrap_or(0);
        let take = query
            .limit
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(usize::MAX);
        let page: Vec<ChannelRow> = rows.into_iter().skip(skip).take(take).collect();

        // The controller strips the four list fields BEFORE projecting the
        // page (`RemoveFields(dtoOptions)`), so the channel DTOs themselves
        // lack CanDelete/CanDownload/DisplayPreferencesId/Etag on the list.
        let mut list_options = options.clone();
        crate::projection::remove_fields(&mut list_options);
        let view_id = self.live_tv_view_id().await;
        let entities: Vec<_> = page
            .iter()
            .map(|r| channel_entity(r, parse_dt, view_id))
            .collect();
        // `AddChannelInfo` is NOT applied here: upstream runs it from inside
        // `DtoService.GetBaseItemDtos` (v10.11.8 DtoService.cs:168-192, which
        // buckets `item is LiveTvChannel` and calls
        // `LivetvManager.AddChannelInfo` at the end), so the projection below
        // already carries `Number`/`ChannelNumber`/`ChannelType` and the
        // current programme — and so does an ordinary `GET /Items/{id}` of the
        // same channel, which is the whole point of putting it there.
        let dtos = self
            .dto_service()?
            .get_base_item_dtos(&entities, &list_options, query.user.as_ref(), None, true)
            .await?;
        Ok(QueryResult::new(Some(start_index), Some(total), dtos))
    }

    async fn get_channel(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        let row: Option<ChannelRow> = crate::guide_repository::channel_row(&self.db, id).await?;
        let Some(row) = row else { return Ok(None) };
        // The single-channel path keeps every requested field (upstream's
        // `GetChannel` never calls `RemoveFields`); only the CurrentProgram
        // projection inside `add_channel_info` strips them. `AddChannelInfo`
        // itself runs inside the DTO service (DtoService.cs:201-204).
        let entity = channel_entity(&row, parse_dt, self.live_tv_view_id().await);
        Ok(Some(
            self.dto_service()?
                .get_base_item_dto(&entity, options, user, None)
                .await?,
        ))
    }

    async fn get_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        // Every filter is pushed into SQL (a guide week is tens of megabytes;
        // `Limit` must reach the query). The rows then project through the DTO
        // service exactly like `LiveTvManager.GetPrograms`: `RemoveFields` on
        // the list path, then the programme/recording post-passes.
        //
        // `SeriesTimerId` scopes the guide to one series timer's airings: the
        // timer whose internal id matches, ignoring case (stored ids are the
        // lower-case `N` form), sets `ExternalSeriesId` to its `SeriesId`. An
        // unknown timer, or one with no series id, answers nothing: "Better to
        // return nothing than every program in the database".
        let scoped;
        let query = match query
            .series_timer_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            Some(id) => {
                let series_id = self
                    .stored_series_timer(&id.to_ascii_lowercase())
                    .await?
                    .and_then(|timer| timer.series_id)
                    .filter(|series_id| !series_id.trim().is_empty());
                let Some(series_id) = series_id else {
                    return Ok(QueryResult::default());
                };
                scoped = InternalItemsQuery {
                    external_series_id: Some(series_id),
                    ..query.clone()
                };
                &scoped
            }
            None => query,
        };

        // One clock for both the row query and the count query: an
        // `isAiring`/`hasAired` request must not see a programme flip state
        // between them and report a total the page cannot contain.
        let now = Utc::now();
        let start_index = query.start_index.unwrap_or(0);
        let rows = self.query_program_rows(query, now).await?;
        let mut list_options = options.clone();
        remove_fields(&mut list_options);
        let items = self
            .program_dtos(&rows, &list_options, query.user.as_ref())
            .await?;

        // Same rule as the item repository: the count is only bought when
        // paging actually truncated the result.
        let total = if query.enable_total_record_count && (query.limit.is_some() || start_index > 0)
        {
            let mut cb: QueryBuilder<Sqlite> = QueryBuilder::new(PROGRAM_COUNT);
            push_program_filters(&mut cb, query, now);
            let count: i64 = cb
                .build_query_scalar()
                .fetch_one(self.db.pool())
                .await
                .map_err(db_err)?;
            i32::try_from(count).unwrap_or(i32::MAX)
        } else {
            i32::try_from(items.len()).unwrap_or(i32::MAX) + start_index
        };
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }

    async fn get_recommended_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        // Port of `LiveTvManager.GetRecommendedProgramsAsync` +
        // `GetRecommendedProgramsInternal` (v10.11.8 LiveTvManager.cs:266-334).
        //
        // When `isAiring` is anything but `true` this endpoint *is*
        // `GetPrograms` — same query, same paging.
        if query.is_airing != Some(true) {
            return self.get_programs(query, options).await;
        }

        // The airing branch is a different query, not a filtered one:
        //  - the sort is forced to StartDate ascending — the caller's `orderBy`
        //    is discarded, because the score is applied on top of it;
        //  - `StartIndex` is dropped (upstream builds `internalQuery` without
        //    it), so this endpoint does not page;
        //  - the limit is over-fetched to `max(limit * 4, 200)` so the ranking
        //    has a pool to rank *within*, and the caller's limit is applied
        //    after scoring;
        //  - `TotalRecordCount` is the size of that fetched pool, not the true
        //    guide total. That reads like a bug and is not one: it is what the
        //    "On Now" row is built against, and reporting the real total would
        //    make a client page into rows this endpoint never ranks.
        let mut internal = query.clone();
        internal.order_by = vec![(ItemSortBy::StartDate, SortOrder::Ascending)];
        internal.start_index = None;
        internal.limit = query.limit.map(|l| l.saturating_mul(4).max(200));

        let now = Utc::now();
        let rows = self.query_program_rows(&internal, now).await?;
        let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);

        let scores = match query
            .user
            .as_ref()
            .and_then(|u| Uuid::parse_str(&u.id).ok())
        {
            Some(uid) => {
                crate::guide_repository::channel_recommendation_data(&self.db, uid).await?
            }
            None => HashMap::new(),
        };

        // `OrderBy(i => i.StartDate.Date).ThenByDescending(score)`: the primary
        // key is the start *date*, not the instant, so everything airing on the
        // same day competes on score. LINQ's OrderBy is stable, so airings that
        // tie on both keep the StartDate order the query returned them in.
        let mut ranked: Vec<_> = rows
            .into_iter()
            .map(|row| {
                let score = recommendation_score(&row, scores.get(&row.channel_id));
                (row, score)
            })
            .collect();
        ranked.sort_by(|(a, sa), (b, sb)| {
            start_day(&a.start_date)
                .cmp(start_day(&b.start_date))
                .then(sb.cmp(sa))
        });
        if let Some(limit) = query.limit {
            ranked.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        let rows: Vec<_> = ranked.into_iter().map(|(row, _)| row).collect();

        let mut list_options = options.clone();
        remove_fields(&mut list_options);
        let items = self
            .program_dtos(&rows, &list_options, query.user.as_ref())
            .await?;
        Ok(QueryResult::new(query.start_index, Some(total), items))
    }

    async fn get_program(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        // Port of `LiveTvManager.GetProgram(id, ct, user)`: the single
        // programme keeps every requested field (no `RemoveFields` anywhere on
        // this path) and still gets the programme/recording post-passes.
        let rows = self
            .query_program_rows(
                &InternalItemsQuery {
                    item_ids: vec![id],
                    ..InternalItemsQuery::default()
                },
                Utc::now(),
            )
            .await?;
        Ok(self.program_dtos(&rows, options, user).await?.pop())
    }

    async fn reset_tuner(&self, id: &str) -> Result<(), ServiceError> {
        // Port of `LiveTvManager.ResetTuner`: the id is `{service key}_{tuner
        // id}`, and a first segment naming no registered `ILiveTvService` is
        // `ArgumentException("Service not found.")` — HTTP 400.
        let service = id.split_once(STREAM_ID_DELIMITER).map_or(id, |(s, _)| s);
        if !eq_ordinal_ignore_case(service, live_tv_service_key()) {
            return Err(ServiceError::invalid_input("Service not found."));
        }
        // The prefix names the one built-in service, whose
        // `DefaultLiveTvService.ResetTuner` is `Task.CompletedTask` — M3U
        // tuners are stateless HTTP streams, so there is nothing to reset.
        //
        // Deliberate divergence: with a matching prefix and no `_`, the C#
        // indexes `parts[1]` of a one-element split and throws
        // `IndexOutOfRangeException` (HTTP 500). Ferrofin no-ops; an unhandled
        // crash is not a contract.
        Ok(())
    }

    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        let generation = self.refresh_generation.fetch_add(1, Ordering::SeqCst) + 1;
        // FETCH FIRST, OUTSIDE THE LOCK. `guide_lock` exists to keep two passes
        // from interleaving their WRITES (see the field doc); holding it across
        // the tuner/guide fetches would put unbounded third-party network I/O
        // inside it, so one wedged M3U source would stall every later refresh —
        // a failure mode upstream does not have, because its one scheduled task
        // is CANCELLED when the next run is queued rather than queued behind.
        // The one read inside it is local: each fetched document's airings,
        // which bind to the channels the write half stores.
        //
        // `GuideManager.RefreshChannels` collects the ids this pass (re)wrote
        // and then drops everything else through `CleanDatabase` — but only
        // when nothing threw: its catch sets `cleanDatabase = false`, so a
        // tuner or guide that could not be read never empties the cache.
        let mut clean_database = true;
        // `(tuner id, tuner url, lineup)`: the url is half of each channel's
        // external id, which the Schedules Direct binding below needs.
        let mut tuner_lineups: Vec<(String, String, Vec<crate::tuner_host::TunerChannel>)> =
            Vec::new();
        for tuner in self.get_tuner_hosts().await? {
            let Some(id) = tuner.id.clone() else {
                clean_database = false;
                continue;
            };
            // `GuideManager.RefreshChannelsInternal` asks the tuner-host
            // manager for the channels, which fans out over the registered
            // hosts (`TunerHostManager`/`BaseTunerHost.GetChannels`). Dispatch
            // on the row's own `Type`: feeding an HDHomeRun's URL to the M3U
            // parser produced an empty lineup and no error at all.
            let type_id = tuner.type_.as_deref().unwrap_or_default();
            let Some(host) = crate::tuner_host::find_host(&self.hosts, type_id) else {
                clean_database = false;
                tracing::warn!(
                    tuner = %id,
                    type_id,
                    "live tv: no tuner host is registered for this type; its channels are skipped"
                );
                continue;
            };
            // Names the tuner on the host's own logs (an M3U playlist's
            // skipped entries).
            let lineup = tracing::Instrument::instrument(
                host.get_channels(&tuner),
                tracing::info_span!("tuner_lineup", tuner = %id),
            );
            match lineup.await {
                Ok(channels) => {
                    tuner_lineups.push((id, tuner.url.clone().unwrap_or_default(), channels));
                }
                Err(e) => {
                    // `BaseTunerHost.GetChannels` logs and falls back to its
                    // channel cache rather than failing the whole refresh, so
                    // one unreachable tuner must not lose the others' guide.
                    clean_database = false;
                    tracing::warn!(
                        tuner = %id,
                        url = tuner.url.as_deref().unwrap_or_default(),
                        error = %e,
                        "live tv: tuner lineup fetch failed"
                    );
                }
            }
        }
        // `GuideManager.RefreshChannelsInternal` asks each listings provider for
        // exactly `[now - 1h, now - 1h + GuideDays)` and keeps nothing else
        // (v10.11.8 GuideManager.cs:215-231). Ingesting the whole document
        // instead is not a harmless superset: it inflates every guide query's
        // `TotalRecordCount`, and it hands clients airings from months the
        // guide window never claimed to cover.
        let days = self.guide_days().await;
        let start = Utc::now() - chrono::Duration::hours(GUIDE_WINDOW_LEAD_IN_HOURS);
        let window = (start, start + chrono::Duration::days(days));
        tracing::info!(days, "live tv: refreshing guide");
        // Each readable provider's document and its channel list. The airings
        // are read in the write half, once the lineup they bind to is stored
        // (`insert_programs`).
        let mut guides: Vec<(ListingsProviderInfo, GuideSource)> = Vec::new();
        for provider in self.get_listing_providers().await? {
            let (source, complete) = self
                .fetch_guide_source(&provider, &tuner_lineups, window, generation)
                .await?;
            clean_database &= complete;
            if let Some(source) = source {
                guides.push((provider, source));
            }
        }

        // `DefaultLiveTvService.GetChannelsAsync` hands the lineup to
        // `AddProviderMetadata` before `GuideManager` stores it, so the stored
        // `ImageUrl` is the guide's logo for a matched channel.
        add_provider_metadata(&mut tuner_lineups, &guides);

        // Now the write half, one pass at a time. Channels go in before
        // programmes because `insert_programs` joins against the stored lineup.
        if self.superseded(generation) {
            // A newer refresh started while this one fetched; it writes the
            // guide from the configuration as it is now. (Should that pass
            // fail before its write, the guide waits for the next refresh, as
            // a cancelled task upstream leaves it.)
            tracing::debug!("live tv: guide refresh superseded by a newer one; not writing");
            return Ok(());
        }
        let guard = self.guide_lock.lock().await;
        let mut kept_channels: HashSet<String> = HashSet::new();
        for (id, _, channels) in &tuner_lineups {
            kept_channels.extend(self.replace_channels(id, channels).await?);
        }
        // The airings this pass wrote, with the artwork their listing gave
        // them: upstream's `newProgramIdList`, plus what `UpdateImages` needs.
        // A provider that could not be read writes none, so its cached
        // airings keep the images they have.
        let mut kept_programs: HashMap<Uuid, ProgramImages> = HashMap::new();
        // By value: each document goes as soon as its airings are written,
        // rather than living through the item mirror below.
        for (provider, source) in guides {
            kept_programs.extend(match source {
                GuideSource::XmlTv { document, channels } => {
                    self.insert_programs(document, &channels, &provider, window)
                        .await?
                }
                GuideSource::SchedulesDirect { channels, programs } => {
                    self.insert_schedules_direct_programs(&channels, &programs, &provider)
                        .await?
                }
            });
        }
        if clean_database {
            // `CleanDatabase(newChannelIdList, [LiveTvChannel], …)` then
            // `CleanDatabase(newProgramIdList, [LiveTvProgram], …)`: everything
            // this pass did not re-emit goes. With no listings provider left the
            // kept programme set is empty and the whole guide drains, which is
            // what removing a provider is supposed to do. Channels go first so a
            // dropped channel takes its airings with it via the FK cascade.
            let stale: Vec<String> = crate::guide_repository::all_channel_ids(&self.db)
                .await?
                .into_iter()
                .filter(|id| !kept_channels.contains(id))
                .collect();
            if !stale.is_empty() {
                crate::guide_repository::delete_channels(&self.db, &stale).await?;
            }
            let stale: Vec<String> = crate::guide_repository::all_program_ids(&self.db)
                .await?
                .into_iter()
                .filter(|id| !Uuid::parse_str(id).is_ok_and(|id| kept_programs.contains_key(&id)))
                .collect();
            if !stale.is_empty() {
                crate::guide_repository::delete_programs(&self.db, &stale).await?;
            }
        }
        // The lineup is settled; mirror it into `BaseItems` before the
        // programmes, exactly as `RefreshChannelsInternal` stores each channel
        // item (and cleans the departed ones) before it fetches programmes —
        // a programme item is parented to its channel's item.
        self.sync_channel_items().await?;
        // Every provider has spoken, so the programme table now holds exactly
        // upstream's `newProgramIdList` across all services — the set its
        // `CleanDatabase(newProgramIdList, [LiveTvProgram])` is taken over.
        let refreshed = self.sync_program_items(kept_programs).await?;
        // `RefreshGuide`'s tail: `RefreshSeriesTimers` then `RefreshTimers`
        // (v12.2 src/Jellyfin.LiveTv/Guide/GuideManager.cs:163-164).
        self.refresh_series_timers().await?;
        self.refresh_timers().await?;
        // Downloads, so outside the write lock: upstream pre-caches each
        // channel's programmes as it stores them; here they all go once the
        // guide is written.
        drop(guard);
        self.pre_cache_images(&refreshed).await;
        Ok(())
    }

    async fn get_guide_info(&self) -> Result<GuideInfo, ServiceError> {
        // Port of `GuideManager.GetGuideInfo` (v10.11.8 GuideManager.cs:82-92):
        // `now .. now + GetGuideDays()`, the same day count the ingest window
        // above uses, so the advertised window and the data behind it cannot
        // drift apart.
        let start = Utc::now();
        Ok(GuideInfo {
            start_date: start,
            end_date: start + chrono::Duration::days(self.guide_days().await),
        })
    }

    async fn add_info_to_program_dto(
        &self,
        dtos: &mut [BaseItemDto],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<(), ServiceError> {
        // `user` is upstream's unused trailing parameter — `AddInfoToProgramDto`
        // threads it no further than its own signature.
        let _ = user;
        // Port of `LiveTvManager.AddInfoToProgramDto` (v10.11.8
        // LiveTvManager.cs:535-576). The C# receives typed `LiveTvProgram`s
        // because `DtoService` already holds them; here the guide rows are
        // looked up by the DTOs' own ids, in ONE query for the page — an id that
        // is not a programme has no row and its DTO is left untouched.
        let ids: Vec<Uuid> = dtos.iter().map(|dto| dto.id).collect();
        let rows = crate::guide_repository::program_rows_by_ids(&self.db, &ids).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let has_channel_info = options.fields.contains(&ItemFields::ChannelInfo);
        let has_channel_image = options.fields.contains(&ItemFields::ChannelImage);
        let tags = if has_channel_image {
            self.channel_image_tags(&rows.values().collect::<Vec<_>>())
                .await
        } else {
            HashMap::new()
        };
        for dto in dtos.iter_mut() {
            let Some(row) = rows.get(&dto.id) else {
                continue;
            };
            let tag = Uuid::parse_str(&row.channel_id)
                .ok()
                .and_then(|id| tags.get(&id));
            Self::apply_program_info(dto, row, has_channel_info, has_channel_image, tag);
        }
        // `AddInfoToProgramDto` tail-calls `AddRecordingInfo` over the same
        // page, so a programme reached through `/Items` carries its
        // `TimerId`/`SeriesTimerId` exactly as one reached through
        // `/LiveTv/Programs` does.
        let timers: Vec<TimerInfoDto> = self.get_timers().await?;
        if timers.is_empty() {
            return Ok(());
        }
        for dto in dtos.iter_mut() {
            let Some(row) = rows.get(&dto.id) else {
                continue;
            };
            Self::apply_recording_info(dto, row, &timers);
        }
        Ok(())
    }

    async fn add_channel_info(
        &self,
        dtos: &mut [BaseItemDto],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<(), ServiceError> {
        // Port of `LiveTvManager.AddChannelInfo` (v10.11.8 LiveTvManager.cs:954-1010).
        // The C# receives `(dto, LiveTvChannel)` pairs because `DtoService`
        // already has the typed items in hand; here the rows are looked up by
        // the DTOs' own ids, in ONE query for the page — an id that is not a
        // channel simply has no row and its DTO is left untouched, so a mixed
        // `/Items` page costs one query and changes nothing.
        let ids: Vec<Uuid> = dtos.iter().map(|dto| dto.id).collect();
        let rows = crate::guide_repository::channel_rows_by_ids(&self.db, &ids).await?;
        if rows.is_empty() {
            return Ok(());
        }
        for dto in dtos.iter_mut() {
            let Some(row) = rows.get(&dto.id) else {
                continue;
            };
            dto.number.clone_from(&row.number);
            dto.channel_number.clone_from(&row.number);
            dto.channel_type = Some(if row.channel_type == "Radio" {
                ChannelType::Radio
            } else {
                ChannelType::Tv
            });
            // `GuideManager.GetChannel` stores `ProviderIds[ExternalServiceId]
            // = "Emby"`; the DTO service projected `{}` (the guide cache has no
            // `BaseItemProviders` rows), so fill it here when the field was
            // requested.
            if let Some(provider_ids) = dto.provider_ids.as_mut() {
                provider_ids.insert("ExternalServiceId".to_owned(), "Emby".to_owned());
            }
        }
        if !options.add_current_program {
            return Ok(());
        }

        // One airing query for the page: `MaxStartDate = MinEndDate = now`,
        // `Limit = channel count`, start-date ascending.
        let now = Utc::now();
        let channel_ids: Vec<Uuid> = rows.keys().copied().collect();
        let query = InternalItemsQuery {
            channel_ids: channel_ids.clone(),
            max_start_date: Some(now),
            min_end_date: Some(now),
            limit: i32::try_from(channel_ids.len()).ok(),
            order_by: vec![(ItemSortBy::StartDate, SortOrder::Ascending)],
            user: user.cloned(),
            ..InternalItemsQuery::default()
        };
        let program_rows = self.query_program_rows(&query, now).await?;
        // Both list and single paths strip the four fields for the programme
        // DTOs (`AddChannelInfo` calls `RemoveFields` before projecting them).
        let mut program_options = options.clone();
        remove_fields(&mut program_options);
        let program_dtos = self
            .program_dtos(&program_rows, &program_options, user)
            .await?;
        let mut by_channel: HashMap<Uuid, BaseItemDto> = HashMap::new();
        for program in program_dtos {
            if let Some(channel_id) = program.channel_id {
                // First per channel wins (rows are start-date ascending, and
                // C# takes `FirstOrDefault`).
                by_channel.entry(channel_id).or_insert(program);
            }
        }
        for dto in dtos.iter_mut() {
            if let Some(program) = by_channel.remove(&dto.id) {
                dto.current_program = Some(Box::new(program));
            }
        }
        Ok(())
    }

    async fn get_channel_stream_url(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let url: Option<String> = sqlx::query_scalar(
            r#"SELECT "StreamUrl" FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(url)
    }

    async fn get_channel_media_sources(
        &self,
        id: Uuid,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let Some((channel, tuner_host_id)) =
            crate::guide_repository::channel_stream_source(&self.db, id).await?
        else {
            return Ok(Vec::new());
        };
        let tuner = self.tuner_host(&tuner_host_id).await?;
        // Each host builds its own sources: the M3U host has exactly one, and
        // an HDHomeRun offers a source per transcode profile the device
        // supports (`BaseTunerHost.GetChannelStreamMediaSources`).
        let host = self.host_for(&tuner)?;
        let mut sources = host.channel_media_sources(&tuner, &channel).await?;
        for source in &mut sources {
            crate::stream::normalize(source);
            Self::stamp_source(source);
            // `LiveTvMediaSourceProvider.GetMediaSourcesInternal`: the open
            // token is `{item type}_{item id "N"}_{source id}`. The
            // media-source manager prefixes the provider key before it reaches
            // a client.
            source.open_token = Some(format!(
                "{OPEN_TOKEN_ITEM_TYPE}{STREAM_ID_DELIMITER}{}{STREAM_ID_DELIMITER}{}",
                id.simple(),
                source.id.clone().unwrap_or_default()
            ));
        }
        Ok(sources)
    }

    async fn open_channel_stream(
        &self,
        channel_id: Uuid,
        media_source_id: Option<&str>,
    ) -> Result<MediaSourceInfo, ServiceError> {
        // C# `GetChannelStream`: a media-source id equal to the channel id is
        // no id at all.
        let channel_key = channel_id.simple().to_string();
        let stream_id: Option<String> = media_source_id
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(&channel_key))
            .map(ToOwned::to_owned);

        // The whole join-or-open runs under one lock: two viewers tuning the
        // same channel at once must share one tuner connection, not race past
        // the join and open two. The lock is held across the DB reads, the
        // share probe and the tuner open — upstream serializes the identical
        // span (`MediaSourceManager._liveStreamLocker` wraps all of
        // `OpenLiveStreamInternal`), so a slow tuner delays other opens.
        let _open_guard = self.open_lock.lock().await;

        // Forget streams whose tuner hung up: their buffer is gone and nobody
        // can join them, so keeping them only grows the map.
        self.live_streams_lock()
            .retain(|_, entry| entry.is_sharing());

        // Upstream resolves the channel item before anything else
        // (`LiveTvMediaSourceProvider.GetChannelStream`), so an unknown id is a
        // 404 whether or not some other stream happens to be open.
        let Some((channel, tuner_host_id)) =
            crate::guide_repository::channel_stream_source(&self.db, channel_id).await?
        else {
            return Err(ServiceError::not_found(format!(
                "live tv channel {channel_id}"
            )));
        };

        // Join a stream already open on the same source (C#
        // `GetChannelStreamWithDirectStreamProvider`'s `ConsumerCount++`).
        if let Some(stream_id) = stream_id.as_deref()
            && let Some(shared) = self.join_open_stream(stream_id)
        {
            return Ok(shared);
        }

        let tuner = self.tuner_host(&tuner_host_id).await?;
        self.enforce_tuner_count(&tuner_host_id, tuner.tuner_count)?;

        // The host decides both the source and HOW it is opened: a plain HTTP
        // tuner URL, or — on a legacy HDHomeRun — the device's UDP control
        // protocol (`BaseTunerHost.GetChannelStream`).
        let host = self.host_for(&tuner)?;
        let chosen = host
            .channel_stream(&tuner, &channel, stream_id.as_deref())
            .await?;
        let mut source = chosen.source;
        let path = source.path.clone().unwrap_or_default();
        crate::stream::normalize(&mut source);
        Self::stamp_source(&mut source);

        let (unique_id, opened_at, alive, kind) = self
            .start_live_stream(&chosen.kind, &tuner, &path, &mut source)
            .await?;

        source.requires_closing = true;
        let live_stream_id = format!(
            "{}{STREAM_ID_DELIMITER}{}",
            live_tv_service_key(),
            source.id.clone().unwrap_or_default()
        );
        source.live_stream_id = Some(live_stream_id.clone());

        let handle = LiveStreamHandle {
            unique_id,
            original_stream_id: stream_id,
            tuner_host_id: Some(tuner_host_id),
            opened_at,
            consumer_count: 1,
            enable_stream_sharing: alive,
            media_source: source.clone(),
            kind,
        };
        // Two channels can share a stream URL, and a token without a
        // media-source id cannot join — either way the key may already be
        // taken. Dropping the old handle would DETACH its copy task (a
        // dropped `JoinHandle` does not abort), leaving a tuner connection
        // and a growing buffer that nothing can ever stop.
        let displaced = self.live_streams_lock().insert(live_stream_id, handle);
        if let Some(old) = displaced {
            tracing::warn!(
                unique_id = old.unique_id,
                "live tv: a new open displaced an existing live stream; closing the old one"
            );
            old.close().await;
        }
        Ok(source)
    }

    async fn get_live_stream_file(
        &self,
        unique_id: &str,
    ) -> Result<Option<LiveStreamFile>, ServiceError> {
        let open = self.live_streams_lock();
        Ok(open
            .values()
            .find(|entry| entry.unique_id.eq_ignore_ascii_case(unique_id) && entry.is_sharing())
            .and_then(|entry| {
                entry.temp_path().map(|path| LiveStreamFile {
                    path: path.to_path_buf(),
                    opened_at: entry.opened_at,
                })
            }))
    }

    async fn close_channel_stream(&self, live_stream_id: &str) -> Result<bool, ServiceError> {
        // C# `MediaSourceManager.CloseLiveStream`: one consumer leaves, and the
        // tuner connection only drops when the last one has.
        let closing = {
            let mut open = self.live_streams_lock();
            match open.get_mut(live_stream_id) {
                Some(entry) => {
                    entry.consumer_count -= 1;
                    tracing::info!(
                        live_stream_id,
                        consumers = entry.consumer_count,
                        "live tv: released a live stream consumer"
                    );
                    if entry.consumer_count <= 0 {
                        open.remove(live_stream_id)
                    } else {
                        None
                    }
                }
                // Nothing open under that id: as far as the caller is
                // concerned it is closed.
                None => return Ok(true),
            }
        };
        match closing {
            Some(handle) => {
                handle.close().await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn get_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        // Port of `DefaultLiveTvService.GetTimersAsync` (v10.11.8
        // DefaultLiveTvService.cs:392-403): a COMPLETED timer is not a scheduled
        // recording any more and is excluded from every list read. The row stays
        // (the recording it made points at it); only this view drops it.
        Ok(self
            .all_timers()
            .await?
            .into_iter()
            .filter(|t| t.status != RecordingStatus::Completed)
            .collect())
    }

    async fn get_timer(&self, id: &str) -> Result<Option<TimerInfoDto>, ServiceError> {
        self.json_get("FerrofinLiveTvTimers", SELECT_TIMER_SQL, id)
            .await
    }

    async fn create_timer(&self, mut timer: TimerInfoDto) -> Result<String, ServiceError> {
        // Port of `DefaultLiveTvService.CreateTimer`: one timer per programme —
        // a cancelled or completed one is revived, a live one is a conflict.
        if let Some(existing) = self.timer_for_program(&timer).await? {
            let existing_id = existing.base.id.clone().unwrap_or_default();
            if matches!(
                existing.status,
                RecordingStatus::Cancelled | RecordingStatus::Completed
            ) {
                let mut revived = existing;
                revived.status = RecordingStatus::New;
                // `existingTimer.IsManual = true` (DefaultLiveTvService.cs:226).
                self.persist_manual_timer(&revived).await?;
                self.arm_timer(&revived);
                return Ok(existing_id);
            }
            return Err(ServiceError::InvalidInput(
                "A scheduled recording already exists for this program.".to_owned(),
            ));
        }

        // The id is always the server's to mint: C# overwrites whatever the
        // client posted with a fresh GUID (`info.Id = Guid.NewGuid().ToString("N")`,
        // DefaultLiveTvService.cs:235) and publishes its MD5 as the DTO id
        // (`Id = GetInternalTimerId(info.Id)`, LiveTvDtoService.cs:56), keeping
        // the GUID itself on the wire as `ExternalId` (:62).
        let external_id = Uuid::new_v4().simple().to_string();
        timer.base.id = Some(ferrofin_traits::stubs::internal_timer_id(&external_id));
        timer.base.external_id = Some(external_id);
        timer.base.type_ = Some("Timer".to_owned());
        timer.base.service_name = Some(ferrofin_traits::stubs::LIVE_TV_SERVICE_NAME.to_owned());
        timer.base.server_id = Some(self.server_id.clone());
        // `CopyProgramInfoToTimerInfo`: the guide is the authority on what this
        // timer is actually recording.
        if let Some(program) = self.program_row_for_timer(&timer.base).await? {
            copy_program_into_timer(&program, &mut timer);
        }
        // `ExternalChannelId = info.ChannelId` (LiveTvDtoService.cs:69).
        timer.base.external_channel_id = self.channel_external_id(timer.base.channel_id).await?;
        // `info.IsManual = true` (DefaultLiveTvService.cs:255).
        let id = self.persist_manual_timer(&timer).await?;
        self.arm_timer(&timer);
        Ok(id)
    }

    async fn update_timer(&self, id: &str, timer: TimerInfoDto) -> Result<(), ServiceError> {
        // Port of `DefaultLiveTvService.UpdateTimerAsync` (v10.11.8
        // DefaultLiveTvService.cs:342-363): an unknown id is a
        // `ResourceNotFoundException`, a timer whose capture is already running
        // is left alone, and the ONLY fields a client may change are the four
        // padding ones. Everything else on the posted DTO — Name, Priority,
        // Status, the dates, the channel — is discarded, because the guide and
        // the server own those.
        let Some(mut existing) = self.get_timer(id).await? else {
            return Err(ServiceError::NotFound(format!(
                "Timer with Id {id} not found"
            )));
        };
        if self.active_recordings_lock().contains_key(id) {
            return Ok(());
        }
        existing.base.pre_padding_seconds = timer.base.pre_padding_seconds;
        existing.base.post_padding_seconds = timer.base.post_padding_seconds;
        existing.base.is_post_padding_required = timer.base.is_post_padding_required;
        existing.base.is_pre_padding_required = timer.base.is_pre_padding_required;
        self.persist_timer(&existing).await?;
        // C# `TimerManager.Update` re-arms the system timer behind it.
        self.arm_timer(&existing);
        Ok(())
    }

    async fn cancel_timer(&self, id: &str) -> Result<(), ServiceError> {
        // `CancelTimerAsync` -> `CancelTimerInternal(timerId, false, true)`
        // (v10.11.8 DefaultLiveTvService.cs:199-203): a hand cancel is manual.
        self.cancel_timer_internal(id, false, true).await
    }

    async fn get_new_timer_defaults(
        &self,
        program_id: Option<Uuid>,
    ) -> Result<SeriesTimerInfoDto, ServiceError> {
        let options = self.live_tv_options().await;
        let mut defaults = ferrofin_traits::stubs::new_timer_defaults(
            options.pre_padding_seconds,
            options.post_padding_seconds,
        );
        // `LiveTvDtoService.GetSeriesTimerInfoDto` sets `ServerId =
        // _appHost.SystemId` on every timer DTO it builds; a strict client that
        // expects a non-null there crashes without it.
        defaults.base.server_id = Some(self.server_id.clone());
        let Some(program_id) = program_id else {
            return Ok(defaults);
        };
        let Some(program) = self.program_row(program_id).await? else {
            return Ok(defaults);
        };

        // `LiveTvManager.GetNewTimerDefaults(programId)`: the programme's own
        // identity replaces the standing defaults.
        defaults.record_new_only = !program.is_repeat;
        defaults.skip_episodes_in_library = defaults.record_new_only;
        defaults.base.name = Some(program.title.clone());
        defaults.base.overview.clone_from(&program.overview);
        defaults.base.channel_id = Uuid::parse_str(&program.channel_id).unwrap_or_default();
        defaults.base.channel_name = Some(program.channel_name.clone());
        if let Some(start) = parse_dt(&program.start_date) {
            defaults.base.start_date = start;
        }
        if let Some(end) = program.end_date.as_deref().and_then(parse_dt) {
            defaults.base.end_date = end;
        }
        // The client posts these straight back as the new timer, so the
        // programme is named the way a DTO names it (`programDto.Id`), with the
        // listing provider's own id alongside.
        defaults.base.program_id = Some(program_id.simple().to_string());
        defaults
            .base
            .external_program_id
            .clone_from(&program.external_id);
        Ok(defaults)
    }

    async fn get_series_timers(
        &self,
        query: &ferrofin_model::live_tv::SeriesTimerQuery,
    ) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
        let mut timers: Vec<SeriesTimerInfoDto> = self
            .json_list(
                "FerrofinLiveTvSeriesTimers",
                r#"SELECT "Id", "Data" FROM "FerrofinLiveTvSeriesTimers""#,
            )
            .await?;
        sort_series_timers(&mut timers, query);
        Ok(timers)
    }

    async fn get_series_timer(&self, id: &str) -> Result<Option<SeriesTimerInfoDto>, ServiceError> {
        self.json_get(
            "FerrofinLiveTvSeriesTimers",
            r#"SELECT "Data" FROM "FerrofinLiveTvSeriesTimers" WHERE "Id" = ?1"#,
            id,
        )
        .await
    }

    async fn create_series_timer(
        &self,
        mut timer: SeriesTimerInfoDto,
    ) -> Result<String, ServiceError> {
        // Port of `DefaultLiveTvService.CreateSeriesTimer` (v10.11.8
        // DefaultLiveTvService.cs:263-309) + the DTO projection
        // `LiveTvDtoService.GetSeriesTimerInfoDto` (LiveTvDtoService.cs:115-158).
        //
        // The id is NEVER the client's: upstream overwrites whatever was posted
        // with a fresh GUID and publishes its MD5. That matters far more here
        // than on the single-timer path, because the body a client posts is the
        // one `GET /LiveTv/Timers/Defaults` handed it, whose `Id` is the
        // constant `internal_series_timer_id("")` for every programme — honour
        // it and every "record series" overwrites the last one.
        let external_id = Uuid::new_v4().simple().to_string();
        let id = ferrofin_traits::stubs::internal_series_timer_id(&external_id);

        // `var program = GetProgramInfoFromCache(info.ProgramId); … else throw
        // new InvalidOperationException("SeriesId for program not found")`
        // (DefaultLiveTvService.cs:267-275). Upstream's throw surfaces as a
        // `500`; a body naming a programme the guide does not have is a bad
        // request, so Ferrofin answers `400` and stores nothing — the divergence
        // is the status code, not the rejection.
        let Some(program) = self.program_row_for_timer(&timer.base).await? else {
            return Err(ServiceError::InvalidInput(
                "SeriesId for program not found".to_owned(),
            ));
        };

        timer.base.id = Some(id.clone());
        timer.base.external_id = Some(external_id.clone());
        timer.base.type_ = Some("SeriesTimer".to_owned());
        timer.base.service_name = Some(ferrofin_traits::stubs::LIVE_TV_SERVICE_NAME.to_owned());
        timer.base.server_id = Some(self.server_id.clone());
        // Name/Overview are the CLIENT's and are never recomputed from the
        // programme: `LiveTvDtoService.GetSeriesTimerInfo` binds `Name =
        // dto.Name` / `Overview = dto.Overview` (v10.11.8
        // LiveTvDtoService.cs:497-499), `DefaultLiveTvService.CreateSeriesTimer`
        // (:263-309) never touches either, and the read projection
        // `GetSeriesTimerInfoDto` (:121) emits `Name = info.Name`. Overwriting
        // them here made a client-chosen series name impossible to store at
        // all, because `UpdateSeriesTimerAsync`'s whitelist (:314-334) has no
        // Name either — and it was pure divergence, since
        // `get_new_timer_defaults` already seeds both from the programme, which
        // is the body the client posts back.
        if let Ok(channel_id) = Uuid::parse_str(&program.channel_id) {
            timer.base.channel_id = channel_id;
        }
        timer.base.channel_name = Some(program.channel_name.clone());
        if let Ok(program_id) = Uuid::parse_str(&program.id) {
            timer.base.program_id = Some(program_id.simple().to_string());
        }
        timer
            .base
            .external_program_id
            .clone_from(&program.external_id);
        // `ExternalChannelId = info.ChannelId` — the tuner-facing id the internal
        // channel GUID was derived from (LiveTvDtoService.cs:137).
        timer.base.external_channel_id = self.channel_external_id(timer.base.channel_id).await?;
        // `dto.DayPattern = GetDayPattern(info.Days)` — recomputed on every read,
        // never the client's (LiveTvDtoService.cs:152).
        timer.day_pattern = day_pattern(&timer.days);
        // "// Set priority from default values" — `LiveTvManager.CreateSeriesTimer`
        // (v10.11.8 LiveTvManager.cs:1145-1147) throws away whatever Priority the
        // client posted and takes the standing defaults' instead. Priority is
        // only settable through `UpdateSeriesTimerAsync`, whose whitelist has it.
        timer.base.priority = self.get_new_timer_defaults(None).await?.base.priority;

        let stored = StoredSeriesTimer {
            external_id,
            series_id: program.external_series_id.clone(),
            dto: timer,
        };
        self.persist_series_timer(&stored).await?;
        // "If any timers have already been manually created, make sure they
        // don't get cancelled" (DefaultLiveTvService.cs:279-303).
        self.adopt_existing_timers(&stored).await?;
        self.update_timers_for_series_timer(&stored, true, false)
            .await?;
        Ok(id)
    }

    async fn update_series_timer(
        &self,
        id: &str,
        timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        // Port of `DefaultLiveTvService.UpdateSeriesTimerAsync` (v10.11.8
        // DefaultLiveTvService.cs:312-340). The row is keyed on the BODY's id
        // and ONLY on it: `LiveTvController.UpdateSeriesTimer` (:933-937) hands
        // `LiveTvManager.UpdateSeriesTimer` the body alone and never looks at
        // the route's `timerId`, and `UpdateSeriesTimerAsync` matches on
        // `info.Id` (:314). Keying on the route as a fallback was tried here and
        // reverted: it is strictly more lenient than upstream, so a client
        // posting a stale or foreign body id to a valid route would update on
        // Ferrofin and no-op on Jellyfin — a silent divergence on a write path.
        // An id that matches no row writes NOTHING; it must not mint a ghost.
        let body_id = timer
            .base
            .id
            .as_deref()
            .map(str::trim)
            .filter(|body_id| !body_id.is_empty());
        if body_id.is_some_and(|body_id| !body_id.eq_ignore_ascii_case(id)) {
            tracing::debug!(
                route_id = %id,
                "live tv: series-timer update body names a different id than the route; \
                 upstream keys on the body, so the body wins"
            );
        }
        let stored = match body_id {
            Some(body_id) => self.stored_series_timer(body_id).await?,
            None => None,
        };
        let Some(mut stored) = stored else {
            return Ok(());
        };

        // The whitelist, verbatim (DefaultLiveTvService.cs:318-332). Name,
        // Overview, ProgramId and SeriesId are deliberately NOT in it: the
        // programme owns them.
        stored.dto.base.channel_id = timer.base.channel_id;
        stored.dto.days = timer.days;
        stored.dto.base.end_date = timer.base.end_date;
        stored.dto.base.is_post_padding_required = timer.base.is_post_padding_required;
        stored.dto.base.is_pre_padding_required = timer.base.is_pre_padding_required;
        stored.dto.base.post_padding_seconds = timer.base.post_padding_seconds;
        stored.dto.base.pre_padding_seconds = timer.base.pre_padding_seconds;
        stored.dto.base.priority = timer.base.priority;
        stored.dto.record_any_channel = timer.record_any_channel;
        stored.dto.record_any_time = timer.record_any_time;
        stored.dto.record_new_only = timer.record_new_only;
        stored.dto.skip_episodes_in_library = timer.skip_episodes_in_library;
        stored.dto.keep_up_to = timer.keep_up_to;
        stored.dto.base.keep_until = timer.base.keep_until;
        stored.dto.base.start_date = timer.base.start_date;

        // `ExternalChannelId`, `ChannelName` and `DayPattern` are all re-derived
        // on every read upstream (`GetSeriesTimerInfoDto` +
        // `LiveTvManager.GetSeriesTimers`' channel lookup), so a changed
        // ChannelId or day list has to carry them with it here.
        stored.dto.base.external_channel_id =
            self.channel_external_id(stored.dto.base.channel_id).await?;
        if let Some(channel) =
            crate::guide_repository::channel_row(&self.db, stored.dto.base.channel_id).await?
        {
            stored.dto.base.channel_name = Some(channel.name);
        }
        stored.dto.day_pattern = day_pattern(&stored.dto.days);

        self.persist_series_timer(&stored).await?;
        self.update_timers_for_series_timer(&stored, true, true)
            .await
    }

    async fn cancel_series_timer(&self, id: &str) -> Result<(), ServiceError> {
        // Port of `LiveTvManager.CancelSeriesTimer` (v10.11.8 LiveTvManager.cs:827-841)
        // + `DefaultLiveTvService.CancelSeriesTimerAsync` (:147-166): an unknown
        // id is a `ResourceNotFoundException`, and every timer the series timer
        // scheduled goes through the real per-timer cancel — which disarms it
        // and stops any capture it had already started — not a bare `DELETE`.
        if self.get_series_timer(id).await?.is_none() {
            return Err(ServiceError::NotFound(format!(
                "SeriesTimer with Id {id} not found"
            )));
        }
        // `CancelSeriesTimerAsync` walks `_timerManager.GetAll()` (:149-152),
        // NOT the `Completed`-excluding `GetTimersAsync` — so a child that has
        // already recorded goes too, and the series timer leaves no orphan row
        // behind pointing at an id nothing answers. `timer_ids_for_series` is
        // the `GetAll()` shape on purpose: it reads the table, not the filtered
        // view `get_timers` publishes.
        for child in self.timer_ids_for_series(id).await? {
            self.cancel_timer_internal(&child, true, true).await?;
        }
        self.delete_by_id(
            r#"DELETE FROM "FerrofinLiveTvSeriesTimers" WHERE "Id" = ?1"#,
            id,
        )
        .await
    }

    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        self.get_recordings_matching(
            &ferrofin_model::live_tv::RecordingQuery::default(),
            None,
            &DtoOptions::default(),
        )
        .await
    }

    async fn get_recordings_matching(
        &self,
        query: &ferrofin_model::live_tv::RecordingQuery,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let rows = crate::dvr_repository::recording_rows(&self.db, query).await?;
        let start_index = query.start_index.unwrap_or(0);
        let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
        let page: Vec<crate::projection::RecordingRow> = rows
            .into_iter()
            .skip(usize::try_from(start_index).unwrap_or(0))
            .take(
                query
                    .limit
                    .and_then(|l| usize::try_from(l).ok())
                    .unwrap_or(usize::MAX),
            )
            .collect();

        // `RemoveFields(options)` on the list path, as the channel and
        // programme lists do.
        let mut list_options = options.clone();
        remove_fields(&mut list_options);
        let dtos = self.recording_dtos(&page, &list_options, user).await?;
        Ok(QueryResult::new(Some(start_index), Some(total), dtos))
    }

    async fn get_recording_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(crate::dvr_repository::recording_row(&self.db, id)
            .await?
            .map(|row| crate::projection::recording_entity(&row, parse_dt)))
    }

    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError> {
        let Some(row) = crate::dvr_repository::recording_row(&self.db, id).await? else {
            return Ok(None);
        };
        // The single-recording path keeps every requested field (upstream's
        // `GetRecording` uses `new DtoOptions()` and never strips).
        Ok(self
            .recording_dtos(
                std::slice::from_ref(&row),
                &DtoOptions::with_all_fields(true),
                None,
            )
            .await?
            .pop())
    }

    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let Some(row) = crate::dvr_repository::recording_row(&self.db, id).await? else {
            return Ok(None);
        };
        // Only report a path that actually points at a captured file.
        Ok(row.path.filter(|p| !p.is_empty()))
    }

    async fn get_active_recording_path(
        &self,
        timer_id: &str,
    ) -> Result<Option<String>, ServiceError> {
        Ok(self
            .active_recordings_lock()
            .get(timer_id)
            .map(|recording| recording.path.display().to_string()))
    }

    async fn active_recording_paths(&self) -> Result<Vec<String>, ServiceError> {
        // `RecordingsManager.GetActiveRecordingInfo`: a capture told to stop,
        // or whose timer is no longer `InProgress`, is not an active recording.
        let live: Vec<(String, String)> = self
            .active_recordings_lock()
            .values()
            .filter(|recording| !recording.is_cancelled())
            .map(|recording| {
                (
                    recording.timer_id.clone(),
                    recording.path.display().to_string(),
                )
            })
            .collect();
        let mut paths = Vec::with_capacity(live.len());
        for (timer_id, path) in live {
            let timer = self.get_timer(&timer_id).await?;
            if timer.is_some_and(|timer| timer.status == RecordingStatus::InProgress) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    async fn live_tv_media_type(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        // `LiveTvChannel.MediaType => ChannelType == ChannelType.Radio ?
        // MediaType.Audio : MediaType.Video` — the same mapping
        // `add_channel_info`/`add_recording_info` already apply to the DTO.
        if let Some(row) = crate::guide_repository::channel_row(&self.db, id).await? {
            return Ok(Some(radio_or_tv_media_type(&row.channel_type).to_owned()));
        }
        // A recording is the capture of one channel's airing, so it carries
        // that channel's media type (upstream's `RecordingHelper` builds an
        // `Audio` item for a radio programme and a `Movie`/`Episode`/`Video`
        // for a television one — all `MediaType.Video`).
        let Some(recording) = crate::dvr_repository::recording_row(&self.db, id).await? else {
            return Ok(None);
        };
        let Ok(channel_id) = Uuid::parse_str(&recording.channel_id) else {
            return Ok(None);
        };
        Ok(crate::guide_repository::channel_row(&self.db, channel_id)
            .await?
            .map(|row| radio_or_tv_media_type(&row.channel_type).to_owned()))
    }

    async fn get_recording_media_sources(
        &self,
        recording_id: Uuid,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let Some(row) = crate::dvr_repository::recording_row(&self.db, recording_id).await? else {
            return Ok(Vec::new());
        };
        let active = row
            .timer_id
            .as_deref()
            .and_then(|id| self.active_recordings_lock().get(id).cloned());

        if let Some(active) = active {
            // Port of `MediaSourceManager.GetRecordingStreamMediaSources`: the
            // growing file is the path, and `EncoderPath` is how a transcode
            // reads it — progressively, back through this server.
            let base_url = self.local_api_url().await?;
            return Ok(vec![MediaSourceInfo {
                id: Some(active.timer_id.clone()),
                encoder_path: Some(format!(
                    "{base_url}/LiveTv/LiveRecordings/{}/stream",
                    active.timer_id
                )),
                encoder_protocol: Some(MediaProtocol::Http),
                path: Some(active.path.display().to_string()),
                protocol: MediaProtocol::File,
                supports_direct_play: false,
                supports_direct_stream: true,
                supports_transcoding: true,
                is_infinite_stream: true,
                requires_opening: false,
                requires_closing: false,
                buffer_ms: Some(0),
                ignore_dts: true,
                ignore_index: true,
                ..MediaSourceInfo::default()
            }]);
        }

        // A finished recording is an ordinary file.
        let Some(path) = row.path.as_deref().filter(|p| !p.is_empty()) else {
            return Ok(Vec::new());
        };
        Ok(vec![MediaSourceInfo {
            id: Some(recording_id.simple().to_string()),
            path: Some(path.to_owned()),
            protocol: MediaProtocol::File,
            container: Some(LIVE_STREAM_BUFFER_CONTAINER.to_owned()),
            name: Some(row.name.clone()),
            ..MediaSourceInfo::default()
        }])
    }

    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError> {
        // A capture still running has to stop before its file can go.
        let row = crate::dvr_repository::recording_row(&self.db, id).await?;
        if let Some(timer_id) = row.as_ref().and_then(|row| row.timer_id.as_deref()) {
            self.cancel_recording(timer_id);
        }
        if let Some(path) = row.and_then(|row| row.path) {
            // Best-effort: a row whose file has already gone still deletes.
            let _ = tokio::fs::remove_file(&path).await;
        }
        crate::dvr_repository::delete_recording(&self.db, id).await
    }

    async fn start_dvr(&self) -> Result<(), ServiceError> {
        // Nothing is capturing yet, so a row still marked `InProgress` is one a
        // crash or restart abandoned. Left alone it would answer
        // `isInProgress=true` for ever and report a percentage that keeps
        // climbing; settle it by whether its file survived.
        for row in crate::dvr_repository::recording_rows(
            &self.db,
            &ferrofin_model::live_tv::RecordingQuery {
                is_in_progress: Some(true),
                ..ferrofin_model::live_tv::RecordingQuery::default()
            },
        )
        .await?
        {
            let Ok(id) = Uuid::parse_str(&row.id) else {
                continue;
            };
            let path = row.path.as_deref().filter(|p| !p.is_empty());
            let kept = match path {
                Some(path) => {
                    tokio::fs::try_exists(path).await.unwrap_or(false)
                        && !crate::dvr::is_empty_file(std::path::Path::new(path)).await
                }
                None => false,
            };
            if kept {
                crate::dvr_repository::finish_recording(
                    &self.db,
                    id,
                    RecordingStatus::Completed,
                    path,
                )
                .await?;
            } else {
                crate::dvr_repository::delete_recording(&self.db, id).await?;
            }
            tracing::warn!(
                recording_id = row.id,
                kept,
                "live tv: settled a recording the last run left in progress"
            );
        }

        // C# `TimerManager.RestartTimers`: every persisted timer is re-armed,
        // so a restart mid-schedule still records. `GetAll()`, not the
        // Completed-excluding list view.
        for timer in self.all_timers().await? {
            self.arm_timer(&timer);
        }
        Ok(())
    }

    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
        self.schedules_direct.get_available_countries().await
    }
}

/// `LiveTvChannel.MediaType` for a stored channel's `ChannelType` column.
///
/// `ChannelType == ChannelType.Radio ? MediaType.Audio : MediaType.Video`.
fn radio_or_tv_media_type(channel_type: &str) -> &'static str {
    if channel_type == "Radio" {
        "Audio"
    } else {
        "Video"
    }
}

impl FerrofinLiveTvManager {
    /// Runs the guide's program query, returning the typed rows the DTO path
    /// consumes. Shared by `get_programs`, `get_program` and the channel
    /// current-program pass.
    async fn query_program_rows(
        &self,
        query: &InternalItemsQuery,
        now: DateTime<Utc>,
    ) -> Result<Vec<GuideProgramRow>, ServiceError> {
        let start_index = query.start_index.unwrap_or(0);
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(PROGRAM_SELECT);
        push_program_filters(&mut qb, query, now);
        push_program_order(&mut qb, &query.order_by);
        push_program_paging(&mut qb, query.limit, start_index);
        qb.build_query_as()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Projects guide programme rows into `BaseItemDto`s: the synthetic
    /// entities run through the DTO service, then the two upstream
    /// post-passes ([`Self::add_info_to_program_dto`],
    /// [`Self::add_recording_info`]).
    async fn program_dtos(
        &self,
        rows: &[GuideProgramRow],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let view_id = self.live_tv_view_id().await;
        let entities: Vec<_> = rows
            .iter()
            .map(|r| program_entity(r, parse_dt, view_id))
            .collect();
        let mut dtos = self
            .dto_service()?
            .get_base_item_dtos(&entities, options, user, None, true)
            .await?;
        // The rows are already in DTO order here, so the same two post-passes
        // the `/Items` seam runs by id run by position.
        let has_channel_info = options.fields.contains(&ItemFields::ChannelInfo);
        let has_channel_image = options.fields.contains(&ItemFields::ChannelImage);
        let tags = if has_channel_image {
            self.channel_image_tags(&rows.iter().collect::<Vec<_>>())
                .await
        } else {
            HashMap::new()
        };
        for (dto, row) in dtos.iter_mut().zip(rows) {
            let tag = Uuid::parse_str(&row.channel_id)
                .ok()
                .and_then(|id| tags.get(&id));
            Self::apply_program_info(dto, row, has_channel_info, has_channel_image, tag);
        }
        self.add_recording_info(&mut dtos, rows).await?;
        Ok(dtos)
    }

    /// Port of `LiveTvManager.AddInfoToProgramDto`'s per-programme body
    /// (v10.11.8 src/Jellyfin.LiveTv/LiveTvManager.cs:535-576): the airing
    /// fields the DTO service does not project (`StartDate`, `EpisodeTitle`,
    /// the true-only flags), and — only when the `ChannelInfo`/`ChannelImage`
    /// field was requested — the owning channel's name/number/media type.
    ///
    /// Split out per-DTO because it has two callers that reach a programme by
    /// different routes: `/LiveTv/Programs`, where the rows arrive in DTO
    /// order, and the `DtoService` seam, where a programme is one item on an
    /// arbitrary `/Items` page and its row is found by id.
    fn apply_program_info(
        dto: &mut BaseItemDto,
        row: &GuideProgramRow,
        has_channel_info: bool,
        has_channel_image: bool,
        channel_image_tag: Option<&String>,
    ) {
        dto.start_date = parse_dt(&row.start_date);
        dto.episode_title.clone_from(&row.episode_title);
        // C# `dto.IsNews |= program.IsNews` on a null `bool?`: a false flag
        // stays null and is never serialized, so only true flags appear.
        let flag = |on: bool| on.then_some(true);
        dto.is_repeat = dto.is_repeat.or_else(|| flag(row.is_repeat));
        dto.is_movie = dto.is_movie.or_else(|| flag(row.is_movie));
        dto.is_series = dto.is_series.or_else(|| flag(row.is_series));
        dto.is_sports = dto.is_sports.or_else(|| flag(row.is_sports));
        dto.is_live = dto.is_live.or_else(|| flag(row.is_live));
        dto.is_news = dto.is_news.or_else(|| flag(row.is_news));
        dto.is_kids = dto.is_kids.or_else(|| flag(row.is_kids));
        dto.is_premiere = dto.is_premiere.or_else(|| flag(row.is_premiere));
        if has_channel_info || has_channel_image {
            dto.channel_name = Some(row.channel_name.clone());
            dto.media_type = if row.channel_media_kind == "Radio" {
                MediaType::Audio
            } else {
                MediaType::Video
            };
            dto.channel_number.clone_from(&row.channel_number);
            // `if (hasChannelImage && liveChannel.HasImage(ImageType.Primary))`.
            if has_channel_image {
                dto.channel_primary_image_tag = channel_image_tag.cloned();
            }
        }
    }

    /// Port of `LiveTvManager.AddRecordingInfo`: links each programme DTO to
    /// the timer that records it (`ProgramId == program.ExternalId`), setting
    /// `TimerId`/`Status` (unless cancelled/errored) and `SeriesTimerId`.
    ///
    /// The upstream fallback that matches a *series timer* by its `SeriesId`
    /// has nothing to match here: Ferrofin's stored `SeriesTimerInfoDto`
    /// carries no series id (the DTO shape has none), so only the direct
    /// `timer.SeriesTimerId` link is applied.
    async fn add_recording_info(
        &self,
        dtos: &mut [BaseItemDto],
        rows: &[GuideProgramRow],
    ) -> Result<(), ServiceError> {
        if dtos.is_empty() {
            return Ok(());
        }
        // One timer read for the page (upstream lazily loads the list once).
        let timers: Vec<TimerInfoDto> = self.get_timers().await?;
        if timers.is_empty() {
            return Ok(());
        }
        for (dto, row) in dtos.iter_mut().zip(rows) {
            Self::apply_recording_info(dto, row, &timers);
        }
        Ok(())
    }

    /// [`Self::add_recording_info`]'s per-programme body, over an
    /// already-loaded timer list.
    fn apply_recording_info(dto: &mut BaseItemDto, row: &GuideProgramRow, timers: &[TimerInfoDto]) {
        let Some(external_id) = row.external_id.as_deref() else {
            return;
        };
        let timer = timers.iter().find(|t| {
            t.base
                .program_id
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(external_id))
        });
        let Some(timer) = timer else {
            return;
        };
        if !matches!(
            timer.status,
            RecordingStatus::Cancelled | RecordingStatus::Error
        ) {
            dto.timer_id.clone_from(&timer.base.id);
            dto.status = Some(recording_status_name(timer.status));
        }
        if let Some(series_timer_id) = timer.series_timer_id.as_deref().filter(|s| !s.is_empty()) {
            dto.series_timer_id = Some(series_timer_id.to_owned());
        }
    }

    // ---- DVR ------------------------------------------------------------

    /// The active-recording registry, locked. The guard never spans an
    /// `.await`.
    fn active_recordings_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, ActiveRecording>> {
        self.active_recordings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The armed-timer registry, locked. The guard never spans an `.await`.
    fn armed_timers_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Arc<std::sync::atomic::AtomicBool>>> {
        self.armed_timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The dashboard's Live TV options, or the defaults when nothing has been
    /// saved.
    ///
    /// A file that cannot be read or does not parse reads as the defaults too,
    /// as upstream's `BaseConfigurationManager.LoadConfiguration` falls back
    /// ("Error loading configuration file"), and is reported: a `warn` the
    /// first time it is met as it stands, `debug` while it stays so, since
    /// this runs per request (LOGGING.md). Upstream lets an `IOException`
    /// through instead; the readers here have no error to answer with, so a
    /// read error is reported and defaulted the same way. A missing file is
    /// a server never configured, and quiet.
    async fn live_tv_options(&self) -> LiveTvOptions {
        let path = &self.paths.options_file;
        if path.as_os_str().is_empty() {
            return LiveTvOptions::default();
        }
        let error = match tokio::fs::read_to_string(path).await {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(options) => {
                    *self
                        .options_warned
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    return options;
                }
                Err(error) => error.to_string(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Gone is not broken: a file put back broken is news again.
                *self
                    .options_warned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                return LiveTvOptions::default();
            }
            Err(error) => error.to_string(),
        };
        let modified = tokio::fs::metadata(path)
            .await
            .and_then(|meta| meta.modified())
            .ok();
        let seen = (path.clone(), modified);
        let first = self
            .options_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(seen.clone())
            != Some(seen);
        if first {
            tracing::warn!(
                path = %path.display(),
                %error,
                "live tv: the Live TV options file cannot be used; using the defaults until it is fixed"
            );
        } else {
            tracing::debug!(
                path = %path.display(),
                %error,
                "live tv: the Live TV options file cannot be used; using the defaults"
            );
        }
        LiveTvOptions::default()
    }

    /// Every stored timer, whatever its status — C# `TimerManager.GetAll()`,
    /// which is what the restart path re-arms from. The trait's `get_timers`
    /// is the FILTERED view on top of this.
    async fn all_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        self.json_list(
            "FerrofinLiveTvTimers",
            r#"SELECT "Id", "Data" FROM "FerrofinLiveTvTimers" ORDER BY "StartDate""#,
        )
        .await
    }

    /// Writes a timer through, DTO and promoted columns together, WITHOUT
    /// claiming the row is manual. A row that already exists keeps its stored
    /// `IsManual` — the flag is sticky-true, see `dvr_repository::upsert_timer`.
    async fn persist_timer(&self, timer: &TimerInfoDto) -> Result<String, ServiceError> {
        crate::dvr_repository::upsert_timer(&self.db, timer, false).await
    }

    /// Writes a timer through and raises `TimerInfo.IsManual`, on a fresh row
    /// and on one that already exists alike — the four upstream sites that set
    /// the flag (`DefaultLiveTvService.cs:178`, `:227`, `:255`, `:302`) are
    /// three updates and one insert.
    async fn persist_manual_timer(&self, timer: &TimerInfoDto) -> Result<String, ServiceError> {
        crate::dvr_repository::upsert_timer(&self.db, timer, true).await
    }

    /// Port of `DefaultLiveTvService.CancelTimerInternal` (v10.11.8
    /// DefaultLiveTvService.cs:168-197): the timer goes to `Cancelled`; one that
    /// nothing scheduled — or whose whole series timer is being cancelled — is
    /// deleted outright, and any capture it started stops.
    ///
    /// `is_manual_cancellation` is upstream's third parameter (`:176-180`), and
    /// it is load-bearing rather than decorative: a person cancelling ONE
    /// showing of a series (`CancelTimerAsync` passes `true`, `:199-203`) leaves
    /// a `Cancelled` row flagged `IsManual`, and the next fan-out over that
    /// series timer sees the flag and leaves the showing alone
    /// (`ShouldCancelTimerForSeriesTimer`'s first arm, `:646`, plus the
    /// `else if (!existingTimer.IsManual)` that would otherwise reset it to
    /// `New`, `:751`). The fan-out's own tidy-up pass passes `false` (`:797`),
    /// because a timer the guide no longer justifies was never a person's
    /// choice.
    async fn cancel_timer_internal(
        &self,
        id: &str,
        is_series_cancelled: bool,
        is_manual_cancellation: bool,
    ) -> Result<(), ServiceError> {
        match self.get_timer(id).await? {
            Some(mut timer) => {
                timer.status = RecordingStatus::Cancelled;
                if is_series_cancelled
                    || timer
                        .series_timer_id
                        .as_deref()
                        .map(str::trim)
                        .is_none_or(str::is_empty)
                {
                    self.delete_by_id(DELETE_TIMER_SQL, id).await?;
                } else if is_manual_cancellation {
                    self.persist_manual_timer(&timer).await?;
                } else {
                    self.persist_timer(&timer).await?;
                }
            }
            None => self.delete_by_id(DELETE_TIMER_SQL, id).await?,
        }
        self.disarm_timer(id);
        self.forget_row_state(id);
        self.cancel_recording(id);
        Ok(())
    }

    /// The tuner-facing external id of a channel, or `None` when the lineup does
    /// not hold it. This is `SeriesTimerInfo.ChannelId` upstream, published as
    /// the DTO's `ExternalChannelId` (`LiveTvDtoService.cs:137`).
    async fn channel_external_id(&self, channel_id: Uuid) -> Result<Option<String>, ServiceError> {
        crate::guide_repository::channel_external_id(&self.db, channel_id).await
    }

    /// The stored series timer with this id — the published DTO plus the two
    /// server-owned identity columns the DTO has no room for.
    async fn stored_series_timer(
        &self,
        id: &str,
    ) -> Result<Option<StoredSeriesTimer>, ServiceError> {
        Ok(crate::dvr_repository::series_timer_row(&self.db, id)
            .await?
            .and_then(|(data, external_id, series_id)| {
                self.parse_stored_row("FerrofinLiveTvSeriesTimers", id, &data)
                    .map(|dto| StoredSeriesTimer {
                        dto,
                        external_id,
                        series_id,
                    })
            }))
    }

    /// Writes a series timer through, DTO and identity columns together.
    async fn persist_series_timer(&self, stored: &StoredSeriesTimer) -> Result<(), ServiceError> {
        crate::dvr_repository::upsert_series_timer(
            &self.db,
            &stored.dto,
            &stored.external_id,
            stored.series_id.as_deref(),
        )
        .await
    }

    /// The ids of every timer this series timer scheduled.
    async fn timer_ids_for_series(&self, series_id: &str) -> Result<Vec<String>, ServiceError> {
        crate::dvr_repository::timer_ids_for_series(&self.db, series_id).await
    }

    /// "If any timers have already been manually created, make sure they don't
    /// get cancelled" — port of `DefaultLiveTvService.CreateSeriesTimer`'s
    /// adoption loop (v10.11.8 DefaultLiveTvService.cs:279-303): every existing
    /// timer for this programme (or this series) joins the new series timer and
    /// is flagged manual so the fan-out leaves it alone.
    async fn adopt_existing_timers(&self, stored: &StoredSeriesTimer) -> Result<(), ServiceError> {
        let series_id = stored.series_id.as_deref().unwrap_or_default();
        for mut timer in self.get_timers().await? {
            let by_program = matches_ignore_case(
                timer.base.program_id.as_deref(),
                stored.dto.base.program_id.as_deref(),
            ) || matches_ignore_case(
                timer.base.external_program_id.as_deref(),
                stored.dto.base.external_program_id.as_deref(),
            );
            // Upstream compares `TimerInfo.SeriesId`, which the wire DTO has no
            // field for, so the timer's own programme is asked for it instead —
            // the guide is where that id came from in the first place.
            let by_series = !by_program
                && !series_id.is_empty()
                && self
                    .program_row_for_timer(&timer.base)
                    .await?
                    .and_then(|row| row.external_series_id)
                    .is_some_and(|found| found.eq_ignore_ascii_case(series_id));
            if !(by_program || by_series) {
                continue;
            }
            if timer.base.id.is_none() {
                continue;
            }
            timer.series_timer_id = stored.dto.base.id.clone();
            timer.external_series_timer_id = Some(stored.external_id.clone());
            // `timer.IsManual = true` (:302) goes through the same write as the
            // rest of the row, because `upsert_timer` raises the flag.
            self.persist_manual_timer(&timer).await?;
        }
        Ok(())
    }

    /// Port of `DefaultLiveTvService.UpdateTimersForSeriesTimer` (v12.2
    /// DefaultLiveTvService.cs:709-804): expand the series timer over the guide,
    /// add a timer for every future airing it should record, refresh the ones
    /// that already exist, and — when asked — drop the pending ones it no longer
    /// matches.
    async fn update_timers_for_series_timer(
        &self,
        stored: &StoredSeriesTimer,
        update_timer_settings: bool,
        delete_invalid_timers: bool,
    ) -> Result<(), ServiceError> {
        let candidates = self.timers_for_series(stored).await?;
        let mut all_ids: Vec<String> = Vec::with_capacity(candidates.len());
        // `enabledTimersForSeries` — the timers this pass left recordable, which
        // is what the duplicate-showing sweep below runs over.
        let mut enabled: Vec<EnabledShowing> = Vec::new();
        for (program, mut timer) in candidates {
            let id = timer.base.id.clone().unwrap_or_default();
            all_ids.push(id.clone());
            let existing = match self.get_timer(&id).await? {
                Some(found) => Some(found),
                None => self.timer_for_program(&timer).await?,
            };
            match existing {
                None => {
                    if should_cancel_timer_for_series_timer(&stored.dto, &program, &timer) {
                        timer.status = RecordingStatus::Cancelled;
                    } else {
                        enabled.push((
                            program_show_id(&program),
                            timer.base.start_date,
                            timer.clone(),
                        ));
                    }
                    self.persist_timer(&timer).await?;
                    self.arm_timer(&timer);
                }
                Some(mut existing) => {
                    let existing_id = existing.base.id.clone().unwrap_or_default();
                    // "Only update if not currently active — test both new timer
                    // and existing in case Id's are different."
                    if self.active_recordings_lock().contains_key(&id)
                        || self.active_recordings_lock().contains_key(&existing_id)
                    {
                        continue;
                    }
                    update_existing_timer_with_new_metadata(&mut existing, &timer);
                    let is_manual =
                        crate::dvr_repository::timer_is_manual(&self.db, &existing_id).await?;
                    if !is_manual
                        && should_cancel_timer_for_series_timer(&stored.dto, &program, &timer)
                    {
                        existing.status = RecordingStatus::Cancelled;
                    } else if !is_manual {
                        existing.status = RecordingStatus::New;
                    }
                    if update_timer_settings {
                        existing.base.keep_until = stored.dto.base.keep_until;
                        existing.base.is_post_padding_required =
                            stored.dto.base.is_post_padding_required;
                        existing.base.is_pre_padding_required =
                            stored.dto.base.is_pre_padding_required;
                        existing.base.post_padding_seconds = stored.dto.base.post_padding_seconds;
                        existing.base.pre_padding_seconds = stored.dto.base.pre_padding_seconds;
                        existing.base.priority = stored.dto.base.priority;
                    }
                    existing.series_timer_id = stored.dto.base.id.clone();
                    existing.external_series_timer_id = Some(stored.external_id.clone());
                    if existing.status != RecordingStatus::Cancelled {
                        enabled.push((
                            program_show_id(&program),
                            existing.base.start_date,
                            existing.clone(),
                        ));
                    }
                    self.persist_timer(&existing).await?;
                    self.arm_timer(&existing);
                }
            }
        }

        // `SearchForDuplicateShowIds(enabledTimersForSeries)`: the same showing
        // scheduled more than once records only the first of them — and only
        // for a series timer that skips episodes already in the library
        // (v12.2 DefaultLiveTvService.cs:776-779, jellyfin/jellyfin PR 16220).
        if stored.dto.skip_episodes_in_library {
            for mut loser in duplicate_showings_to_cancel(&enabled) {
                loser.status = RecordingStatus::Cancelled;
                self.persist_timer(&loser).await?;
                self.arm_timer(&loser);
            }
        }

        if delete_invalid_timers {
            let series_id = stored.dto.base.id.clone().unwrap_or_default();
            let now = Utc::now();
            for id in self.timer_ids_for_series(&series_id).await? {
                if all_ids.iter().any(|kept| kept.eq_ignore_ascii_case(&id)) {
                    continue;
                }
                let Some(timer) = self.get_timer(&id).await? else {
                    continue;
                };
                if timer.status == RecordingStatus::New && timer.base.start_date > now {
                    // `CancelTimerInternal(timer.Id, false, false)` (:797): the
                    // guide moved, nobody asked for this — so it must NOT be
                    // flagged manual, or the row would become un-reschedulable.
                    self.cancel_timer_internal(&id, false, false).await?;
                }
            }
        }
        Ok(())
    }

    /// Port of `DefaultLiveTvService.GetTimersForSeries` (v10.11.8
    /// DefaultLiveTvService.cs:803-828) + `CreateTimer(LiveTvProgram, …)`
    /// (:833-880): every future airing of this series — matched on the listings
    /// series id, or on the programme title when the listings carry none, and
    /// pinned to the series timer's channel unless it records any channel.
    async fn timers_for_series(
        &self,
        stored: &StoredSeriesTimer,
    ) -> Result<Vec<(GuideProgramRow, TimerInfoDto)>, ServiceError> {
        let series_id = stored
            .series_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut query = InternalItemsQuery {
            min_end_date: Some(Utc::now()),
            external_series_id: series_id.map(str::to_owned),
            order_by: vec![(ItemSortBy::StartDate, SortOrder::Ascending)],
            ..InternalItemsQuery::default()
        };
        if series_id.is_none() {
            query.name.clone_from(&stored.dto.base.name);
        }
        if !stored.dto.record_any_channel {
            query.channel_ids = vec![stored.dto.base.channel_id];
        }
        let rows = self.query_program_rows(&query, Utc::now()).await?;
        // C# `tempChannelCache`: one channel lookup per distinct channel, not one
        // per airing — a series timer over a week's guide walks the same one or
        // two channels dozens of times.
        let mut channel_cache: HashMap<Uuid, Option<String>> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut channel_external = None;
            if let Ok(channel_id) = Uuid::parse_str(&row.channel_id) {
                if let Some(cached) = channel_cache.get(&channel_id) {
                    channel_external = cached.clone();
                } else {
                    channel_external = self.channel_external_id(channel_id).await?;
                    channel_cache.insert(channel_id, channel_external.clone());
                }
            }
            let timer = series_child_timer(&row, stored, channel_external);
            out.push((row, timer));
        }
        Ok(out)
    }

    /// The timer already scheduled for this timer's programme, if any.
    async fn timer_for_program(
        &self,
        timer: &TimerInfoDto,
    ) -> Result<Option<TimerInfoDto>, ServiceError> {
        let Some(program_id) = timer
            .base
            .program_id
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(None);
        };
        // The first match that parses: upstream's list never holds a row that
        // does not, and one that does not must not hide the next.
        Ok(crate::dvr_repository::timers_for_program(
            &self.db,
            program_id,
            timer.base.external_program_id.as_deref(),
        )
        .await?
        .into_iter()
        .find_map(|(id, data)| self.parse_stored_row("FerrofinLiveTvTimers", &id, &data)))
    }

    /// One guide programme by id, or `None` when it is not in the guide.
    async fn program_row(&self, id: Uuid) -> Result<Option<GuideProgramRow>, ServiceError> {
        Ok(self
            .query_program_rows(
                &InternalItemsQuery {
                    item_ids: vec![id],
                    ..InternalItemsQuery::default()
                },
                Utc::now(),
            )
            .await?
            .pop())
    }

    /// The guide programme a timer names, by its internal id or the listing
    /// provider's own — a timer created from `Timers/Defaults` carries both.
    ///
    /// Takes the shared base DTO so the single-timer and series-timer paths can
    /// both ask (`GetProgramInfoFromCache` serves both upstream).
    async fn program_row_for_timer(
        &self,
        timer: &ferrofin_model::live_tv::BaseTimerInfoDto,
    ) -> Result<Option<GuideProgramRow>, ServiceError> {
        if let Some(id) = timer
            .program_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            && let Some(row) = self.program_row(id).await?
        {
            return Ok(Some(row));
        }
        let Some(external) = timer
            .external_program_id
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        else {
            return Ok(None);
        };
        // The listing provider's id is not the row's key, so fall back to the
        // channel's guide and match on it (upstream's cache lookup does the
        // same by external id).
        Ok(self
            .query_program_rows(
                &InternalItemsQuery {
                    channel_ids: vec![timer.channel_id],
                    ..InternalItemsQuery::default()
                },
                Utc::now(),
            )
            .await?
            .into_iter()
            .find(|row| {
                row.external_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(external))
            }))
    }

    /// Arms (or re-arms) the system timer behind one recording timer.
    ///
    /// Port of `TimerManager.AddOrUpdateSystemTimer`: a finished or cancelled
    /// timer arms nothing, one whose start has already passed fires now, and
    /// anything else sleeps until `StartDate - PrePaddingSeconds`.
    fn arm_timer(&self, timer: &TimerInfoDto) {
        let Some(id) = timer.base.id.clone().filter(|id| !id.is_empty()) else {
            return;
        };
        self.disarm_timer(&id);
        if matches!(
            timer.status,
            RecordingStatus::Completed | RecordingStatus::Cancelled
        ) {
            return;
        }

        let start = timer.base.start_date
            - chrono::Duration::seconds(i64::from(timer.base.pre_padding_seconds));
        let delay = (start - Utc::now())
            .to_std()
            .unwrap_or(std::time::Duration::ZERO);
        tracing::info!(
            timer_id = id,
            name = timer.base.name.as_deref().unwrap_or_default(),
            in_seconds = delay.as_secs(),
            "live tv: recording timer armed"
        );
        self.schedule_fire(id, delay);
    }

    /// Registers timer `id` as armed and spawns the task that fires it after
    /// `delay`; [`disarm_timer`](Self::disarm_timer) stops it.
    fn schedule_fire(&self, id: String, delay: std::time::Duration) {
        // The flag is registered BEFORE the task exists, so a zero-delay timer
        // cannot fire before it is observable. A fire already pending for the
        // same id is stopped, so one timer never fires twice.
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(previous) = self
            .armed_timers_lock()
            .insert(id.clone(), Arc::clone(&cancelled))
        {
            previous.store(true, Ordering::SeqCst);
        }
        self.spawn_fire(id, delay, cancelled);
    }

    /// Spawns the task that fires timer `id` after `delay`, unless `cancelled`
    /// (the flag registered for it in `armed_timers`) is set first.
    fn spawn_fire(
        &self,
        id: String,
        delay: std::time::Duration,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) {
        // Disarming sets a flag the waiting task reads; it never aborts the
        // task. A `JoinHandle::abort` would be a live hazard: by the time
        // `cancel_timer` runs, the task may already BE the capture, and killing
        // it there would strand the active recording, leave the tuner open and
        // freeze the row at `InProgress`. Cancelling a capture is
        // `cancel_recording`'s job (C# `RecordingsManager.CancelRecording`);
        // this only stops one that has not started.
        let manager = self.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // The system timer is one-shot: once it has fired it is no longer
            // armed, and — crucially — cancelling the recording timer from
            // here on must stop the CAPTURE (`RecordingsManager.CancelRecording`)
            // rather than abort the task that is running it. Only this task's
            // own flag is taken out: a timer re-armed meanwhile keeps its arm.
            {
                let mut armed = manager.armed_timers_lock();
                if cancelled.load(Ordering::SeqCst)
                    || !armed
                        .get(&id)
                        .is_some_and(|flag| Arc::ptr_eq(flag, &cancelled))
                {
                    return;
                }
                armed.remove(&id);
            }
            manager.on_timer_fired(id).await;
        });
    }

    /// Cancels the pending fire of one timer (C# `TimerManager.StopTimer`).
    ///
    /// A timer that has already fired is no longer armed, so this cannot touch
    /// the capture it started.
    fn disarm_timer(&self, id: &str) {
        if let Some(cancelled) = self.armed_timers_lock().remove(id) {
            cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// Stops a capture in flight (C# `RecordingsManager.CancelRecording`).
    fn cancel_recording(&self, timer_id: &str) {
        if let Some(recording) = self.active_recordings_lock().get(timer_id) {
            tracing::info!(timer_id, "live tv: cancelling the recording in progress");
            recording.cancel();
        }
    }

    /// Runs one fired timer.
    ///
    /// Port of `DefaultLiveTvService.OnTimerManagerTimerFired`: a programme
    /// that has already ended is dropped, one already recording is left alone,
    /// and otherwise the guide is re-read and the capture starts.
    async fn on_timer_fired(&self, timer_id: String) {
        let data = match self.stored_data(SELECT_TIMER_SQL, &timer_id).await {
            Ok(data) => {
                self.read_retry_counts_lock().remove(&timer_id);
                data
            }
            // The database closes when a server lifetime ends, so this timer
            // belongs to a host an in-process restart tore down; the new host
            // re-armed every stored timer itself, and retrying this one would
            // record the programme twice.
            Err(error) if self.db.is_closed() => {
                tracing::debug!(
                    timer_id,
                    %error,
                    "live tv: a recording timer fired after its server stopped; ignoring it"
                );
                return;
            }
            Err(error) => {
                self.retry_unread_timer(timer_id, &error);
                return;
            }
        };
        // C# `TimerManager.TimerCallback` fires nothing for a timer that has
        // gone from the store since it was armed.
        let Some(data) = data else {
            tracing::debug!(
                timer_id,
                "live tv: the fired recording timer no longer exists"
            );
            return;
        };
        // Not `parse_stored_row`: its warning may already be spent on a list
        // read, and a recording missed for a broken row must say so when it
        // is missed (once per fire).
        let timer: TimerInfoDto = match serde_json::from_str(&data) {
            Ok(timer) => timer,
            Err(error) => {
                // No retry, so `read_retry_counts` needs nothing: the read
                // above succeeded and already cleared it.
                tracing::warn!(
                    timer_id,
                    %error,
                    "live tv: the fired recording timer's stored row does not parse; the recording is missed"
                );
                return;
            }
        };
        tracing::info!(
            timer_id,
            name = timer.base.name.as_deref().unwrap_or_default(),
            "live tv: recording timer fired"
        );
        let mut info = TimerRecordingInfo::from_timer(&timer);
        match self.program_row_for_timer(&timer.base).await {
            Ok(Some(program)) => apply_program_to_recording_info(&program, &mut info),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                timer_id,
                %error,
                "live tv: cannot re-read the guide for the fired recording timer; recording with the timer's own details"
            ),
        }

        if info.recording_end_date() <= Utc::now() {
            tracing::warn!(
                timer_id,
                "live tv: the recording timer fired but the programme has already ended"
            );
            if let Err(error) = self.delete_by_id(DELETE_TIMER_SQL, &timer_id).await {
                tracing::warn!(
                    timer_id,
                    %error,
                    "live tv: deleting the ended recording timer failed"
                );
            }
            return;
        }
        // `Skipping RecordStream because it's already in progress.` A timer
        // re-armed while its capture is starting fires again (each guide
        // refresh re-arms every live timer, `RefreshTimers`), and the capture
        // registers itself only after several awaits; the claim, held until
        // `record_stream` returns, makes the check and that registration one
        // step, so a showing is never captured twice.
        let Some(_firing) = FiringClaim::take(&self.firing, &timer_id) else {
            tracing::info!(timer_id, "live tv: that recording is already in progress");
            return;
        };
        if self.active_recordings_lock().contains_key(&timer_id) {
            tracing::info!(timer_id, "live tv: that recording is already in progress");
            return;
        }
        // `record_stream` settles the timer itself; an error here is the
        // settle failing, which is the only thing left to report.
        if let Err(error) = self.record_stream(timer, info).await {
            tracing::error!(timer_id, %error, "live tv: settling the recording timer failed");
        }
    }

    /// Fires a timer whose row could not be read once more,
    /// [`RETRY_INTERVAL_SECONDS`](crate::dvr::RETRY_INTERVAL_SECONDS) from
    /// now, up to [`MAX_RETRY_COUNT`](crate::dvr::MAX_RETRY_COUNT) failed
    /// reads in a row (the interval and count of
    /// `RecordingsManager.RecordStream`'s capture retry, counted apart from
    /// it). Once that is spent the recording is missed, but the row stays, so
    /// the next boot re-arms it. A timer armed again meanwhile (an edit, a
    /// guide refresh) keeps that arm and is not retried.
    fn retry_unread_timer(&self, timer_id: String, error: &ServiceError) {
        /// What became of the retry.
        enum Retry {
            /// Armed again for attempt `.0`, under flag `.1`.
            Scheduled(u32, Arc<AtomicBool>),
            /// Someone else armed the timer while its read was failing.
            Superseded,
            /// The budget is spent.
            Exhausted,
        }
        let retry = {
            // Checked and registered under one lock, so a concurrent re-arm
            // is never cancelled by this retry.
            let mut armed = self.armed_timers_lock();
            if armed.contains_key(&timer_id) {
                Retry::Superseded
            } else {
                let mut counts = self.read_retry_counts_lock();
                let count = counts.entry(timer_id.clone()).or_insert(0);
                if *count < crate::dvr::MAX_RETRY_COUNT {
                    *count += 1;
                    let cancelled = Arc::new(AtomicBool::new(false));
                    armed.insert(timer_id.clone(), Arc::clone(&cancelled));
                    Retry::Scheduled(*count, cancelled)
                } else {
                    counts.remove(&timer_id);
                    Retry::Exhausted
                }
            }
        };
        match retry {
            Retry::Scheduled(attempt, cancelled) => {
                tracing::warn!(
                    timer_id,
                    %error,
                    attempt,
                    retry_in_seconds = crate::dvr::RETRY_INTERVAL_SECONDS,
                    "live tv: cannot read the fired recording timer; retrying"
                );
                self.spawn_fire(
                    timer_id,
                    std::time::Duration::from_secs(
                        crate::dvr::RETRY_INTERVAL_SECONDS.unsigned_abs(),
                    ),
                    cancelled,
                );
            }
            Retry::Superseded => tracing::debug!(
                timer_id,
                %error,
                "live tv: cannot read the fired recording timer, but it has been armed again"
            ),
            Retry::Exhausted => tracing::error!(
                timer_id,
                %error,
                "live tv: cannot read the fired recording timer; the recording is missed"
            ),
        }
    }

    /// The fire path's read-retry counts, locked. The guard never spans an
    /// `.await`.
    fn read_retry_counts_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, u32>> {
        self.read_retry_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forgets the in-memory state kept for a stored timer or series timer
    /// row: its capture and read retry counts and whether it was reported as
    /// unparseable. A cancel or a delete ends all of them.
    fn forget_row_state(&self, id: &str) {
        self.retry_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        self.read_retry_counts_lock().remove(id);
        for rows in self
            .unparseable_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            rows.remove(id);
        }
    }

    /// Captures one programme.
    ///
    /// Port of `RecordingsManager.RecordStream`: open the channel, choose the
    /// recorder, register the capture and write the row, copy until the
    /// programme ends or the timer is cancelled, then close the live stream and
    /// settle the timer (retry, complete, or drop).
    /// Captures one programme, and settles the timer however it ends.
    ///
    /// Port of `RecordingsManager.RecordStream`, whose whole body is inside one
    /// try/catch: every failure — the channel gone, the tuner busy, ffmpeg
    /// missing, a database hiccup — becomes a *failed recording* that retries,
    /// never an error that silently leaves a timer armed at nothing.
    async fn record_stream(
        &self,
        mut timer: TimerInfoDto,
        info: TimerRecordingInfo,
    ) -> Result<(), ServiceError> {
        let options = self.live_tv_options().await;
        let (target, series_path) =
            crate::dvr::recording_path(&info, &options, &self.paths.data_dir);
        let target = {
            let active: Vec<ActiveRecording> =
                self.active_recordings_lock().values().cloned().collect();
            crate::dvr::ensure_file_unique(&target, &info.id, &active)
        };
        // `ReportFileSystemChangeBeginning(recordingPath)`, completed once the
        // empty-file cleanup and the refresh are done (`RecordingsManager.cs:342,400`).
        let _write = SelfWrite::begin(self.self_writes.as_ref(), target.to_string_lossy());

        let capture = self
            .capture(
                &mut timer,
                &info,
                &target,
                (series_path.as_deref(), &options),
            )
            .await;
        let (recording_id, outcome) = match capture {
            Ok(captured) => captured,
            // Nothing was ever opened or registered, so there is nothing to
            // unwind — but the timer still has to settle, or it stays armed at
            // a recording that never happens.
            Err(error) => (None, Err(error)),
        };

        // A zero-byte file is a failed capture, not a recording.
        if crate::dvr::is_empty_file(&target).await {
            let _ = tokio::fs::remove_file(&target).await;
        }
        let recorded = tokio::fs::try_exists(&target).await.unwrap_or(false);
        self.settle_timer(timer, &info, recording_id, &target, recorded, outcome)
            .await
    }

    /// Opens the channel, registers the capture and runs the recorder,
    /// unwinding both the live stream and the registry whatever happens.
    ///
    /// Reports the recording row it created (when it got that far) and how the
    /// capture itself ended.
    async fn capture(
        &self,
        timer: &mut TimerInfoDto,
        info: &TimerRecordingInfo,
        target: &std::path::Path,
        sidecars: (Option<&std::path::Path>, &LiveTvOptions),
    ) -> Result<(Option<Uuid>, Result<(), ServiceError>), ServiceError> {
        // The tuner source, opened if it needs opening.
        let sources = self.get_channel_media_sources(info.channel_id).await?;
        let Some(source) = sources.into_iter().next() else {
            return Err(ServiceError::not_found(format!(
                "live tv channel {}",
                info.channel_id
            )));
        };
        let mut live_stream_id = None;
        let opened = if source.requires_opening {
            let opened = self
                .open_channel_stream(info.channel_id, source.id.as_deref())
                .await?;
            live_stream_id.clone_from(&opened.live_stream_id);
            opened
        } else {
            source
        };

        // From here on the tuner is open: every exit goes through
        // `finish_capture`, which closes it and de-registers.
        let started = self
            .start_capture(timer, info, target, &opened, sidecars)
            .await;
        let (recording_id, outcome) = match started {
            Ok((recording_id, cancel)) => {
                let outcome = self
                    .run_recorder(info, target, &opened, &cancel, live_stream_id.as_deref())
                    .await;
                (Some(recording_id), outcome)
            }
            Err(error) => (None, Err(error)),
        };
        self.finish_capture(&info.id, live_stream_id.as_deref())
            .await;
        Ok((recording_id, outcome))
    }

    /// C# `OnStarted`: the recording row, the active-recording registry and the
    /// timer's status all move together, before the first byte is written.
    async fn start_capture(
        &self,
        timer: &mut TimerInfoDto,
        info: &TimerRecordingInfo,
        target: &std::path::Path,
        opened: &MediaSourceInfo,
        (series, options): (Option<&std::path::Path>, &LiveTvOptions),
    ) -> Result<(Uuid, Arc<std::sync::atomic::AtomicBool>), ServiceError> {
        let _ = opened;
        let recording_id = Uuid::new_v4();
        let recording = ActiveRecording::new(info.id.clone(), recording_id, target.to_path_buf());
        let cancel = recording.cancellation();
        crate::dvr_repository::insert_recording(
            &self.db,
            recording_id,
            info,
            target,
            recording.started_at,
        )
        .await?;
        self.active_recordings_lock()
            .insert(info.id.clone(), recording);
        timer.status = RecordingStatus::InProgress;
        self.persist_timer(timer).await?;
        // `SaveRecordingMetadata`, which upstream's `async void OnStarted`
        // runs without the recorder waiting on it. A failure is logged, as
        // its catch-all logs it: a sidecar never fails a recording.
        if options.save_recording_nfo {
            let info = info.clone();
            let target = target.to_path_buf();
            let series = series.map(std::path::Path::to_path_buf);
            let release_date = Arc::clone(&self.release_date_format);
            let span = tracing::info_span!("recording_nfo", timer_id = %info.id);
            tokio::spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(error) = crate::recording_metadata::save_recording_nfo(
                        &info,
                        &target,
                        series.as_deref(),
                        &release_date,
                        Utc::now(),
                    )
                    .await
                    {
                        tracing::warn!(
                            path = %target.display(),
                            %error,
                            "live tv: could not save the recording's NFO"
                        );
                    }
                },
                span,
            ));
        }
        // TODO(parity, open work item — NOT an accepted divergence; gl-749):
        // with `save_recording_images`, upstream also copies the programme's
        // artwork beside the recording (`SaveRecordingImages`), but the guide
        // carries no programme images yet (gl-367). Un-defer path: once it
        // does, port `SaveRecordingImages` into `recording_metadata` and call
        // it here, gated on that option.
        Ok((recording_id, cancel))
    }

    /// Runs the recorder upstream's `GetRecorder` would have chosen.
    async fn run_recorder(
        &self,
        info: &TimerRecordingInfo,
        target: &std::path::Path,
        opened: &MediaSourceInfo,
        cancel: &std::sync::atomic::AtomicBool,
        live_stream_id: Option<&str>,
    ) -> Result<(), ServiceError> {
        let _ = live_stream_id;
        // C# reads the buffered copy through the direct-stream provider rather
        // than back out over HTTP; the buffer file IS that provider here, and
        // a reader that joins a stream someone is already watching starts near
        // the live edge rather than replaying the backlog into the recording
        // (`LiveStream.GetStream`'s tail seek).
        let unique_id = opened
            .path
            .as_deref()
            .and_then(|p| p.split("/LiveTv/LiveStreamFiles/").nth(1))
            .and_then(|rest| rest.split('/').next())
            .map(ToOwned::to_owned);
        let buffer = match unique_id {
            Some(unique_id) => self.get_live_stream_file(&unique_id).await?,
            None => None,
        };
        let input = match buffer {
            Some(file) => RecordingInput::Buffer {
                path: file.path,
                opened_at: file.opened_at,
            },
            None => RecordingInput::Url {
                url: opened.path.clone().unwrap_or_default(),
                headers: opened.required_http_headers.clone(),
            },
        };

        let duration = (info.recording_end_date() - Utc::now())
            .to_std()
            .unwrap_or(std::time::Duration::ZERO);
        tracing::info!(
            timer_id = info.id,
            path = %target.display(),
            minutes = duration.as_secs() / 60,
            "live tv: recording started"
        );

        match RecorderKind::choose(opened) {
            RecorderKind::Direct => {
                crate::dvr::record_direct(
                    self.tuner_source.as_ref(),
                    &input,
                    target,
                    duration,
                    cancel,
                )
                .await
            }
            RecorderKind::Encoded => match self.encoder.as_ref() {
                Some(encoder) => {
                    crate::dvr::record_encoded(
                        &encoder.encoder_path(),
                        opened,
                        &input,
                        target,
                        duration,
                        cancel,
                    )
                    .await
                }
                None => Err(ServiceError::backend(
                    "this source needs remuxing to record, and no media encoder is configured"
                        .to_owned(),
                )),
            },
        }
    }

    /// Releases everything a capture held, however it ended.
    async fn finish_capture(&self, timer_id: &str, live_stream_id: Option<&str>) {
        if let Some(live_stream_id) = live_stream_id {
            // Logged and swallowed: the recording is what matters, and the
            // stream is torn down either way.
            if let Err(error) = self.close_channel_stream(live_stream_id).await {
                tracing::error!(%error, "live tv: closing the recording's live stream failed");
            }
        }
        self.active_recordings_lock().remove(timer_id);
    }

    /// Decides what a finished capture leaves behind: a retry, a completed
    /// recording, or nothing.
    ///
    /// Port of the tail of `RecordingsManager.RecordStream`.
    async fn settle_timer(
        &self,
        mut timer: TimerInfoDto,
        info: &TimerRecordingInfo,
        recording_id: Option<Uuid>,
        target: &std::path::Path,
        recorded: bool,
        outcome: Result<(), ServiceError>,
    ) -> Result<(), ServiceError> {
        let failed = outcome.is_err();
        if let Err(error) = outcome {
            tracing::error!(timer_id = info.id, %error, "live tv: the capture ended in error");
        }

        let retries = {
            let mut counts = self
                .retry_counts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *counts.entry(info.id.clone()).or_insert(0)
        };
        if failed && Utc::now() < info.end_date && retries < crate::dvr::MAX_RETRY_COUNT {
            // Try again shortly, without the pre-padding that has already
            // elapsed (C# `RetryIntervalSeconds`). The failed attempt's row
            // goes with it: upstream has no row at all until the recording is
            // in the library, and a fileless "recording" per retry would fill
            // the client's list with ten ghosts.
            if let Some(recording_id) = recording_id {
                crate::dvr_repository::delete_recording(&self.db, recording_id).await?;
            }
            self.retry_counts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(info.id.clone(), retries + 1);
            timer.status = RecordingStatus::New;
            timer.base.pre_padding_seconds = 0;
            timer.base.start_date =
                Utc::now() + chrono::Duration::seconds(crate::dvr::RETRY_INTERVAL_SECONDS);
            self.persist_timer(&timer).await?;
            self.arm_timer(&timer);
            return Ok(());
        }

        self.retry_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&info.id);
        match (recorded, recording_id) {
            (true, Some(recording_id)) => {
                crate::dvr_repository::finish_recording(
                    &self.db,
                    recording_id,
                    RecordingStatus::Completed,
                    Some(target.display().to_string().as_str()),
                )
                .await?;
                timer.status = RecordingStatus::Completed;
                self.persist_timer(&timer).await?;
                tracing::info!(timer_id = info.id, path = %target.display(), "live tv: recording completed");
            }
            // Nothing was captured: the row and the timer both go, as upstream
            // deletes a timer whose file never appeared.
            (_, recording_id) => {
                if let Some(recording_id) = recording_id {
                    crate::dvr_repository::delete_recording(&self.db, recording_id).await?;
                }
                self.delete_by_id(DELETE_TIMER_SQL, &info.id).await?;
            }
        }
        Ok(())
    }

    /// Projects recording rows through the DTO service and applies the
    /// recording post-pass.
    async fn recording_dtos(
        &self,
        rows: &[crate::projection::RecordingRow],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        let entities: Vec<_> = rows
            .iter()
            .map(|row| crate::projection::recording_entity(row, parse_dt))
            .collect();
        let mut dtos = self
            .dto_service()?
            .get_base_item_dtos(&entities, options, user, None, true)
            .await?;
        Self::add_info_to_recording_dtos(&mut dtos, rows);
        Ok(dtos)
    }

    /// Port of `LiveTvManager.AddInfoToRecordingDto`: the timer link, the
    /// status, the programme flags, the channel name and how far through the
    /// capture is.
    ///
    /// Upstream reaches this only for an item with an ACTIVE recording
    /// (`DtoService` gates it on `GetActiveRecordingInfo(item.Path)`), and that
    /// same branch re-types the item as a `Recording` with no runtime and no
    /// download. A finished recording is an ordinary library `Video` and gets
    /// none of it — so neither does one here.
    fn add_info_to_recording_dtos(
        dtos: &mut [BaseItemDto],
        rows: &[crate::projection::RecordingRow],
    ) {
        for (dto, row) in dtos.iter_mut().zip(rows) {
            if row.status != "InProgress" {
                continue;
            }
            // The in-progress shape: jellyfin-web keys its recording card and
            // its progress bar off `Type === "Recording"`.
            dto.type_ = BaseItemKind::Recording;
            dto.can_download = Some(false);
            dto.run_time_ticks = None;
            dto.series_timer_id = row.series_timer_id.clone().filter(|id| !id.is_empty());
            dto.timer_id = row.timer_id.clone().filter(|id| !id.is_empty());
            dto.start_date = parse_dt(&row.start_date);
            dto.end_date = row.end_date.as_deref().and_then(parse_dt);
            dto.status = Some(row.status.clone());
            dto.is_repeat = Some(row.is_repeat);
            dto.episode_title.clone_from(&row.episode_title);
            dto.is_movie = Some(row.is_movie);
            dto.is_series = Some(row.is_series);
            dto.is_sports = Some(row.is_sports);
            dto.is_live = Some(row.is_live);
            dto.is_news = Some(row.is_news);
            dto.is_kids = Some(row.is_kids);
            dto.is_premiere = Some(row.is_premiere);
            dto.channel_name.clone_from(&row.channel_name);
            dto.completion_percentage = completion_percentage(row, parse_dt);
        }
    }

    /// The open-live-stream map, locked. The guard never spans an `.await`.
    fn live_streams_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveStreamHandle>> {
        self.live_streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The tuner host with this id.
    ///
    /// A channel outlives its deleted host until the guide refresh prunes it
    /// (migration 0046), and opening it meanwhile is upstream's
    /// `DefaultLiveTvService.GetChannelStream` finding no host:
    /// `ResourceNotFoundException("Tuner not found.")`, a 404.
    async fn tuner_host(&self, id: &str) -> Result<TunerHostInfo, ServiceError> {
        self.get_tuner_hosts()
            .await?
            .into_iter()
            .find(|t| t.id.as_deref() == Some(id))
            .ok_or_else(|| ServiceError::not_found("Tuner not found."))
    }

    /// The registered host that owns this tuner row.
    ///
    /// `TunerHostManager` only ever holds rows whose `Type` a registered host
    /// claims — [`LiveTvManager::save_tuner_host`] rejects the rest — so a miss
    /// here means the row predates the check or names a host that has been
    /// removed. Reporting it as not-found beats silently streaming it as M3U.
    fn host_for(
        &self,
        tuner: &TunerHostInfo,
    ) -> Result<&Arc<dyn crate::tuner_host::TunerHost>, ServiceError> {
        // No "m3u" default here either: `save_tuner_host` rejects a row whose
        // `Type` names no registered host, so a stored row always has one, and
        // guessing on the playback path would send an HDHomeRun row down the
        // M3U arm. `refresh_guide` reads the same field the same way.
        let type_id = tuner.type_.as_deref().unwrap_or_default();
        crate::tuner_host::find_host(&self.hosts, type_id)
            .ok_or_else(|| ServiceError::not_found(format!("tuner host type {type_id}")))
    }

    /// Starts the live stream the host chose, buffering it when it can be
    /// shared, and rewrites `source` to point at whatever a client should read.
    ///
    /// The three arms are upstream's three: `HdHomerunUdpStream` (which always
    /// buffers — there is no URL a client could read instead, only datagrams
    /// this server is receiving), `SharedHttpStream` (one tuner connection
    /// feeding every consumer through the temp file), and the bare `LiveStream`
    /// pass-through for a tuner whose stream cannot be shared.
    async fn start_live_stream(
        &self,
        kind: &crate::tuner_host::ChannelStreamKind,
        tuner: &TunerHostInfo,
        path: &str,
        source: &mut MediaSourceInfo,
    ) -> Result<
        (
            String,
            chrono::DateTime<Utc>,
            Arc<std::sync::atomic::AtomicBool>,
            LiveStreamKind,
        ),
        ServiceError,
    > {
        // A buffered stream is served from the local API URL, resolved before
        // the buffer opens so a failure leaves no buffer running unregistered.
        let opened = match kind {
            crate::tuner_host::ChannelStreamKind::LegacyUdp(plan) => {
                let base_url = self.local_api_url().await?;
                let transcode_dir = self.transcode_dir().await?;
                Some((
                    crate::hdhomerun::udp_stream::open_legacy_udp_stream(
                        plan,
                        &transcode_dir,
                        rand_lockkey(),
                    )
                    .await?,
                    base_url,
                ))
            }
            crate::tuner_host::ChannelStreamKind::Http => {
                let share = tuner.allow_stream_sharing
                    && source.protocol == MediaProtocol::Http
                    && !source.requires_looping
                    && self.can_share_stream(path, source).await;
                if share {
                    let base_url = self.local_api_url().await?;
                    let transcode_dir = self.transcode_dir().await?;
                    Some((
                        crate::stream::open_shared_http_stream(
                            self.tuner_source.as_ref(),
                            path,
                            &source.required_http_headers,
                            &transcode_dir,
                        )
                        .await?,
                        base_url,
                    ))
                } else {
                    None
                }
            }
        };

        let Some((opened, base_url)) = opened else {
            // The pass-through stream: nothing is buffered and the media source
            // keeps the tuner URL (C# bare `LiveStream`).
            return Ok((
                Uuid::new_v4().simple().to_string(),
                Utc::now(),
                Arc::new(std::sync::atomic::AtomicBool::new(true)),
                LiveStreamKind::Direct,
            ));
        };

        // Every consumer now reads the one buffered copy, not the tuner
        // (C# `SharedHttpStream.Open`).
        source.path = Some(format!(
            "{base_url}/LiveTv/LiveStreamFiles/{}/stream.{LIVE_STREAM_BUFFER_CONTAINER}",
            opened.unique_id
        ));
        source.protocol = MediaProtocol::Http;
        source.container = Some(LIVE_STREAM_BUFFER_CONTAINER.to_owned());
        Ok((
            opened.unique_id,
            opened.opened_at,
            opened.alive,
            LiveStreamKind::Shared {
                temp_path: opened.temp_path,
                task: opened.task,
            },
        ))
    }

    /// The per-source stamp `LiveTvMediaSourceProvider.GetMediaSourcesInternal`
    /// applies to every Live TV source.
    fn stamp_source(source: &mut MediaSourceInfo) {
        source.type_ = MediaSourceType::Default;
        if source.buffer_ms.is_none() {
            source.buffer_ms = Some(DEFAULT_LIVE_STREAM_BUFFER_MS);
        }
    }

    /// Joins an open, shareable stream on `stream_id`, returning its media
    /// source once the consumer count has been raised.
    fn join_open_stream(&self, stream_id: &str) -> Option<MediaSourceInfo> {
        let mut open = self.live_streams_lock();
        let entry = open.values_mut().find(|entry| {
            entry.is_sharing()
                && entry
                    .original_stream_id
                    .as_deref()
                    .is_some_and(|original| original.eq_ignore_ascii_case(stream_id))
        })?;
        entry.consumer_count += 1;
        // The stored source is the one handed out at open time, i.e. before the
        // media-source manager probed it (C# hands back the same object the
        // probe mutated). The joiner's probe is a cache hit, so the two end up
        // reporting the same streams.
        tracing::info!(
            stream_id,
            consumers = entry.consumer_count,
            "live tv: joined an open live stream"
        );
        Some(entry.media_source.clone())
    }

    /// Rejects an open that would exceed the tuner host's simultaneous-stream
    /// limit (C# `M3UTunerHost.GetChannelStream`'s `LiveTvConflictException`).
    fn enforce_tuner_count(
        &self,
        tuner_host_id: &str,
        tuner_count: i32,
    ) -> Result<(), ServiceError> {
        if tuner_count <= 0 {
            return Ok(());
        }
        let open = self.live_streams_lock();
        // A stream whose tuner has already hung up occupies nothing: its
        // buffer is gone and no consumer can join it. Upstream counts those
        // too, and so runs out of tuners it is not using.
        // `string.Equals(i.TunerHostId, tunerHostId, OrdinalIgnoreCase)`, so a
        // stream opened before its host was re-posted in another case counts.
        let in_use = open
            .values()
            .filter(|entry| {
                entry.is_sharing()
                    && entry
                        .tuner_host_id
                        .as_deref()
                        .is_some_and(|id| eq_ordinal_ignore_case(id, tuner_host_id))
            })
            .count();
        if i64::try_from(in_use).unwrap_or(i64::MAX) >= i64::from(tuner_count) {
            return Err(ServiceError::Conflict(
                "M3U simultaneous stream limit has been reached.".to_owned(),
            ));
        }
        Ok(())
    }

    /// Whether the tuner's HTTP stream may be shared between consumers: the
    /// URL's extension decides, and a URL without one is settled by a `HEAD`
    /// probe of the `Content-Type` (C# `M3UTunerHost.GetChannelStream`).
    async fn can_share_stream(&self, path: &str, source: &MediaSourceInfo) -> bool {
        match crate::stream::extension_can_share(path) {
            Some(can_share) => can_share,
            None => self
                .tuner_source
                .content_type(path, &source.required_http_headers)
                .await
                .is_some_and(|content_type| crate::stream::mime_type_can_share(&content_type)),
        }
    }

    /// The directory a shared live stream's buffer is written to, prepared on
    /// the blocking pool: the preparation walks the whole transcode tree.
    async fn transcode_dir(&self) -> Result<PathBuf, ServiceError> {
        let path = self.paths.transcode_dir.resolve();
        if path.as_os_str().is_empty() {
            return Err(ServiceError::Backend(
                "live tv transcode path not wired".to_owned(),
            ));
        }
        ferrofin_traits::system::prepare_transcode_directory_off_runtime(path).await
    }

    /// The API base URL a live stream's buffered file is served from.
    async fn local_api_url(&self) -> Result<String, ServiceError> {
        self.local_api
            .get()
            .ok_or_else(|| ServiceError::Backend("live tv local api url not wired".to_owned()))?
            .get_api_url_for_local_access()
            .await
    }

    /// Reads the `Id` and JSON `Data` columns across all rows of `sql` (over
    /// `table`), deserializing each and skipping a row that does not parse.
    async fn json_list<T: DeserializeOwned>(
        &self,
        table: &'static str,
        sql: &'static str,
    ) -> Result<Vec<T>, ServiceError> {
        let rows = sqlx::query(sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                self.parse_stored_row(
                    table,
                    r.get::<String, _>("Id").as_str(),
                    r.get::<String, _>("Data").as_str(),
                )
            })
            .collect())
    }

    /// Reads and deserializes a single JSON `Data` column of `table` by id; a
    /// row that does not parse reads as absent.
    async fn json_get<T: DeserializeOwned>(
        &self,
        table: &'static str,
        sql: &'static str,
        id: &str,
    ) -> Result<Option<T>, ServiceError> {
        Ok(self
            .stored_data(sql, id)
            .await?
            .and_then(|d| self.parse_stored_row(table, id, &d)))
    }

    /// Reads a single JSON `Data` column by id, unparsed.
    async fn stored_data(
        &self,
        sql: &'static str,
        id: &str,
    ) -> Result<Option<String>, ServiceError> {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Deserializes one stored `Data` blob, reporting a row that does not
    /// parse rather than letting it vanish (C# `ItemDataProvider` logs "Error
    /// deserializing" for a store that does not parse).
    ///
    /// The readers run per request, so a broken row is a `warn` the first time
    /// it is seen and a `debug` while it stays broken; once it parses again,
    /// breaking it again is reported again.
    fn parse_stored_row<T: DeserializeOwned>(
        &self,
        table: &'static str,
        row_id: &str,
        data: &str,
    ) -> Option<T> {
        let parsed = serde_json::from_str(data);
        let mut reported = self
            .unparseable_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match parsed {
            Ok(value) => {
                if let Some(rows) = reported.get_mut(table) {
                    rows.remove(row_id);
                }
                Some(value)
            }
            Err(error) => {
                let first = reported.entry(table).or_default().insert(row_id.to_owned());
                drop(reported);
                if first {
                    tracing::warn!(
                        table,
                        row_id,
                        %error,
                        "live tv: a stored row does not parse; skipping it until it is fixed"
                    );
                } else {
                    tracing::debug!(
                        table,
                        row_id,
                        %error,
                        "live tv: a stored row does not parse; skipping it"
                    );
                }
                None
            }
        }
    }

    /// Runs a `DELETE … WHERE "Id" = ?1` statement over a timer or series
    /// timer row, and forgets the in-memory state kept for that row.
    async fn delete_by_id(&self, sql: &'static str, id: &str) -> Result<(), ServiceError> {
        sqlx::query(sql)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        self.forget_row_state(id);
        Ok(())
    }
}

/// Emits `WHERE` for the first predicate of a query and `AND` for each one
/// after it.
fn push_separator(qb: &mut QueryBuilder<Sqlite>, first: &mut bool) {
    qb.push(if *first { " WHERE " } else { " AND " });
    *first = false;
}

/// Pushes the `WHERE` clause of a program query onto `qb`.
///
/// Port of the program-relevant arms of C#
/// `BaseItemRepository.TranslateQuery`: the channel scope, the start/end date
/// window (with `HasAired` *overriding* the end-date bounds exactly as upstream
/// does), the airing flag, the exact-name scope `librarySeriesId` sets, and the
/// genre scope.
///
/// The classification flags (`IsMovie`/`IsSeries`/`IsNews`/`IsKids`/`IsSports`)
/// match the columns the guide refresh derives per listings provider.
/// `GenreIds` is deliberately not faked — it needs genre identity rows the
/// guide cache does not hold — and `SeriesTimerId` never reaches this builder:
/// `get_programs` answers it with the empty result upstream returns when the
/// series-timer scope cannot be built.
fn push_program_filters(
    qb: &mut QueryBuilder<Sqlite>,
    query: &InternalItemsQuery,
    now: DateTime<Utc>,
) {
    let mut first = true;
    let now_db = datetime_to_db(now);

    if !query.item_ids.is_empty() {
        push_separator(qb, &mut first);
        qb.push(r#"p."Id" IN ("#);
        let mut list = qb.separated(",");
        for id in &query.item_ids {
            list.push_bind(guid_to_db(*id));
        }
        qb.push(")");
    }

    if !query.channel_ids.is_empty() {
        push_separator(qb, &mut first);
        qb.push(r#"p."ChannelId" IN ("#);
        let mut list = qb.separated(",");
        for id in &query.channel_ids {
            list.push_bind(guid_to_db(*id));
        }
        qb.push(")");
    }

    // C# `HasAired` *replaces* the end-date bound rather than intersecting it.
    let (mut min_end, mut max_end) = (query.min_end_date, query.max_end_date);
    match query.has_aired {
        Some(true) => max_end = Some(now),
        Some(false) => min_end = Some(now),
        None => {}
    }
    for (column, op, bound) in [
        (r#"p."StartDate""#, " >= ", query.min_start_date),
        (r#"p."StartDate""#, " <= ", query.max_start_date),
        (r#"p."EndDate""#, " >= ", min_end),
        (r#"p."EndDate""#, " <= ", max_end),
    ] {
        if let Some(bound) = bound {
            push_separator(qb, &mut first);
            qb.push(column).push(op).push_bind(datetime_to_db(bound));
        }
    }

    if let Some(is_airing) = query.is_airing {
        push_separator(qb, &mut first);
        if is_airing {
            qb.push(r#"(p."StartDate" <= "#)
                .push_bind(now_db.clone())
                .push(r#" AND p."EndDate" >= "#)
                .push_bind(now_db)
                .push(")");
        } else {
            // Not on now: `StartDate > now || EndDate < now`, as v12.2's
            // `TranslateQuery` has it since jellyfin/jellyfin#17588.
            qb.push(r#"(p."StartDate" > "#)
                .push_bind(now_db.clone())
                .push(r#" OR p."EndDate" < "#)
                .push_bind(now_db)
                .push(")");
        }
    }

    if let Some(series_id) = query
        .external_series_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        // `InternalItemsQuery.ExternalSeriesId` — the listings series id a
        // series timer's fan-out queries the guide with
        // (`DefaultLiveTvService.GetTimersForSeries`, v10.11.8
        // DefaultLiveTvService.cs:808).
        push_separator(qb, &mut first);
        qb.push(r#"p."ExternalSeriesId" = "#)
            .push_bind(series_id.to_owned());
    }

    if let Some(name) = query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        // Upstream compares the item's CleanName with `GetCleanValue(name)`
        // (`TranslateQuery.cs:476-477`); the guide cache has no cleaned
        // column, so the title is cleaned here.
        push_separator(qb, &mut first);
        qb.push(r#"ferrofin_clean_value(p."Title") = "#)
            .push_bind(ferrofin_util::string_extensions::get_clean_value(name));
    }

    for (column, wanted) in [
        (r#"p."IsMovie""#, query.is_movie),
        (r#"p."IsSeries""#, query.is_series),
        (r#"p."IsNews""#, query.is_news),
        (r#"p."IsKids""#, query.is_kids),
        (r#"p."IsSports""#, query.is_sports),
    ] {
        if let Some(wanted) = wanted {
            push_separator(qb, &mut first);
            qb.push(column).push(" = ").push_bind(i32::from(wanted));
        }
    }

    if !query.genres.is_empty() {
        // `Genres` holds the XMLTV `<category>` list as a JSON array; upstream
        // matches any one of the requested genres by clean value
        // (`TranslateQuery.cs:658-662`).
        push_separator(qb, &mut first);
        qb.push(
            r#"EXISTS (SELECT 1 FROM json_each(p."Genres") WHERE ferrofin_clean_value("value") IN ("#,
        );
        let mut list = qb.separated(",");
        for genre in &query.genres {
            list.push_bind(ferrofin_util::string_extensions::get_clean_value(genre));
        }
        qb.push("))");
    }
}

/// The guide column a sort key reads, or `None` when the guide cache holds
/// nothing to sort by (those keys are skipped rather than faked).
fn program_sort_column(sort: ItemSortBy) -> Option<&'static str> {
    match sort {
        ItemSortBy::Default | ItemSortBy::StartDate => Some(r#"p."StartDate""#),
        ItemSortBy::Name | ItemSortBy::SortName => Some(r#"p."Title""#),
        ItemSortBy::OfficialRating => Some(r#"p."OfficialRating""#),
        ItemSortBy::ProductionYear => Some(r#"p."ProductionYear""#),
        ItemSortBy::Random => Some("RANDOM()"),
        _ => None,
    }
}

/// Pushes the `ORDER BY` clause, falling back to start-date ascending — the
/// order C# `LiveTvManager.GetPrograms` relies on ("order by start date to take
/// advantage of a specialized index"). The id tiebreaker makes paging stable
/// across requests.
fn push_program_order(qb: &mut QueryBuilder<Sqlite>, order_by: &[(ItemSortBy, SortOrder)]) {
    let mut wrote = false;
    for (column, order) in order_by {
        let Some(sql) = program_sort_column(*column) else {
            continue;
        };
        qb.push(if wrote { ", " } else { " ORDER BY " })
            .push(sql)
            .push(if *order == SortOrder::Descending {
                " DESC"
            } else {
                " ASC"
            });
        wrote = true;
    }
    if !wrote {
        qb.push(r#" ORDER BY p."StartDate" ASC"#);
    }
    qb.push(r#", p."Id""#);
}

/// Pushes `LIMIT`/`OFFSET` for the requested page.
fn push_program_paging(qb: &mut QueryBuilder<Sqlite>, limit: Option<i32>, start_index: i32) {
    if let Some(limit) = limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit.max(0)));
    } else if start_index > 0 {
        // SQLite has no bare OFFSET: -1 is its "no limit" sentinel.
        qb.push(" LIMIT -1");
    }
    if start_index > 0 {
        qb.push(" OFFSET ").push_bind(i64::from(start_index));
    }
}

/// The stored `Status` string for a [`RecordingStatus`].
fn recording_status_name(status: RecordingStatus) -> String {
    status.json_name().into_owned()
}

/// Parses a timestamp stored in the guide cache: the canonical storage format
/// (`YYYY-MM-DD HH:MM:SS.fffffff`, UTC by convention — see
/// [`ferrofin_db::store`]), falling back to RFC-3339 for rows written before the
/// cache switched to the canonical format.
fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
        .or_else(|| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

/// Maps a `sqlx` error into a [`ServiceError`] via `ferrofin-db`'s `DbError`, for
/// consistency with the repository layer's error text.
/// The `DELETE` that removes one timer row.
const DELETE_TIMER_SQL: &str = r#"DELETE FROM "FerrofinLiveTvTimers" WHERE "Id" = ?1"#;

/// Reads one timer's stored `Data` by id.
const SELECT_TIMER_SQL: &str = r#"SELECT "Data" FROM "FerrofinLiveTvTimers" WHERE "Id" = ?1"#;

/// A file a warning was logged for: its path and its modification time
/// (`None` when it has none to read).
type WarnedFile = (PathBuf, Option<std::time::SystemTime>);

/// One stored series timer: the DTO clients see, plus the two server-owned
/// identity values the wire DTO has no field for.
///
/// Upstream these live on `SeriesTimerInfo` — `Id` (the external GUID, published
/// as the DTO's `ExternalId`) and `SeriesId` (the listings series id the fan-out
/// queries the guide with). See migration
/// `0024_livetv_series_timer_identity.sql`.
struct StoredSeriesTimer {
    /// The series timer as clients see it. Its `base.id` is the published,
    /// derived id and the row's primary key.
    dto: SeriesTimerInfoDto,
    /// `SeriesTimerInfo.Id`: the fresh GUID `CreateSeriesTimer` minted, which
    /// the published id is the MD5 of.
    external_id: String,
    /// `SeriesTimerInfo.SeriesId`: `program.ExternalSeriesId` at create time.
    series_id: Option<String>,
}

/// Whether two optional ids name the same thing, case-insensitively — C#'s
/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)` over values that
/// are only meaningful when both are present and non-empty.
fn matches_ignore_case(a: Option<&str>, b: Option<&str>) -> bool {
    match (
        a.map(str::trim).filter(|v| !v.is_empty()),
        b.map(str::trim).filter(|v| !v.is_empty()),
    ) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// The day pattern a day list spells out, or `None` when it spells none.
///
/// Port of `LiveTvDtoService.GetDayPattern` (v10.11.8 LiveTvDtoService.cs:360-387).
/// It is recomputed on every read upstream, so the value a client posted is
/// never trusted: post `Days=[Sat,Sun]` with `DayPattern=Daily` and the answer
/// is `Weekends`.
fn day_pattern(days: &[DayOfWeek]) -> Option<ferrofin_model::live_tv::DayPattern> {
    use ferrofin_model::live_tv::DayPattern;
    match days.len() {
        7 => Some(DayPattern::Daily),
        2 if days.contains(&DayOfWeek::Saturday) && days.contains(&DayOfWeek::Sunday) => {
            Some(DayPattern::Weekends)
        }
        5 if [
            DayOfWeek::Monday,
            DayOfWeek::Tuesday,
            DayOfWeek::Wednesday,
            DayOfWeek::Thursday,
            DayOfWeek::Friday,
        ]
        .iter()
        .all(|d| days.contains(d)) =>
        {
            Some(DayPattern::Weekdays)
        }
        _ => None,
    }
}

/// The CLDR root collator the name segments compare through, built once.
///
/// `Collator::try_new` reads ICU4X's *baked* root collation data, so the only
/// way it can fail is a build without that data — in which case the sort
/// degrades to code-point order rather than taking the process down, and says
/// so once.
fn invariant_collator() -> Option<&'static icu_collator::CollatorBorrowed<'static>> {
    static COLLATOR: std::sync::OnceLock<Option<icu_collator::CollatorBorrowed<'static>>> =
        std::sync::OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            match icu_collator::Collator::try_new(
                icu_collator::CollatorPreferences::default(),
                icu_collator::options::CollatorOptions::default(),
            ) {
                Ok(collator) => Some(collator),
                Err(error) => {
                    tracing::error!(
                        %error,
                        "live tv: CLDR root collation data is unavailable; \
                         name sorting falls back to code-point order"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Compares two names the way every Jellyfin name sort does.
///
/// Port of `AlphanumericComparator.CompareValues` (v10.11.8
/// Jellyfin.Extensions/AlphanumericComparator.cs): the strings are walked in
/// alternating digit/non-digit runs, digit runs compare as numbers (leading
/// zeros trimmed, then by length) and every run then compares as *text* under
/// `StringComparison.InvariantCulture` — CLDR root collation, which is what
/// [`invariant_collator`] supplies. Master replaced the whole class with
/// `StringComparer.Create(CultureInfo.InvariantCulture, CompareOptions.NumericOrdering)`
/// (MediaBrowser.Controller/Sorting/SortExtensions.cs:13, commit 098e8c6fed),
/// so both trees demand invariant collation; only the run segmentation differs
/// between them, and this keeps 10.11.8's, which is what the contract is pinned
/// to (`"Show 007" > "Show 7"` via the trailing length tie-break).
///
/// Code-point order is NOT a stand-in for it even on plain ASCII: root
/// collation orders by letter at the primary level and puts lowercase before
/// uppercase at the tertiary level, so `"apple" < "Banana"` where
/// `'B'(0x42) < 'a'(0x61)` says the opposite.
fn alphanumeric_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // The C# `len1`/`len2` are UTF-16 lengths, used for the empty checks and
    // the trailing `len1 - len2` tie-break; a char count carries the same
    // meaning here.
    let (bytes_a, bytes_b) = (a.as_bytes(), b.as_bytes());
    let by_length = || a.chars().count().cmp(&b.chars().count());
    if bytes_a.is_empty() || bytes_b.is_empty() {
        return by_length();
    }
    // Run boundaries are always char boundaries: `is_ascii_digit` is false for
    // every byte of a multi-byte sequence, so a digit run can neither start nor
    // end inside one.
    let (mut pos_a, mut pos_b) = (0_usize, 0_usize);
    loop {
        let (start_a, start_b) = (pos_a, pos_b);
        let is_num_a = bytes_a[pos_a].is_ascii_digit();
        let is_num_b = bytes_b[pos_b].is_ascii_digit();
        pos_a += 1;
        pos_b += 1;
        while pos_a < bytes_a.len() && bytes_a[pos_a].is_ascii_digit() == is_num_a {
            pos_a += 1;
        }
        while pos_b < bytes_b.len() && bytes_b[pos_b].is_ascii_digit() == is_num_b {
            pos_b += 1;
        }
        let mut span_a = &a[start_a..pos_a];
        let mut span_b = &b[start_b..pos_b];
        if is_num_a && is_num_b {
            // "Trim leading zeros so we can compare the length of the strings
            // to find the largest number" — and the C# keeps the TRIMMED spans
            // for the text compare that follows.
            span_a = span_a.trim_start_matches('0');
            span_b = span_b.trim_start_matches('0');
            match span_a.len().cmp(&span_b.len()) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        let ordering = match invariant_collator() {
            Some(collator) => collator.compare(span_a, span_b),
            None => span_a.cmp(span_b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
        if pos_a >= bytes_a.len() || pos_b >= bytes_b.len() {
            return by_length();
        }
    }
}

/// Orders series timers the way `LiveTvManager.GetSeriesTimers` does (v10.11.8
/// LiveTvManager.cs:919-934).
///
/// The `Priority` branch's ascending arm really is `OrderByDescending(Priority)`
/// and its descending arm `OrderBy(Priority)` — upstream's inversion, ported
/// verbatim rather than "fixed", because clients ask for
/// `sortBy=Priority&sortOrder=Descending` and expect what Jellyfin returns.
fn sort_series_timers(
    timers: &mut [SeriesTimerInfoDto],
    query: &ferrofin_model::live_tv::SeriesTimerQuery,
) {
    let descending = query.sort_order == SortOrder::Descending;
    let by_priority = query
        .sort_by
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("Priority"));
    let name = |t: &SeriesTimerInfoDto| t.base.name.clone().unwrap_or_default();
    if by_priority {
        timers.sort_by(|a, b| {
            let priority = if descending {
                a.base.priority.cmp(&b.base.priority)
            } else {
                b.base.priority.cmp(&a.base.priority)
            };
            priority.then_with(|| {
                let by_name = alphanumeric_compare(&name(a), &name(b));
                if descending {
                    by_name.reverse()
                } else {
                    by_name
                }
            })
        });
    } else {
        timers.sort_by(|a, b| {
            let by_name = alphanumeric_compare(&name(a), &name(b));
            if descending {
                by_name.reverse()
            } else {
                by_name
            }
        });
    }
}

/// Whether a series timer's fan-out should cancel this airing rather than record
/// it.
///
/// Port of `DefaultLiveTvService.ShouldCancelTimerForSeriesTimer` (v10.11.8
/// DefaultLiveTvService.cs:644-669). The `IsManual` arm is the caller's — a
/// hand-created timer is never cancelled, and only the caller knows whether the
/// stored row carries that flag.
///
/// TODO(work item, not a deferral): upstream's last arm is
/// `seriesTimer.SkipEpisodesInLibrary && IsProgramAlreadyInLibrary(timer)`
/// (DefaultLiveTvService.cs:668, :961-1000), which asks the library whether the
/// episode is already on disk. `FerrofinLiveTvManager` holds no item repository,
/// so porting it means adding that seam (an `Arc<dyn ItemRepository>` `OnceLock`
/// alongside `dto`, wired in `apps/ferrofin-server`) and then querying Series by
/// name + season/episode. Until then this arm never fires. It can only differ on
/// a guide that carries season+episode numbers or an episode title
/// (`IsProgramAlreadyInLibrary` returns false without them), which the XMLTV
/// parity fixture does not.
fn should_cancel_timer_for_series_timer(
    series: &SeriesTimerInfoDto,
    program: &GuideProgramRow,
    timer: &TimerInfoDto,
) -> bool {
    if !series.record_any_time {
        let series_tod = series.base.start_date.time();
        let timer_tod = timer.base.start_date.time();
        let delta = (series_tod - timer_tod).num_minutes().abs();
        if delta >= 10 {
            return true;
        }
    }
    if series.record_new_only && program.is_repeat {
        return true;
    }
    !series.record_any_channel && timer.base.channel_id != series.base.channel_id
}

/// Refreshes a stored timer from the guide, keeping its status.
///
/// Port of `DefaultLiveTvService.UpdateExistingTimerWithNewMetadata` (v10.11.8
/// DefaultLiveTvService.cs:365-391), restricted to the fields the wire
/// `TimerInfoDto` carries — the richer programme facts upstream copies here have
/// no DTO field and are re-read from the guide when the recorder starts.
fn update_existing_timer_with_new_metadata(existing: &mut TimerInfoDto, updated: &TimerInfoDto) {
    existing.base.channel_id = updated.base.channel_id;
    existing
        .base
        .channel_name
        .clone_from(&updated.base.channel_name);
    existing
        .base
        .external_channel_id
        .clone_from(&updated.base.external_channel_id);
    existing.base.end_date = updated.base.end_date;
    existing.base.start_date = updated.base.start_date;
    existing.base.name.clone_from(&updated.base.name);
    existing.base.overview.clone_from(&updated.base.overview);
    existing
        .base
        .program_id
        .clone_from(&updated.base.program_id);
    existing
        .base
        .external_program_id
        .clone_from(&updated.base.external_program_id);
    existing.run_time_ticks = updated.run_time_ticks;
}

/// The identity a listings provider gives one SHOWING of a programme, which is
/// what duplicate-showing suppression groups on.
///
/// The value is computed at ingest (`CategoryClasses::show_id`) and stored on
/// the row, because upstream computes it from the airing's PRE-movie-reset
/// fields — a movie has had its episode number and episode title cleared by the
/// time it reaches this table, so re-deriving here would hash a different
/// string for exactly the rows a movie series-rule looks at. The fallback covers
/// rows written before the column existed, which no refresh has rewritten yet.
///
/// The one knowing gap: upstream's `else` arm takes the listings' own
/// `program.ProgramId` when the guide supplies one
/// (`<episode-num system="dd_progid">`); Ferrofin's XMLTV reader does not
/// capture that element yet, so the MD5 arm always runs. A guide carrying
/// dd_progid would group differently — porting it means reading the element in
/// [`crate::xmltv`] and carrying it into `CategoryClasses::show_id`.
fn program_show_id(program: &GuideProgramRow) -> String {
    if let Some(stored) = program.show_id.as_deref().filter(|s| !s.is_empty()) {
        return stored.to_owned();
    }
    derive_show_id(
        &program.title,
        program.episode_title.as_deref(),
        program.season_number,
        program.episode_number,
        program.is_series,
        program.is_repeat,
        parse_dt(&program.start_date),
    )
}

/// `XmlTvListingsProvider.GetProgramInfo`'s `ShowId` derivation (v10.11.8
/// XmlTvListingsProvider.cs:186-213), bugs included: the season and episode
/// branches REPLACE `uniqueString` rather than appending to it, so a programme
/// with an episode number hashes `"-{episode}"` alone. Ported verbatim, because
/// this value decides which showings a series timer drops.
fn derive_show_id(
    title: &str,
    episode_title: Option<&str>,
    season_number: Option<i32>,
    episode_number: Option<i32>,
    is_series: bool,
    is_repeat: bool,
    start: Option<DateTime<Utc>>,
) -> String {
    let mut unique = format!("{}{}", title, episode_title.unwrap_or_default());
    if let Some(season) = season_number {
        unique = format!("-{season}");
    }
    if let Some(episode) = episode_number {
        unique = format!("-{episode}");
    }
    let mut show_id = ferrofin_common::extensions::get_md5(&unique)
        .simple()
        .to_string();
    // "If we don't have valid episode info, assume it's a unique program,
    // otherwise recordings might be skipped."
    if is_series && !is_repeat && episode_number.unwrap_or(0) == 0 {
        show_id.push_str(&dotnet_ticks(start.unwrap_or_else(Utc::now)).to_string());
    }
    show_id
}

/// A UTC instant as .NET `DateTime.Ticks`: 100-nanosecond intervals since
/// `0001-01-01T00:00:00`, which is what `ShowId`'s uniqueness suffix appends.
pub(crate) fn dotnet_ticks(at: DateTime<Utc>) -> i64 {
    /// Ticks between `0001-01-01` and the Unix epoch.
    const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    UNIX_EPOCH_TICKS + at.timestamp_micros() * 10
}

/// One timer the fan-out left recordable, tagged with the showing it records:
/// `(ShowId, StartDate, timer)` — the three things
/// [`duplicate_showings_to_cancel`] groups and orders by.
type EnabledShowing = (String, DateTime<Utc>, TimerInfoDto);

/// Cancels every showing of a programme but the one to record, and returns the
/// timers that lost.
///
/// Port of `DefaultLiveTvService.SearchForDuplicateShowIds` +
/// `HandleDuplicateShowIds` (v12.2 DefaultLiveTvService.cs:671-707): timers
/// are grouped by `ShowId`, a blank group is skipped, a group of one is skipped,
/// a group whose key ends `"0000"` is skipped (upstream's guard for a
/// SchedulesDirect show id with no sub-key, jellyfin/jellyfin#5856), and in what
/// is left every showing after the first is cancelled.
///
/// Upstream orders the group
/// `OrderByDescending(t => GetLiveTvChannel(t).IsHD).ThenBy(t => t.StartDate)`
/// (v12.2 DefaultLiveTvService.cs:674), but that `IsHD` is the channel library
/// item's `BaseItem.IsHD` (`Height >= 720`, BaseItem.cs:404), and
/// `GuideManager.GetChannel` never sets a channel's `Height`: it is false for
/// every timer. The stable sort is therefore `StartDate` alone, so the stored
/// `IsHd` channel column (the tuner's `ChannelInfo.IsHD`) deliberately takes no
/// part here, and the earliest showing always wins.
fn duplicate_showings_to_cancel(enabled: &[EnabledShowing]) -> Vec<TimerInfoDto> {
    let mut groups: HashMap<&str, Vec<&EnabledShowing>> = HashMap::new();
    for entry in enabled {
        groups.entry(entry.0.as_str()).or_default().push(entry);
    }
    let mut losers = Vec::new();
    for (show_id, mut group) in groups {
        if show_id.trim().is_empty() || group.len() < 2 || show_id.ends_with("0000") {
            continue;
        }
        group.sort_by_key(|entry| entry.1);
        for entry in group.into_iter().skip(1) {
            losers.push(entry.2.clone());
        }
    }
    losers
}

/// The timer a series timer schedules for one guide airing.
///
/// Port of `DefaultLiveTvService.CreateTimer(LiveTvProgram, SeriesTimerInfo, …)`
/// (v10.11.8 DefaultLiveTvService.cs:833-880). The id is derived, not random:
/// `(seriesTimer.Id + parent.ExternalId).GetMD5()`, so re-running the fan-out
/// finds the timer it made last time instead of scheduling the airing twice.
fn series_child_timer(
    program: &GuideProgramRow,
    stored: &StoredSeriesTimer,
    channel_external: Option<String>,
) -> TimerInfoDto {
    let external_id = ferrofin_common::extensions::get_md5(&format!(
        "{}{}",
        stored.external_id,
        program.external_id.as_deref().unwrap_or_default()
    ))
    .simple()
    .to_string();
    let mut timer = TimerInfoDto {
        status: RecordingStatus::New,
        series_timer_id: stored.dto.base.id.clone(),
        external_series_timer_id: Some(stored.external_id.clone()),
        base: ferrofin_model::live_tv::BaseTimerInfoDto {
            id: Some(ferrofin_traits::stubs::internal_timer_id(&external_id)),
            external_id: Some(external_id),
            type_: Some("Timer".to_owned()),
            service_name: Some(ferrofin_traits::stubs::LIVE_TV_SERVICE_NAME.to_owned()),
            server_id: stored.dto.base.server_id.clone(),
            external_channel_id: channel_external,
            pre_padding_seconds: stored.dto.base.pre_padding_seconds,
            post_padding_seconds: stored.dto.base.post_padding_seconds,
            is_pre_padding_required: stored.dto.base.is_pre_padding_required,
            is_post_padding_required: stored.dto.base.is_post_padding_required,
            keep_until: stored.dto.base.keep_until,
            priority: stored.dto.base.priority,
            ..ferrofin_model::live_tv::BaseTimerInfoDto::default()
        },
        ..TimerInfoDto::default()
    };
    if let Ok(program_id) = Uuid::parse_str(&program.id) {
        timer.base.program_id = Some(program_id.simple().to_string());
    }
    // `CopyProgramInfoToTimerInfo(parent, timer, …)` — name, overview, window,
    // channel and the listing's own programme id all come from the guide.
    copy_program_into_timer(program, &mut timer);
    timer
}

/// Copies what the guide knows about a programme onto the timer that records it.
///
/// Port of `DefaultLiveTvService.CopyProgramInfoToTimerInfo`, restricted to the
/// fields the wire `TimerInfoDto` carries; the richer programme facts the
/// recorder needs travel on [`TimerRecordingInfo`] instead.
fn copy_program_into_timer(program: &GuideProgramRow, timer: &mut TimerInfoDto) {
    timer.base.name = Some(program.title.clone());
    timer.base.overview.clone_from(&program.overview);
    if let Some(start) = parse_dt(&program.start_date) {
        timer.base.start_date = start;
    }
    if let Some(end) = program.end_date.as_deref().and_then(parse_dt) {
        timer.base.end_date = end;
    }
    if let Ok(channel_id) = Uuid::parse_str(&program.channel_id) {
        timer.base.channel_id = channel_id;
    }
    timer.base.channel_name = Some(program.channel_name.clone());
    timer
        .base
        .external_program_id
        .clone_from(&program.external_id);
    timer.run_time_ticks =
        Some((timer.base.end_date - timer.base.start_date).num_milliseconds() * 10_000);
}

/// Fills in the programme facts the recorder and the recording row need.
///
/// The other half of `CopyProgramInfoToTimerInfo` — the fields that have no
/// place on the wire DTO but decide the recording's name and folder.
fn apply_program_to_recording_info(program: &GuideProgramRow, info: &mut TimerRecordingInfo) {
    info.name.clone_from(&program.title);
    info.overview.clone_from(&program.overview);
    if let Some(start) = parse_dt(&program.start_date) {
        info.start_date = start;
    }
    if let Some(end) = program.end_date.as_deref().and_then(parse_dt) {
        info.end_date = end;
    }
    if let Ok(channel_id) = Uuid::parse_str(&program.channel_id) {
        info.channel_id = channel_id;
    }
    info.episode_title.clone_from(&program.episode_title);
    info.season_number = program.season_number;
    info.episode_number = program.episode_number;
    info.production_year = program.production_year;
    info.is_program_series = program.is_series;
    info.is_movie = program.is_movie;
    info.is_kids = program.is_kids;
    info.is_sports = program.is_sports;
    info.is_news = program.is_news;
    info.is_live = program.is_live;
    info.is_repeat = program.is_repeat;
    info.is_premiere = program.is_premiere;
    info.external_program_id.clone_from(&program.external_id);
    info.genres = program
        .genres
        .as_deref()
        .and_then(|g| serde_json::from_str(g).ok())
        .unwrap_or_default();
    info.official_rating.clone_from(&program.official_rating);
}

/// How far through a running capture is, as a percentage.
///
/// Port of `AddInfoToRecordingDto`'s `InProgress` branch: the padded window is
/// what is being recorded, so it is the window the percentage is of.
fn completion_percentage(
    row: &crate::projection::RecordingRow,
    parse: fn(&str) -> Option<DateTime<Utc>>,
) -> Option<f64> {
    let start =
        parse(&row.start_date)? - chrono::Duration::seconds(i64::from(row.pre_padding_seconds));
    let end = row.end_date.as_deref().and_then(parse)?
        + chrono::Duration::seconds(i64::from(row.post_padding_seconds));
    let total = (end - start).num_milliseconds();
    if total <= 0 {
        return None;
    }
    let elapsed = (Utc::now() - start).num_milliseconds();
    #[allow(clippy::cast_precision_loss)] // a percentage; millisecond precision is ample
    Some((elapsed as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

/// The MD5 of the built-in Live TV service's C# type name, in `"N"` form —
/// the first half of every Live TV `LiveStreamId`.
///
/// Port of `LiveTvMediaSourceProvider.GetChannelStream`'s `idPrefix`. Computed
/// once: it is a constant of the port, not of the configuration.
fn live_tv_service_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        ferrofin_common::extensions::get_md5(LIVE_TV_SERVICE_TYPE_NAME)
            .simple()
            .to_string()
    })
}

fn db_err(e: sqlx::Error) -> ServiceError {
    ServiceError::from(ferrofin_db::DbError::from(e))
}

#[cfg(test)]
mod tests {
    mod guide_mirror;
    mod schedules_direct;

    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::Utc;
    use uuid::Uuid;

    use super::{
        DEFAULT_LIVE_STREAM_BUFFER_MS, GuideProgramRow, LiveTvPaths, alphanumeric_compare,
        day_pattern, duplicate_showings_to_cancel, live_tv_service_key, sort_series_timers,
    };
    use ferrofin_model::live_tv::{
        BaseTimerInfoDto, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
    };

    /// The local API URL the tests' live streams and recordings are served
    /// from.
    struct LoopbackApi;

    #[async_trait::async_trait]
    impl ferrofin_traits::system::LocalApiAccess for LoopbackApi {
        async fn get_api_url_for_local_access(&self) -> Result<String, ServiceError> {
            Ok("http://127.0.0.1:8096".to_owned())
        }
    }

    use ferrofin_db::Database;
    use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
    use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
    use ferrofin_traits::stubs::LiveTvManager;

    use ferrofin_model::dto::{DayOfWeek, SortOrder};
    use ferrofin_model::live_tv::ItemSortBy;

    use super::{DELETE_TIMER_SQL, FerrofinLiveTvManager, SourceFetcher, derive_show_id, parse_dt};

    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_model::dto::BaseItemDto;
    use ferrofin_model::querying::ItemFields;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::stubs::LiveTvChannelQuery;

    /// A minimal [`ferrofin_traits::dto::DtoService`] for the manager tests:
    /// projects just the entity fields these tests assert (the REAL projection
    /// — user data, images, the kind hooks — is covered by
    /// `ferrofin-core`'s `dto_service` tests over the same synthetic entities).
    /// Field-gated values (`Etag`, `ProviderIds`, `SortName`, `DateCreated`)
    /// honour the gate so the `RemoveFields` list/detail split is observable.
    struct FakeDto;

    #[async_trait::async_trait]
    impl ferrofin_traits::dto::DtoService for FakeDto {
        async fn get_primary_image_aspect_ratio(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<f64>, ServiceError> {
            Ok(None)
        }
        async fn get_base_item_dto(
            &self,
            item: &BaseItemEntity,
            options: &DtoOptions,
            user: Option<&UserEntity>,
            owner_id: Option<Uuid>,
        ) -> Result<BaseItemDto, ServiceError> {
            Ok(self
                .get_base_item_dtos(std::slice::from_ref(item), options, user, owner_id, true)
                .await?
                .pop()
                .expect("one dto"))
        }
        async fn get_base_item_dtos(
            &self,
            items: &[BaseItemEntity],
            options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
            _skip_visibility_check: bool,
        ) -> Result<Vec<BaseItemDto>, ServiceError> {
            Ok(items
                .iter()
                .map(|item| BaseItemDto {
                    id: Uuid::parse_str(&item.id).unwrap_or_default(),
                    name: item.name.clone(),
                    // `GetClientTypeName`, as the real projection maps it.
                    type_: if item.type_.ends_with("LiveTvChannel") {
                        ferrofin_model::data::BaseItemKind::TvChannel
                    } else {
                        ferrofin_model::data::BaseItemKind::Program
                    },
                    channel_id: item
                        .channel_id
                        .as_deref()
                        .and_then(|s| Uuid::parse_str(s).ok()),
                    end_date: item.end_date,
                    run_time_ticks: item.run_time_ticks,
                    index_number: item.index_number.and_then(|n| i32::try_from(n).ok()),
                    parent_index_number: item
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    production_year: item.production_year.and_then(|y| i32::try_from(y).ok()),
                    official_rating: item.official_rating.clone(),
                    genres: options.contains_field(ItemFields::Genres).then(|| {
                        item.genres
                            .as_deref()
                            .map(|g| g.split('|').map(str::to_owned).collect())
                            .unwrap_or_default()
                    }),
                    overview: options
                        .contains_field(ItemFields::Overview)
                        .then(|| item.overview.clone())
                        .flatten(),
                    sort_name: options
                        .contains_field(ItemFields::SortName)
                        .then(|| item.sort_name.clone())
                        .flatten(),
                    etag: options
                        .contains_field(ItemFields::Etag)
                        .then(|| "fake-etag".to_owned()),
                    provider_ids: options
                        .contains_field(ItemFields::ProviderIds)
                        .then(HashMap::new),
                    date_created: options
                        .contains_field(ItemFields::DateCreated)
                        .then_some(item.date_created)
                        .flatten(),
                    ..BaseItemDto::default()
                })
                .collect())
        }
        async fn get_item_by_name_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _tagged_item_ids: Option<&[Uuid]>,
            _user: Option<&UserEntity>,
        ) -> Result<BaseItemDto, ServiceError> {
            unimplemented!("not a by-name path")
        }
    }

    /// An in-memory [`SourceFetcher`] mapping URL → body for offline tests.
    struct FakeFetcher(HashMap<String, String>);

    #[async_trait::async_trait]
    impl SourceFetcher for FakeFetcher {
        async fn fetch(&self, url: &str) -> Result<String, ferrofin_traits::error::ServiceError> {
            self.0
                .get(url)
                .cloned()
                .ok_or_else(|| ferrofin_traits::error::ServiceError::Backend(format!("no {url}")))
        }
    }

    /// A [`SourceFetcher`] that never answers, counting how many callers got as
    /// far as asking. The shape of a wedged M3U/XMLTV source.
    #[derive(Default)]
    struct HangingFetcher(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl SourceFetcher for HangingFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, ferrofin_traits::error::ServiceError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    /// A [`SourceFetcher`] whose bodies can be swapped between refreshes, for
    /// the tests that need a second pass to see different upstream content.
    struct SwappableFetcher(std::sync::Mutex<HashMap<String, String>>);

    #[async_trait::async_trait]
    impl SourceFetcher for SwappableFetcher {
        async fn fetch(&self, url: &str) -> Result<String, ferrofin_traits::error::ServiceError> {
            self.0
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| ferrofin_traits::error::ServiceError::Backend(format!("no {url}")))
        }
    }

    async fn manager_with(fetcher: FakeFetcher) -> FerrofinLiveTvManager {
        manager_with_fetcher(std::sync::Arc::new(fetcher)).await
    }

    async fn manager_with_fetcher(
        fetcher: std::sync::Arc<dyn SourceFetcher>,
    ) -> FerrofinLiveTvManager {
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        FerrofinLiveTvManager::new(
            db,
            fetcher,
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_dto(Arc::new(FakeDto))
    }

    /// `info` as an XMLTV provider: the dashboard's XMLTV page always posts
    /// `Type: "xmltv"`, and a missing type is no provider (404).
    fn as_xmltv(info: ListingsProviderInfo) -> ListingsProviderInfo {
        ListingsProviderInfo {
            type_: info.type_.or_else(|| Some("xmltv".to_owned())),
            ..info
        }
    }

    const M3U: &str = "#EXTM3U\n\
        #EXTINF:-1 tvg-id=\"one.tv\" tvg-chno=\"1\",Channel One\nhttp://tuner/one\n\
        #EXTINF:-1 tvg-id=\"two.tv\" tvg-chno=\"2\",Channel Two\nhttp://tuner/two\n";
    /// The `YYYYMMDD` an XMLTV test fixture should be dated `offset` days out.
    ///
    /// The guide refresh only ingests `[now - 1h, now - 1h + GuideDays)`, the way
    /// `GuideManager.RefreshChannelsInternal` does, so a fixture pinned to a
    /// literal calendar date silently empties itself once that date rolls past.
    /// Day 0 is *tomorrow*: that is inside the window whatever time of day the
    /// suite runs at, where today at 06:00 is not.
    fn guide_day(offset: i64) -> String {
        (chrono::Utc::now().date_naive() + chrono::Duration::days(1 + offset))
            .format("%Y%m%d")
            .to_string()
    }

    /// A fixture dated with [`guide_day`] instead of a literal date.
    fn dated(template: &str) -> String {
        template.replace("20260725", &guide_day(0))
    }

    const XMLTV: &str = "<tv>\
        <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
        <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
        <title>Morning Show</title><desc>News.</desc><category>News</category></programme>\
        </tv>";

    #[tokio::test]
    async fn info_always_has_emby_service_and_is_enabled() {
        // No tuner host configured: the built-in "Emby" service is still
        // present and IsEnabled is true, mirroring DefaultLiveTvService.
        let mgr = manager_with(FakeFetcher(HashMap::from([(
            "http://tuner/playlist.m3u".to_owned(),
            M3U.to_owned(),
        )])))
        .await;
        let info = mgr.get_live_tv_info().await.expect("info");
        assert!(info.is_enabled);
        assert_eq!(info.services.len(), 1);
        assert_eq!(info.services[0].name.as_deref(), Some("Emby"));

        // A tuner host is not a service: the list stays exactly [Emby] (Jellyfin
        // lists ILiveTvServices, of which a stock server has one).
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        let info = mgr.get_live_tv_info().await.expect("info2");
        assert!(info.is_enabled);
        assert_eq!(info.services.len(), 1);
        assert_eq!(info.services[0].name.as_deref(), Some("Emby"));
    }

    #[tokio::test]
    async fn info_lists_the_users_allowed_live_tv_once_a_tuner_exists() {
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let users: Arc<dyn ferrofin_traits::library::UserManager> = Arc::new(
            ferrofin_core::user_manager::FerrofinUserManager::new(db.clone()),
        );
        let allowed = users.create_user("tv").await.expect("user");
        let denied = users.create_user("radio").await.expect("user");
        let allowed_id = Uuid::parse_str(&allowed.id).expect("guid");
        let mut policy = users
            .get_user_dto(&allowed, None)
            .await
            .expect("dto")
            .policy
            .expect("policy");
        policy.enable_live_tv_access = true;
        users
            .update_policy(allowed_id, &policy)
            .await
            .expect("policy");
        let mut policy = users
            .get_user_dto(&denied, None)
            .await
            .expect("dto")
            .policy
            .expect("policy");
        policy.enable_live_tv_access = false;
        users
            .update_policy(Uuid::parse_str(&denied.id).expect("guid"), &policy)
            .await
            .expect("policy");

        let mgr = FerrofinLiveTvManager::new(
            db,
            // `M3UTunerHost.Validate` opens the playlist before the save.
            Arc::new(FakeFetcher(HashMap::from([(
                "http://tuner/playlist.m3u".to_owned(),
                M3U.to_owned(),
            )]))),
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_users(users);
        // `IsLiveTvEnabled`: the permission alone is not enough — a tuner host must exist.
        let enabled_users = mgr.get_live_tv_info().await.expect("info").enabled_users;
        assert!(enabled_users.is_empty(), "{enabled_users:?}");
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        // Ids are `ToString("N")`.
        assert_eq!(
            mgr.get_live_tv_info().await.expect("info").enabled_users,
            vec![allowed_id.simple().to_string()]
        );
    }

    #[tokio::test]
    async fn tuner_host_crud_roundtrips() {
        let mgr = manager_with(FakeFetcher(HashMap::from([(
            "http://tuner/playlist.m3u".to_owned(),
            M3U.to_owned(),
        )])))
        .await;
        // The synchronous flag the "Refresh Guide" task's hidden rule polls
        // tracks the store through every mutation.
        assert!(!mgr.has_tuner_hosts(), "no host configured yet");
        let saved = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("save");
        let id = saved.id.clone().expect("id assigned");
        assert_eq!(saved.type_.as_deref(), Some("m3u"));
        assert!(mgr.has_tuner_hosts(), "saving a host reveals the task");

        let hosts = mgr.get_tuner_hosts().await.expect("list");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].url.as_deref(), Some("http://tuner/playlist.m3u"));
        assert!(mgr.has_tuner_hosts());

        mgr.delete_tuner_host(&id).await.expect("delete");
        let tuner_hosts = mgr.get_tuner_hosts().await.expect("list2");
        assert!(tuner_hosts.is_empty(), "{tuner_hosts:?}");
        assert!(
            !mgr.has_tuner_hosts(),
            "deleting the last host hides the task again"
        );
    }

    #[tokio::test]
    async fn tuner_host_without_url_is_rejected() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let err = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect_err("no url");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::InvalidInput(_)
        ));
    }

    #[rstest::rstest]
    #[case::absent(None)]
    #[case::empty(Some(""))]
    #[case::blank(Some("   "))]
    #[case::unregistered(Some("nosuchtype"))]
    #[tokio::test]
    async fn a_tuner_type_no_host_claims_is_not_found(#[case] type_: Option<&str>) {
        // `TunerHostManager.SaveTunerHost`'s lookup is
        // `_tunerHosts.FirstOrDefault(i => string.Equals(info.Type, i.Type,
        // OrdinalIgnoreCase))` (v10.11.8 TunerHostManager.cs:63) with NO
        // carve-out: null, "" and "   " match neither "hdhomerun" nor "m3u",
        // so all four of these are `ResourceNotFoundException` — a 404.
        // Ferrofin briefly defaulted the first three to "m3u", which answered
        // 200 and stored a row where Jellyfin answered 404 (measured on the
        // parity pair 2026-08-30).
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let err = mgr
            .save_tuner_host(TunerHostInfo {
                type_: type_.map(str::to_owned),
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect_err("a type no host claims must not be stored");
        assert!(
            matches!(err, ferrofin_traits::error::ServiceError::NotFound(_)),
            "{err}"
        );
        assert!(
            mgr.get_tuner_hosts().await.expect("list").is_empty(),
            "the rejected host must not have been persisted"
        );
    }

    #[tokio::test]
    async fn an_unregistered_tuner_type_is_not_found() {
        // `TunerHostManager.SaveTunerHost` (v10.11.8 TunerHostManager.cs:60-69)
        // resolves the provider first and throws `ResourceNotFoundException`
        // when nothing claims `info.Type` — Jellyfin answers 404 here.
        // Ferrofin used to default the unknown type to "m3u" and answer 200,
        // storing a tuner no host would ever read.
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let err = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("nosuchtype".to_owned()),
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect_err("an unregistered type must not be stored");
        assert!(
            matches!(err, ferrofin_traits::error::ServiceError::NotFound(_)),
            "{err}"
        );
        assert!(
            mgr.get_tuner_hosts().await.expect("list").is_empty(),
            "the rejected host must not have been persisted"
        );
        assert!(!mgr.has_tuner_hosts());
    }

    /// A tuner host that reports a fixed set of devices from `DiscoverDevices`.
    struct DiscoveringHost(Vec<TunerHostInfo>);

    #[async_trait::async_trait]
    impl crate::tuner_host::TunerHost for DiscoveringHost {
        fn name(&self) -> &'static str {
            "Discovering"
        }
        fn type_id(&self) -> &'static str {
            "discovering"
        }
        async fn get_channels(
            &self,
            _tuner: &TunerHostInfo,
        ) -> Result<Vec<crate::tuner_host::TunerChannel>, ferrofin_traits::error::ServiceError>
        {
            Ok(Vec::new())
        }
        async fn discover_devices(
            &self,
            _duration_ms: u64,
        ) -> Result<Vec<TunerHostInfo>, ferrofin_traits::error::ServiceError> {
            Ok(self.0.clone())
        }
        async fn channel_media_sources(
            &self,
            _tuner: &TunerHostInfo,
            _channel: &crate::tuner_host::StoredChannel,
        ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ferrofin_traits::error::ServiceError>
        {
            Ok(Vec::new())
        }
        async fn channel_stream(
            &self,
            _tuner: &TunerHostInfo,
            _channel: &crate::tuner_host::StoredChannel,
            _stream_id: Option<&str>,
        ) -> Result<crate::tuner_host::ChannelStream, ferrofin_traits::error::ServiceError>
        {
            Err(ferrofin_traits::error::ServiceError::backend("no"))
        }
    }

    /// `TunerHostManager.DiscoverTuners(newDevicesOnly)` (v10.11.8
    /// TunerHostManager.cs:102-121) filters the discovered devices against the
    /// `DeviceId`s already on a configured tuner host. Dropping the flag —
    /// which the handler did until the type list gained a second backend —
    /// reports an already-added device as a new one.
    #[tokio::test]
    async fn discovery_can_drop_the_devices_already_configured() {
        let device = |id: &str, url: &str| TunerHostInfo {
            type_: Some("discovering".to_owned()),
            device_id: Some(id.to_owned()),
            url: Some(url.to_owned()),
            ..TunerHostInfo::default()
        };
        let mgr = manager_with(FakeFetcher(HashMap::new()))
            .await
            .with_tuner_hosts(vec![Arc::new(DiscoveringHost(vec![
                device("AAAA1111", "http://one/"),
                device("BBBB2222", "http://two/"),
                // `Contains(tuner.DeviceId, OrdinalIgnoreCase)` never matches a
                // null, so a device that did not name itself is always new.
                TunerHostInfo {
                    type_: Some("discovering".to_owned()),
                    url: Some("http://three/".to_owned()),
                    ..TunerHostInfo::default()
                },
            ]))]);

        // Nothing configured: both flags report all three.
        assert_eq!(mgr.discover_tuners(0, false).await.expect("all").len(), 3);
        assert_eq!(mgr.discover_tuners(0, true).await.expect("new").len(), 3);

        // Configure the first device, in the OTHER case — the C# comparison is
        // `StringComparer.OrdinalIgnoreCase`.
        mgr.save_tuner_host(device("aaaa1111", "http://one/"))
            .await
            .expect("save");
        let all = mgr.discover_tuners(0, false).await.expect("all");
        assert_eq!(
            all.len(),
            3,
            "newDevicesOnly=false still reports every device"
        );
        let new_only = mgr.discover_tuners(0, true).await.expect("new");
        assert_eq!(
            new_only
                .iter()
                .map(|d| d.device_id.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["BBBB2222".to_owned(), String::new()],
            "the configured device is dropped; the unnamed one is not"
        );
    }

    #[tokio::test]
    async fn the_advertised_tuner_types_are_the_registered_hosts() {
        // The exact bytes Jellyfin 10.11.8 answers on
        // `GET /LiveTv/TunerHosts/Types`, in its order (`OrderBy(i => i.Name)`).
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        assert_eq!(
            mgr.tuner_host_types()
                .into_iter()
                .map(|p| (p.name.unwrap_or_default(), p.id.unwrap_or_default()))
                .collect::<Vec<_>>(),
            vec![
                ("HD Homerun".to_owned(), "hdhomerun".to_owned()),
                ("M3U Tuner".to_owned(), "m3u".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_hdhomerun_tuner_refreshes_through_the_hdhomerun_host() {
        // The dispatch bug this replaces: `refresh_guide` fed EVERY tuner's URL
        // to the M3U parser, so an HDHomeRun's `discover.json` parsed as a
        // playlist with no channels and no error at all.
        let device = HashMap::from([
            (
                "http://192.168.1.182/discover.json".to_owned(),
                r#"{"FriendlyName":"HDHomeRun PRIME","ModelNumber":"HDHR3-CC","DeviceID":"FFFFFFFF",
                    "TunerCount":3,"BaseURL":"http://192.168.1.182:80",
                    "LineupURL":"http://192.168.1.182:80/lineup.json"}"#
                    .to_owned(),
            ),
            (
                "http://192.168.1.182:80/lineup.json".to_owned(),
                r#"[{"GuideNumber":"4.1","GuideName":"WCMH-DT","HD":1,
                     "VideoCodec":"MPEG2","AudioCodec":"AC3",
                     "URL":"http://192.168.1.111:5004/auto/v4.1"},
                    {"GuideNumber":"4.2","GuideName":"MeTV",
                     "VideoCodec":"MPEG2","AudioCodec":"AC3",
                     "URL":"http://192.168.1.111:5004/auto/v4.2"},
                    {"GuideNumber":"9.9","GuideName":"Scrambled","DRM":1,
                     "URL":"http://192.168.1.111:5004/auto/v9.9"}]"#
                    .to_owned(),
            ),
        ]);
        let mgr = manager_with(FakeFetcher(device)).await;
        let saved = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("hdhomerun".to_owned()),
                url: Some("http://192.168.1.182".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("save");
        // `IConfigurableTunerHost.Validate` ran and adopted the device id.
        assert_eq!(saved.device_id.as_deref(), Some("FFFFFFFF"));

        mgr.refresh_guide().await.expect("refresh");
        // Read back through the repository, not inline SQL (the SQL-boundary
        // ratchet): `channel_rows` is the lineup in stored order, and
        // `channel_stream_source` is the per-channel tuner facts.
        let rows = crate::guide_repository::channel_rows(&mgr.db)
            .await
            .expect("channels");
        // The DRM row is dropped, and the identity is `hdhr_{GuideNumber}` —
        // NOT the M3U `m3u_{md5}{md5}` shape.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "WCMH-DT");
        assert_eq!(rows[0].number.as_deref(), Some("4.1"));
        assert_eq!(rows[1].name, "MeTV");
        for (guide_number, is_hd) in [("4.1", Some(true)), ("4.2", Some(false))] {
            let external = format!("hdhr_{guide_number}");
            let (stored, tuner_id) = crate::guide_repository::channel_stream_source(
                &mgr.db,
                super::internal_channel_id(&external),
            )
            .await
            .expect("stream source")
            .expect("the channel exists under the id derived from its external id");
            assert_eq!(stored.external_id, external);
            // `HD` is absent on 4.2, so the flag is false — not "unknown".
            assert_eq!(stored.is_hd, is_hd);
            assert_eq!(stored.video_codec.as_deref(), Some("MPEG2"));
            assert_eq!(stored.audio_codec.as_deref(), Some("AC3"));
            assert_eq!(
                stored.path,
                format!("http://192.168.1.111:5004/auto/v{guide_number}")
            );
            assert_eq!(tuner_id, saved.id.clone().expect("id"));
        }

        // …and the media source is the HDHomeRun one, not the M3U one: an
        // HDHR3-CC does not transcode, so exactly the native profile exists.
        let channel_id = super::internal_channel_id("hdhr_4.1");
        let sources = mgr
            .get_channel_media_sources(channel_id)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert!(
            source
                .id
                .as_deref()
                .is_some_and(|id| id.starts_with("native_")),
            "{:?}",
            source.id
        );
        assert_eq!(source.container.as_deref(), Some("ts"));
        assert!(source.supports_direct_play);
        assert!(source.use_most_compatible_transcoding_profile);
        // `isHd` ⇒ the android-tv 1200 condition's dummy resolution.
        assert_eq!(source.media_streams[0].width, Some(1920));
        assert_eq!(source.media_streams[0].height, Some(1080));
        assert_eq!(source.media_streams[0].bit_rate, Some(15_000_000));
        assert_eq!(source.media_streams[1].bit_rate, Some(448_000));
    }

    #[tokio::test]
    async fn listing_provider_crud_roundtrips() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let saved = mgr
            .save_listing_provider(
                as_xmltv(ListingsProviderInfo {
                    path: Some("http://guide/xmltv.xml".to_owned()),
                    ..ListingsProviderInfo::default()
                }),
                false,
                false,
            )
            .await
            .expect("save");
        assert_eq!(saved.type_.as_deref(), Some("xmltv"));
        let id = saved.id.clone().expect("id");
        assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 1);
        mgr.delete_listing_provider(&id).await.expect("delete");
        let listing_providers = mgr.get_listing_providers().await.expect("list2");
        assert!(listing_providers.is_empty(), "{listing_providers:?}");
    }

    /// `XmlTvListingsProvider.Validate` (v12.2 :333-349): a local guide path
    /// that is not a file is `FileNotFoundException` (`404`) and nothing is
    /// saved; an existing file and a URL whose scheme is in any case save.
    #[tokio::test]
    async fn an_xmltv_provider_must_name_an_existing_file_or_a_url() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = |path: &str| {
            as_xmltv(ListingsProviderInfo {
                path: Some(path.to_owned()),
                ..ListingsProviderInfo::default()
            })
        };

        let missing = dir.path().join("missing.xml");
        let err = mgr
            .save_listing_provider(provider(missing.to_str().expect("utf-8")), false, false)
            .await
            .expect_err("refused");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        let listing_providers = mgr.get_listing_providers().await.expect("list");
        assert!(listing_providers.is_empty(), "{listing_providers:?}");

        let guide = dir.path().join("guide.xml");
        std::fs::write(&guide, XMLTV).expect("write");
        mgr.save_listing_provider(provider(guide.to_str().expect("utf-8")), false, false)
            .await
            .expect("an existing file");
        mgr.save_listing_provider(provider("HTTP://guide/xmltv.xml"), false, false)
            .await
            .expect("a URL");
        assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 2);
    }

    /// `XmlTvListingsProvider.Validate` (v12.4 :333-349) tests the raw string:
    /// an empty or blank `Path` does not start with `http` and `File.Exists`
    /// is false for it, so the save is `FileNotFoundException` (`404`) and
    /// nothing is stored. Only a missing `Path` differs: upstream dereferences
    /// the null (a `500`), Ferrofin answers `400`.
    #[tokio::test]
    async fn an_xmltv_provider_with_an_empty_or_blank_path_is_not_found() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let provider = |path: Option<&str>| {
            as_xmltv(ListingsProviderInfo {
                path: path.map(str::to_owned),
                ..ListingsProviderInfo::default()
            })
        };

        for path in ["", " ", "\t"] {
            let err = mgr
                .save_listing_provider(provider(Some(path)), false, false)
                .await
                .expect_err("refused");
            assert!(
                matches!(err, ServiceError::NotFound(_)),
                "{path:?}: {err:?}"
            );
        }
        let err = mgr
            .save_listing_provider(provider(None), false, false)
            .await
            .expect_err("refused");
        assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
        let listing_providers = mgr.get_listing_providers().await.expect("list");
        assert!(listing_providers.is_empty(), "{listing_providers:?}");
    }

    /// `XmlTvListingsProvider.GetLanguage`: a guide is read in its provider's
    /// `PreferredLanguage`, else in the server's `PreferredMetadataLanguage`,
    /// read as it is when the guide is read.
    #[tokio::test]
    async fn a_guide_is_read_in_the_provider_or_the_server_language() {
        let guide = "<tv><channel id=\"a\">\
            <display-name lang=\"de\">Erstes</display-name>\
            <display-name lang=\"en\">First</display-name></channel></tv>";
        let server = Arc::new(std::sync::Mutex::new("de".to_owned()));
        let mgr = manager_with(FakeFetcher(HashMap::from([(
            "http://guide/xmltv.xml".to_owned(),
            guide.to_owned(),
        )])))
        .await
        .with_metadata_language({
            let server = Arc::clone(&server);
            Arc::new(move || server.lock().expect("lock").clone())
        });
        let name = |language: Option<&str>| {
            let mgr = mgr.clone();
            let provider = as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                preferred_language: language.map(str::to_owned),
                ..ListingsProviderInfo::default()
            });
            async move {
                mgr.provider_channels(&provider).await.expect("channels")[0]
                    .name
                    .clone()
            }
        };

        assert_eq!(name(Some("en")).await, "First");
        assert_eq!(name(None).await, "Erstes");
        assert_eq!(name(Some("  ")).await, "Erstes", "a blank one is none");
        *server.lock().expect("lock") = "en".to_owned();
        assert_eq!(name(None).await, "First", "the setting is read live");
    }

    /// `TunerHostManager.SaveTunerHost` runs `M3UTunerHost.Validate` first, so
    /// a tuner whose playlist does not open is refused and never stored.
    #[tokio::test]
    async fn an_m3u_tuner_whose_playlist_does_not_open_is_not_saved() {
        let mgr = manager_with_fetcher(Arc::new(crate::fetch::ReqwestFetcher::new())).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.m3u");
        let err = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                url: Some(missing.to_str().expect("utf-8").to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect_err("refused");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        let tuner_hosts = mgr.get_tuner_hosts().await.expect("list");
        assert!(tuner_hosts.is_empty(), "{tuner_hosts:?}");
    }

    /// `ListingsManager.SaveListingProvider`'s id rule: a posted id matching a
    /// provider ignoring case replaces it under the posted spelling; one that
    /// matches nothing (an untrimmed one included) gets a fresh id.
    #[tokio::test]
    async fn a_reposted_provider_id_replaces_the_provider_as_spelled() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let provider = |id: Option<&str>, path: &str| {
            as_xmltv(ListingsProviderInfo {
                id: id.map(str::to_owned),
                path: Some(path.to_owned()),
                ..ListingsProviderInfo::default()
            })
        };
        let id = mgr
            .save_listing_provider(provider(None, "http://guide/a.xml"), false, false)
            .await
            .expect("save")
            .id
            .expect("id");

        let upper = id.to_uppercase();
        let replaced = mgr
            .save_listing_provider(provider(Some(&upper), "http://guide/b.xml"), false, false)
            .await
            .expect("replace");
        assert_eq!(replaced.id.as_deref(), Some(upper.as_str()));
        let listed = mgr.get_listing_providers().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_deref(), Some(upper.as_str()));
        assert_eq!(listed[0].path.as_deref(), Some("http://guide/b.xml"));

        let padded = format!(" {upper} ");
        let added = mgr
            .save_listing_provider(provider(Some(&padded), "http://guide/c.xml"), false, false)
            .await
            .expect("add");
        assert_ne!(added.id.as_deref(), Some(padded.as_str()));
        assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 2);
    }

    /// `TunerHostManager.SaveTunerHost`: a posted id matching a stored host
    /// ignoring case replaces it (`config.TunerHosts[index] = info`), so the
    /// host keeps the posted spelling, and its channels follow it without
    /// changing their ids. An id `FindIndex` matches nothing with (padded with
    /// spaces) adds a host under a fresh id.
    #[tokio::test]
    async fn a_reposted_tuner_id_replaces_the_host_as_spelled() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;
        let tuner = |id: Option<&str>| TunerHostInfo {
            id: id.map(str::to_owned),
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        };
        let id = mgr
            .save_tuner_host(tuner(None))
            .await
            .expect("save")
            .id
            .expect("id");
        mgr.refresh_guide().await.expect("refresh");
        let channels_before = crate::guide_repository::all_channel_ids(&mgr.db)
            .await
            .expect("channels");
        assert_eq!(channels_before.len(), 2);

        let upper = id.to_uppercase();
        let replaced = mgr
            .save_tuner_host(tuner(Some(&upper)))
            .await
            .expect("replace");
        assert_eq!(replaced.id.as_deref(), Some(upper.as_str()));
        let listed = mgr.get_tuner_hosts().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_deref(), Some(upper.as_str()));

        let mut channels_after = crate::guide_repository::all_channel_ids(&mgr.db)
            .await
            .expect("channels");
        channels_after.sort();
        let mut expected = channels_before.clone();
        expected.sort();
        assert_eq!(channels_after, expected, "channel ids do not move");
        for channel in &channels_before {
            let channel = Uuid::parse_str(channel).expect("guid");
            let (_, host_id) = crate::guide_repository::channel_stream_source(&mgr.db, channel)
                .await
                .expect("read")
                .expect("channel");
            assert_eq!(host_id, upper, "every channel follows its host");
            mgr.tuner_host(&host_id).await.expect("the host is found");
        }

        // The same spelling again changes nothing but the host's settings.
        mgr.save_tuner_host(tuner(Some(&upper)))
            .await
            .expect("same");
        assert_eq!(mgr.get_tuner_hosts().await.expect("list").len(), 1);

        let padded = format!(" {upper} ");
        let added = mgr
            .save_tuner_host(tuner(Some(&padded)))
            .await
            .expect("add");
        assert_ne!(added.id.as_deref(), Some(padded.as_str()));
        assert_ne!(added.id.as_deref(), Some(upper.as_str()));
        assert_eq!(mgr.get_tuner_hosts().await.expect("list").len(), 2);
    }

    /// A guide with a news airing, a movie, and a series episode (xmltv_ns
    /// `0.5.` → S1E6), so every classification branch of
    /// `XmlTvListingsProvider.GetProgramInfo` is exercised.
    const CLASSIFIED_XMLTV: &str = "<tv>\
        <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
        <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
        <title>Morning Show</title><category>News</category><previously-shown/></programme>\
        <programme start=\"20260725070000 +0000\" stop=\"20260725090000 +0000\" channel=\"one.tv\">\
        <title>Heat</title><sub-title>ignored for movies</sub-title><category>Movie</category>\
        <episode-num system=\"xmltv_ns\">0.5.</episode-num></programme>\
        <programme start=\"20260725090000 +0000\" stop=\"20260725093000 +0000\" channel=\"one.tv\">\
        <title>Bluey</title><sub-title>Keepy Uppy</sub-title><category>Kids</category>\
        <episode-num system=\"xmltv_ns\">0.5.</episode-num><new/><previously-shown/></programme>\
        <programme start=\"20260725093000 +0000\" stop=\"20260725100000 +0000\" channel=\"one.tv\">\
        <title>Late Talk</title><sub-title>With a guest</sub-title></programme>\
        </tv>";

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one guide, every classification branch
    async fn guide_refresh_classifies_programmes_like_the_xmltv_provider() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(CLASSIFIED_XMLTV));
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        // A provider posted without category lists gets Jellyfin's defaults.
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                news_categories: None,
                movie_categories: None,
                kids_categories: None,
                sports_categories: None,
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");

        let all = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(all.total_record_count, 4);
        let by_name = |name: &str| {
            all.items
                .iter()
                .find(|p| p.name.as_deref() == Some(name))
                .cloned()
                .expect(name)
        };

        let news = by_name("Morning Show");
        assert_eq!(
            news.type_,
            ferrofin_model::data::BaseItemKind::Program,
            "GetClientTypeName"
        );
        assert_eq!(news.is_news, Some(true));
        assert_eq!(
            // False flags are absent (`|=` on a null bool? stays null).
            (news.is_movie, news.is_series, news.is_kids),
            (None, None, None)
        );
        // `IsRepeat = IsPreviouslyShown && !IsNew`.
        assert_eq!(news.is_repeat, Some(true));
        // `RunTimeTicks = (EndDate - StartDate).Ticks`: one hour.
        assert_eq!(news.run_time_ticks, Some(36_000_000_000));
        assert_eq!(news.index_number, None);

        // A movie: IsSeries cleared, and with it the episode number and title.
        let movie = by_name("Heat");
        assert_eq!(movie.is_movie, Some(true));
        assert_eq!(movie.is_series, None);
        assert_eq!(movie.index_number, None);
        assert_eq!(movie.episode_title, None);

        // An episode: IsSeries from the episode number, S1E6 from `0.5.`, and
        // <new/> wins over <previously-shown/>.
        let kids = by_name("Bluey");
        assert_eq!((kids.is_kids, kids.is_series), (Some(true), Some(true)));
        assert_eq!(
            (kids.parent_index_number, kids.index_number),
            (Some(1), Some(6))
        );
        assert_eq!(kids.episode_title.as_deref(), Some("Keepy Uppy"));
        assert_eq!(kids.is_repeat, None);

        // A sub-title alone makes a series (`GuideManager.GetProgram` widens
        // IsSeries by the episode title).
        let talk = by_name("Late Talk");
        assert_eq!(talk.is_series, Some(true));
        assert_eq!(talk.index_number, None);

        // The guide filters read the same columns.
        for (query, expected) in [
            (
                InternalItemsQuery {
                    is_news: Some(true),
                    ..Default::default()
                },
                "Morning Show",
            ),
            (
                InternalItemsQuery {
                    is_movie: Some(true),
                    ..Default::default()
                },
                "Heat",
            ),
            (
                InternalItemsQuery {
                    is_kids: Some(true),
                    ..Default::default()
                },
                "Bluey",
            ),
        ] {
            let hits = mgr
                .get_programs(&query, &DtoOptions::default())
                .await
                .expect("filtered");
            assert_eq!(hits.total_record_count, 1, "{expected}");
            assert_eq!(hits.items[0].name.as_deref(), Some(expected));
        }
        assert_eq!(
            mgr.get_programs(
                &InternalItemsQuery {
                    is_sports: Some(true),
                    ..Default::default()
                },
                &DtoOptions::default()
            )
            .await
            .expect("sports")
            .total_record_count,
            0
        );
        assert_eq!(
            mgr.get_programs(
                &InternalItemsQuery {
                    is_series: Some(true),
                    ..Default::default()
                },
                &DtoOptions::default()
            )
            .await
            .expect("series")
            .total_record_count,
            2
        );
    }

    /// Flags and ratings as Jellyfin 12 reads them (`Jellyfin.XmlTv`
    /// 10.12.0-pre1, `XmlTvListingsProvider.GetProgramInfo`): `<live>` and
    /// `<premiere>` count in either form, the rating comes from `<rating>`
    /// alone, and programme child names are matched case-insensitively.
    const FLAGGED_XMLTV: &str = "<tv>\
        <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
        <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
        <title>NFL Football</title><live/></programme>\
        <programme start=\"20260725070000 +0000\" stop=\"20260725090000 +0000\" channel=\"one.tv\">\
        <title>Cup Final</title><live>yes</live><premiere lang=\"en\">First showing</premiere>\
        <star-rating system=\"IMDb\"><value>7.2/10</value></star-rating></programme>\
        <programme start=\"20260725090000 +0000\" stop=\"20260725100000 +0000\" channel=\"one.tv\">\
        <Title>Shouted</Title><Rating system=\"VCHIP\"><Value>TV-14</Value></Rating></programme>\
        </tv>";

    #[tokio::test]
    async fn guide_refresh_reads_live_premiere_and_rating_like_jellyfin_12() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(FLAGGED_XMLTV));
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");

        let all = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        let by_name = |name: &str| {
            all.items
                .iter()
                .find(|p| p.name.as_deref() == Some(name))
                .cloned()
                .expect(name)
        };

        // `IsLive = program.IsLive`, set by a `<live>` child in either form.
        let football = by_name("NFL Football");
        assert_eq!(football.is_live, Some(true));
        assert_eq!(football.is_premiere, None);

        // `IsPremiere = program.Premiere is not null`: a `<premiere>` with text
        // counts as much as `<premiere/>`. A star rating is not an
        // OfficialRating (`Ratings.FirstOrDefault()?.Value`).
        let final_ = by_name("Cup Final");
        assert_eq!(
            (final_.is_live, final_.is_premiere),
            (Some(true), Some(true))
        );
        assert_eq!(final_.official_rating, None);

        // Programme children are lower-cased before matching.
        let shouted = by_name("Shouted");
        assert_eq!(shouted.official_rating.as_deref(), Some("TV-14"));
        assert_eq!(shouted.is_live, None);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one lineup's life: refresh, delete, refresh
    async fn refresh_populates_channels_and_guide() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(XMLTV));
        let mgr = manager_with(FakeFetcher(sources)).await;

        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");

        mgr.refresh_guide().await.expect("refresh");

        // Info reports enabled once a tuner is configured.
        assert!(mgr.get_live_tv_info().await.expect("info").is_enabled);

        let channels =
            channels_with_info(&mgr, &LiveTvChannelQuery::default(), &DtoOptions::default()).await;
        assert_eq!(channels.total_record_count, 2);
        assert_eq!(channels.items[0].name.as_deref(), Some("Channel One"));
        assert_eq!(channels.items[0].channel_number.as_deref(), Some("1"));

        let chan_id = channels.items[0].id;
        let stream = mgr
            .get_channel_stream_url(chan_id)
            .await
            .expect("stream")
            .expect("url");
        assert_eq!(stream, "http://tuner/one");

        // The guide programme binds to Channel One (tvg-id one.tv) only.
        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(programs.total_record_count, 1);
        assert_eq!(programs.items[0].name.as_deref(), Some("Morning Show"));
        assert_eq!(programs.items[0].channel_id, Some(chan_id));

        // Refresh is idempotent: stable ids mean the counts don't grow.
        mgr.refresh_guide().await.expect("refresh2");
        assert_eq!(
            mgr.get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
                .await
                .expect("c2")
                .total_record_count,
            2
        );
        assert_eq!(
            mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
                .await
                .expect("p2")
                .total_record_count,
            1
        );

        // Deleting the tuner host leaves its channels and their programmes
        // listed, as `TunerHostManager.DeleteTunerHost` only edits the
        // configuration; the refresh it queues removes them
        // (`CleanDatabase`).
        let host_id = mgr
            .get_tuner_hosts()
            .await
            .expect("h")
            .pop()
            .unwrap()
            .id
            .unwrap();
        mgr.delete_tuner_host(&host_id).await.expect("del host");
        let tuner_hosts = mgr.get_tuner_hosts().await.expect("h2");
        assert!(tuner_hosts.is_empty(), "{tuner_hosts:?}");
        assert_eq!(
            mgr.get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
                .await
                .expect("c3")
                .total_record_count,
            2,
            "listed until the refresh"
        );
        assert_eq!(
            mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
                .await
                .expect("p3")
                .total_record_count,
            1,
            "airings stay with their channel"
        );
        // Meanwhile the channel has no tuner to open: "Tuner not found.".
        let err = mgr
            .open_channel_stream(chan_id, None)
            .await
            .expect_err("no tuner to open");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        let err = mgr
            .get_channel_media_sources(chan_id)
            .await
            .expect_err("no tuner to ask");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        mgr.refresh_guide().await.expect("refresh3");
        assert_eq!(
            mgr.get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
                .await
                .expect("c4")
                .total_record_count,
            0,
            "the refresh removes a deleted tuner's channels"
        );
        assert_eq!(
            mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
                .await
                .expect("p4")
                .total_record_count,
            0
        );
    }

    /// A channel whose id contains an XML-escapable character must still bind
    /// its programmes. The M3U carries the raw `A&E.us` (M3U is not XML), the
    /// XMLTV carries `A&amp;E.us`; if the guide parser left the entity raw the
    /// `TvgId` join in `insert_programs` would find nothing and every programme
    /// on that channel would be silently dropped.
    #[tokio::test]
    async fn guide_binds_channels_whose_id_needs_xml_escaping() {
        const M3U_AMP: &str = "#EXTM3U\n\
            #EXTINF:-1 tvg-id=\"A&E.us\" tvg-chno=\"9\",A&E\nhttp://tuner/ae\n";
        const XMLTV_AMP: &str = "<tv>\
            <channel id=\"A&amp;E.us\"><display-name>A&amp;E</display-name></channel>\
            <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"A&amp;E.us\">\
            <title>Storage Wars</title></programme>\
            </tv>";

        let mut sources = HashMap::new();
        sources.insert("http://tuner/ae.m3u".to_owned(), M3U_AMP.to_owned());
        sources.insert("http://guide/ae.xml".to_owned(), dated(XMLTV_AMP));
        let mgr = manager_with(FakeFetcher(sources)).await;

        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/ae.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/ae.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");

        let channels = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("chans");
        assert_eq!(channels.total_record_count, 1);

        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(
            programs.total_record_count, 1,
            "the escaped guide id must join the tuner's raw tvg-id"
        );
        assert_eq!(programs.items[0].name.as_deref(), Some("Storage Wars"));
        assert_eq!(programs.items[0].channel_id, Some(channels.items[0].id));
    }

    #[tokio::test]
    async fn bulk_guide_sync_inserts_every_channel_and_program() {
        // 150 channels and 5000 programmes exceed a single insert chunk in both
        // paths, so this exercises the chunk boundaries and asserts no rows are
        // lost. It also prints the sync wall-time for before/after comparison.
        use std::fmt::Write as _;
        let mut m3u = String::from("#EXTM3U\n");
        for c in 0..150 {
            let _ = write!(
                m3u,
                "#EXTINF:-1 tvg-id=\"ch{c}.tv\" tvg-chno=\"{c}\",Channel {c}\nhttp://tuner/{c}\n"
            );
        }
        let mut xmltv = String::from("<tv>");
        // The guide must DECLARE its channels: `ListingsManager.GetProgramsAsync`
        // resolves a tuner channel to an EPG channel through
        // `GetEpgChannelFromTunerChannel` and gives up when none matches, so a
        // document carrying only <programme> elements yields no listings at all.
        for c in 0..150 {
            let _ = write!(
                xmltv,
                "<channel id=\"ch{c}.tv\"><display-name>Channel {c}</display-name></channel>"
            );
        }
        for p in 0..5000u32 {
            let ch = p % 150;
            let day = guide_day(i64::from(p / 24 / 60 % 8));
            let hh = p / 60 % 24;
            let mm = p % 60;
            let _ = write!(
                xmltv,
                "<programme start=\"{day}{hh:02}{mm:02}00 +0000\" \
                 stop=\"{day}{hh:02}{mm:02}30 +0000\" channel=\"ch{ch}.tv\">\
                 <title>Show {p}</title></programme>"
            );
        }
        xmltv.push_str("</tv>");

        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), m3u);
        sources.insert("http://guide/xmltv.xml".to_owned(), xmltv);
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");

        let started = std::time::Instant::now();
        mgr.refresh_guide().await.expect("refresh");
        eprintln!(
            "bulk guide sync (150 ch / 5000 prog): {:?}",
            started.elapsed()
        );

        let channels = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("chans");
        assert_eq!(channels.total_record_count, 150);
        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(programs.total_record_count, 5000);
    }

    /// Builds a two-channel guide whose airings sit at fixed offsets from *now*,
    /// so the airing/has-aired filters can be asserted without freezing the
    /// clock. Offsets are in minutes relative to the current instant.
    fn relative_guide() -> String {
        guide_of(&[
            // Already over, but inside the refresh's one-hour lead-in — an
            // airing that ended more than an hour ago is not in the guide
            // window at all, so upstream would not have it either and neither
            // does Ferrofin.
            ("one.tv", -55, -45, "Aired", "News"),
            ("one.tv", -30, 30, "Now Playing", "Drama"),
            ("one.tv", 120, 180, "Later", "Comedy"),
            ("two.tv", -10, 50, "Two Now", "News"),
        ])
    }

    /// A guide over the two [`M3U`] channels with `airings` as
    /// `(channel, from, to, title, genre)`, offsets in minutes from now.
    fn guide_of(airings: &[(&str, i64, i64, &str, &str)]) -> String {
        use std::fmt::Write as _;
        let now = chrono::Utc::now();
        let mut xml = String::from(
            "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
        );
        for &(channel, from, to, title, genre) in airings {
            let start = (now + chrono::Duration::minutes(from)).format("%Y%m%d%H%M%S");
            let stop = (now + chrono::Duration::minutes(to)).format("%Y%m%d%H%M%S");
            let _ = write!(
                xml,
                "<programme start=\"{start} +0000\" stop=\"{stop} +0000\" channel=\"{channel}\">\
                 <title>{title}</title><category>{genre}</category></programme>"
            );
        }
        xml.push_str("</tv>");
        xml
    }

    /// A manager whose guide holds [`relative_guide`] over the two [`M3U`]
    /// channels.
    async fn manager_with_relative_guide() -> FerrofinLiveTvManager {
        manager_with_guide(relative_guide()).await
    }

    /// A manager whose guide is `guide` over the two [`M3U`] channels.
    async fn manager_with_guide(guide: String) -> FerrofinLiveTvManager {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), guide);
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");
        mgr
    }

    /// The Live TV `UserView` the channel items must parent to. A fake because
    /// provisioning the row is `ferrofin-core`'s job (and its test's); what this
    /// test is about is that the guide refresh USES it.
    struct FixedLiveTvView(Uuid);

    #[async_trait::async_trait]
    impl ferrofin_traits::library::UserViewManager for FixedLiveTvView {
        async fn get_user_views(
            &self,
            _user_id: Uuid,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_media_folders(
            &self,
            _user_id: Uuid,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_latest_items(
            &self,
            _query: &ferrofin_traits::options::LatestItemsQuery,
            _options: &DtoOptions,
        ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_internal_live_tv_folder_id(&self) -> Result<Option<Uuid>, ServiceError> {
            Ok(Some(self.0))
        }
    }

    /// The stored `BaseItems` channel-item rows, id-ordered.
    async fn stored_channel_items(
        mgr: &FerrofinLiveTvManager,
    ) -> Vec<(String, Option<String>, Option<String>)> {
        crate::guide_repository::test_support::stored_channel_items(&mgr.db)
            .await
            .expect("channel items")
    }

    /// The wiring a channel-item test needs: a manager with an item store, a
    /// Live TV view row to parent to, and one M3U tuner in the lineup.
    struct ChannelItemLab {
        mgr: FerrofinLiveTvManager,
        persistence: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService>,
        items: Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        view: Uuid,
        tuner: TunerHostInfo,
        /// The playlist and guide bodies, swappable between refreshes.
        sources: Arc<SwappableFetcher>,
    }

    async fn channel_item_lab() -> ChannelItemLab {
        item_lab(M3U, None).await
    }

    /// [`channel_item_lab`] over the playlist `m3u`, plus a listings provider
    /// serving `xmltv` when there is one.
    async fn item_lab(m3u: &str, xmltv: Option<String>) -> ChannelItemLab {
        item_lab_reading_through(m3u, xmltv, |items| items).await
    }

    /// [`item_lab`], with the guide refresh reading items through the
    /// repository `wrap` returns over the real one; the lab's own `items`
    /// stays the real one.
    async fn item_lab_reading_through(
        m3u: &str,
        xmltv: Option<String>,
        wrap: impl FnOnce(
            Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        ) -> Arc<dyn ferrofin_traits::persistence::ItemRepository>,
    ) -> ChannelItemLab {
        let view = Uuid::from_u128(0x2b2b_ca16_aacc_8a14_d53a_11bb_829e_afa5);
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), m3u.to_owned());
        if let Some(xmltv) = &xmltv {
            sources.insert("http://guide/xmltv.xml".to_owned(), xmltv.clone());
        }
        let sources = Arc::new(SwappableFetcher(std::sync::Mutex::new(sources)));
        let mgr = manager_with_fetcher(Arc::clone(&sources) as Arc<dyn SourceFetcher>).await;
        let item_type_lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::ItemTypeLookup::new());
        let persistence: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService> = Arc::new(
            ferrofin_core::FerrofinItemPersistenceService::new(mgr.db.clone()),
        );
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(
            ferrofin_core::FerrofinItemRepository::new(mgr.db.clone(), item_type_lookup),
        );
        // The parent row has to exist: `BaseItems.ParentId` is a real foreign
        // key, which is exactly why upstream's `GetInternalLiveTvFolder()`
        // CREATES the view before a channel is stored under it.
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(view),
                type_: "MediaBrowser.Controller.Entities.UserView".to_owned(),
                name: Some("Live TV".to_owned()),
                is_folder: true,
                ..BaseItemEntity::default()
            }])
            .await
            .expect("live tv view");
        mgr.set_item_store(
            Arc::clone(&persistence),
            wrap(Arc::clone(&items)),
            Arc::new(FixedLiveTvView(view)),
        );
        let tuner = mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("tuner");
        if xmltv.is_some() {
            mgr.save_listing_provider(
                as_xmltv(ListingsProviderInfo {
                    path: Some("http://guide/xmltv.xml".to_owned()),
                    ..ListingsProviderInfo::default()
                }),
                false,
                false,
            )
            .await
            .expect("listings provider");
        }
        ChannelItemLab {
            mgr,
            persistence,
            items,
            view,
            tuner,
            sources,
        }
    }

    /// A guide refresh mirrors the lineup into `BaseItems` and removes the rows
    /// that left it.
    ///
    /// Port of `GuideManager.GetChannel`'s `CreateItem`/`UpdateItemAsync`
    /// (v10.11.8 GuideManager.cs:375-468) plus
    /// `CleanDatabase(newChannelIdList, [BaseItemKind.LiveTvChannel], …)`
    /// (:147). Without the first half `GET /Items/{channelId}` answers 404 for
    /// an id `GET /LiveTv/Channels` just handed the client; without the second,
    /// a removed tuner's channels haunt every recursive query for ever.
    #[tokio::test]
    async fn a_guide_refresh_stores_the_lineup_as_items_and_cleans_the_departed() {
        let lab = channel_item_lab().await;
        let (mgr, persistence) = (&lab.mgr, &lab.persistence);
        mgr.refresh_guide().await.expect("refresh");

        let lineup = channel_ids(mgr).await;
        assert_eq!(lineup.len(), 2);
        let stored = stored_channel_items(mgr).await;
        assert_eq!(
            stored.len(),
            2,
            "every channel in the lineup is an item row"
        );
        let parent = ferrofin_db::store::guid_to_db(lab.view);
        for (id, parent_id, top_parent_id) in &stored {
            assert!(
                lineup.contains(&Uuid::parse_str(id).expect("guid")),
                "a stored channel item is one of the lineup's channels"
            );
            // `item.ParentId = parentFolderId`, and the same id is the row's
            // TopParentId — which is what puts a channel in the recursive user
            // universe.
            assert_eq!(parent_id.as_deref(), Some(parent.as_str()));
            assert_eq!(top_parent_id.as_deref(), Some(parent.as_str()));
        }

        // A channel item whose channel is no longer in any tuner's lineup goes
        // on the next refresh — and only that one.
        let ghost = Uuid::new_v4();
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(ghost),
                type_: crate::projection::CHANNEL_TYPE_NAME.to_owned(),
                name: Some("Departed".to_owned()),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("ghost");
        assert_eq!(stored_channel_items(mgr).await.len(), 3);

        mgr.refresh_guide().await.expect("second refresh");
        let after = stored_channel_items(mgr).await;
        assert_eq!(after.len(), 2, "the departed channel item was cleaned");
        assert!(
            !after
                .iter()
                .any(|(id, ..)| Uuid::parse_str(id).ok() == Some(ghost))
        );
    }

    /// `TunerHostManager.DeleteTunerHost` queues a guide refresh, and that
    /// refresh's `CleanDatabase` passes remove the host's channel and programme
    /// items, the last host's included: the refresh has no early return for an
    /// empty tuner list. The programme items are what this adds to
    /// `an_empty_lineup_cleans_every_channel_item`: they belong to a listings
    /// provider that is still configured.
    #[tokio::test]
    async fn a_refresh_after_the_last_tuner_is_deleted_removes_its_items() {
        let lab = item_lab(M3U, Some(dated(XMLTV))).await;
        let mgr = &lab.mgr;
        mgr.refresh_guide().await.expect("refresh");
        assert_eq!(stored_channel_items(mgr).await.len(), 2);
        let found = stored_program_items(mgr).await;
        assert!(!found.is_empty(), "{found:?}");

        mgr.delete_tuner_host(lab.tuner.id.as_deref().expect("tuner id"))
            .await
            .expect("delete");
        // The delete leaves the items to the refresh, as upstream's does.
        assert_eq!(stored_channel_items(mgr).await.len(), 2);
        let found = stored_program_items(mgr).await;
        assert!(!found.is_empty(), "{found:?}");
        mgr.refresh_guide().await.expect("refresh after the delete");
        let found = stored_channel_items(mgr).await;
        assert!(found.is_empty(), "{found:?}");
        let found = stored_program_items(mgr).await;
        assert!(found.is_empty(), "{found:?}");
    }

    /// A channel that left the lineup and comes back gets its users' data
    /// back. `CleanDatabase` deleted its item, which parked the rows on the
    /// deletion placeholder. Upstream's first refresh of the recreated item
    /// re-attaches them (`MetadataService.SaveItemAsync` →
    /// `ReattachUserDataAsync`), and so does creating the item here.
    #[tokio::test]
    async fn a_channel_that_returns_to_the_lineup_gets_its_user_data_back() {
        use crate::guide_repository::test_support::{favorite_holder, seed_favorite};
        let lab = channel_item_lab().await;
        let (mgr, persistence) = (&lab.mgr, &lab.persistence);
        mgr.refresh_guide().await.expect("refresh");
        let users: Arc<dyn ferrofin_traits::library::UserManager> = Arc::new(
            ferrofin_core::user_manager::FerrofinUserManager::new(mgr.db.clone()),
        );
        let user = users.create_user("couch").await.expect("user");
        let channel = stored_channel_items(mgr).await[0].0.clone();
        let key = channel.to_lowercase();
        seed_favorite(&mgr.db, &channel, &user.id)
            .await
            .expect("favourite");

        // The channel leaves the lineup: its item goes, the favourite is parked.
        persistence
            .delete_items(&[Uuid::parse_str(&channel).expect("guid")])
            .await
            .expect("delete");
        let parked = favorite_holder(&mgr.db, &user.id, &key)
            .await
            .expect("read");
        assert_ne!(parked.as_deref(), Some(channel.as_str()), "parked");

        // It comes back: the recreated item takes the favourite again.
        mgr.refresh_guide().await.expect("refresh again");
        assert_eq!(
            favorite_holder(&mgr.db, &user.id, &key)
                .await
                .expect("read"),
            Some(channel)
        );
    }

    /// A guide refresh assigns only the properties it owns, onto the item it
    /// LOADED — everything else on a channel survives it.
    ///
    /// `GuideManager.GetChannel` (v10.11.8 GuideManager.cs:375-468) reads the
    /// item back with `GetItemById`, assigns Tags/ParentId/ChannelType/
    /// ServiceName/ExternalId/Number/Name onto it and persists with
    /// `UpdateItemAsync`. A channel item is a REAL item now, so
    /// `POST /Items/{channelId}` can write to it — and Ferrofin's `save_items`
    /// is a full-column upsert, so mirroring a freshly built entity instead
    /// would revert every edit at the next `RefreshGuide`, silently, on a 24h
    /// timer.
    #[tokio::test]
    async fn a_guide_refresh_does_not_revert_an_edited_channel() {
        let lab = channel_item_lab().await;
        let (mgr, persistence, items) = (&lab.mgr, &lab.persistence, &lab.items);
        mgr.refresh_guide().await.expect("refresh");
        let edited = Uuid::parse_str(&stored_channel_items(mgr).await[0].0).expect("guid");

        let mut row = channel_item(items, edited).await;
        row.overview = Some("Written by the metadata editor".to_owned());
        row.custom_rating = Some("PG".to_owned());
        row.is_locked = true;
        persistence.save_items(&[row]).await.expect("editor write");

        mgr.refresh_guide().await.expect("second refresh");
        let kept = channel_item(items, edited).await;
        assert_eq!(
            kept.overview.as_deref(),
            Some("Written by the metadata editor"),
            "a guide refresh must not revert an edited channel"
        );
        assert_eq!(kept.custom_rating.as_deref(), Some("PG"));
        assert!(kept.is_locked);
        // …and the fields the refresh DOES own are still the lineup's.
        assert_eq!(
            kept.parent_id.as_deref(),
            Some(ferrofin_db::store::guid_to_db(lab.view).as_str())
        );
        assert_eq!(
            kept.type_,
            crate::projection::CHANNEL_TYPE_NAME,
            "the refresh still owns the item's type"
        );
    }

    /// `CleanDatabase` runs on an empty-but-successful lineup too.
    ///
    /// The upstream guard is the `cleanDatabase` flag, which ONLY a thrown
    /// `RefreshChannelsInternal` clears (v10.11.8 GuideManager.cs:130-134,
    /// :145-148) — never an empty lineup. Removing the last tuner leaves no
    /// lineup, so the next refresh sees an empty one and must take its channel
    /// rows and items with it rather than leave them answering
    /// `GET /Items/{id}` for ever. Ferrofin has no empty-because-failed case to
    /// confuse this with: a failed lineup fetch never reaches
    /// `replace_channels`, so the cached channel rows survive and the lineup
    /// read back is NOT empty.
    #[tokio::test]
    async fn an_empty_lineup_cleans_every_channel_item() {
        let lab = channel_item_lab().await;
        let mgr = &lab.mgr;
        mgr.refresh_guide().await.expect("refresh");
        assert_eq!(stored_channel_items(mgr).await.len(), 2);

        mgr.delete_tuner_host(lab.tuner.id.as_deref().expect("tuner id"))
            .await
            .expect("delete tuner");
        mgr.refresh_guide().await.expect("second refresh");
        assert!(
            stored_channel_items(mgr).await.is_empty(),
            "an empty lineup cleans every channel item"
        );
    }

    /// The stored `BaseItems` programme rows, id-ordered.
    async fn stored_program_items(
        mgr: &FerrofinLiveTvManager,
    ) -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
        crate::guide_repository::test_support::stored_program_items(&mgr.db)
            .await
            .expect("programme items")
    }

    /// A programme's `ShowId` is the listings provider's, byte for byte.
    ///
    /// The expected value is not this test's invention: it is the `ShowId`
    /// column a real Jellyfin 10.11.8 wrote for the airing named
    /// "Parity Show 16 on parity1" in the parity lab's database. `GetMD5`
    /// hashes UTF-16LE bytes and wraps them in a .NET `Guid`, whose first three
    /// fields print little-endian — get either half wrong and a DVR series rule
    /// groups different showings than Jellyfin's would.
    #[test]
    fn a_show_id_matches_the_listings_provider_hash() {
        assert_eq!(
            derive_show_id(
                "Parity Show 16 on parity1",
                None,
                None,
                None,
                false,
                false,
                None
            ),
            "b99e02d458f3a83412cf9e878f0464aa"
        );
        // The episode branch REPLACES the accumulated string rather than
        // appending to it — upstream's code as written
        // (XmlTvListingsProvider.cs:186-213).
        assert_eq!(
            derive_show_id(
                "Ignored",
                Some("Also ignored"),
                Some(1),
                Some(6),
                true,
                false,
                None
            ),
            ferrofin_common::extensions::get_md5("-6")
                .simple()
                .to_string()
        );
    }

    /// A guide refresh mirrors the ingested airings into `BaseItems` and
    /// removes the rows that left the guide.
    ///
    /// Port of `GuideManager`'s `CreateItems(newPrograms, currentChannel, …)` /
    /// `UpdateItemsAsync(updatedPrograms, …)` (v10.11.8 GuideManager.cs:277-291)
    /// and the `CleanDatabase(newProgramIdList, [BaseItemKind.LiveTvProgram], …)`
    /// one line below the channel one (:147). Without it
    /// `GET /Items?recursive=true&includeItemTypes=LiveTvProgram` is empty and
    /// `GET /Items/{programId}` 404s for an id `GET /LiveTv/Programs` just
    /// handed the client.
    #[tokio::test]
    async fn a_guide_refresh_stores_its_airings_as_items_and_cleans_the_departed() {
        let lab = program_item_lab().await;
        let (mgr, persistence) = (&lab.mgr, &lab.persistence);
        mgr.refresh_guide().await.expect("refresh");

        let stored = stored_program_items(mgr).await;
        assert_eq!(stored.len(), 1, "the guide's one airing is an item");
        let channels = channel_ids(mgr).await;
        let view = ferrofin_db::store::guid_to_db(lab.view);
        let (id, parent_id, top_parent_id, presentation_key) = &stored[0];
        // `CreateItems(newPrograms, currentChannel, …)` — the CHANNEL is the
        // parent; `TopParentId` is the walk up to the Live TV view, and it is
        // what puts the airing inside the recursive user universe.
        assert!(
            channels
                .contains(&Uuid::parse_str(parent_id.as_deref().expect("parent")).expect("guid"))
        );
        assert_eq!(top_parent_id.as_deref(), Some(view.as_str()));
        // NULL, because the guide refresh calls `RefreshMetadata` on a CHANNEL
        // only (GuideManager.cs:305) and only `MetadataService` populates this
        // column. SQLite groups NULLs together, so the whole guide collapses to
        // one row on an unfiltered recursive page — which is what a real
        // 10.11.8 does.
        assert_eq!(presentation_key.as_deref(), None);

        // An airing the guide no longer carries goes on the next refresh — and
        // only that one.
        let ghost = Uuid::new_v4();
        persistence
            .save_items(&[BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(ghost),
                type_: crate::projection::PROGRAM_TYPE_NAME.to_owned(),
                name: Some("Departed".to_owned()),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("ghost");
        assert_eq!(stored_program_items(mgr).await.len(), 2);

        mgr.refresh_guide().await.expect("second refresh");
        let after = stored_program_items(mgr).await;
        assert_eq!(after.len(), 1, "the departed airing was cleaned");
        assert_eq!(&after[0].0, id);
    }

    /// A guide refresh assigns only the properties the listing owns, onto the
    /// item it LOADED — everything else on a programme survives it.
    ///
    /// The programme half of `a_guide_refresh_does_not_revert_an_edited_channel`,
    /// and for the same reason: `GuideManager.GetProgram` (GuideManager.cs:473-640)
    /// assigns onto the item pulled out of `existingPrograms`, while Ferrofin's
    /// `save_items` is a full-column upsert that would revert every edit on the
    /// guide timer.
    #[tokio::test]
    async fn a_guide_refresh_does_not_revert_an_edited_programme() {
        let lab = program_item_lab().await;
        let (mgr, persistence, items) = (&lab.mgr, &lab.persistence, &lab.items);
        mgr.refresh_guide().await.expect("refresh");
        let edited = Uuid::parse_str(&stored_program_items(mgr).await[0].0).expect("guid");

        let mut row = channel_item(items, edited).await;
        row.custom_rating = Some("PG".to_owned());
        row.is_locked = true;
        persistence.save_items(&[row]).await.expect("editor write");

        mgr.refresh_guide().await.expect("second refresh");
        let kept = channel_item(items, edited).await;
        assert_eq!(kept.custom_rating.as_deref(), Some("PG"));
        assert!(kept.is_locked);
        // …and the fields the listing DOES own are still the guide's.
        assert_eq!(kept.name.as_deref(), Some("Morning Show"));
        assert_eq!(kept.tags.as_deref(), Some("News"));
        assert_eq!(kept.genres.as_deref(), Some("News"));
        assert!(kept.is_virtual_item);
        assert_eq!(kept.unrated_type.as_deref(), Some("LiveTvProgram"));
    }

    /// Each airing's genres become its `ItemValues` links, written in the
    /// batched form, and an airing whose stored list differs has its links
    /// replaced by the listing's.
    #[tokio::test]
    async fn a_guide_refresh_links_each_airings_genres_and_replaces_a_changed_list() {
        let lab = program_item_lab().await;
        let (mgr, persistence, items) = (&lab.mgr, &lab.persistence, &lab.items);
        mgr.refresh_guide().await.expect("refresh");
        let links = crate::guide_repository::test_support::program_genre_links(&mgr.db)
            .await
            .expect("links");
        let id = stored_program_items(mgr).await[0].0.clone();
        assert_eq!(links, [(id.clone(), "News".to_owned())]);

        // A stored list that no longer matches the listing, with its links.
        let airing = Uuid::parse_str(&id).expect("guid");
        let mut row = channel_item(items, airing).await;
        row.genres = Some("Drama".to_owned());
        persistence.save_items(&[row]).await.expect("stale genres");
        persistence
            .save_item_values(
                airing,
                &[(
                    i32::from(ferrofin_db::enums::ItemValueType::Genre),
                    "Drama".to_owned(),
                )],
            )
            .await
            .expect("stale links");

        mgr.refresh_guide().await.expect("second refresh");
        let links = crate::guide_repository::test_support::program_genre_links(&mgr.db)
            .await
            .expect("links");
        assert_eq!(links, [(id, "News".to_owned())]);
    }

    /// The airings are read once the channels are bound, so a guide that lists
    /// its `<channel>` elements after the programmes (XMLTV allows either
    /// order) still binds, and only the mapped channels' airings inside the
    /// window are stored: an unmapped channel's and a far-future one's are
    /// dropped as they are read.
    #[tokio::test]
    async fn a_guide_whose_channels_follow_its_airings_binds_only_the_mapped_window() {
        use std::fmt::Write as _;
        let now = chrono::Utc::now();
        let mut xml = String::from("<tv>");
        for (channel, from, to, title) in [
            ("one.tv", -30, 30, "One Now"),
            ("two.tv", 60, 120, "Two Later"),
            ("three.tv", -30, 30, "Unmapped"),
            ("one.tv", 60 * 24 * 30, 60 * 24 * 30 + 60, "Next Month"),
        ] {
            let start = (now + chrono::Duration::minutes(from)).format("%Y%m%d%H%M%S");
            let stop = (now + chrono::Duration::minutes(to)).format("%Y%m%d%H%M%S");
            let _ = write!(
                xml,
                "<programme start=\"{start} +0000\" stop=\"{stop} +0000\" channel=\"{channel}\">\
                 <title>{title}</title></programme>"
            );
        }
        xml.push_str(
            "<channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>\
             <channel id=\"three.tv\"><display-name>Channel Three</display-name></channel>\
             </tv>",
        );
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), xml);
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");

        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("programs");
        let mut names: Vec<&str> = programs
            .items
            .iter()
            .filter_map(|p| p.name.as_deref())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["One Now", "Two Later"]);
    }

    /// The wiring a programme-item test needs: the channel lab plus a listings
    /// provider whose XMLTV covers the lineup.
    async fn program_item_lab() -> ChannelItemLab {
        item_lab(M3U, Some(dated(XMLTV))).await
    }

    /// An item's image rows, as `(type, path)`.
    async fn item_images(
        items: &Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        id: Uuid,
    ) -> Vec<(ferrofin_model::entities::ImageType, String)> {
        items
            .get_image_infos(id)
            .await
            .expect("images")
            .into_iter()
            .map(|image| (image.image_type, image.path))
            .collect()
    }

    /// The image services for the artwork tests: a remote-image cache that
    /// records what it was asked to fetch, and a processor whose tag is the
    /// image's path.
    #[derive(Default)]
    struct ArtServices {
        cached: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ferrofin_traits::providers::ProviderManager for ArtServices {
        async fn queue_refresh(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
            _priority: ferrofin_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_single_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
            Ok(ferrofin_traits::providers::ItemUpdateType::None)
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn cached_remote_image(&self, path: &str) -> Result<String, ServiceError> {
            self.cached.lock().expect("cache log").push(path.to_owned());
            Ok(format!("/cache/{}", path.len()))
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &ferrofin_model::providers::RemoteImageQuery,
        ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: ferrofin_traits::providers::ItemUpdateType,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError>
        {
            Ok(Vec::new())
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
            Ok(ferrofin_model::configuration::MetadataOptions::default())
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl ferrofin_traits::drawing::ImageProcessor for ArtServices {
        fn supported_input_formats(&self) -> Vec<String> {
            Vec::new()
        }
        fn supports_image_collage_creation(&self) -> bool {
            false
        }
        fn supported_image_output_formats(&self) -> Vec<ferrofin_model::drawing::ImageFormat> {
            Vec::new()
        }
        async fn get_image_dimensions(
            &self,
            _path: &str,
        ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
            Err(ServiceError::backend("unused"))
        }
        async fn get_item_image_dimensions(
            &self,
            _item_id: Uuid,
            _info: &ferrofin_traits::options::ItemImageInfo,
        ) -> Result<ferrofin_model::drawing::ImageDimensions, ServiceError> {
            Err(ServiceError::backend("unused"))
        }
        async fn get_image_blur_hash(&self, _path: &str) -> Result<String, ServiceError> {
            Err(ServiceError::backend("unused"))
        }
        async fn get_image_blur_hash_sized(
            &self,
            _path: &str,
            _image_dimensions: ferrofin_model::drawing::ImageDimensions,
        ) -> Result<String, ServiceError> {
            Err(ServiceError::backend("unused"))
        }
        async fn get_image_cache_tag(
            &self,
            item_path: Option<&str>,
            image: &ferrofin_traits::options::ItemImageInfo,
        ) -> Result<Option<String>, ServiceError> {
            Ok(Some(match item_path {
                Some(owner) => format!("tag:{}@{owner}", image.path),
                None => format!("tag:{}", image.path),
            }))
        }
        async fn get_image_cache_tag_for_path(
            &self,
            _base_item_path: &str,
            _image_date_modified: chrono::DateTime<Utc>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn process_image(
            &self,
            _options: &ferrofin_traits::options::ImageProcessingOptions,
        ) -> Result<ferrofin_traits::drawing::ProcessedImage, ServiceError> {
            Err(ServiceError::backend("unused"))
        }
        async fn create_image_collage(
            &self,
            _options: &ferrofin_traits::options::ImageCollageOptions,
            _library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A guide refresh gives channels and programmes the artwork the tuner and
    /// the guide supply, as remote image rows.
    ///
    /// Port of `ListingsManager.AddMetadata` (a matched channel takes the
    /// guide's `<icon>` over the playlist's `tvg-logo`), `GuideManager.GetChannel`
    /// (the channel's Primary image) and `GuideManager.UpdateImages` (a
    /// programme's `<icon>`, `<image type="still">` and `<image
    /// type="backdrop">` as its Primary, Thumb and Backdrop), v12.2.
    #[tokio::test]
    async fn a_guide_refresh_attaches_the_tuner_and_guide_artwork() {
        use ferrofin_model::entities::ImageType;
        const ART_M3U: &str = "#EXTM3U\n\
            #EXTINF:-1 tvg-id=\"one.tv\" tvg-logo=\"http://img/one.png\",Channel One\nhttp://tuner/one\n\
            #EXTINF:-1 tvg-id=\"two.tv\" tvg-logo=\"http://img/two.png\",Channel Two\nhttp://tuner/two\n";
        const ART_XMLTV: &str = "<tv>\
            <channel id=\"one.tv\"><display-name>Channel One</display-name><icon src=\"http://img/one-guide.png\"/></channel>\
            <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>\
            <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
            <title>Morning Show</title><icon src=\"http://img/show.png\"/>\
            <image type=\"still\">http://img/still.jpg</image><image type=\"backdrop\">http://img/back.jpg</image>\
            </programme>\
            <programme start=\"20260728060000 +0000\" stop=\"20260728070000 +0000\" channel=\"one.tv\">\
            <title>Later Show</title><icon src=\"http://img/later.png\"/></programme></tv>";
        // The second airing is three days out: in the guide, past `MaxCacheDays`.
        let xmltv = dated(ART_XMLTV).replace("20260728", &guide_day(3));
        let lab = item_lab(ART_M3U, Some(xmltv)).await;
        let art = Arc::new(ArtServices::default());
        lab.mgr.set_images(art.clone(), art.clone());
        lab.mgr.refresh_guide().await.expect("refresh");

        let channels = channel_ids(&lab.mgr).await;
        assert_eq!(
            item_images(&lab.items, channels[0]).await,
            [(ImageType::Primary, "http://img/one-guide.png".to_owned())],
            "a matched channel takes the guide's logo"
        );
        assert_eq!(
            item_images(&lab.items, channels[1]).await,
            [(ImageType::Primary, "http://img/two.png".to_owned())],
            "a guide channel without a logo leaves the playlist's"
        );
        let programs = lab
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![ferrofin_model::data::BaseItemKind::LiveTvProgram],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("programmes");
        assert_eq!(programs.len(), 2);
        let program = programs
            .iter()
            .find(|p| p.name.as_deref() == Some("Morning Show"))
            .and_then(|p| Uuid::parse_str(&p.id).ok())
            .expect("the morning show");
        // Listed by type: Primary, Backdrop, then Thumb.
        assert_eq!(
            item_images(&lab.items, program).await,
            [
                (ImageType::Primary, "http://img/show.png".to_owned()),
                (ImageType::Backdrop, "http://img/back.jpg".to_owned()),
                (ImageType::Thumb, "http://img/still.jpg".to_owned()),
            ]
        );

        // The morning show ends tomorrow, inside `MaxCacheDays`, so its three
        // images were pre-cached; the later show's waits for its first request,
        // as do the channel logos.
        let mut cached = art.cached.lock().expect("cache log").clone();
        cached.sort();
        assert_eq!(
            cached,
            [
                "http://img/back.jpg",
                "http://img/show.png",
                "http://img/still.jpg"
            ]
        );
        art.cached.lock().expect("cache log").clear();

        // A programme DTO asked for its channel's image carries the logo's tag.
        let dtos = lab
            .mgr
            .program_dtos(
                &crate::guide_repository::test_support::program_rows(&lab.mgr.db)
                    .await
                    .expect("rows"),
                &DtoOptions {
                    fields: vec![ItemFields::ChannelImage],
                    ..DtoOptions::default()
                },
                None,
            )
            .await
            .expect("programme dtos");
        assert_eq!(
            dtos[0].channel_primary_image_tag.as_deref(),
            Some("tag:http://img/one-guide.png")
        );

        // An unchanged guide leaves every image row, and the cache, alone.
        let before = lab.items.get_image_infos(program).await.expect("images");
        lab.mgr.refresh_guide().await.expect("second refresh");
        assert_eq!(
            lab.items.get_image_infos(program).await.expect("images"),
            before
        );
        assert!(art.cached.lock().expect("cache log").is_empty());
    }

    /// `ListingsManager.AddProviderMetadata`: only a provider enabled for the
    /// channel's tuner lends its logo, a blank one lends nothing, and a later
    /// provider's wins.
    #[test]
    fn a_tuner_channel_takes_the_logo_of_each_enabled_provider_in_turn() {
        use super::GuideSource;
        use crate::mapping::EpgChannel;
        use crate::xmltv::XmltvChannel;
        let guide_channel = |icon: Option<&str>| XmltvChannel {
            id: "one.tv".to_owned(),
            display_name: "Channel One".to_owned(),
            icon: icon.map(str::to_owned),
            ..XmltvChannel::default()
        };
        let provider = |all: bool, tuners: &[&str]| ListingsProviderInfo {
            enable_all_tuners: all,
            enabled_tuners: tuners.iter().map(|t| (*t).to_owned()).collect(),
            ..ListingsProviderInfo::default()
        };
        let mut lineups = vec![(
            "tuner-a".to_owned(),
            String::new(),
            vec![crate::tuner_host::TunerChannel {
                external_id: "m3u_1".to_owned(),
                guide_key: "one.tv".to_owned(),
                name: "Channel One".to_owned(),
                image_url: Some("http://img/playlist.png".to_owned()),
                ..crate::tuner_host::TunerChannel::default()
            }],
        )];
        let logo = |lineups: &[(String, String, Vec<crate::tuner_host::TunerChannel>)]| {
            lineups[0].2[0].image_url.clone()
        };
        let xmltv = |channel: XmltvChannel| GuideSource::XmlTv {
            document: crate::xmltv_cache::XmlTvDocument::empty_for_test(),
            channels: vec![channel],
        };
        // A provider for another tuner, and one whose channel has no logo.
        super::add_provider_metadata(
            &mut lineups,
            &[
                (
                    provider(false, &["tuner-b"]),
                    xmltv(guide_channel(Some("http://img/b.png"))),
                ),
                (provider(true, &[]), xmltv(guide_channel(Some("  ")))),
            ],
        );
        assert_eq!(logo(&lineups).as_deref(), Some("http://img/playlist.png"));
        // Two enabled providers: the later one's logo wins.
        super::add_provider_metadata(
            &mut lineups,
            &[
                (
                    provider(false, &["tuner-a"]),
                    xmltv(guide_channel(Some("http://img/first.png"))),
                ),
                (
                    provider(true, &[]),
                    xmltv(guide_channel(Some("http://img/second.png"))),
                ),
            ],
        );
        assert_eq!(logo(&lineups).as_deref(), Some("http://img/second.png"));
        // A Schedules Direct station's logo (`SchedulesDirect.GetChannels`'s
        // `ImageUrl = station.Logo.Url`) is lent the same way.
        super::add_provider_metadata(
            &mut lineups,
            &[(
                provider(true, &[]),
                GuideSource::SchedulesDirect {
                    channels: vec![EpgChannel {
                        id: "one.tv".to_owned(),
                        name: "Channel One".to_owned(),
                        number: "one.tv".to_owned(),
                        image_url: Some("http://img/station.png".to_owned()),
                    }],
                    programs: HashMap::new(),
                },
            )],
        );
        assert_eq!(logo(&lineups).as_deref(), Some("http://img/station.png"));
    }

    /// One stored channel item, whole.
    async fn channel_item(
        items: &Arc<dyn ferrofin_traits::persistence::ItemRepository>,
        id: Uuid,
    ) -> BaseItemEntity {
        items
            .get_item_list(&InternalItemsQuery {
                item_ids: vec![id],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("item")
            .pop()
            .expect("the channel item")
    }

    /// The ids of the channels in the lineup, in order.
    async fn channel_ids(mgr: &FerrofinLiveTvManager) -> Vec<Uuid> {
        crate::guide_repository::test_support::channel_ids(&mgr.db)
            .await
            .expect("channels")
            .iter()
            .map(|id| Uuid::parse_str(id).expect("guid"))
            .collect()
    }

    /// The id of the first channel in the lineup, by `SortIndex`.
    async fn first_channel_id(mgr: &FerrofinLiveTvManager) -> Uuid {
        channel_ids(mgr).await.remove(0)
    }

    /// The `{uniqueId}` segment of a `LiveStreamFiles` path.
    fn unique_id_of(path: &str) -> String {
        path.split("/LiveTv/LiveStreamFiles/")
            .nth(1)
            .expect("live stream path")
            .split('/')
            .next()
            .expect("unique id")
            .to_owned()
    }

    /// A manager over the relative guide whose tuner is an in-memory endless
    /// MPEG-TS broadcast, buffering into `transcode_dir`.
    async fn manager_with_tuner(
        transcode_dir: &std::path::Path,
        opens: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> FerrofinLiveTvManager {
        let tuner = crate::stream::tests::LoopingTuner {
            chunk: vec![0x47; 188],
            opens: Arc::clone(opens),
            // The fixture M3U's URLs carry no extension, so the share decision
            // falls through to the HEAD probe — as it does for a real IPTV
            // tuner that serves `/live` rather than `/live.ts`.
            content_type: Some("video/MP2T".to_owned()),
        };
        let mgr = manager_with_relative_guide()
            .await
            .with_tuner_source(Arc::new(tuner))
            .with_paths(LiveTvPaths {
                transcode_dir: transcode_dir.to_path_buf().into(),
                ..LiveTvPaths::default()
            });
        mgr.set_local_api_access(Arc::new(LoopbackApi));
        mgr
    }

    /// `LiveTvChannel.MediaType` — what the media-source manager's per-user
    /// `SupportsTranscoding`/`SupportsDirectStream` overwrite branches on
    /// (`MediaSourceManager.GetPlaybackMediaSources`, v10.11.8
    /// MediaSourceManager.cs:204-217). A channel is not a `BaseItems` row here,
    /// so the Live TV manager is the only thing that can answer.
    #[tokio::test]
    async fn a_channel_reports_the_media_type_of_its_kind() {
        let mgr = manager_with_relative_guide().await;
        let channel = first_channel_id(&mgr).await;
        assert_eq!(
            mgr.live_tv_media_type(channel).await.expect("type"),
            Some("Video".to_owned()),
            "a television channel is MediaType.Video"
        );
        // The radio arm: a `radio="true"` playlist entry, through the same M3U
        // host and the same refresh, so `ChannelType == Radio ? Audio : Video`
        // is exercised end to end rather than by hand-editing the stored row.
        let mut sources = HashMap::new();
        sources.insert(
            "http://tuner/radio.m3u".to_owned(),
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"jazz.fm\" radio=\"true\",Jazz FM\nhttp://tuner/jazz\n"
                .to_owned(),
        );
        let radio_mgr = manager_with(FakeFetcher(sources)).await;
        radio_mgr
            .save_tuner_host(TunerHostInfo {
                type_: Some("m3u".to_owned()),
                url: Some("http://tuner/radio.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("tuner");
        radio_mgr.refresh_guide().await.expect("refresh");
        let radio = first_channel_id(&radio_mgr).await;
        assert_eq!(
            radio_mgr.live_tv_media_type(radio).await.expect("type"),
            Some("Audio".to_owned()),
            "a radio channel is MediaType.Audio"
        );
        // An id that is neither a channel nor a recording has no media type,
        // and the caller must then leave the source as it was built.
        assert_eq!(
            mgr.live_tv_media_type(Uuid::from_u128(0xfeed))
                .await
                .expect("type"),
            None
        );
    }

    #[tokio::test]
    async fn a_channel_media_source_is_unopened_and_carries_its_open_token() {
        let mgr = manager_with_relative_guide().await;
        let channel = first_channel_id(&mgr).await;
        let sources = mgr
            .get_channel_media_sources(channel)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        // `CreateMediaSourceInfo`: the raw tuner URL, opened on demand.
        assert_eq!(source.path.as_deref(), Some("http://tuner/one"));
        assert!(source.requires_opening && source.requires_closing);
        assert!(source.is_infinite_stream);
        assert_eq!(source.buffer_ms, Some(DEFAULT_LIVE_STREAM_BUFFER_MS));
        assert_eq!(
            source.id.as_deref(),
            Some(
                ferrofin_common::extensions::get_md5("http://tuner/one")
                    .simple()
                    .to_string()
                    .as_str()
            ),
            "the source id is the MD5 of the tuner path, as upstream derives it"
        );
        // Two placeholder streams whose real indexes nothing knows yet.
        assert_eq!(source.media_streams.len(), 2);
        assert!(source.media_streams.iter().all(|s| s.index == -1));
        assert_eq!(
            source
                .required_http_headers
                .get("User-Agent")
                .map(String::as_str),
            Some(crate::stream::DEFAULT_TUNER_USER_AGENT)
        );
        assert_eq!(
            source.open_token.as_deref(),
            Some(
                format!(
                    "LiveTvChannel_{}_{}",
                    channel.simple(),
                    source.id.clone().unwrap_or_default()
                )
                .as_str()
            )
        );
        // An unknown id is not a channel.
        let channel_media_sources = mgr
            .get_channel_media_sources(Uuid::from_u128(0xdead))
            .await
            .expect("sources");
        assert!(
            channel_media_sources.is_empty(),
            "{channel_media_sources:?}"
        );
    }

    #[tokio::test]
    async fn opening_a_channel_shares_one_tuner_connection_until_the_last_consumer_closes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager_with_tuner(dir.path(), &opens).await;
        let channel = first_channel_id(&mgr).await;
        let source_id = mgr
            .get_channel_media_sources(channel)
            .await
            .expect("sources")[0]
            .id
            .clone();

        let opened = mgr
            .open_channel_stream(channel, source_id.as_deref())
            .await
            .expect("open");
        let path = opened.path.clone().expect("path");
        assert!(
            path.starts_with("http://127.0.0.1:8096/LiveTv/LiveStreamFiles/")
                && path.ends_with("/stream.ts"),
            "the opened source must play the buffered copy: {path}"
        );
        assert_eq!(opened.container.as_deref(), Some("ts"));
        assert!(opened.requires_closing);
        let live_id = opened.live_stream_id.clone().expect("live stream id");
        assert_eq!(
            live_id,
            format!(
                "{}_{}",
                live_tv_service_key(),
                source_id.clone().unwrap_or_default()
            )
        );

        // The buffer exists and is being written.
        let unique_id = unique_id_of(&path);
        let file = mgr
            .get_live_stream_file(&unique_id)
            .await
            .expect("lookup")
            .expect("open stream");
        assert!(file.path.exists());

        // A second open of the same source JOINS the stream: one tuner, two
        // consumers (C# `ConsumerCount++`).
        let joined = mgr
            .open_channel_stream(channel, source_id.as_deref())
            .await
            .expect("join");
        assert_eq!(joined.live_stream_id.as_deref(), Some(live_id.as_str()));
        assert_eq!(joined.path.as_deref(), Some(path.as_str()));
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 1);

        // The first close only decrements; the stream stays up for the other.
        mgr.close_channel_stream(&live_id).await.expect("close");
        assert!(
            mgr.get_live_stream_file(&unique_id)
                .await
                .expect("lookup")
                .is_some()
        );

        // The last close drops the tuner and deletes the buffer.
        mgr.close_channel_stream(&live_id).await.expect("close");
        assert!(
            mgr.get_live_stream_file(&unique_id)
                .await
                .expect("lookup")
                .is_none()
        );
        assert!(!file.path.exists(), "the buffer must be deleted");
        // Closing an unknown id is a no-op, never an error.
        mgr.close_channel_stream("nope").await.expect("close");
    }

    #[tokio::test]
    async fn a_tuner_at_its_stream_limit_rejects_a_second_channel() {
        let dir = tempfile::tempdir().expect("temp dir");
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager_with_tuner(dir.path(), &opens).await;
        // One tuner: a second, DIFFERENT source on the same host must be
        // refused (C# `LiveTvConflictException`).
        let mut tuner = mgr.get_tuner_hosts().await.expect("tuners").remove(0);
        tuner.tuner_count = 1;
        mgr.save_tuner_host(tuner).await.expect("save");

        let channels = channel_ids(&mgr).await;
        let (first, second) = (channels[0], channels[1]);
        let first_source = mgr.get_channel_media_sources(first).await.expect("sources")[0]
            .id
            .clone();
        let second_source = mgr
            .get_channel_media_sources(second)
            .await
            .expect("sources")[0]
            .id
            .clone();

        let opened = mgr
            .open_channel_stream(first, first_source.as_deref())
            .await
            .expect("open");
        let error = mgr
            .open_channel_stream(second, second_source.as_deref())
            .await
            .expect_err("the tuner is busy");
        assert!(
            matches!(error, ServiceError::Conflict(ref m) if m.contains("simultaneous stream limit")),
            "{error}"
        );

        // Re-posted under its id in another case, the tuner is still busy with
        // the stream opened under the old spelling (`OrdinalIgnoreCase`).
        let mut tuner = mgr.get_tuner_hosts().await.expect("tuners").remove(0);
        tuner.id = tuner.id.map(|id| id.to_uppercase());
        mgr.save_tuner_host(tuner).await.expect("re-post");
        let error = mgr
            .open_channel_stream(second, second_source.as_deref())
            .await
            .expect_err("the tuner is still busy");
        assert!(
            matches!(error, ServiceError::Conflict(ref m) if m.contains("simultaneous stream limit")),
            "{error}"
        );
        mgr.close_channel_stream(&opened.live_stream_id.unwrap_or_default())
            .await
            .expect("close");
    }

    #[tokio::test]
    async fn a_stream_the_tuner_forbids_sharing_stays_a_pass_through() {
        let dir = tempfile::tempdir().expect("temp dir");
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager_with_tuner(dir.path(), &opens).await;
        let mut tuner = mgr.get_tuner_hosts().await.expect("tuners").remove(0);
        tuner.allow_stream_sharing = false;
        mgr.save_tuner_host(tuner).await.expect("save");

        let channel = first_channel_id(&mgr).await;
        let source_id = mgr
            .get_channel_media_sources(channel)
            .await
            .expect("sources")[0]
            .id
            .clone();
        let opened = mgr
            .open_channel_stream(channel, source_id.as_deref())
            .await
            .expect("open");
        // Nothing is buffered: the media source keeps the tuner URL and the
        // `LiveStreamFiles` route has no file to serve.
        assert_eq!(opened.path.as_deref(), Some("http://tuner/one"));
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            mgr.get_live_stream_file("anything")
                .await
                .expect("lookup")
                .is_none()
        );
        mgr.close_channel_stream(&opened.live_stream_id.unwrap_or_default())
            .await
            .expect("close");
    }

    #[tokio::test]
    async fn opening_a_shared_stream_without_the_wiring_fails_loudly() {
        // A shareable tuner, but no transcode directory: there is nowhere to
        // buffer to, and that must be said out loud rather than silently
        // handing back a source that plays nothing.
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager_with_tuner(std::path::Path::new(""), &opens).await;
        let channel = first_channel_id(&mgr).await;
        let error = mgr
            .open_channel_stream(channel, Some("some-source"))
            .await
            .expect_err("no transcode path");
        assert!(error.to_string().contains("transcode path"), "{error}");
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A manager over the relative guide whose tuner is an in-memory endless
    /// broadcast and whose DVR writes under `root`.
    async fn manager_with_dvr(root: &std::path::Path) -> FerrofinLiveTvManager {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tuner = crate::stream::tests::LoopingTuner {
            chunk: vec![0x47; 188],
            opens,
            content_type: Some("video/MP2T".to_owned()),
        };
        let mgr = manager_with_relative_guide()
            .await
            .with_tuner_source(Arc::new(tuner))
            .with_paths(LiveTvPaths {
                transcode_dir: root.join("transcodes").into(),
                data_dir: root.join("data"),
                options_file: root.join("named").join("livetv.json"),
            });
        mgr.set_local_api_access(Arc::new(LoopbackApi));
        mgr
    }

    /// A manager whose `LiveTvOptions` file holds `guide_days`, or none at all.
    async fn manager_with_guide_days(days: Option<i32>) -> FerrofinLiveTvManager {
        let root =
            std::env::temp_dir().join(format!("ferrofin-guide-days-{}", Uuid::new_v4().simple()));
        let named = root.join("named");
        std::fs::create_dir_all(&named).expect("mkdir");
        if days.is_some() {
            // Serialize a whole `LiveTvOptions`, which is what
            // `POST /System/Configuration/livetv` writes — a hand-trimmed
            // fragment does not deserialize, and `live_tv_options` would
            // silently hand back the defaults.
            let options = ferrofin_model::live_tv::LiveTvOptions {
                guide_days: days,
                ..ferrofin_model::live_tv::LiveTvOptions::default()
            };
            std::fs::write(
                named.join("livetv.json"),
                serde_json::to_string(&options).expect("serialize options"),
            )
            .expect("write options");
        }
        manager_with(FakeFetcher(HashMap::new()))
            .await
            .with_paths(LiveTvPaths {
                transcode_dir: root.join("transcodes").into(),
                data_dir: root.join("data"),
                options_file: named.join("livetv.json"),
            })
    }

    /// `GuideManager.GetGuideInfo` is `now .. now + GetGuideDays()`, and
    /// `GetGuideDays` is `GuideDays.HasValue ? Clamp(value, 1, 14) : 7`
    /// (v10.11.8 GuideManager.cs:82-92, 161-168). The handler used to answer a
    /// hardcoded 7 days and could not read the setting at all — it took no
    /// state — so the dashboard's "Guide days" control changed nothing.
    #[rstest::rstest]
    #[case(None, 7)]
    #[case(Some(7), 7)]
    #[case(Some(14), 14)]
    #[case(Some(1), 1)]
    // Clamped, not rejected: upstream uses Math.Clamp, so an out-of-range value
    // still yields a usable window.
    #[case(Some(100), 14)]
    #[case(Some(0), 1)]
    #[case(Some(-5), 1)]
    #[tokio::test]
    async fn guide_info_window_follows_the_configured_guide_days(
        #[case] configured: Option<i32>,
        #[case] expected_days: i64,
    ) {
        let mgr = manager_with_guide_days(configured).await;
        let before = chrono::Utc::now();
        let info = mgr.get_guide_info().await.expect("guide info");
        let after = chrono::Utc::now();
        assert!(info.start_date >= before && info.start_date <= after);
        assert_eq!(
            info.end_date - info.start_date,
            chrono::Duration::days(expected_days),
            "GuideDays={configured:?}"
        );
    }

    /// The advertised window and the window the refresh actually ingests read
    /// the same setting — an advertised fortnight over a stored week is how a
    /// client ends up scrolling into an empty guide.
    #[tokio::test]
    async fn the_ingest_window_and_the_advertised_window_agree() {
        for days in [None, Some(1), Some(14), Some(100)] {
            let mgr = manager_with_guide_days(days).await;
            let advertised = mgr.get_guide_info().await.expect("guide info");
            let ingested = mgr.guide_days().await;
            assert_eq!(
                advertised.end_date - advertised.start_date,
                chrono::Duration::days(ingested),
                "GuideDays={days:?}"
            );
        }
    }

    /// The guide programme airing right now on the first channel.
    async fn now_playing(mgr: &FerrofinLiveTvManager) -> GuideProgramRow {
        mgr.query_program_rows(&InternalItemsQuery::default(), chrono::Utc::now())
            .await
            .expect("rows")
            .into_iter()
            .find(|r| r.title == "Now Playing")
            .expect("now playing")
    }

    #[tokio::test]
    async fn timer_defaults_describe_the_programme_they_would_record() {
        let mgr = manager_with_relative_guide().await;
        let program = now_playing(&mgr).await;
        let program_id = Uuid::parse_str(&program.id).expect("guid");

        let defaults = mgr
            .get_new_timer_defaults(Some(program_id))
            .await
            .expect("defaults");
        // The standing defaults (C# `GetNewTimerDefaultsAsync`).
        assert!(defaults.record_any_time);
        assert!(!defaults.record_any_channel);
        assert_eq!(defaults.days.len(), 7);
        assert_eq!(
            defaults.day_pattern,
            Some(ferrofin_model::live_tv::DayPattern::Daily)
        );
        assert_eq!(
            defaults.base.keep_until,
            ferrofin_model::live_tv::KeepUntil::UntilDeleted
        );
        assert_eq!(defaults.base.service_name.as_deref(), Some("Emby"));
        // …and the programme's own identity on top.
        assert_eq!(defaults.base.name.as_deref(), Some("Now Playing"));
        assert_eq!(
            defaults.base.channel_id,
            Uuid::parse_str(&program.channel_id).expect("guid")
        );
        assert_eq!(
            defaults.base.program_id.as_deref(),
            Some(program_id.simple().to_string().as_str()),
            "the client posts this back, so it must name the programme the way a DTO does"
        );
        assert_eq!(defaults.base.external_program_id, program.external_id);
        assert_eq!(
            defaults.base.start_date,
            parse_dt(&program.start_date).unwrap()
        );
        // `LiveTvManager.GetNewTimerDefaultsInternal` nulls the SeriesTimerInfo's
        // EXTERNAL id, but `LiveTvDtoService.GetSeriesTimerInfoDto` then derives
        // the DTO id from it unconditionally — so the id is a fixed hash of
        // "emby" + "" + "4", not null.
        assert_eq!(
            defaults.base.id.as_deref(),
            Some("eb075d6a62e2edc6b764a304633d33c0")
        );
        // ...and `ServerId` is set on every timer DTO the service builds.
        assert!(defaults.base.server_id.is_some());

        // Without a programme, just the standing defaults.
        let bare = mgr.get_new_timer_defaults(None).await.expect("defaults");
        assert_eq!(bare.base.name, None);
        assert_eq!(bare.base.program_id, None);
        assert!(bare.base.server_id.is_some());
    }

    #[tokio::test]
    async fn a_second_timer_for_the_same_programme_is_a_conflict_until_the_first_is_cancelled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let program_id = Uuid::parse_str(&program.id).expect("guid");
        // A timer far enough out that it never fires during the test.
        let mut defaults = mgr
            .get_new_timer_defaults(Some(program_id))
            .await
            .expect("defaults");
        defaults.base.start_date = chrono::Utc::now() + chrono::Duration::hours(6);
        defaults.base.end_date = chrono::Utc::now() + chrono::Duration::hours(7);
        let timer = timer_from_defaults(&defaults);

        let id = mgr.create_timer(timer.clone()).await.expect("create");
        assert_ne!(id, "");
        let error = mgr
            .create_timer(timer.clone())
            .await
            .expect_err("the programme is already scheduled");
        assert!(
            matches!(error, ServiceError::InvalidInput(ref m) if m.contains("already exists")),
            "{error}"
        );

        // Cancelling a manual timer removes it, so the programme is free again.
        mgr.cancel_timer(&id).await.expect("cancel");
        assert!(mgr.get_timer(&id).await.expect("get").is_none());
        let again = mgr.create_timer(timer).await.expect("re-create");
        assert_ne!(again, "");
        mgr.cancel_timer(&again).await.expect("cancel");
    }

    #[tokio::test]
    async fn the_timer_query_selects_by_channel_and_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let channels = channel_ids(&mgr).await;
        for (index, channel) in channels.iter().enumerate() {
            let hours = i64::try_from(index).expect("a handful of channels");
            let timer = TimerInfoDto {
                status: RecordingStatus::New,
                base: BaseTimerInfoDto {
                    channel_id: *channel,
                    name: Some(format!("Timer {index}")),
                    start_date: chrono::Utc::now() + chrono::Duration::hours(6 + hours),
                    end_date: chrono::Utc::now() + chrono::Duration::hours(7 + hours),
                    ..BaseTimerInfoDto::default()
                },
                ..TimerInfoDto::default()
            };
            mgr.create_timer(timer).await.expect("create");
        }

        let all = mgr
            .get_timers_matching(&ferrofin_model::live_tv::TimerQuery::default())
            .await
            .expect("timers");
        assert_eq!(all.len(), 2);
        // Ordered by start date, as `GetTimersInternal` orders them.
        assert!(all[0].base.start_date <= all[1].base.start_date);

        let on_first = mgr
            .get_timers_matching(&ferrofin_model::live_tv::TimerQuery {
                channel_id: Some(channels[0].simple().to_string()),
                ..ferrofin_model::live_tv::TimerQuery::default()
            })
            .await
            .expect("timers");
        assert_eq!(on_first.len(), 1);
        assert_eq!(on_first[0].base.channel_id, channels[0]);

        // Nothing is recording yet, so `isActive=true` is empty and
        // `isScheduled=true` is everything.
        let timers_matching = mgr
            .get_timers_matching(&ferrofin_model::live_tv::TimerQuery {
                is_active: Some(true),
                ..ferrofin_model::live_tv::TimerQuery::default()
            })
            .await
            .expect("timers");
        assert!(timers_matching.is_empty(), "{timers_matching:?}");
        assert_eq!(
            mgr.get_timers_matching(&ferrofin_model::live_tv::TimerQuery {
                is_scheduled: Some(true),
                ..ferrofin_model::live_tv::TimerQuery::default()
            })
            .await
            .expect("timers")
            .len(),
            2
        );
        for timer in all {
            mgr.cancel_timer(&timer.base.id.unwrap_or_default())
                .await
                .expect("cancel");
        }
    }

    /// Port of `GetActiveRecordingInfo`'s two exclusions: a capture whose
    /// cancel flag is set, and one whose timer is not `InProgress` (or is
    /// gone), are not active recordings.
    #[tokio::test]
    async fn only_a_live_capture_of_an_in_progress_timer_is_an_active_recording() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        for (id, status) in [
            ("recording", RecordingStatus::InProgress),
            ("stopping", RecordingStatus::InProgress),
            ("finished", RecordingStatus::Completed),
        ] {
            let timer = TimerInfoDto {
                base: BaseTimerInfoDto {
                    id: Some(id.to_owned()),
                    start_date: Utc::now() + chrono::Duration::minutes(600),
                    end_date: Utc::now() + chrono::Duration::minutes(660),
                    ..BaseTimerInfoDto::default()
                },
                status,
                ..TimerInfoDto::default()
            };
            mgr.persist_timer(&timer).await.expect("plant");
        }
        for id in ["recording", "stopping", "finished", "no-timer-row"] {
            let path = dir.path().join(format!("{id}.ts"));
            mgr.active_recordings_lock().insert(
                id.to_owned(),
                crate::dvr::ActiveRecording::new(id.to_owned(), Uuid::new_v4(), path),
            );
        }
        mgr.cancel_recording("stopping");

        let expected = dir.path().join("recording.ts").display().to_string();
        assert_eq!(
            mgr.active_recording_paths().await.expect("paths"),
            vec![expected]
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_a_timer_mid_capture_releases_it_on_a_real_runtime() {
        // On a multi-thread runtime the firing task and the cancel really do
        // run at once. Disarming must stop only a PENDING fire — an abort here
        // would strand the active recording, leave the tuner open and freeze
        // the row at `InProgress` for ever.
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let defaults = mgr
            .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
            .await
            .expect("defaults");
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");

        let path = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .ok()
                .flatten()
        })
        .await
        .expect("the capture must start");
        // `Video.IsActiveRecording`'s source: the file being written, once
        // the timer row says `InProgress`.
        let active = wait_for(|| async {
            let paths = mgr.active_recording_paths().await.expect("paths");
            (!paths.is_empty()).then_some(paths)
        })
        .await;
        assert_eq!(active, Some(vec![path.clone()]));
        mgr.cancel_timer(&timer_id).await.expect("cancel");

        let released = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .expect("path")
                .is_none()
                .then_some(())
        })
        .await;
        assert!(
            released.is_some(),
            "the capture must be released, not orphaned"
        );
        let active_recording_paths = mgr.active_recording_paths().await.expect("paths");
        assert!(
            active_recording_paths.is_empty(),
            "{active_recording_paths:?}"
        );
        // `finish_capture` releases the active entry before `settle_timer`
        // finishes the async database writes. Wait for that second phase too:
        // seeing no active path does not yet mean the recording row has settled.
        let settled = wait_for(|| async {
            mgr.get_recordings_matching(
                &ferrofin_model::live_tv::RecordingQuery {
                    is_in_progress: Some(true),
                    ..ferrofin_model::live_tv::RecordingQuery::default()
                },
                None,
                &DtoOptions::default(),
            )
            .await
            .expect("recordings")
            .items
            .is_empty()
            .then_some(())
        })
        .await;
        assert!(
            settled.is_some(),
            "nothing may still report itself as recording"
        );
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn a_timer_whose_channel_has_gone_retries_instead_of_dying_quietly() {
        // A capture can fail before it ever starts — the channel deleted, every
        // tuner busy, no transcode directory. Upstream turns that into a failed
        // recording that retries; a timer that just vanished from the schedule
        // would be a silent data loss.
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let mut defaults = mgr
            .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
            .await
            .expect("defaults");
        // A channel that is not in the lineup: the open cannot succeed. The
        // programme ids go too, or `CopyProgramInfoToTimerInfo` would put the
        // real channel back.
        defaults.base.channel_id = Uuid::from_u128(0xdead_beef);
        defaults.base.program_id = None;
        defaults.base.external_program_id = None;
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");

        let retried = wait_for(|| async {
            let timer = mgr.get_timer(&timer_id).await.expect("get")?;
            // The retry re-arms it for a minute out, with the elapsed
            // pre-padding dropped.
            (timer.status == RecordingStatus::New
                && timer.base.start_date > chrono::Utc::now()
                && timer.base.pre_padding_seconds == 0)
                .then_some(())
        })
        .await;
        assert!(
            retried.is_some(),
            "a failed capture must reschedule the timer"
        );
        // …and no ghost recording is left behind for it.
        assert!(
            mgr.get_recordings()
                .await
                .expect("recordings")
                .items
                .is_empty(),
            "a failed attempt leaves no fileless recording"
        );
        mgr.cancel_timer(&timer_id).await.expect("cancel");
    }

    /// A self-write guard recording each call, in order.
    #[derive(Default)]
    struct RecordingWrites(std::sync::Mutex<Vec<String>>);

    impl ferrofin_traits::library::SelfWriteGuard for RecordingWrites {
        fn begin(&self, path: &str) {
            self.0.lock().unwrap().push(format!("begin {path}"));
        }
        fn complete(&self, path: &str, refresh_path: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete {path} {refresh_path}"));
        }
    }

    /// `RecordingsManager.RecordStream` brackets the recording from the moment
    /// its file is chosen until it is settled (`RecordingsManager.cs:342,400`):
    /// begun while the capture runs, completed once it has stopped.
    #[tokio::test]
    async fn a_recording_is_bracketed_while_it_is_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let writes = Arc::new(RecordingWrites::default());
        let mgr = manager_with_dvr(dir.path())
            .await
            .with_self_writes(writes.clone());
        let program = now_playing(&mgr).await;
        let program_id = Uuid::parse_str(&program.id).expect("guid");
        let defaults = mgr
            .get_new_timer_defaults(Some(program_id))
            .await
            .expect("defaults");
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");
        let path = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .expect("path")
        })
        .await
        .expect("a capture is in progress");
        assert_eq!(
            writes.0.lock().unwrap().clone(),
            [format!("begin {path}")],
            "begun, not yet complete, while it records"
        );
        mgr.cancel_timer(&timer_id).await.expect("cancel");
        let completed = wait_for(|| async {
            writes
                .0
                .lock()
                .unwrap()
                .contains(&format!("complete {path} false"))
                .then_some(())
        })
        .await;
        assert!(completed.is_some(), "{:?}", writes.0.lock().unwrap());
    }

    /// `RecordingsManager.RecordStream`'s `OnStarted` saves the guide's
    /// metadata beside the recording (`RecordingsMetadataManager
    /// .SaveRecordingMetadata`, with "Save EPG metadata to NFO files" on by
    /// default): a programme that is neither a series nor a movie gets a
    /// locked `<movie>` NFO, dated in UTC.
    #[tokio::test]
    async fn a_started_recording_gets_its_guide_nfo_beside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let defaults = mgr
            .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
            .await
            .expect("defaults");
        let before = chrono::Utc::now() - chrono::Duration::seconds(2);
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");
        let path = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .expect("path")
        })
        .await
        .expect("a capture is in progress");
        let nfo = std::path::Path::new(&path).with_extension("nfo");
        let body = wait_for(|| async { tokio::fs::read_to_string(&nfo).await.ok() })
            .await
            .expect("the recording's NFO");
        let after = chrono::Utc::now() + chrono::Duration::seconds(2);
        mgr.cancel_timer(&timer_id).await.expect("cancel");

        let head = "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n\
                    <movie>\n  <title>Now Playing</title>\n  <dateadded>";
        assert!(body.starts_with(head), "{body}");
        let tail = "</dateadded>\n  <plot />\n  <genre>Drama</genre>\n  \
                    <lockdata>true</lockdata>\n</movie>";
        assert!(body.ends_with(tail), "{body}");
        let added = chrono::NaiveDateTime::parse_from_str(
            &body[head.len()..body.len() - tail.len()],
            "%Y-%m-%d %H:%M:%S",
        )
        .expect("dateadded")
        .and_utc();
        assert!(before <= added && added <= after, "{added} is not UTC now");
    }

    /// With "Save EPG metadata to NFO files" off, a recording gets no NFO.
    #[tokio::test]
    async fn a_recording_gets_no_nfo_when_the_option_is_off() {
        let dir = tempfile::tempdir().expect("temp dir");
        let named = dir.path().join("named");
        std::fs::create_dir_all(&named).expect("mkdir");
        let options = ferrofin_model::live_tv::LiveTvOptions {
            save_recording_nfo: false,
            ..ferrofin_model::live_tv::LiveTvOptions::default()
        };
        std::fs::write(
            named.join("livetv.json"),
            serde_json::to_string(&options).expect("serialize options"),
        )
        .expect("write options");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let defaults = mgr
            .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
            .await
            .expect("defaults");
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");
        let path = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .expect("path")
        })
        .await
        .expect("a capture is in progress");
        // The NFO, when on, is written within a fraction of this.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        mgr.cancel_timer(&timer_id).await.expect("cancel");
        assert!(!std::path::Path::new(&path).with_extension("nfo").exists());
    }

    #[tokio::test]
    async fn a_timer_on_a_programme_airing_now_records_it_and_the_recording_is_playable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mgr = manager_with_dvr(dir.path()).await;
        let program = now_playing(&mgr).await;
        let program_id = Uuid::parse_str(&program.id).expect("guid");
        let defaults = mgr
            .get_new_timer_defaults(Some(program_id))
            .await
            .expect("defaults");
        // The programme started in the past, so the timer fires immediately —
        // which is the whole point: `arm_timer` must not wait for a start that
        // has already gone by.
        let timer_id = mgr
            .create_timer(timer_from_defaults(&defaults))
            .await
            .expect("create");

        // The capture registers itself before the first byte is written.
        let recording = wait_for(|| async {
            let recordings = mgr
                .get_recordings_matching(
                    &ferrofin_model::live_tv::RecordingQuery {
                        is_in_progress: Some(true),
                        ..ferrofin_model::live_tv::RecordingQuery::default()
                    },
                    None,
                    &DtoOptions::default(),
                )
                .await
                .expect("recordings");
            recordings.items.into_iter().next()
        })
        .await
        .expect("the timer must have started a recording");

        assert_eq!(recording.name.as_deref(), Some("Now Playing"));
        assert_eq!(recording.status.as_deref(), Some("InProgress"));
        assert_eq!(recording.timer_id.as_deref(), Some(timer_id.as_str()));
        assert!(
            recording.completion_percentage.is_some_and(|p| p >= 0.0),
            "an in-progress recording reports how far through it is"
        );
        // The capture is keyed by the TIMER's id, which is what
        // `/LiveTv/LiveRecordings/{id}/stream` takes.
        let active_path = mgr
            .get_active_recording_path(&timer_id)
            .await
            .expect("path")
            .expect("a capture is in progress");
        assert!(
            std::path::Path::new(&active_path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ts")),
            "{active_path}"
        );
        assert!(
            std::path::Path::new(&active_path).starts_with(dir.path().join("data")),
            "the recording must land under the data directory: {active_path}"
        );

        // PlaybackInfo on the recording reaches it through the EncoderPath.
        let sources = mgr
            .get_recording_media_sources(recording.id)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].encoder_path.as_deref(),
            Some(format!("http://127.0.0.1:8096/LiveTv/LiveRecordings/{timer_id}/stream").as_str())
        );
        assert_eq!(sources[0].path.as_deref(), Some(active_path.as_str()));
        assert!(sources[0].is_infinite_stream);
        assert!(!sources[0].supports_direct_play);

        // Deleting the timer stops the capture and, with it, the tuner.
        mgr.cancel_timer(&timer_id).await.expect("cancel");
        let stopped = wait_for(|| async {
            mgr.get_active_recording_path(&timer_id)
                .await
                .expect("path")
                .is_none()
                .then_some(())
        })
        .await;
        assert!(
            stopped.is_some(),
            "cancelling the timer must stop the capture"
        );

        // …and deleting the recording removes both the row and the file.
        mgr.delete_recording(recording.id).await.expect("delete");
        assert!(
            mgr.get_recording(recording.id)
                .await
                .expect("get")
                .is_none(),
            "the deleted recording must be gone"
        );
    }

    /// The timer a client creates from `Timers/Defaults` — the same JSON the
    /// parity harness POSTs straight back.
    fn timer_from_defaults(defaults: &SeriesTimerInfoDto) -> TimerInfoDto {
        let json = serde_json::to_string(defaults).expect("serialize");
        serde_json::from_str(&json).expect("a SeriesTimerInfoDto body binds as a TimerInfoDto")
    }

    /// Polls `f` until it yields a value, up to a few seconds.
    async fn wait_for<T, F, Fut>(mut f: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        for _ in 0..200 {
            if let Some(value) = f().await {
                return Some(value);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        None
    }

    /// The programme titles a query returns, in the order it returned them.
    async fn titles(mgr: &FerrofinLiveTvManager, query: &InternalItemsQuery) -> Vec<String> {
        mgr.get_programs(query, &DtoOptions::default())
            .await
            .expect("programs")
            .items
            .into_iter()
            .filter_map(|item| item.name)
            .collect()
    }

    #[tokio::test]
    async fn program_query_orders_by_start_date_and_scopes_to_channels() {
        let mgr = manager_with_relative_guide().await;
        assert_eq!(
            titles(&mgr, &InternalItemsQuery::default()).await,
            ["Aired", "Now Playing", "Two Now", "Later"],
            "the default order is start-date ascending"
        );

        let two = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("channels")
            .items
            .into_iter()
            .find(|c| c.name.as_deref() == Some("Channel Two"))
            .expect("channel two")
            .id;
        let query = InternalItemsQuery {
            channel_ids: vec![two],
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &query).await, ["Two Now"]);
    }

    #[tokio::test]
    async fn program_query_applies_the_airing_window() {
        let mgr = manager_with_relative_guide().await;
        let now = chrono::Utc::now();

        let airing = InternalItemsQuery {
            is_airing: Some(true),
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &airing).await, ["Now Playing", "Two Now"]);

        // Not on now: `StartDate > now || EndDate < now`, as v12.2's
        // `TranslateQuery` has it since jellyfin/jellyfin#17588.
        let not_airing = InternalItemsQuery {
            is_airing: Some(false),
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &not_airing).await, ["Aired", "Later"]);

        // HasAired replaces the end-date bound: everything that has finished.
        let aired = InternalItemsQuery {
            has_aired: Some(true),
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &aired).await, ["Aired"]);

        // An explicit window keeps only the airings wholly inside it.
        let window = InternalItemsQuery {
            min_start_date: Some(now),
            max_end_date: Some(now + chrono::Duration::hours(4)),
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &window).await, ["Later"]);
    }

    #[tokio::test]
    async fn program_query_pages_and_counts() {
        let mgr = manager_with_relative_guide().await;

        let limited = InternalItemsQuery {
            limit: Some(2),
            ..InternalItemsQuery::default()
        };
        let page = mgr
            .get_programs(&limited, &DtoOptions::default())
            .await
            .expect("page");
        assert_eq!(page.items.len(), 2, "Limit truncates the page");
        assert_eq!(
            page.total_record_count, 4,
            "the total still counts every match"
        );

        let offset = InternalItemsQuery {
            start_index: Some(2),
            limit: Some(1),
            ..InternalItemsQuery::default()
        };
        let page = mgr
            .get_programs(&offset, &DtoOptions::default())
            .await
            .expect("page2");
        assert_eq!(page.start_index, 2);
        assert_eq!(page.total_record_count, 4);
        assert_eq!(
            page.items.first().and_then(|i| i.name.clone()).as_deref(),
            Some("Two Now"),
            "StartIndex skips into the start-date order"
        );

        // No paging: the count is the item count, not a second query.
        let all = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("all");
        assert_eq!(all.total_record_count, 4);
        assert_eq!(all.start_index, 0);
    }

    #[tokio::test]
    async fn program_query_sorts_filters_by_genre_and_by_name() {
        let mgr = manager_with_relative_guide().await;

        let descending = InternalItemsQuery {
            order_by: vec![(ItemSortBy::StartDate, SortOrder::Descending)],
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            titles(&mgr, &descending).await,
            ["Later", "Two Now", "Now Playing", "Aired"]
        );

        let by_title = InternalItemsQuery {
            order_by: vec![(ItemSortBy::Name, SortOrder::Ascending)],
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            titles(&mgr, &by_title).await,
            ["Aired", "Later", "Now Playing", "Two Now"]
        );

        // Genres come from the XMLTV <category> list and match case-insensitively.
        let news = InternalItemsQuery {
            genres: vec!["news".to_owned()],
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &news).await, ["Aired", "Two Now"]);

        // The exact-name scope `librarySeriesId` sets.
        let named = InternalItemsQuery {
            name: Some("two now".to_owned()),
            ..InternalItemsQuery::default()
        };
        assert_eq!(titles(&mgr, &named).await, ["Two Now"]);
    }

    /// Name and genre filters compare clean values on both sides, as upstream
    /// matches `CleanName == GetCleanValue(name)` and genre `CleanValue`s
    /// (`BaseItemRepository.TranslateQuery.cs:476-477,658-662`). SQLite's own
    /// `LOWER()` folds ASCII only, so a Cyrillic or accented title never
    /// matched, and punctuation still told titles apart.
    #[tokio::test]
    async fn program_name_and_genre_filters_match_clean_values() {
        let mgr = manager_with_guide(guide_of(&[
            ("one.tv", -10, 50, "Новости", "Новости"),
            ("one.tv", 60, 120, "Élan vital", "Émission"),
            ("two.tv", -10, 50, "Bob's Burgers", "Animation"),
        ]))
        .await;
        for (name, expected) in [
            ("Новости", "Новости"),
            ("ÉLAN VITAL", "Élan vital"),
            ("Bob\u{2019}s Burgers", "Bob's Burgers"),
        ] {
            let named = InternalItemsQuery {
                name: Some(name.to_owned()),
                ..InternalItemsQuery::default()
            };
            assert_eq!(titles(&mgr, &named).await, [expected], "name {name}");
        }
        for (genre, expected) in [("Новости", "Новости"), ("émission", "Élan vital")]
        {
            let genres = InternalItemsQuery {
                genres: vec![genre.to_owned()],
                ..InternalItemsQuery::default()
            };
            assert_eq!(titles(&mgr, &genres).await, [expected], "genre {genre}");
        }
    }

    #[tokio::test]
    async fn timer_crud_roundtrips() {
        use ferrofin_model::live_tv::{BaseTimerInfoDto, TimerInfoDto};
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let ch = uuid::Uuid::new_v4();
        let timer = TimerInfoDto {
            base: BaseTimerInfoDto {
                channel_id: ch,
                name: Some("Record the news".to_owned()),
                start_date: parse_dt("2026-07-25T06:00:00Z").unwrap(),
                end_date: parse_dt("2026-07-25T07:00:00Z").unwrap(),
                ..BaseTimerInfoDto::default()
            },
            ..TimerInfoDto::default()
        };

        let id = mgr.create_timer(timer).await.expect("create");
        assert_ne!(id, "");
        let timers = mgr.get_timers().await.expect("list");
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].base.channel_id, ch);
        assert_eq!(timers[0].base.name.as_deref(), Some("Record the news"));

        let got = mgr.get_timer(&id).await.expect("get").expect("some");
        assert_eq!(got.base.id.as_deref(), Some(id.as_str()));

        mgr.cancel_timer(&id).await.expect("cancel");
        let timers = mgr.get_timers().await.expect("list2");
        assert!(timers.is_empty(), "{timers:?}");
    }

    /// A guide whose one channel airs the same title several times in the
    /// future — what a series timer's fan-out actually needs, and what the
    /// parity fixture's XMLTV also looks like (no `episode-num`, so the listings
    /// carry no series id and the fan-out matches on the title).
    fn series_guide() -> String {
        use std::fmt::Write as _;
        let now = chrono::Utc::now();
        let mut xml = String::from(
            "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
        );
        for (channel, from, to, title) in [
            // Already over: `MinEndDate = UtcNow` must exclude it.
            ("one.tv", -120, -60, "Recurring"),
            ("one.tv", 60, 120, "Recurring"),
            ("one.tv", 1500, 1560, "Recurring"),
            ("one.tv", 2940, 3000, "Recurring"),
            // Same title, other channel: only reachable with RecordAnyChannel.
            ("two.tv", 90, 150, "Recurring"),
            ("one.tv", 200, 260, "Something Else"),
        ] {
            let start = (now + chrono::Duration::minutes(from)).format("%Y%m%d%H%M%S");
            let stop = (now + chrono::Duration::minutes(to)).format("%Y%m%d%H%M%S");
            let _ = write!(
                xml,
                "<programme start=\"{start} +0000\" stop=\"{stop} +0000\" channel=\"{channel}\">\
                 <title>{title}</title><category>Drama</category></programme>"
            );
        }
        xml.push_str("</tv>");
        xml
    }

    /// A manager whose guide holds [`series_guide`] over the two [`M3U`] channels.
    async fn manager_with_series_guide() -> FerrofinLiveTvManager {
        manager_with_guide(series_guide()).await
    }

    /// The guide rows on the first channel whose title matches, start-date ascending.
    async fn airings_of(mgr: &FerrofinLiveTvManager, title: &str) -> Vec<GuideProgramRow> {
        mgr.query_program_rows(
            &InternalItemsQuery {
                name: Some(title.to_owned()),
                order_by: vec![(ItemSortBy::StartDate, SortOrder::Ascending)],
                ..InternalItemsQuery::default()
            },
            Utc::now(),
        )
        .await
        .expect("guide")
    }

    /// The `Timers/Defaults` body a client posts back verbatim to create a
    /// series timer — including its constant `Id`, which is the whole point.
    async fn defaults_for(
        mgr: &FerrofinLiveTvManager,
        program: &GuideProgramRow,
    ) -> SeriesTimerInfoDto {
        mgr.get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
            .await
            .expect("defaults")
    }

    async fn all_series_timers(mgr: &FerrofinLiveTvManager) -> Vec<SeriesTimerInfoDto> {
        mgr.get_series_timers(&ferrofin_model::live_tv::SeriesTimerQuery::default())
            .await
            .expect("list")
    }

    #[tokio::test]
    async fn series_timer_crud_and_cascade() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create st");
        assert_eq!(all_series_timers(&mgr).await.len(), 1);

        // The fan-out scheduled a timer per FUTURE airing of that title on that
        // channel, and only those — the aired one and the other channel's are out.
        let timers = mgr.get_timers().await.expect("timers");
        assert_eq!(
            timers.len(),
            3,
            "one timer per future airing on this channel"
        );
        assert!(
            timers
                .iter()
                .all(|t| t.series_timer_id.as_deref() == Some(st_id.as_str())),
            "every timer the fan-out made belongs to the series timer"
        );
        // …and the three showings share a ShowId, so only the EARLIEST records:
        // `SearchForDuplicateShowIds` cancels the rest.
        let mut by_start: Vec<_> = timers.iter().collect();
        by_start.sort_by_key(|t| t.base.start_date);
        assert_eq!(by_start[0].status, RecordingStatus::New);
        assert!(
            by_start[1..]
                .iter()
                .all(|t| t.status == RecordingStatus::Cancelled),
            "a repeat showing of the same ShowId is cancelled, not recorded twice"
        );

        mgr.cancel_series_timer(&st_id).await.expect("cancel st");
        let found = all_series_timers(&mgr).await;
        assert!(found.is_empty(), "{found:?}");
        assert!(
            mgr.get_timers().await.expect("t2").is_empty(),
            "cancelling a series timer drops its timers"
        );
        assert!(
            matches!(
                mgr.cancel_series_timer(&st_id).await,
                Err(ServiceError::NotFound(_))
            ),
            "cancelling an unknown series timer is a 404, not a silent 204"
        );
    }

    /// The duplicate-showing sweep runs only for a series timer that skips
    /// episodes already in the library.
    ///
    /// Jellyfin 12 gates `SearchForDuplicateShowIds(enabledTimersForSeries)` on
    /// `seriesTimer.SkipEpisodesInLibrary` (v12.2
    /// src/Jellyfin.LiveTv/DefaultLiveTvService.cs:776-779, jellyfin/jellyfin
    /// PR 16220): with jellyfin-web's "Don't record episodes that are already
    /// in my library" cleared, every airing of a `ShowId` records. Turning it
    /// off on a timer the sweep already thinned brings the cancelled showings
    /// back, because the fan-out resets every non-manual existing timer to
    /// `New` first.
    #[tokio::test]
    async fn the_duplicate_sweep_runs_only_when_skipping_library_episodes() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let mut body = defaults_for(&mgr, future).await;
        body.skip_episodes_in_library = false;
        let st_id = mgr.create_series_timer(body).await.expect("create st");

        let statuses = |timers: Vec<TimerInfoDto>| {
            let mut by_start = timers;
            by_start.sort_by_key(|t| t.base.start_date);
            by_start.into_iter().map(|t| t.status).collect::<Vec<_>>()
        };
        assert_eq!(
            statuses(mgr.get_timers().await.expect("timers")),
            [RecordingStatus::New; 3],
            "without SkipEpisodesInLibrary every showing of the ShowId records"
        );

        let mut stored = mgr
            .get_series_timer(&st_id)
            .await
            .expect("get st")
            .expect("st row");
        stored.skip_episodes_in_library = true;
        mgr.update_series_timer(&st_id, stored.clone())
            .await
            .expect("skip on");
        assert_eq!(
            statuses(mgr.get_timers().await.expect("timers")),
            [
                RecordingStatus::New,
                RecordingStatus::Cancelled,
                RecordingStatus::Cancelled
            ],
            "with it, only the earliest showing of the ShowId records"
        );

        stored.skip_episodes_in_library = false;
        mgr.update_series_timer(&st_id, stored)
            .await
            .expect("skip off");
        assert_eq!(
            statuses(mgr.get_timers().await.expect("timers")),
            [RecordingStatus::New; 3],
            "clearing it again brings the swept showings back"
        );
    }

    /// A future airing of the series guide, and a timer factory on its channel.
    async fn refresh_test_airing(mgr: &FerrofinLiveTvManager) -> GuideProgramRow {
        airings_of(mgr, "Recurring")
            .await
            .into_iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing")
    }

    /// A one-hour stored timer on the airing's channel, with its id.
    fn refresh_test_timer(
        airing: &GuideProgramRow,
        name: &str,
        status: RecordingStatus,
        program: Option<String>,
        start: chrono::DateTime<Utc>,
    ) -> (String, ferrofin_model::live_tv::TimerInfoDto) {
        use ferrofin_model::live_tv::{BaseTimerInfoDto, TimerInfoDto};
        let id = Uuid::new_v4().simple().to_string();
        let dto = TimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some(id.clone()),
                channel_id: Uuid::parse_str(&airing.channel_id).expect("channel"),
                program_id: program,
                name: Some(name.to_owned()),
                start_date: start,
                end_date: start + chrono::Duration::hours(1),
                ..BaseTimerInfoDto::default()
            },
            status,
            ..TimerInfoDto::default()
        };
        (id, dto)
    }

    /// Every stored timer by id.
    async fn timers_by_id(
        mgr: &FerrofinLiveTvManager,
    ) -> HashMap<String, ferrofin_model::live_tv::TimerInfoDto> {
        mgr.all_timers()
            .await
            .expect("timers")
            .into_iter()
            .filter_map(|t| t.base.id.clone().map(|id| (id, t)))
            .collect()
    }

    /// `RefreshGuide` ends with `RefreshTimers` (v12.2 `GuideManager.cs:164`,
    /// `DefaultLiveTvService.cs:88-117`): over every timer but the `Completed`
    /// ones, one that has ended (and is not recording) is deleted, one whose
    /// programme has left the guide is deleted, one with no programme is left
    /// alone, and the rest take the refreshed programme's facts.
    #[tokio::test]
    async fn a_guide_refresh_tidies_and_refreshes_the_timers() {
        let mgr = manager_with_series_guide().await;
        let airing = refresh_test_airing(&mgr).await;
        let past = Utc::now() - chrono::Duration::hours(3);
        let future = Utc::now() + chrono::Duration::hours(3);
        let gone_program = Some(Uuid::new_v4().to_string());
        let (ended, ended_dto) =
            refresh_test_timer(&airing, "ended", RecordingStatus::Cancelled, None, past);
        let (gone, gone_dto) =
            refresh_test_timer(&airing, "gone", RecordingStatus::New, gone_program, future);
        let (stale, stale_dto) = refresh_test_timer(
            &airing,
            "stale name",
            RecordingStatus::New,
            Some(airing.id.clone()),
            future,
        );
        let (unlinked, unlinked_dto) =
            refresh_test_timer(&airing, "unlinked", RecordingStatus::New, None, future);
        for dto in [&ended_dto, &gone_dto, &stale_dto, &unlinked_dto] {
            mgr.persist_timer(dto).await.expect("seed timer");
        }

        mgr.refresh_guide().await.expect("refresh");

        let after = timers_by_id(&mgr).await;
        assert!(!after.contains_key(&ended), "an ended timer is deleted");
        assert!(
            !after.contains_key(&gone),
            "a timer whose programme left is deleted"
        );
        assert_eq!(
            after[&unlinked].base.name.as_deref(),
            Some("unlinked"),
            "a timer with no programme is left alone"
        );
        assert_eq!(
            after[&stale].base.name.as_deref(),
            Some(airing.title.as_str()),
            "a live timer takes the refreshed programme's name"
        );
        assert!(
            mgr.armed_timers_lock().contains_key(&stale),
            "and is re-armed"
        );
    }

    /// `RefreshTimers` calls `_timerManager.Update` for every live timer, not
    /// only one the guide changed, so a timer whose arm was lost is armed again
    /// by the next guide refresh even when its programme is as it was.
    #[tokio::test]
    async fn a_guide_refresh_rearms_a_timer_the_guide_left_unchanged() {
        let mgr = manager_with_series_guide().await;
        let airing = refresh_test_airing(&mgr).await;
        let future = Utc::now() + chrono::Duration::hours(3);
        let (id, dto) = refresh_test_timer(
            &airing,
            "lost arm",
            RecordingStatus::New,
            Some(airing.id.clone()),
            future,
        );
        mgr.persist_timer(&dto).await.expect("seed timer");
        mgr.refresh_guide().await.expect("refresh");
        let refreshed = timers_by_id(&mgr).await[&id].clone();
        let arm = Arc::clone(&mgr.armed_timers_lock()[&id]);

        // Unchanged and still armed: the arm stays as it is.
        mgr.refresh_guide().await.expect("refresh again");
        assert!(Arc::ptr_eq(&arm, &mgr.armed_timers_lock()[&id]));

        // The arm is lost; the next refresh finds the programme as it was.
        mgr.disarm_timer(&id);
        assert!(!mgr.armed_timers_lock().contains_key(&id));
        mgr.refresh_guide().await.expect("second refresh");
        assert_eq!(
            timers_by_id(&mgr).await[&id],
            refreshed,
            "the guide changed nothing"
        );
        assert!(
            mgr.armed_timers_lock().contains_key(&id),
            "the unchanged timer is armed again"
        );
    }

    /// What `RefreshTimers` must leave alone: `Completed` rows (recordings
    /// reference them; `GetTimersAsync` excludes them), an ended timer whose
    /// capture is still running (`GetActiveRecordingPath`), and a showing
    /// cancelled by hand, which stays cancelled and unarmed.
    #[tokio::test]
    async fn a_guide_refresh_keeps_completed_recording_and_cancelled_timers() {
        let mgr = manager_with_series_guide().await;
        let airing = refresh_test_airing(&mgr).await;
        let past = Utc::now() - chrono::Duration::hours(3);
        let future = Utc::now() + chrono::Duration::hours(3);
        let (completed, completed_dto) =
            refresh_test_timer(&airing, "completed", RecordingStatus::Completed, None, past);
        let (completed_gone, completed_gone_dto) = refresh_test_timer(
            &airing,
            "completed, programme gone",
            RecordingStatus::Completed,
            Some(Uuid::new_v4().to_string()),
            future,
        );
        let (recording, recording_dto) = refresh_test_timer(
            &airing,
            "recording",
            RecordingStatus::InProgress,
            None,
            past,
        );
        let (cancelled, cancelled_dto) = refresh_test_timer(
            &airing,
            "cancelled",
            RecordingStatus::Cancelled,
            Some(airing.id.clone()),
            future,
        );
        for dto in [
            &completed_dto,
            &completed_gone_dto,
            &recording_dto,
            &cancelled_dto,
        ] {
            mgr.persist_timer(dto).await.expect("seed timer");
        }
        let dir = tempfile::tempdir().expect("tmp");
        mgr.active_recordings_lock().insert(
            recording.clone(),
            crate::dvr::ActiveRecording::new(
                recording.clone(),
                Uuid::new_v4(),
                dir.path().join("recording.ts"),
            ),
        );

        mgr.refresh_guide().await.expect("refresh");

        let after = timers_by_id(&mgr).await;
        assert!(after.contains_key(&completed), "a completed timer is kept");
        assert!(
            after.contains_key(&completed_gone),
            "a completed timer is never refreshed"
        );
        assert!(
            after.contains_key(&recording),
            "an ended timer that is still recording is kept"
        );
        assert_eq!(after[&cancelled].status, RecordingStatus::Cancelled);
        assert!(
            !mgr.armed_timers_lock().contains_key(&cancelled),
            "a hand-cancelled showing is not armed"
        );
    }

    /// A guide refresh RE-EXPANDS every series timer over the airings it just
    /// ingested.
    ///
    /// `RefreshGuide` ends with `coreService.RefreshSeriesTimers(ct)` (v10.11.8
    /// src/Jellyfin.LiveTv/Guide/GuideManager.cs:150-156), which runs
    /// `UpdateTimersForSeriesTimer(timer, false, true)` over every stored rule
    /// (DefaultLiveTvService.cs:78-86). Ferrofin ran the fan-out only on create
    /// and edit, so a rule stopped producing timers as soon as the rolling guide
    /// window moved past the airings it was created against — which is a series
    /// recording that silently stops recording.
    #[tokio::test]
    async fn a_guide_refresh_re_expands_every_series_timer() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create st");
        let before = mgr.get_timers().await.expect("timers");
        assert!(!before.is_empty(), "the create-time fan-out made timers");

        // Drop every timer row the create-time fan-out made, WITHOUT touching
        // the series timer — the state a rolling guide leaves behind once its
        // airings have aged out and the rule has never been re-expanded. The
        // rows are deleted rather than CANCELLED on purpose: `cancel_timer`
        // stamps `IsManual`, which the next fan-out is required to honour, and
        // the test would then measure that guard instead of the re-expansion.
        for timer in &before {
            let id = timer.base.id.clone().expect("timer id");
            mgr.delete_by_id(DELETE_TIMER_SQL, &id)
                .await
                .expect("delete timer row");
        }
        let found = mgr.get_timers().await.expect("t");
        assert!(found.is_empty(), "{found:?}");

        // A refresh alone brings them back — no client call, no edit.
        mgr.refresh_guide().await.expect("second refresh");
        let after = mgr.get_timers().await.expect("timers again");
        assert_eq!(
            after.len(),
            before.len(),
            "a guide refresh re-expands the series timer over the refreshed guide"
        );
        assert!(
            after
                .iter()
                .all(|t| t.series_timer_id.as_deref() == Some(st_id.as_str())),
            "and every timer it re-made still belongs to the series timer"
        );
    }

    /// The `IsManual` contract, end to end: a person cancels ONE showing of a
    /// series by hand, then edits the series timer — and the showing they
    /// cancelled must STAY cancelled, with the next one promoted in its place.
    ///
    /// `CancelTimerAsync` passes `isManualCancellation: true`
    /// (v10.11.8 DefaultLiveTvService.cs:199-203), which sets `timer.IsManual`
    /// on the surviving `Cancelled` row (`:176-180`); the next
    /// `UpdateTimersForSeriesTimer` copies that flag onto the candidate
    /// (`:745`) and both of the arms that could revive it are guarded on it
    /// (`ShouldCancelTimerForSeriesTimer`'s first arm `:646`, and the
    /// `else if (!existingTimer.IsManual)` at `:751`).
    ///
    /// The regression this pins: the flag was only ever written on INSERT, so
    /// cancelling a child — always an UPDATE, because a child keeps its
    /// `SeriesTimerId` and is not deleted — never raised it, and the next edit
    /// of the series timer put the showing back to `New` and recorded it.
    #[tokio::test]
    async fn a_hand_cancelled_showing_stays_cancelled_across_the_next_fan_out() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create st");

        let mut timers = mgr.get_timers().await.expect("timers");
        timers.sort_by_key(|t| t.base.start_date);
        assert_eq!(timers.len(), 3);
        assert_eq!(timers[0].status, RecordingStatus::New);
        let hand_cancelled = timers[0].base.id.clone().expect("id");
        let next_up = timers[1].base.id.clone().expect("id");

        // The person cancels the earliest showing.
        mgr.cancel_timer(&hand_cancelled).await.expect("cancel");
        assert!(
            crate::dvr_repository::timer_is_manual(&mgr.db, &hand_cancelled)
                .await
                .expect("flag"),
            "a hand cancel flags the surviving row IsManual"
        );

        // …then edits the series timer, which re-runs the whole fan-out.
        let mut edit = mgr
            .get_series_timer(&st_id)
            .await
            .expect("get st")
            .expect("row");
        edit.base.pre_padding_seconds = 90;
        mgr.update_series_timer(&st_id, edit).await.expect("update");

        let after: HashMap<String, RecordingStatus> = mgr
            .all_timers()
            .await
            .expect("after")
            .into_iter()
            .filter_map(|t| t.base.id.clone().map(|id| (id, t.status)))
            .collect();
        assert_eq!(
            after.get(&hand_cancelled),
            Some(&RecordingStatus::Cancelled),
            "the fan-out must not resurrect a showing the user cancelled by hand"
        );
        assert_eq!(
            after.get(&next_up),
            Some(&RecordingStatus::New),
            "with the earliest showing off the table the next one records instead"
        );
    }

    /// The fan-out's own tidy-up pass is NOT a manual cancellation
    /// (`CancelTimerInternal(timer.Id, false, false)`, v10.11.8
    /// DefaultLiveTvService.cs:797): a timer the guide no longer justifies is
    /// cancelled but must NOT be flagged `IsManual`, or the next fan-out would
    /// treat a machine decision as the user's and could never revive it.
    #[tokio::test]
    async fn the_fan_out_tidy_up_is_not_a_manual_cancellation() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create st");
        // A pending timer claiming this series timer that the guide never
        // produced: exactly what `deleteInvalidTimers` sweeps.
        let orphan = TimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some("orphan-timer".to_owned()),
                name: Some("Not in the guide".to_owned()),
                start_date: Utc::now() + chrono::Duration::minutes(600),
                end_date: Utc::now() + chrono::Duration::minutes(660),
                ..BaseTimerInfoDto::default()
            },
            status: RecordingStatus::New,
            series_timer_id: Some(st_id.clone()),
            ..TimerInfoDto::default()
        };
        mgr.persist_timer(&orphan).await.expect("plant");

        let mut edit = mgr
            .get_series_timer(&st_id)
            .await
            .expect("get st")
            .expect("row");
        edit.base.post_padding_seconds = 30;
        mgr.update_series_timer(&st_id, edit).await.expect("update");

        // It keeps its `SeriesTimerId`, so `CancelTimerInternal` updates rather
        // than deletes it — the row survives as `Cancelled`.
        let swept = mgr
            .get_timer("orphan-timer")
            .await
            .expect("get")
            .expect("still there");
        assert_eq!(
            swept.status,
            RecordingStatus::Cancelled,
            "the tidy-up pass cancels a pending timer the guide no longer justifies"
        );
        assert!(
            !crate::dvr_repository::timer_is_manual(&mgr.db, "orphan-timer")
                .await
                .expect("flag"),
            "…and does NOT flag it manual — only a person's own cancel does that"
        );
    }

    /// `CancelSeriesTimerAsync` walks `_timerManager.GetAll()` (v10.11.8
    /// DefaultLiveTvService.cs:149-152) — not the `Completed`-excluding
    /// `GetTimersAsync` — so a child that has ALREADY recorded is deleted with
    /// the rest. Skipping it left an orphan row pointing at a series timer that
    /// no longer exists, invisible to `GET /LiveTv/Timers` (which does filter
    /// `Completed`) and therefore invisible to the parity probe as well.
    #[tokio::test]
    async fn cancelling_a_series_timer_removes_its_completed_children_too() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("a future airing");
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create st");

        let mut timers = mgr.get_timers().await.expect("timers");
        timers.sort_by_key(|t| t.base.start_date);
        let recorded = timers.first().cloned().expect("a child");
        let recorded_id = recorded.base.id.clone().expect("id");
        let mut done = recorded;
        done.status = RecordingStatus::Completed;
        mgr.persist_timer(&done).await.expect("complete it");
        assert!(
            mgr.get_timers()
                .await
                .expect("filtered")
                .iter()
                .all(|t| t.base.id.as_deref() != Some(recorded_id.as_str())),
            "the filtered view already hides it — which is why this needed a test"
        );

        mgr.cancel_series_timer(&st_id).await.expect("cancel st");
        assert!(
            mgr.all_timers().await.expect("all").is_empty(),
            "no orphan child survives its series timer, Completed or not"
        );
    }

    /// The data-loss regression: `GET /LiveTv/Timers/Defaults` hands every
    /// programme the SAME `Id` (`internal_series_timer_id("")`), and clients post
    /// that body straight back. Honouring it made every "record series" overwrite
    /// the last one. The id is the server's to mint.
    #[tokio::test]
    async fn two_creates_from_the_same_defaults_body_make_two_series_timers() {
        let mgr = manager_with_series_guide().await;
        let recurring = airings_of(&mgr, "Recurring").await;
        let other = airings_of(&mgr, "Something Else").await;
        let first = recurring
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("future airing");
        let mut a = defaults_for(&mgr, first).await;
        a.base.priority = 42;
        let b = defaults_for(&mgr, other.first().expect("other")).await;
        let posted_id = a.base.id.clone();
        assert_eq!(posted_id, b.base.id, "the defaults id is the same constant");

        let id_a = mgr.create_series_timer(a).await.expect("a");
        let id_b = mgr.create_series_timer(b).await.expect("b");
        assert_ne!(id_a, id_b, "two creates, two rows");
        assert_ne!(Some(id_a.clone()), posted_id, "the posted id is discarded");
        assert_eq!(all_series_timers(&mgr).await.len(), 2);

        // …and the published id really is the MD5 of the minted external id.
        let stored = mgr
            .stored_series_timer(&id_a)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(
            id_a,
            ferrofin_traits::stubs::internal_series_timer_id(&stored.external_id)
        );
        assert_eq!(
            stored.dto.base.external_id.as_deref(),
            Some(stored.external_id.as_str())
        );
        assert!(
            stored.dto.base.external_channel_id.is_some(),
            "ExternalChannelId is the tuner-facing channel id, not null"
        );
        assert_eq!(
            stored.dto.base.priority, 0,
            "the posted Priority is discarded on create (LiveTvManager.cs:1145-1147)"
        );
        assert!(
            id_a.len() == 32
                && id_a
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    /// A programme the guide does not have is a rejected create, not a phantom
    /// row (C# throws `InvalidOperationException("SeriesId for program not found")`).
    #[tokio::test]
    async fn create_series_timer_rejects_an_unknown_programme() {
        let mgr = manager_with_series_guide().await;
        let err = mgr
            .create_series_timer(SeriesTimerInfoDto::default())
            .await
            .expect_err("no programme");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
        let found = all_series_timers(&mgr).await;
        assert!(found.is_empty(), "{found:?}");
    }

    /// `UpdateSeriesTimerAsync` copies a fixed whitelist and no-ops on an id
    /// nothing matches — it never mints a row, and it never renames one.
    #[tokio::test]
    async fn update_series_timer_copies_only_the_whitelist() {
        use ferrofin_model::live_tv::KeepUntil;
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("future airing");
        let id = mgr
            .create_series_timer(defaults_for(&mgr, future).await)
            .await
            .expect("create");
        let before = mgr.get_series_timer(&id).await.expect("get").expect("row");
        assert_eq!(
            before.base.priority, 0,
            "create takes Priority from the standing defaults, not the posted body"
        );

        let mut posted = before.clone();
        posted.base.pre_padding_seconds = 120;
        posted.base.is_pre_padding_required = true;
        posted.base.priority = 7;
        posted.keep_up_to = 3;
        posted.base.keep_until = KeepUntil::UntilSpaceNeeded;
        posted.days = vec![DayOfWeek::Saturday, DayOfWeek::Sunday];
        posted.base.name = Some("RENAMED".to_owned());
        posted.base.overview = Some("RENAMED".to_owned());
        posted.day_pattern = Some(ferrofin_model::live_tv::DayPattern::Daily);
        mgr.update_series_timer(&id, posted).await.expect("update");

        let after = mgr
            .get_series_timer(&id)
            .await
            .expect("get2")
            .expect("row2");
        assert_eq!(after.base.pre_padding_seconds, 120);
        assert!(after.base.is_pre_padding_required);
        assert_eq!(after.base.priority, 7);
        assert_eq!(after.keep_up_to, 3);
        assert_eq!(after.base.keep_until, KeepUntil::UntilSpaceNeeded);
        assert_eq!(after.base.name, before.base.name, "Name is not updatable");
        assert_eq!(after.base.overview, before.base.overview);
        assert_eq!(
            after.day_pattern,
            Some(ferrofin_model::live_tv::DayPattern::Weekends),
            "DayPattern is recomputed from Days, never the client's"
        );
        assert_eq!(after.base.id, before.base.id);
        assert_eq!(after.base.external_id, before.base.external_id);

        // An id nothing matches writes nothing at all.
        let ghost = "0000000000000000000000000000dead";
        mgr.update_series_timer(ghost, after)
            .await
            .expect("no-op update");
        assert!(mgr.get_series_timer(ghost).await.expect("g").is_none());
        assert_eq!(all_series_timers(&mgr).await.len(), 1);
    }

    /// `UpdateTimerAsync` takes the four padding fields and discards the rest of
    /// the posted DTO; an unknown id is a `404`, not a phantom timer.
    #[tokio::test]
    async fn update_timer_takes_only_the_padding_fields() {
        let mgr = manager_with_series_guide().await;
        let airings = airings_of(&mgr, "Recurring").await;
        let future = airings
            .iter()
            .find(|r| parse_dt(&r.start_date).is_some_and(|s| s > Utc::now()))
            .expect("future airing");
        let defaults = defaults_for(&mgr, future).await;
        let id = mgr
            .create_timer(TimerInfoDto {
                base: defaults.base.clone(),
                ..TimerInfoDto::default()
            })
            .await
            .expect("create timer");
        let before = mgr.get_timer(&id).await.expect("get").expect("row");

        let mut posted = before.clone();
        posted.base.pre_padding_seconds = 300;
        posted.base.post_padding_seconds = 600;
        posted.base.is_pre_padding_required = true;
        posted.base.is_post_padding_required = true;
        posted.base.name = Some("MUTATED".to_owned());
        posted.base.priority = 42;
        posted.status = RecordingStatus::Cancelled;
        posted.base.start_date = before.base.start_date + chrono::Duration::days(365);
        mgr.update_timer(&id, posted).await.expect("update");

        let after = mgr.get_timer(&id).await.expect("get2").expect("row2");
        assert_eq!(after.base.pre_padding_seconds, 300);
        assert_eq!(after.base.post_padding_seconds, 600);
        assert!(after.base.is_pre_padding_required && after.base.is_post_padding_required);
        assert_eq!(after.base.name, before.base.name);
        assert_eq!(after.base.priority, before.base.priority);
        assert_eq!(after.status, before.status);
        assert_eq!(after.base.start_date, before.base.start_date);

        let ghost = "00000000000000000000000000009999";
        assert!(matches!(
            mgr.update_timer(ghost, after).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(mgr.get_timer(ghost).await.expect("g").is_none());
    }

    /// `SearchForDuplicateShowIds`' three skips and its earliest-wins rule.
    #[test]
    fn duplicate_showings_keep_the_earliest_and_honour_the_skips() {
        let showing = |show_id: &str, minutes: i64| {
            let start =
                parse_dt("2026-07-25T06:00:00Z").expect("dt") + chrono::Duration::minutes(minutes);
            (
                show_id.to_owned(),
                start,
                TimerInfoDto {
                    base: ferrofin_model::live_tv::BaseTimerInfoDto {
                        id: Some(format!("{show_id}-{minutes}")),
                        start_date: start,
                        ..ferrofin_model::live_tv::BaseTimerInfoDto::default()
                    },
                    ..TimerInfoDto::default()
                },
            )
        };
        let ids = |v: Vec<TimerInfoDto>| {
            let mut out: Vec<String> = v
                .into_iter()
                .map(|t| t.base.id.unwrap_or_default())
                .collect();
            out.sort();
            out
        };

        // Three showings of one programme, posted out of order: the earliest
        // records, the two later ones are cancelled.
        let group = [showing("aa", 120), showing("aa", 0), showing("aa", 60)];
        assert_eq!(
            ids(duplicate_showings_to_cancel(&group)),
            ["aa-120", "aa-60"]
        );
        // A lone showing is not a duplicate…
        let found = duplicate_showings_to_cancel(&[showing("aa", 0)]);
        assert!(found.is_empty(), "{found:?}");
        // …a blank ShowId groups nothing (upstream skips the empty key)…
        let found = duplicate_showings_to_cancel(&[showing("", 0), showing("", 60)]);
        assert!(found.is_empty(), "{found:?}");
        // …and neither does one ending "0000" (jellyfin/jellyfin#5856).
        let found = duplicate_showings_to_cancel(&[showing("ab0000", 0), showing("ab0000", 60)]);
        assert!(found.is_empty(), "{found:?}");
        // Different programmes never suppress each other.
        let found = duplicate_showings_to_cancel(&[showing("aa", 0), showing("bb", 60)]);
        assert!(found.is_empty(), "{found:?}");
    }

    /// `AlphanumericComparator.CompareValues`, against the C# as the oracle.
    ///
    /// The digit-run arms are v10.11.8's (`Jellyfin.Extensions/AlphanumericComparator.cs`),
    /// including the trailing `len1 - len2` tie-break master dropped when it
    /// moved to `CompareOptions.NumericOrdering`; the text arm is the
    /// `StringComparison.InvariantCulture` BOTH trees ask for.
    #[test]
    fn alphanumeric_compare_matches_the_upstream_comparator() {
        // Numbers sort as numbers, not as text.
        assert_eq!(
            alphanumeric_compare("Show 2", "Show 10"),
            std::cmp::Ordering::Less
        );
        // Leading zeros do not change the NUMBER, but upstream's final
        // `return len1 - len2` still breaks the tie on the raw lengths.
        assert_eq!(
            alphanumeric_compare("Show 007", "Show 7"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            alphanumeric_compare("Show", "Show 1"),
            std::cmp::Ordering::Less
        );
        assert_eq!(alphanumeric_compare("", "a"), std::cmp::Ordering::Less);
        // `StringComparison.InvariantCulture` is CLDR root collation, not
        // code-point order: the primary level orders by letter, so a lowercase
        // 'a' sorts before an uppercase 'B' even though 'B'(0x42) < 'a'(0x61).
        assert_eq!(
            alphanumeric_compare("apple parity", "Banana parity"),
            std::cmp::Ordering::Less
        );
        assert_eq!(alphanumeric_compare("a", "B"), std::cmp::Ordering::Less);
        // …and the tertiary level puts lowercase before uppercase within a
        // letter, which code-point order has backwards.
        assert_eq!(alphanumeric_compare("a", "A"), std::cmp::Ordering::Less);
        // A multi-byte run is segmented on char boundaries, not byte ones.
        assert_eq!(
            alphanumeric_compare("Café 2", "Café 10"),
            std::cmp::Ordering::Less
        );
    }

    /// `GetDayPattern`, `AlphanumericComparator` and `GetSeriesTimers`' ordering,
    /// against the C# as the oracle — including upstream's inverted priority arm.
    #[test]
    fn day_pattern_and_series_timer_ordering_match_upstream() {
        use ferrofin_model::live_tv::{DayPattern, SeriesTimerQuery};
        assert_eq!(day_pattern(&[]), None);
        assert_eq!(
            day_pattern(&[DayOfWeek::Saturday, DayOfWeek::Sunday]),
            Some(DayPattern::Weekends)
        );
        assert_eq!(
            day_pattern(&[DayOfWeek::Monday, DayOfWeek::Wednesday]),
            None
        );
        assert_eq!(
            day_pattern(&[
                DayOfWeek::Monday,
                DayOfWeek::Tuesday,
                DayOfWeek::Wednesday,
                DayOfWeek::Thursday,
                DayOfWeek::Friday
            ]),
            Some(DayPattern::Weekdays)
        );
        assert_eq!(
            day_pattern(&[
                DayOfWeek::Sunday,
                DayOfWeek::Monday,
                DayOfWeek::Tuesday,
                DayOfWeek::Wednesday,
                DayOfWeek::Thursday,
                DayOfWeek::Friday,
                DayOfWeek::Saturday
            ]),
            Some(DayPattern::Daily)
        );
        // A 5-day list that is not Mon–Fri spells no pattern.
        assert_eq!(
            day_pattern(&[
                DayOfWeek::Sunday,
                DayOfWeek::Monday,
                DayOfWeek::Tuesday,
                DayOfWeek::Wednesday,
                DayOfWeek::Thursday
            ]),
            None
        );

        let timer = |name: &str, priority: i32| SeriesTimerInfoDto {
            base: ferrofin_model::live_tv::BaseTimerInfoDto {
                name: Some(name.to_owned()),
                priority,
                ..ferrofin_model::live_tv::BaseTimerInfoDto::default()
            },
            ..SeriesTimerInfoDto::default()
        };
        let names = |v: &[SeriesTimerInfoDto]| {
            v.iter()
                .map(|t| t.base.name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        let mut v = vec![timer("B", 0), timer("A", 9), timer("C", 9)];
        sort_series_timers(&mut v, &SeriesTimerQuery::default());
        assert_eq!(names(&v), ["A", "B", "C"], "no sortBy: by name ascending");

        sort_series_timers(
            &mut v,
            &SeriesTimerQuery {
                sort_by: Some("priority".to_owned()),
                sort_order: SortOrder::Ascending,
            },
        );
        assert_eq!(
            names(&v),
            ["A", "C", "B"],
            "upstream's ascending arm is OrderByDescending(Priority).ThenByString(Name)"
        );
        sort_series_timers(
            &mut v,
            &SeriesTimerQuery {
                sort_by: Some("Priority".to_owned()),
                sort_order: SortOrder::Descending,
            },
        );
        assert_eq!(
            names(&v),
            ["B", "C", "A"],
            "…and its descending arm is OrderBy(Priority).ThenByStringDescending(Name)"
        );
    }

    // ---- Channel query/projection (plan A) --------------------------------

    /// `get_channels` plus the `AddChannelInfo` pass the DTO service runs for
    /// it.
    ///
    /// In production this is one call: upstream's `DtoService.GetBaseItemDtos`
    /// buckets `item is LiveTvChannel` and finishes the DTOs through
    /// `LivetvManager.AddChannelInfo` (v10.11.8 DtoService.cs:168-192), and
    /// `FerrofinDtoService` does the same. These tests project through
    /// `FakeDto`, which has no Live TV seam, so the second half is applied here
    /// — the same two steps in the same order, over the same options the list
    /// path strips.
    async fn channels_with_info(
        mgr: &FerrofinLiveTvManager,
        query: &LiveTvChannelQuery,
        options: &DtoOptions,
    ) -> ferrofin_model::querying::QueryResult<BaseItemDto> {
        let mut page = mgr.get_channels(query, options).await.expect("channels");
        let mut list_options = options.clone();
        crate::projection::remove_fields(&mut list_options);
        mgr.add_channel_info(&mut page.items, &list_options, None)
            .await
            .expect("channel info");
        page
    }

    /// [`channels_with_info`]'s single-channel twin (the detail path strips
    /// nothing).
    async fn channel_with_info(
        mgr: &FerrofinLiveTvManager,
        id: Uuid,
        options: &DtoOptions,
    ) -> Option<BaseItemDto> {
        let mut dto = vec![mgr.get_channel(id, None, options).await.expect("get")?];
        mgr.add_channel_info(&mut dto, options, None)
            .await
            .expect("channel info");
        dto.pop()
    }

    /// The list path: `AddChannelInfo` fields land, the four detail fields are
    /// stripped (`RemoveFields`), and each channel carries its currently-airing
    /// programme from the one page-wide airing query.
    #[tokio::test]
    async fn channel_list_attaches_channel_info_and_current_program() {
        let mgr = manager_with_relative_guide().await;
        let channels =
            channels_with_info(&mgr, &LiveTvChannelQuery::default(), &DtoOptions::default()).await;
        assert_eq!(channels.total_record_count, 2);
        assert_eq!(channels.start_index, 0);

        let one = &channels.items[0];
        assert_eq!(one.name.as_deref(), Some("Channel One"));
        assert_eq!(one.number.as_deref(), Some("1"));
        assert_eq!(one.channel_number.as_deref(), Some("1"));
        assert_eq!(
            one.channel_type,
            Some(ferrofin_model::live_tv::ChannelType::Tv)
        );
        // RemoveFields ran on the list even though all fields were requested.
        assert_eq!(one.etag, None, "Etag is stripped on the list path");

        let program = one.current_program.as_deref().expect("current program");
        assert_eq!(program.name.as_deref(), Some("Now Playing"));
        assert_eq!(program.channel_id, Some(one.id));
        assert_eq!(program.etag, None, "programme DTOs are stripped too");
        assert!(program.run_time_ticks.is_some());

        // Channel Two's airing is News-classified; only true flags appear.
        let two = &channels.items[1];
        let two_program = two.current_program.as_deref().expect("two's program");
        assert_eq!(two_program.name.as_deref(), Some("Two Now"));
        assert_eq!(two_program.is_news, Some(true));
        assert_eq!(two_program.is_movie, None);
    }

    /// `addCurrentProgram=false` suppresses the airing query entirely.
    #[tokio::test]
    async fn channel_list_can_skip_the_current_program() {
        let mgr = manager_with_relative_guide().await;
        let options = DtoOptions {
            add_current_program: false,
            ..DtoOptions::default()
        };
        let channels = mgr
            .get_channels(&LiveTvChannelQuery::default(), &options)
            .await
            .expect("channels");
        assert!(
            channels.items.iter().all(|c| c.current_program.is_none()),
            "no current program when the option is off"
        );
    }

    /// Type filter, the flag filters (channels carry no movie/news flags, so a
    /// `true` filter matches nothing) and paging with the true total.
    #[tokio::test]
    async fn channel_list_filters_and_pages() {
        let mgr = manager_with_relative_guide().await;

        let radio = mgr
            .get_channels(
                &LiveTvChannelQuery {
                    channel_type: Some(ferrofin_model::live_tv::ChannelType::Radio),
                    ..LiveTvChannelQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("radio");
        assert_eq!(radio.total_record_count, 0);

        let movies = mgr
            .get_channels(
                &LiveTvChannelQuery {
                    is_movie: Some(true),
                    ..LiveTvChannelQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("movies");
        assert_eq!(
            movies.total_record_count, 0,
            "no guide programme classifies as a movie, so no channel aggregates the flag"
        );

        let page = mgr
            .get_channels(
                &LiveTvChannelQuery {
                    start_index: Some(1),
                    limit: Some(5),
                    ..LiveTvChannelQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("page");
        assert_eq!(page.total_record_count, 2, "total is pre-paging");
        assert_eq!(page.start_index, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].name.as_deref(), Some("Channel Two"));
    }

    /// Favourite filter + favourite-first sorting over a real `UserData` row.
    #[tokio::test]
    async fn channel_favorites_filter_and_sort_first() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let users: Arc<dyn ferrofin_traits::library::UserManager> = Arc::new(
            ferrofin_core::user_manager::FerrofinUserManager::new(db.clone()),
        );
        let user = users.create_user("couch").await.expect("user");
        let mgr = FerrofinLiveTvManager::new(
            db.clone(),
            Arc::new(FakeFetcher(sources)),
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_dto(Arc::new(FakeDto));
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.refresh_guide().await.expect("refresh");

        // Favourite Channel Two the way the playstate endpoint stores it.
        let two = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("channels")
            .items
            .into_iter()
            .find(|c| c.name.as_deref() == Some("Channel Two"))
            .expect("two");
        let two_db = ferrofin_db::store::guid_to_db(two.id);
        // `UserData.ItemId` is FK-bound to `BaseItems`; satisfy it with a
        // minimal row so the favourite write lands (a real favourite on a
        // channel needs the same backing row — see the ChannelUserData note).
        crate::guide_repository::test_support::seed_base_item_stub(&db, &two_db)
            .await
            .expect("base item row");
        crate::guide_repository::test_support::seed_favorite(&db, &two_db, &user.id)
            .await
            .expect("user data");

        let favorites = mgr
            .get_channels(
                &LiveTvChannelQuery {
                    user: Some(user.clone()),
                    is_favorite: Some(true),
                    ..LiveTvChannelQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("favorites");
        assert_eq!(favorites.total_record_count, 1);
        assert_eq!(favorites.items[0].name.as_deref(), Some("Channel Two"));

        let sorted = mgr
            .get_channels(
                &LiveTvChannelQuery {
                    user: Some(user.clone()),
                    enable_favorite_sorting: true,
                    ..LiveTvChannelQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("sorted");
        assert_eq!(
            sorted.items[0].name.as_deref(),
            Some("Channel Two"),
            "favourites sort first, then SortName"
        );
    }

    /// The default channel order follows Jellyfin 12's `CreateSortName`
    /// (`{0:0000000000.00000}`): six-digit numbers sort numerically, and
    /// subchannels that agree to one decimal still sort by number.
    #[tokio::test]
    async fn channels_sort_by_number_like_jellyfin_12() {
        let m3u = "#EXTM3U\n\
            #EXTINF:-1 tvg-id=\"a\" tvg-chno=\"100010\",Hundred Ten\nhttp://tuner/a\n\
            #EXTINF:-1 tvg-id=\"b\" tvg-chno=\"10002\",Ten Two\nhttp://tuner/b\n\
            #EXTINF:-1 tvg-id=\"c\" tvg-chno=\"10001\",Ten One\nhttp://tuner/c\n\
            #EXTINF:-1 tvg-id=\"d\" tvg-chno=\"4.12\",Alpha\nhttp://tuner/d\n\
            #EXTINF:-1 tvg-id=\"e\" tvg-chno=\"4.11\",Zulu\nhttp://tuner/e\n";
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), m3u.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.refresh_guide().await.expect("refresh");

        let names: Vec<String> = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("channels")
            .items
            .into_iter()
            .filter_map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            ["Zulu", "Alpha", "Ten One", "Ten Two", "Hundred Ten"],
            "4.11 < 4.12 < 10001 < 10002 < 100010"
        );
    }

    /// The single-channel path keeps every requested field (no `RemoveFields`)
    /// and carries the `ExternalServiceId` provider id.
    #[tokio::test]
    async fn single_channel_keeps_detail_fields() {
        let mgr = manager_with_relative_guide().await;
        let id = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("channels")
            .items[0]
            .id;

        let channel = channel_with_info(&mgr, id, &DtoOptions::default())
            .await
            .expect("some");
        assert_eq!(channel.etag.as_deref(), Some("fake-etag"));
        assert_eq!(
            channel
                .provider_ids
                .as_ref()
                .and_then(|p| p.get("ExternalServiceId"))
                .map(String::as_str),
            Some("Emby")
        );
        // The current programme still attaches — with the four fields stripped.
        let program = channel.current_program.as_deref().expect("program");
        assert_eq!(program.name.as_deref(), Some("Now Playing"));
        assert_eq!(program.etag, None);

        assert!(
            mgr.get_channel(Uuid::new_v4(), None, &DtoOptions::default())
                .await
                .expect("get")
                .is_none(),
            "unknown channel is None"
        );
    }

    /// A channel keeps its first-seen `DateCreated` across guide refreshes
    /// (upstream stamps it only on a NEW item).
    #[tokio::test]
    async fn channel_date_created_survives_refresh() {
        let mgr = manager_with_relative_guide().await;
        // Pin a sentinel first-seen instant, then refresh again.
        crate::guide_repository::test_support::pin_channel_dates(
            &mgr.db,
            "2000-01-01 00:00:00.0000000",
        )
        .await
        .expect("pin");
        mgr.refresh_guide().await.expect("refresh again");
        let kept = crate::guide_repository::test_support::channel_dates(&mgr.db)
            .await
            .expect("read");
        assert!(!kept.is_empty(), "{kept:?}");
        assert!(
            kept.iter().all(|d| d == "2000-01-01 00:00:00.0000000"),
            "refresh must not restamp DateCreated: {kept:?}"
        );
    }

    /// The channel kind filters match upstream's split: `isMovie`/`isSeries`
    /// hit the aggregated channel columns, `isKids` the aggregated "Kids"
    /// tag, and `isNews`/`isSports` translate to channel tags the guide never
    /// writes — `true` matches nothing.
    #[tokio::test]
    async fn channel_kind_filters_follow_the_guide_aggregation() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(CLASSIFIED_XMLTV));
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");

        // Every classified programme airs on Channel One; Channel Two is bare.
        let names = |result: ferrofin_model::querying::QueryResult<BaseItemDto>| {
            result
                .items
                .into_iter()
                .filter_map(|c| c.name)
                .collect::<Vec<_>>()
        };
        let with = |query: LiveTvChannelQuery| {
            let mgr = mgr.clone();
            async move {
                mgr.get_channels(&query, &DtoOptions::default())
                    .await
                    .expect("channels")
            }
        };
        assert_eq!(
            names(
                with(LiveTvChannelQuery {
                    is_movie: Some(true),
                    ..LiveTvChannelQuery::default()
                })
                .await
            ),
            ["Channel One"]
        );
        assert_eq!(
            names(
                with(LiveTvChannelQuery {
                    is_movie: Some(false),
                    ..LiveTvChannelQuery::default()
                })
                .await
            ),
            ["Channel Two"]
        );
        assert_eq!(
            names(
                with(LiveTvChannelQuery {
                    is_series: Some(true),
                    ..LiveTvChannelQuery::default()
                })
                .await
            ),
            ["Channel One"]
        );
        assert_eq!(
            names(
                with(LiveTvChannelQuery {
                    is_kids: Some(true),
                    ..LiveTvChannelQuery::default()
                })
                .await
            ),
            ["Channel One"]
        );
        // News/Sports are channel *tags* upstream never writes: true matches
        // nothing, false everything — even though Channel One airs news.
        let found = names(
            with(LiveTvChannelQuery {
                is_news: Some(true),
                ..LiveTvChannelQuery::default()
            })
            .await,
        );
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            with(LiveTvChannelQuery {
                is_news: Some(false),
                ..LiveTvChannelQuery::default()
            })
            .await
            .total_record_count,
            2
        );
    }

    // ---- Programme projection (plan B) ------------------------------------

    /// The list path strips the four detail fields and only sends
    /// `ChannelName`/`ChannelNumber`/`MediaType` when the `ChannelInfo` field
    /// was requested (Jellyfin sends none of them on a default list).
    #[tokio::test]
    async fn program_list_gates_channel_info_on_the_field() {
        let mgr = manager_with_relative_guide().await;

        // Default list options: no fields at all (the handler's
        // `program_dto_options` starts empty like C# `DtoOptions{Fields}`).
        let bare = DtoOptions {
            fields: Vec::new(),
            ..DtoOptions::default()
        };
        let plain = mgr
            .get_programs(&InternalItemsQuery::default(), &bare)
            .await
            .expect("programs");
        let now_playing = plain
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some("Now Playing"))
            .expect("now playing");
        assert_eq!(now_playing.channel_name, None, "ChannelInfo not requested");
        assert_eq!(now_playing.channel_number, None);
        assert_eq!(now_playing.overview, None, "Overview is field-gated");
        assert!(now_playing.start_date.is_some());
        assert!(now_playing.run_time_ticks.is_some());

        // All-fields list: ChannelInfo is in the set, but RemoveFields still
        // strips Etag/CanDelete/CanDownload/DisplayPreferencesId.
        let all = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("programs all");
        let with_info = all
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some("Now Playing"))
            .expect("now playing");
        assert_eq!(with_info.channel_name.as_deref(), Some("Channel One"));
        assert_eq!(with_info.channel_number.as_deref(), Some("1"));
        assert_eq!(
            with_info.media_type,
            ferrofin_model::data::MediaType::Video,
            "ChannelInfo substitutes the channel's media type"
        );
        assert_eq!(with_info.etag, None, "Etag is stripped on the list path");
    }

    /// The single programme keeps every requested field — no `RemoveFields` on
    /// `GetProgram` — and an unknown id is `None`.
    #[tokio::test]
    async fn single_program_keeps_detail_fields() {
        let mgr = manager_with_relative_guide().await;
        let id = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("programs")
            .items
            .into_iter()
            .find(|p| p.name.as_deref() == Some("Two Now"))
            .expect("two now")
            .id;

        let program = mgr
            .get_program(id, None, &DtoOptions::default())
            .await
            .expect("get")
            .expect("some");
        assert_eq!(program.etag.as_deref(), Some("fake-etag"));
        assert_eq!(program.channel_name.as_deref(), Some("Channel Two"));
        assert_eq!(program.is_news, Some(true), "News category classifies");

        assert!(
            mgr.get_program(Uuid::new_v4(), None, &DtoOptions::default())
                .await
                .expect("get")
                .is_none()
        );
    }

    /// `LiveTvManager.GetPrograms`' `SeriesTimerId`: the series timer whose
    /// internal id matches (ignoring case) scopes the guide to its `SeriesId`,
    /// the listings series id its programme carried. An unknown timer, or one
    /// with no series id, answers nothing ("Better to return nothing than
    /// every program in the database"), and a blank value scopes nothing.
    #[tokio::test]
    async fn series_timer_id_scopes_the_guide_to_the_timers_series() {
        use std::fmt::Write as _;
        let now = Utc::now();
        let mut xml = String::from(
            "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
        );
        // "Recurring" carries episode numbers, so its listings have a series
        // id; "Movie Night" carries none.
        for (channel, from, to, title, episode) in [
            ("one.tv", 60, 120, "Recurring", Some("0.0.")),
            ("one.tv", 1500, 1560, "Recurring", Some("0.1.")),
            ("two.tv", 90, 150, "Recurring", Some("0.2.")),
            ("one.tv", 200, 260, "Something Else", Some("0.0.")),
            ("one.tv", 300, 400, "Movie Night", None),
        ] {
            let start = (now + chrono::Duration::minutes(from)).format("%Y%m%d%H%M%S");
            let stop = (now + chrono::Duration::minutes(to)).format("%Y%m%d%H%M%S");
            let episode = episode.map_or_else(String::new, |e| {
                format!("<episode-num system=\"xmltv_ns\">{e}</episode-num>")
            });
            let _ = write!(
                xml,
                "<programme start=\"{start} +0000\" stop=\"{stop} +0000\" channel=\"{channel}\">\
                 <title>{title}</title>{episode}</programme>"
            );
        }
        xml.push_str("</tv>");
        let mgr = manager_with_guide(xml).await;
        let titles = |query: InternalItemsQuery| {
            let mgr = &mgr;
            async move {
                let mut names: Vec<String> = mgr
                    .get_programs(&query, &DtoOptions::default())
                    .await
                    .expect("programs")
                    .items
                    .into_iter()
                    .filter_map(|p| p.name)
                    .collect();
                names.sort();
                names
            }
        };
        let scoped = |id: &str| InternalItemsQuery {
            series_timer_id: Some(id.to_owned()),
            ..InternalItemsQuery::default()
        };

        let first = airings_of(&mgr, "Recurring").await;
        let st_id = mgr
            .create_series_timer(defaults_for(&mgr, &first[0]).await)
            .await
            .expect("create st");
        for id in [st_id.clone(), st_id.to_uppercase()] {
            assert_eq!(
                titles(scoped(&id)).await,
                ["Recurring", "Recurring", "Recurring"],
                "{id}"
            );
        }
        assert!(
            titles(scoped("not-a-timer")).await.is_empty(),
            "unknown timer"
        );

        // A series timer whose programme carried no series id scopes nothing.
        let movie = airings_of(&mgr, "Movie Night").await;
        let movie_st = mgr
            .create_series_timer(defaults_for(&mgr, &movie[0]).await)
            .await
            .expect("create movie st");
        assert!(titles(scoped(&movie_st)).await.is_empty(), "no series id");

        for blank in ["", "   "] {
            assert_eq!(
                titles(scoped(blank)).await.len(),
                5,
                "a blank seriesTimerId must not blank the guide ({blank:?})"
            );
        }
    }

    /// `AddRecordingInfo`: a timer whose `ProgramId` matches the programme's
    /// `ExternalId` links `TimerId`/`Status`/`SeriesTimerId`; a cancelled
    /// timer stays invisible.
    #[tokio::test]
    async fn program_dtos_link_their_recording_timer() {
        use ferrofin_model::live_tv::{BaseTimerInfoDto, RecordingStatus, TimerInfoDto};
        let mgr = manager_with_relative_guide().await;

        // The guide row's ExternalId is `{channelId}_{start:O}`; read it back
        // from the stored rows rather than re-deriving it here.
        let rows = mgr
            .query_program_rows(&InternalItemsQuery::default(), chrono::Utc::now())
            .await
            .expect("rows");
        let target = rows
            .iter()
            .find(|r| r.title == "Now Playing")
            .expect("now playing row");
        let external_id = target.external_id.clone().expect("external id");

        let timer_id = mgr
            .create_timer(TimerInfoDto {
                status: RecordingStatus::New,
                series_timer_id: Some("st-9".to_owned()),
                base: BaseTimerInfoDto {
                    channel_id: Uuid::parse_str(&target.channel_id).expect("guid"),
                    program_id: Some(external_id.clone()),
                    // Far enough out that the scheduler leaves it alone: this
                    // test is about the programme→timer link, not the capture.
                    start_date: chrono::Utc::now() + chrono::Duration::hours(6),
                    end_date: chrono::Utc::now() + chrono::Duration::hours(7),
                    ..BaseTimerInfoDto::default()
                },
                ..TimerInfoDto::default()
            })
            .await
            .expect("timer");

        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("programs");
        let linked = programs
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some("Now Playing"))
            .expect("linked");
        assert_eq!(linked.timer_id.as_deref(), Some(timer_id.as_str()));
        assert_eq!(linked.status.as_deref(), Some("New"));
        assert_eq!(linked.series_timer_id.as_deref(), Some("st-9"));
        let unlinked = programs
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some("Aired"))
            .expect("unlinked");
        assert_eq!(unlinked.timer_id, None);
        assert_eq!(unlinked.status, None);

        // A cancelled timer keeps its SeriesTimerId link but drops
        // TimerId/Status (upstream's `!= Cancelled && != Error` gate).
        // Through the real cancel path: `UpdateTimerAsync` only ever takes the
        // four padding fields, so a client cannot set `Status` this way.
        mgr.cancel_timer(&timer_id).await.expect("cancel");
        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("programs");
        let after = programs
            .items
            .iter()
            .find(|p| p.name.as_deref() == Some("Now Playing"))
            .expect("after");
        assert_eq!(after.timer_id, None);
        assert_eq!(after.status, None);
    }

    /// Countries come off the shared fetcher and cache under the manager's own
    /// cache directory (`{cache}/sd-countries.json`, as upstream).
    #[tokio::test]
    async fn schedules_direct_countries_come_from_the_shared_fetcher_and_cache_dir() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let mut map = HashMap::new();
        map.insert(
            format!("{}/available/countries", crate::schedules_direct::API_URL),
            r#"{"Europe":[{"shortName":"GBR"}]}"#.to_owned(),
        );
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let mgr = FerrofinLiveTvManager::new(
            db,
            std::sync::Arc::new(FakeFetcher(map)),
            "srv".to_owned(),
            cache_dir.path(),
        );

        let bytes = mgr
            .get_schedules_direct_countries()
            .await
            .expect("countries");
        assert_eq!(bytes, br#"{"Europe":[{"shortName":"GBR"}]}"#);
        // The disk cache lands in the manager's cache directory.
        assert_eq!(
            std::fs::read(cache_dir.path().join("sd-countries.json")).expect("cache file"),
            bytes
        );
        // Debug stays free of the fetcher and cache internals.
        assert!(format!("{mgr:?}").contains("srv"));
    }

    // ---- tuner-host / listings-provider administration -------------------

    /// A manager over the two-channel M3U and the one-channel XMLTV guide,
    /// with the tuner host and listings provider already saved.
    async fn mapping_manager() -> (FerrofinLiveTvManager, String, String) {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        // A second playlist, for the tests that save another tuner.
        sources.insert("http://tuner/other.m3u".to_owned(), M3U.to_owned());
        // `dated`, not the literal fixture: the refresh ingests only
        // `[now - 1h, now - 1h + GuideDays)`, so a fixture pinned to a calendar
        // date silently empties itself once that date rolls past.
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(XMLTV));
        let mgr = manager_with(FakeFetcher(sources)).await;
        let tuner = mgr
            .save_tuner_host(TunerHostInfo {
                // `SaveTunerHost` resolves the host by `Type` first and 404s when nothing
                // claims it (v10.11.8 TunerHostManager.cs:60-69), so the fixture names the
                // M3U host this playlist belongs to.
                type_: Some("m3u".to_owned()),
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("tuner");
        let provider = mgr
            .save_listing_provider(
                as_xmltv(ListingsProviderInfo {
                    path: Some("http://guide/xmltv.xml".to_owned()),
                    ..ListingsProviderInfo::default()
                }),
                false,
                false,
            )
            .await
            .expect("provider");
        mgr.refresh_guide().await.expect("refresh");
        (
            mgr,
            tuner.id.expect("tuner id"),
            provider.id.expect("provider id"),
        )
    }

    #[tokio::test]
    async fn config_ids_are_minted_the_way_guid_tostring_n_does() {
        let (_mgr, tuner_id, provider_id) = mapping_manager().await;
        // `Guid.NewGuid().ToString("N", InvariantCulture)` — 32 lowercase hex,
        // no dashes (TunerHostManager.cs:85, ListingsManager.cs:69). The old
        // uppercase-hyphenated form is what a client round-trips into
        // `?providerId=` and `?id=`, so it is visible, not cosmetic.
        for id in [&tuner_id, &provider_id] {
            assert_eq!(id.len(), 32, "{id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn a_supplied_id_is_honored_only_when_it_names_an_existing_row() {
        let (mgr, tuner_id, _) = mapping_manager().await;
        // `index == -1` → a fresh guid, whatever the client asked for.
        let invented = mgr
            .save_tuner_host(TunerHostInfo {
                // `SaveTunerHost` resolves the host by `Type` first and 404s when nothing
                // claims it (v10.11.8 TunerHostManager.cs:60-69), so the fixture names the
                // M3U host this playlist belongs to.
                type_: Some("m3u".to_owned()),
                id: Some("not-a-configured-host".to_owned()),
                url: Some("http://tuner/other.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("save");
        assert_ne!(invented.id.as_deref(), Some("not-a-configured-host"));
        assert_eq!(mgr.get_tuner_hosts().await.expect("hosts").len(), 2);

        // An existing id — in a different case — updates that row in place
        // (`Array.FindIndex(..., OrdinalIgnoreCase)`), it does not add a third.
        mgr.save_tuner_host(TunerHostInfo {
            // `SaveTunerHost` resolves the host by `Type` first and 404s when nothing
            // claims it (v10.11.8 TunerHostManager.cs:60-69), so the fixture names the
            // M3U host this playlist belongs to.
            type_: Some("m3u".to_owned()),
            id: Some(tuner_id.to_ascii_uppercase()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            friendly_name: Some("renamed".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("update");
        let hosts = mgr.get_tuner_hosts().await.expect("hosts");
        assert_eq!(hosts.len(), 2);
        assert!(
            hosts
                .iter()
                .any(|h| h.friendly_name.as_deref() == Some("renamed"))
        );
    }

    #[tokio::test]
    async fn deletes_match_the_id_case_insensitively() {
        let (mgr, tuner_id, provider_id) = mapping_manager().await;
        // C# filters both lists with `StringComparison.OrdinalIgnoreCase`; the
        // stored key is a BINARY-collated TEXT primary key, so a case-differing
        // id used to 204 and delete nothing.
        mgr.delete_listing_provider(&provider_id.to_ascii_uppercase())
            .await
            .expect("delete provider");
        let listing_providers = mgr.get_listing_providers().await.expect("providers");
        assert!(listing_providers.is_empty(), "{listing_providers:?}");
        mgr.delete_tuner_host(&tuner_id.to_ascii_uppercase())
            .await
            .expect("delete tuner");
        let tuner_hosts = mgr.get_tuner_hosts().await.expect("hosts");
        assert!(tuner_hosts.is_empty(), "{tuner_hosts:?}");
    }

    #[tokio::test]
    async fn reset_tuner_validates_the_service_prefix() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        // `LiveTvManager.ResetTuner` splits `{service key}_{tuner id}` and
        // throws `ArgumentException("Service not found.")` — 400 — when the
        // first segment names no registered service.
        assert_eq!(live_tv_service_key(), "af999c25a00715699361240d4c6c7a53");
        mgr.reset_tuner(&format!("{}_1", live_tv_service_key()))
            .await
            .expect("known service");
        // OrdinalIgnoreCase.
        mgr.reset_tuner(&format!(
            "{}_tuner0",
            live_tv_service_key().to_ascii_uppercase()
        ))
        .await
        .expect("known service, upper");
        // A bare prefix with no `_` is where the C# indexes `parts[1]` of a
        // one-element split and crashes; Ferrofin no-ops rather than 500.
        mgr.reset_tuner(live_tv_service_key())
            .await
            .expect("bare prefix");
        assert!(matches!(
            mgr.reset_tuner("abc").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn channel_mapping_options_matches_the_lineup_against_the_guide() {
        let (mgr, _tuner_id, provider_id) = mapping_manager().await;
        let opts = mgr
            .get_channel_mapping_options(&provider_id)
            .await
            .expect("options");

        assert_eq!(opts.provider_name.as_deref(), Some("XmlTV"));
        assert_eq!(opts.mappings, Vec::new());
        // `XmlTvListingsProvider.GetChannels` → the guide's `<channel>` list.
        assert_eq!(opts.provider_channels.len(), 1);
        assert_eq!(opts.provider_channels[0].id.as_deref(), Some("one.tv"));
        assert_eq!(
            opts.provider_channels[0].name.as_deref(),
            Some("Channel One")
        );

        // `GetTunerChannelMapping`: "{Number} {Name}", the external
        // `ChannelInfo.Id`, and the provider columns only where a guide channel
        // matched (channel two has no counterpart in this guide).
        assert_eq!(opts.tuner_channels.len(), 2);
        assert_eq!(
            opts.tuner_channels[0].name.as_deref(),
            Some("1 Channel One")
        );
        assert_eq!(
            opts.tuner_channels[0].id.as_deref(),
            Some(
                crate::mapping::m3u_channel_id("http://tuner/playlist.m3u", "http://tuner/one")
                    .as_str()
            )
        );
        assert_eq!(
            opts.tuner_channels[0].provider_channel_id.as_deref(),
            Some("one.tv")
        );
        assert_eq!(
            opts.tuner_channels[0].provider_channel_name.as_deref(),
            Some("Channel One")
        );
        assert_eq!(
            opts.tuner_channels[1].name.as_deref(),
            Some("2 Channel Two")
        );
        assert!(opts.tuner_channels[1].provider_channel_id.is_none());

        // An unresolvable provider id is 404, not a 200 with an empty DTO.
        assert!(matches!(
            mgr.get_channel_mapping_options("nope").await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_channel_mapping_options("").await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn set_channel_mapping_moves_the_listings_and_a_self_map_unmaps() {
        let (mgr, _tuner_id, provider_id) = mapping_manager().await;
        let two = crate::mapping::m3u_channel_id("http://tuner/playlist.m3u", "http://tuner/two");

        // Before: the guide's one airing binds to channel one only.
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );

        let row = mgr
            .set_channel_mapping(&provider_id, &two, "one.tv")
            .await
            .expect("map");
        // The response is the RESOLVED row, not an echo of the request.
        assert_eq!(row.id.as_deref(), Some(two.as_str()));
        assert_eq!(row.name.as_deref(), Some("2 Channel Two"));
        assert_eq!(row.provider_channel_id.as_deref(), Some("one.tv"));
        assert_eq!(row.provider_channel_name.as_deref(), Some("Channel One"));

        // The rebuild is the QUEUED task's work, not the POST's (upstream
        // `CancelIfRunningAndQueue`, ported one layer out as
        // `handlers::live_tv::queue_guide_refresh`) — so the test runs it, the
        // way the queued task would.
        mgr.refresh_guide().await.expect("queued refresh");

        // And it has an EFFECT: the airing now binds to both channels, because
        // the guide join runs through `GetEpgChannelFromTunerChannel`.
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            2
        );
        let stored = mgr.get_listing_providers().await.expect("p")[0]
            .channel_mappings
            .clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name.as_deref(), Some(two.as_str()));
        assert_eq!(stored[0].value.as_deref(), Some("one.tv"));

        // Re-posting the SAME pair TOGGLES it off — `channelMappingExists`
        // suppresses the re-add after the unconditional removal
        // (ListingsManager.cs:233-249). Verified against the live Jellyfin
        // 10.11.8 lab, which goes [] on the second identical POST.
        mgr.set_channel_mapping(&provider_id, &two, "one.tv")
            .await
            .expect("re-post");
        mgr.refresh_guide().await.expect("queued refresh");
        let providers = mgr.get_listing_providers().await.expect("p");
        let channel_mappings = &providers[0].channel_mappings;
        assert!(channel_mappings.is_empty(), "{channel_mappings:?}");
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );
        // Map it back so the unmap gesture below has something to remove.
        mgr.set_channel_mapping(&provider_id, &two, "one.tv")
            .await
            .expect("remap");
        mgr.refresh_guide().await.expect("queued refresh");
        assert_eq!(
            mgr.get_listing_providers().await.expect("p")[0]
                .channel_mappings
                .len(),
            1
        );

        // Mapping a channel onto ITSELF is the C# unmap gesture: the pair is
        // removed and none stored, and the listings go back with it.
        mgr.set_channel_mapping(&provider_id, &two, &two)
            .await
            .expect("unmap");
        mgr.refresh_guide().await.expect("queued refresh");
        let providers = mgr.get_listing_providers().await.expect("p");
        let channel_mappings = &providers[0].channel_mappings;
        assert!(channel_mappings.is_empty(), "{channel_mappings:?}");
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );

        // A tuner channel that is not in the lineup cannot be mapped.
        assert!(matches!(
            mgr.set_channel_mapping(&provider_id, "m3u_bogus", "one.tv")
                .await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.set_channel_mapping("nope", &two, "one.tv").await,
            Err(ServiceError::NotFound(_))
        ));
    }

    /// A [`SourceFetcher`] that counts its fetches and fails while no body is
    /// set — a remote guide host that can go down.
    #[derive(Default)]
    struct CountingFetcher {
        body: std::sync::Mutex<Option<String>>,
        fetches: std::sync::atomic::AtomicUsize,
    }

    impl CountingFetcher {
        fn fetches(&self) -> usize {
            self.fetches.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SourceFetcher for CountingFetcher {
        async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
            self.fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.body
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| ServiceError::Backend(format!("{url} is down")))
        }
    }

    /// A manager over `fetcher` with one saved remote XMLTV provider.
    async fn cached_guide_manager(
        fetcher: &Arc<CountingFetcher>,
    ) -> (FerrofinLiveTvManager, ListingsProviderInfo) {
        let mgr = manager_with_fetcher(Arc::clone(fetcher) as Arc<dyn SourceFetcher>).await;
        let provider = mgr
            .save_listing_provider(
                as_xmltv(ListingsProviderInfo {
                    path: Some("http://guide/xmltv.xml".to_owned()),
                    ..ListingsProviderInfo::default()
                }),
                false,
                false,
            )
            .await
            .expect("provider");
        (mgr, provider)
    }

    /// `XmlTvListingsProvider.GetXml`: within `_maxCacheAge` every reader —
    /// lineups, the mapping dialog, the guide refresh — shares one download.
    #[tokio::test]
    async fn guide_readers_share_one_download_within_the_cache_age() {
        let fetcher = Arc::new(CountingFetcher::default());
        *fetcher.body.lock().unwrap() = Some(XMLTV.to_owned());
        let (mgr, provider) = cached_guide_manager(&fetcher).await;
        let id = provider.id.clone().expect("id");

        for _ in 0..3 {
            let lineups = mgr
                .get_lineups(Some(&id), Some("xmltv"), None, None)
                .await
                .expect("lineups");
            assert_eq!(lineups.len(), 1);
        }
        mgr.refresh_guide().await.expect("refresh");
        assert_eq!(fetcher.fetches(), 1);

        // `SaveListingProvider` → `InvalidateListingsProviderCache`: the next
        // read downloads again, so a changed Path never serves the old guide.
        mgr.save_listing_provider(as_xmltv(provider), false, false)
            .await
            .expect("resave");
        mgr.get_lineups(Some(&id), Some("xmltv"), None, None)
            .await
            .expect("lineups after save");
        assert_eq!(fetcher.fetches(), 2);
    }

    /// A failed download falls back to the previous document, and a remote
    /// path that failed is not retried within `_maxCacheAge`.
    #[tokio::test]
    async fn a_failed_download_falls_back_and_is_not_retried() {
        let fetcher = Arc::new(CountingFetcher::default());
        *fetcher.body.lock().unwrap() = Some(XMLTV.to_owned());
        let (mgr, provider) = cached_guide_manager(&fetcher).await;
        mgr.xmltv.get_xml(&provider).await.expect("first download");

        // Age the cached file past `_maxCacheAge`, then take the host down.
        let cache_file = mgr
            .xmltv
            .cache_file_for_test(provider.id.as_deref().expect("id"));
        let stale = std::time::SystemTime::now() - std::time::Duration::from_hours(2);
        std::fs::File::options()
            .write(true)
            .open(&cache_file)
            .expect("cache file")
            .set_modified(stale)
            .expect("age");
        *fetcher.body.lock().unwrap() = None;

        assert_eq!(
            mgr.xmltv
                .get_xml(&provider)
                .await
                .expect("fallback")
                .text_for_test(),
            XMLTV
        );
        assert_eq!(
            mgr.xmltv
                .get_xml(&provider)
                .await
                .expect("backoff")
                .text_for_test(),
            XMLTV
        );
        assert_eq!(fetcher.fetches(), 2, "the failed path is not retried");

        // With no cached copy the recent failure is an error, still unfetched.
        std::fs::remove_file(&cache_file).expect("remove");
        assert!(matches!(
            mgr.xmltv.get_xml(&provider).await,
            Err(ServiceError::Backend(_))
        ));
        assert_eq!(fetcher.fetches(), 2);

        // Saving the provider is an explicit retry (`Validate`).
        *fetcher.body.lock().unwrap() = Some(XMLTV.to_owned());
        let provider = mgr
            .save_listing_provider(as_xmltv(provider), false, false)
            .await
            .expect("resave");
        assert_eq!(
            mgr.xmltv
                .get_xml(&provider)
                .await
                .expect("retried")
                .text_for_test(),
            XMLTV
        );
        assert_eq!(fetcher.fetches(), 3);
    }

    /// `UnzipIfNeededAndCopy` throws on an empty document: it never replaces
    /// the cached copy, and it counts as a failed download.
    #[tokio::test]
    async fn an_empty_download_keeps_the_previous_document() {
        let fetcher = Arc::new(CountingFetcher::default());
        *fetcher.body.lock().unwrap() = Some(XMLTV.to_owned());
        let (mgr, provider) = cached_guide_manager(&fetcher).await;
        mgr.xmltv.get_xml(&provider).await.expect("first download");
        let cache_file = mgr
            .xmltv
            .cache_file_for_test(provider.id.as_deref().expect("id"));
        std::fs::File::options()
            .write(true)
            .open(&cache_file)
            .expect("cache file")
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_hours(2))
            .expect("age");

        *fetcher.body.lock().unwrap() = Some(String::new());
        assert_eq!(
            mgr.xmltv
                .get_xml(&provider)
                .await
                .expect("fallback")
                .text_for_test(),
            XMLTV
        );
        assert_eq!(std::fs::read_to_string(&cache_file).expect("kept"), XMLTV);
        mgr.xmltv.get_xml(&provider).await.expect("backoff");
        assert_eq!(
            fetcher.fetches(),
            2,
            "the empty download counts as a failure"
        );
    }

    #[tokio::test]
    async fn lineups_are_the_guide_channels_and_need_a_resolvable_provider() {
        let (mgr, _tuner_id, provider_id) = mapping_manager().await;
        let lineups = mgr
            .get_lineups(Some(&provider_id), Some("xmltv"), None, None)
            .await
            .expect("lineups");
        assert_eq!(lineups.len(), 1);
        assert_eq!(lineups[0].id.as_deref(), Some("one.tv"));
        assert_eq!(lineups[0].name.as_deref(), Some("Channel One"));

        // `GetProvider(null)`/`FirstOrDefault(...) ?? throw` — 404, not 200 [].
        for args in [
            (None, None),
            (None, Some("xmltv")),
            (None, Some("bogus")),
            (Some("nope"), Some("xmltv")),
        ] {
            assert!(
                matches!(
                    mgr.get_lineups(args.0, args.1, None, None).await,
                    Err(ServiceError::NotFound(_))
                ),
                "{args:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_wedged_source_does_not_hold_the_guide_lock() {
        // `guide_lock` serializes the WRITE half only. Held across the fetches,
        // one unreachable tuner URL would park the lock forever and every later
        // refresh — scheduled or queued by a configuration write — would stall
        // behind it. Two concurrent refreshes must therefore BOTH reach their
        // fetch, even though neither can ever finish.
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager_with_fetcher(std::sync::Arc::new(HangingFetcher(entered.clone()))).await;
        // Stored directly: `save_tuner_host` opens the playlist first
        // (`M3UTunerHost.Validate`), and this source never answers.
        let tuner = TunerHostInfo {
            id: Some("wedged".to_owned()),
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        };
        mgr.db
            .upsert_live_tv_tuner_host(
                "wedged",
                "http://tuner/playlist.m3u",
                "m3u",
                &serde_json::to_string(&tuner).expect("json"),
            )
            .await
            .expect("tuner");

        let both = async { tokio::join!(mgr.refresh_guide(), mgr.refresh_guide()) };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), both)
                .await
                .is_err(),
            "the fetcher never answers, so neither refresh can complete"
        );
        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the second refresh must reach its fetch while the first is still in flight"
        );
    }

    #[tokio::test]
    async fn refresh_guide_drops_listings_the_pass_did_not_re_emit() {
        let (mgr, _tuner_id, provider_id) = mapping_manager().await;
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );

        // `GuideManager.CleanDatabase`: with the listings provider gone, the
        // refresh re-emits nothing and the guide drains — while the tuner's
        // channels, which belong to the tuner host, survive.
        mgr.delete_listing_provider(&provider_id)
            .await
            .expect("delete");
        mgr.refresh_guide().await.expect("refresh");
        let found = crate::guide_repository::all_program_ids(&mgr.db)
            .await
            .expect("ids");
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            crate::guide_repository::channel_rows(&mgr.db)
                .await
                .expect("channels")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_channel_dropped_from_the_playlist_is_pruned_with_its_airings() {
        // `CleanDatabase(newChannelIdList, [LiveTvChannel], …)`. The refresh
        // UPSERTS the lineup rather than deleting it first — deleting would
        // cascade every airing away for the duration of the pass, so a client
        // reading the guide mid-refresh would see it empty — and the channels
        // the pass did not re-emit are removed here instead.
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        // `dated`, not the literal fixture: the refresh ingests only
        // `[now - 1h, now - 1h + GuideDays)`, so a fixture pinned to a calendar
        // date silently empties itself once that date rolls past.
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(XMLTV));
        let fetcher = Arc::new(SwappableFetcher(std::sync::Mutex::new(sources)));
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let mgr = FerrofinLiveTvManager::new(
            db,
            Arc::clone(&fetcher) as Arc<dyn SourceFetcher>,
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_dto(Arc::new(FakeDto));
        mgr.save_tuner_host(TunerHostInfo {
            // `SaveTunerHost` resolves the host by `Type` first and 404s when nothing
            // claims it (v10.11.8 TunerHostManager.cs:60-69), so the fixture names the
            // M3U host this playlist belongs to.
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");
        let before = crate::guide_repository::channel_rows(&mgr.db)
            .await
            .expect("channels");
        assert_eq!(before.len(), 2);
        let kept_id = before[0].id.clone();
        let found = crate::guide_repository::all_program_ids(&mgr.db)
            .await
            .expect("ids");
        assert!(!found.is_empty(), "{found:?}");

        // The playlist now carries only the first channel; the second must go.
        fetcher.0.lock().unwrap().insert(
            "http://tuner/playlist.m3u".to_owned(),
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"one.tv\" tvg-chno=\"1\",Channel One\nhttp://tuner/one\n"
                .to_owned(),
        );
        mgr.refresh_guide().await.expect("refresh2");

        let after = crate::guide_repository::channel_rows(&mgr.db)
            .await
            .expect("channels2");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, kept_id, "the surviving channel keeps its id");
    }

    /// Jellyfin 12.1's `hasErrors` rule (`GuideManager.RefreshGuide`,
    /// `cleanDatabase &= !hasErrors`): a guide body that is empty or not a
    /// readable XMLTV document is a listings error, so the stored airings stay
    /// and the provider writes nothing. A well-formed guide with no programmes
    /// is not an error and still prunes. With no cached copy left, an empty
    /// body fails as a download (`UnzipIfNeededAndCopy`) before it is parsed.
    #[rstest::rstest]
    #[case::empty("", 1)]
    #[case::whitespace("  \n", 1)]
    #[case::truncated_mid_tag(
        "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel><programme start=\"2026",
        1
    )]
    #[case::truncated_after_channels(
        "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>",
        1
    )]
    #[case::html_page(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>503</title></head><body>Down<br>for maintenance</body></html>",
        1
    )]
    #[case::well_formed_empty("<tv></tv>", 0)]
    // `ReadToDescendant("tv")` finds no `<tv>` in a well-formed page, so the
    // provider reads as an empty guide and drains, as upstream's does.
    #[case::well_formed_xhtml_page(
        "<!DOCTYPE html><html><head><title>503</title></head><body>Down for maintenance</body></html>",
        0
    )]
    #[tokio::test]
    async fn an_unreadable_guide_body_keeps_the_stored_guide(
        #[case] body: &str,
        #[case] programs_after: usize,
    ) {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), dated(XMLTV));
        let fetcher = Arc::new(SwappableFetcher(std::sync::Mutex::new(sources)));
        let mgr = manager_with_fetcher(Arc::clone(&fetcher) as Arc<dyn SourceFetcher>).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        let provider_id = mgr
            .save_listing_provider(
                as_xmltv(ListingsProviderInfo {
                    path: Some("http://guide/xmltv.xml".to_owned()),
                    ..ListingsProviderInfo::default()
                }),
                false,
                false,
            )
            .await
            .expect("provider")
            .id
            .expect("provider id");
        mgr.refresh_guide().await.expect("refresh");
        let count = || async {
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len()
        };
        assert_eq!(count().await, 1);

        fetcher
            .0
            .lock()
            .unwrap()
            .insert("http://guide/xmltv.xml".to_owned(), body.to_owned());
        // The first refresh cached the document for `MAX_CACHE_AGE`; drop it, as
        // saving the provider does, so the second refresh downloads the new body.
        mgr.xmltv.invalidate(&provider_id).await;
        mgr.refresh_guide().await.expect("refresh2");
        assert_eq!(count().await, programs_after);
        assert_eq!(
            crate::guide_repository::channel_rows(&mgr.db)
                .await
                .expect("channels")
                .len(),
            2,
            "the tuner lineup is untouched by the guide's state"
        );
        // Upstream's `GetChannels` throws on a bad document, so the mapping
        // dialog errors instead of listing no guide channels.
        let unreadable = programs_after == 1;
        assert_eq!(
            mgr.get_channel_mapping_options(&provider_id).await.is_err(),
            unreadable
        );
    }

    /// `ListingsManager.GetEpgChannelFromTunerChannel`'s number arm: a tuner
    /// channel numbered `4.1` binds to the guide channel whose display-names
    /// carry `4.1`, though its id (`I4.1.12345.zap2it.com`) and name differ.
    #[tokio::test]
    async fn a_tuner_channel_binds_to_the_guide_by_display_name_number() {
        let mut sources = HashMap::new();
        sources.insert(
            "http://tuner/playlist.m3u".to_owned(),
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"nomatch\" tvg-chno=\"4.1\",WNBC-HD\nhttp://tuner/wnbc\n"
                .to_owned(),
        );
        sources.insert(
            "http://guide/xmltv.xml".to_owned(),
            dated(
                "<tv><channel id=\"I4.1.12345.zap2it.com\">\
                 <display-name>4.1 WNBC</display-name><display-name>4.1</display-name>\
                 <display-name>WNBC</display-name></channel>\
                 <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" \
                 channel=\"I4.1.12345.zap2it.com\"><title>Today</title></programme></tv>",
            ),
        );
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1,
            "the airing binds to the tuner channel by number"
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_never_empties_the_guide() {
        let (mgr, _tuner_id, _provider_id) = mapping_manager().await;
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );

        // A provider whose document cannot be read is the C# catch that sets
        // `cleanDatabase = false`: the pass re-emits nothing for it, and the
        // cache must NOT be taken as authoritative.
        mgr.save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/offline.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("second provider");
        mgr.refresh_guide().await.expect("refresh");
        assert_eq!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .len(),
            1
        );
    }

    /// Log lines written while a [`CapturedLogs::install`] guard is held.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        /// Captures this thread's events at every level until the guard drops.
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(self.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::DEBUG)
                    .finish(),
            )
        }

        /// The captured lines logged at `level` (`"WARN"`, `"DEBUG"`, …) that
        /// mention `needle`.
        fn lines_at(&self, level: &str, needle: &str) -> Vec<String> {
            let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
            text.lines()
                .filter(|line| line.contains(&format!(" {level} ")) && line.contains(needle))
                .map(str::to_owned)
                .collect()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A stored timer for an hour-long programme an hour from now, by id.
    async fn stored_fire_test_timer(mgr: &FerrofinLiveTvManager) -> (String, TimerInfoDto) {
        let id = Uuid::new_v4().simple().to_string();
        let start = Utc::now() + chrono::Duration::hours(1);
        let timer = TimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some(id.clone()),
                channel_id: Uuid::new_v4(),
                name: Some("fire test".to_owned()),
                start_date: start,
                end_date: start + chrono::Duration::hours(1),
                ..BaseTimerInfoDto::default()
            },
            ..TimerInfoDto::default()
        };
        mgr.persist_timer(&timer).await.expect("seed timer");
        (id, timer)
    }

    /// Hides (or restores) the timers table, so a timer read fails with the
    /// pool still open: what a busy or I/O error looks like to the fire path.
    async fn hide_timers_table(mgr: &FerrofinLiveTvManager, hidden: bool) {
        crate::dvr_repository::test_support::set_timers_table_hidden(&mgr.db, hidden)
            .await
            .expect("rename");
    }

    /// The fire path's failed reads so far for `id`.
    fn read_retries_of(mgr: &FerrofinLiveTvManager, id: &str) -> Option<u32> {
        mgr.read_retry_counts_lock().get(id).copied()
    }

    /// The capture retries so far for `id`.
    fn capture_retries_of(mgr: &FerrofinLiveTvManager, id: &str) -> Option<u32> {
        mgr.retry_counts.lock().unwrap().get(id).copied()
    }

    /// A timer whose row cannot be read when it fires (the pool still open) is
    /// not dropped: it warns and is armed again after `RETRY_INTERVAL_SECONDS`.
    /// The failed reads are counted apart from the capture retries, and a good
    /// read clears them.
    #[tokio::test]
    async fn fired_timer_unreadable_row_retries() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            hide_timers_table(&mgr, true).await;
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);

            assert_eq!(
                logs.lines_at("WARN", &id).len(),
                1,
                "one warn naming the timer"
            );
            assert!(
                mgr.armed_timers_lock().contains_key(&id),
                "the fire is retried"
            );
            assert_eq!(read_retries_of(&mgr, &id), Some(1));
            assert_eq!(capture_retries_of(&mgr, &id), None, "not a capture retry");

            // The retry reads the row; its capture then fails (the channel is
            // in no lineup), which is the capture budget's business alone.
            hide_timers_table(&mgr, false).await;
            mgr.on_timer_fired(id.clone()).await;
            assert_eq!(read_retries_of(&mgr, &id), None, "a good read clears it");
            assert_eq!(capture_retries_of(&mgr, &id), Some(1));
        })
        .await
        .expect("test timed out");
    }

    /// A fire of a timer whose capture another fire is still starting (a guide
    /// refresh re-arms every live timer, so one under way fires again) stands
    /// down before it opens anything, as upstream's "already in progress" skip
    /// does; once that fire is done, the timer captures again.
    #[tokio::test]
    async fn a_second_fire_while_the_capture_starts_stands_down() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            let starting = super::FiringClaim::take(&mgr.firing, &id).expect("first claim");
            assert!(
                super::FiringClaim::take(&mgr.firing, &id).is_none(),
                "one claim per timer"
            );
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);
            assert_eq!(
                logs.lines_at("INFO", &id)
                    .iter()
                    .filter(|line| line.contains("already in progress"))
                    .count(),
                1
            );
            assert_eq!(capture_retries_of(&mgr, &id), None, "nothing was opened");

            // The first fire is done: the next one tries the capture (which
            // fails here, the channel being in no lineup).
            drop(starting);
            mgr.on_timer_fired(id.clone()).await;
            assert_eq!(capture_retries_of(&mgr, &id), Some(1));
            assert!(
                super::FiringClaim::take(&mgr.firing, &id).is_some(),
                "a finished fire releases its claim"
            );
        })
        .await
        .expect("test timed out");
    }

    /// A timer armed again while its read was failing (an edit, a guide
    /// refresh) keeps that arm: the retry neither replaces nor cancels it.
    #[tokio::test]
    async fn fired_timer_rearmed_meanwhile_is_not_retried() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, timer) = stored_fire_test_timer(&mgr).await;
            mgr.arm_timer(&timer);
            let arm = Arc::clone(&mgr.armed_timers_lock()[&id]);
            hide_timers_table(&mgr, true).await;
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);

            assert!(logs.lines_at("WARN", &id).is_empty(), "no retry warn");
            assert!(
                Arc::ptr_eq(&mgr.armed_timers_lock()[&id], &arm),
                "the new arm stands"
            );
            assert!(
                !arm.load(std::sync::atomic::Ordering::SeqCst),
                "and is live"
            );
            assert_eq!(read_retries_of(&mgr, &id), None);
        })
        .await
        .expect("test timed out");
    }

    /// Once the read budget is spent the miss is an `error`, nothing is armed,
    /// and the row stays for the next boot to re-arm.
    #[tokio::test]
    async fn fired_timer_retry_budget_exhausted() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            mgr.read_retry_counts_lock()
                .insert(id.clone(), crate::dvr::MAX_RETRY_COUNT);
            hide_timers_table(&mgr, true).await;
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);

            let errors = logs.lines_at("ERROR", &id);
            assert_eq!(errors.len(), 1, "one error naming the timer");
            assert!(errors[0].contains("recording is missed"), "{errors:?}");
            assert!(!mgr.armed_timers_lock().contains_key(&id), "nothing armed");
            assert_eq!(read_retries_of(&mgr, &id), None, "the budget is cleared");
            hide_timers_table(&mgr, false).await;
            assert!(
                mgr.get_timer(&id).await.expect("read").is_some(),
                "the row is kept"
            );
        })
        .await
        .expect("test timed out");
    }

    /// A fired timer whose row no longer parses says so at fire time, even
    /// when a list read has already spent that row's once-only warning.
    #[tokio::test]
    async fn fired_timer_unparseable_row_warns_at_fire_time() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            crate::dvr_repository::test_support::corrupt_timer_data(&mgr.db, &id)
                .await
                .expect("corrupt");
            let all_timers = mgr.all_timers().await.expect("list");
            assert!(all_timers.is_empty(), "{all_timers:?}");
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);

            let warns = logs.lines_at("WARN", &id);
            assert_eq!(warns.len(), 1, "{warns:?}");
            assert!(warns[0].contains("recording is missed"), "{warns:?}");
            assert!(!mgr.armed_timers_lock().contains_key(&id), "nothing armed");
            assert_eq!(read_retries_of(&mgr, &id), None, "not retried");
        })
        .await
        .expect("test timed out");
    }

    /// Cancelling or deleting a timer forgets its retry counts.
    #[tokio::test]
    async fn cancelled_timer_forgets_its_retries() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            mgr.read_retry_counts_lock().insert(id.clone(), 3);
            mgr.retry_counts.lock().unwrap().insert(id.clone(), 2);

            mgr.cancel_timer(&id).await.expect("cancel");

            assert_eq!(read_retries_of(&mgr, &id), None);
            assert_eq!(capture_retries_of(&mgr, &id), None);
        })
        .await
        .expect("test timed out");
    }

    /// How many times `id` has fired on a manager whose pool is closed: each
    /// fire ends at once at its "fired after its server stopped" line, so a
    /// fire is countable without the database I/O a paused clock would race.
    fn fires_of(logs: &CapturedLogs, id: &str) -> usize {
        logs.lines_at("DEBUG", id)
            .iter()
            .filter(|line| line.contains("fired after its server stopped"))
            .count()
    }

    /// A manager whose pool is closed, for the paused-clock fire tests.
    async fn manager_with_closed_pool() -> FerrofinLiveTvManager {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        mgr.db.close().await;
        mgr
    }

    /// A pending fire superseded before it is due never runs and never takes
    /// out the arm that superseded it, whether that arm cancelled it (a
    /// re-arm) or not (a registration racing it); the live arm fires once.
    #[tokio::test]
    async fn a_superseded_fire_task_neither_fires_nor_disarms() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let mgr = tokio::time::timeout(Duration::from_secs(60), manager_with_closed_pool())
            .await
            .expect("setup timed out");
        // Paused only now: the database work above must not race a paused clock.
        tokio::time::pause();
        let logs = CapturedLogs::default();
        let guard = logs.install();
        tokio::time::timeout(Duration::from_secs(600), async {
            let rearmed = "rearmed".to_owned();
            mgr.schedule_fire(rearmed.clone(), Duration::from_secs(5));
            let stale = Arc::clone(&mgr.armed_timers_lock()[&rearmed]);
            mgr.schedule_fire(rearmed.clone(), Duration::from_secs(10));
            let fresh = Arc::clone(&mgr.armed_timers_lock()[&rearmed]);
            assert!(stale.load(Ordering::SeqCst), "the re-arm cancels the first");

            let raced = "raced".to_owned();
            mgr.schedule_fire(raced.clone(), Duration::from_secs(5));
            let foreign = Arc::new(AtomicBool::new(false));
            mgr.armed_timers_lock()
                .insert(raced.clone(), Arc::clone(&foreign));

            // Past both stale deadlines.
            tokio::time::sleep(Duration::from_secs(6)).await;
            assert_eq!(
                fires_of(&logs, &rearmed),
                0,
                "the cancelled task did not fire"
            );
            assert_eq!(fires_of(&logs, &raced), 0, "nor did the superseded one");
            assert!(Arc::ptr_eq(&mgr.armed_timers_lock()[&rearmed], &fresh));
            assert!(
                Arc::ptr_eq(&mgr.armed_timers_lock()[&raced], &foreign),
                "the newer arm is left in place"
            );

            // Past the live deadline.
            tokio::time::sleep(Duration::from_secs(5)).await;
            assert_eq!(fires_of(&logs, &rearmed), 1, "the live arm fires once");
            assert!(
                !mgr.armed_timers_lock().contains_key(&rearmed),
                "and takes its own arm out"
            );
        })
        .await
        .expect("test timed out");
        drop(guard);
    }

    /// A scheduled read retry really fires through its task, once,
    /// `RETRY_INTERVAL_SECONDS` after the failed read.
    #[tokio::test]
    async fn a_read_retry_fires_after_the_retry_interval() {
        use std::time::Duration;
        let interval = Duration::from_secs(crate::dvr::RETRY_INTERVAL_SECONDS.unsigned_abs());
        let (mgr, id) = tokio::time::timeout(Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            hide_timers_table(&mgr, true).await;
            mgr.on_timer_fired(id.clone()).await;
            assert_eq!(read_retries_of(&mgr, &id), Some(1));
            // The retry then meets a closed pool, which ends it at once.
            mgr.db.close().await;
            (mgr, id)
        })
        .await
        .expect("setup timed out");
        tokio::time::pause();
        let logs = CapturedLogs::default();
        let guard = logs.install();
        tokio::time::timeout(Duration::from_secs(600), async {
            tokio::time::sleep(interval.saturating_sub(Duration::from_secs(1))).await;
            assert_eq!(fires_of(&logs, &id), 0, "not yet");
            assert!(mgr.armed_timers_lock().contains_key(&id));

            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(fires_of(&logs, &id), 1, "the retry fired");
            assert!(!mgr.armed_timers_lock().contains_key(&id));
        })
        .await
        .expect("test timed out");
        drop(guard);
    }

    /// A timer of a host whose database has been closed (an in-process restart
    /// tore it down; the new host re-armed its own) is ignored without noise
    /// and never retried, so it cannot record twice.
    #[tokio::test]
    async fn fired_timer_after_db_closed_is_ignored() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, _) = stored_fire_test_timer(&mgr).await;
            mgr.db.close().await;
            let logs = CapturedLogs::default();
            let guard = logs.install();

            mgr.on_timer_fired(id.clone()).await;
            drop(guard);

            assert!(logs.lines_at("WARN", "").is_empty(), "no warn");
            assert!(logs.lines_at("ERROR", "").is_empty(), "no error");
            assert_eq!(logs.lines_at("DEBUG", &id).len(), 1, "a debug line only");
            assert!(!mgr.armed_timers_lock().contains_key(&id), "nothing armed");
            assert_eq!(read_retries_of(&mgr, &id), None);
        })
        .await
        .expect("test timed out");
    }

    /// A stored row that no longer parses is skipped with a `warn` naming its
    /// table and id the first time it is read, and at `debug` while it stays
    /// broken (the readers run per request). Once it reads again, breaking it
    /// again is news.
    #[tokio::test]
    async fn stored_row_unparseable_is_logged_once() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mgr = manager_with(FakeFetcher(HashMap::new())).await;
            let (id, timer) = stored_fire_test_timer(&mgr).await;
            let corrupt = || async {
                crate::dvr_repository::test_support::corrupt_timer_data(&mgr.db, &id)
                    .await
                    .expect("corrupt");
            };
            corrupt().await;
            let logs = CapturedLogs::default();
            let guard = logs.install();

            let all_timers = mgr.all_timers().await.expect("list");
            assert!(all_timers.is_empty(), "{all_timers:?}");
            assert!(mgr.get_timer(&id).await.expect("get").is_none());
            let all_timers = mgr.all_timers().await.expect("list");
            assert!(all_timers.is_empty(), "{all_timers:?}");
            let warns = logs.lines_at("WARN", &id);
            assert_eq!(warns.len(), 1, "{warns:?}");
            assert!(warns[0].contains("FerrofinLiveTvTimers"), "{warns:?}");
            assert_eq!(logs.lines_at("DEBUG", &id).len(), 2, "repeats at debug");

            mgr.persist_timer(&timer).await.expect("repair");
            assert_eq!(mgr.all_timers().await.expect("list").len(), 1);
            corrupt().await;
            let all_timers = mgr.all_timers().await.expect("list");
            assert!(all_timers.is_empty(), "{all_timers:?}");
            drop(guard);
            assert_eq!(logs.lines_at("WARN", &id).len(), 2, "broken again is news");
        })
        .await
        .expect("test timed out");
    }

    /// A `livetv.json` that cannot be read or does not parse reads as the
    /// defaults, as upstream's `LoadConfiguration` falls back, but not
    /// silently: a `warn` naming the file once per change of it, then `debug`.
    /// A missing file is the quiet default of a server never configured.
    #[tokio::test]
    async fn a_broken_live_tv_options_file_warns_once_per_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("livetv.json");
        let paths = |options_file: std::path::PathBuf| LiveTvPaths {
            transcode_dir: dir.path().join("transcodes").into(),
            data_dir: dir.path().join("data"),
            options_file,
        };
        let mgr = manager_with(FakeFetcher(HashMap::new()))
            .await
            .with_paths(paths(file.clone()));
        let logs = CapturedLogs::default();
        let guard = logs.install();

        assert!(mgr.live_tv_options().await.guide_days.is_none());
        assert!(
            logs.lines_at("WARN", "livetv.json").is_empty(),
            "missing is quiet"
        );

        std::fs::write(&file, "{").expect("write");
        assert!(mgr.live_tv_options().await.guide_days.is_none());
        assert!(mgr.live_tv_options().await.guide_days.is_none());
        assert_eq!(logs.lines_at("WARN", "livetv.json").len(), 1);
        assert_eq!(logs.lines_at("DEBUG", "livetv.json").len(), 1);

        // Changed and still broken: news again.
        std::fs::write(&file, "{ ").expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("open")
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .expect("mtime");
        let _ = mgr.live_tv_options().await;
        assert_eq!(logs.lines_at("WARN", "livetv.json").len(), 2);

        // Fixed, then broken again with the very same modification time: the
        // fix reset the latch, so it is news. So is a file deleted and put
        // back broken as it was.
        let mtime = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .expect("mtime");
        let break_as_it_was = |body: &str| {
            std::fs::write(&file, body).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&file)
                .expect("open")
                .set_modified(mtime)
                .expect("mtime");
        };
        let fixed = ferrofin_model::live_tv::LiveTvOptions {
            guide_days: Some(3),
            ..ferrofin_model::live_tv::LiveTvOptions::default()
        };
        break_as_it_was(&serde_json::to_string(&fixed).expect("serialize"));
        assert_eq!(mgr.live_tv_options().await.guide_days, Some(3), "fixed");
        break_as_it_was("{ ");
        let _ = mgr.live_tv_options().await;
        assert_eq!(logs.lines_at("WARN", "livetv.json").len(), 3, "after a fix");
        std::fs::remove_file(&file).expect("remove");
        let _ = mgr.live_tv_options().await;
        break_as_it_was("{ ");
        let _ = mgr.live_tv_options().await;
        assert_eq!(
            logs.lines_at("WARN", "livetv.json").len(),
            4,
            "after a removal"
        );

        // A read error other than a missing file is not quiet either.
        let unreadable = dir.path().join("dir").join("livetv.json");
        std::fs::create_dir_all(&unreadable).expect("a directory in its place");
        let mgr = mgr.with_paths(paths(unreadable));
        assert!(mgr.live_tv_options().await.guide_days.is_none());
        drop(guard);
        assert_eq!(logs.lines_at("WARN", "dir/livetv.json").len(), 1);
    }

    /// The tuner-host, listing-provider, series-timer and timer-by-programme
    /// readers skip a row that does not parse the same way: a `warn` naming
    /// its table and id the first time, `debug` while it stays broken
    /// (`ItemDataProvider.EnsureLoaded` logs "Error deserializing").
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // four stores seeded, broken and read
    async fn every_stored_row_reader_reports_a_row_that_does_not_parse() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut sources = HashMap::new();
            sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
            sources.insert("http://guide/xmltv.xml".to_owned(), dated(XMLTV));
            let mgr = manager_with(FakeFetcher(sources)).await;
            let tuner = mgr
                .save_tuner_host(TunerHostInfo {
                    type_: Some("m3u".to_owned()),
                    url: Some("http://tuner/playlist.m3u".to_owned()),
                    ..TunerHostInfo::default()
                })
                .await
                .expect("tuner")
                .id
                .expect("tuner id");
            let provider = mgr
                .save_listing_provider(
                    as_xmltv(ListingsProviderInfo {
                        path: Some("http://guide/xmltv.xml".to_owned()),
                        ..ListingsProviderInfo::default()
                    }),
                    false,
                    false,
                )
                .await
                .expect("provider")
                .id
                .expect("provider id");
            let series = Uuid::new_v4().simple().to_string();
            crate::dvr_repository::upsert_series_timer(
                &mgr.db,
                &SeriesTimerInfoDto {
                    base: BaseTimerInfoDto {
                        id: Some(series.clone()),
                        channel_id: Uuid::new_v4(),
                        ..BaseTimerInfoDto::default()
                    },
                    ..SeriesTimerInfoDto::default()
                },
                "external",
                None,
            )
            .await
            .expect("series timer");
            let (timer_id, mut timer) = stored_fire_test_timer(&mgr).await;
            timer.base.program_id = Some("programme-1".to_owned());
            mgr.persist_timer(&timer).await.expect("timer");
            // A second programme's timers: a broken one stored first, then a
            // good one, which the broken one must not hide.
            let (hidden_id, mut hidden) = stored_fire_test_timer(&mgr).await;
            hidden.base.program_id = Some("programme-2".to_owned());
            mgr.persist_timer(&hidden).await.expect("broken timer");
            let (good_id, mut good) = stored_fire_test_timer(&mgr).await;
            good.base.program_id = Some("programme-2".to_owned());
            mgr.persist_timer(&good).await.expect("good timer");
            crate::dvr_repository::test_support::corrupt_timer_data(&mgr.db, &hidden_id)
                .await
                .expect("corrupt the first");
            let found = mgr
                .timer_for_program(&good)
                .await
                .expect("read")
                .expect("the good timer");
            assert_eq!(found.base.id.as_deref(), Some(good_id.as_str()));

            let config = crate::guide_repository::test_support::corrupt_config_data;
            config(&mgr.db, crate::guide_repository::TUNER_HOSTS_TABLE, &tuner)
                .await
                .expect("corrupt tuner");
            config(
                &mgr.db,
                crate::guide_repository::LISTING_PROVIDERS_TABLE,
                &provider,
            )
            .await
            .expect("corrupt provider");
            crate::dvr_repository::test_support::corrupt_series_timer_data(&mgr.db, &series)
                .await
                .expect("corrupt series timer");
            crate::dvr_repository::test_support::corrupt_timer_data(&mgr.db, &timer_id)
                .await
                .expect("corrupt timer");

            let logs = CapturedLogs::default();
            let guard = logs.install();
            for _ in 0..2 {
                let tuner_hosts = mgr.get_tuner_hosts().await.expect("tuners");
                assert!(tuner_hosts.is_empty(), "{tuner_hosts:?}");
                let listing_providers = mgr.get_listing_providers().await.expect("providers");
                assert!(listing_providers.is_empty(), "{listing_providers:?}");
                assert!(
                    mgr.stored_series_timer(&series)
                        .await
                        .expect("series")
                        .is_none()
                );
                assert!(
                    mgr.timer_for_program(&timer)
                        .await
                        .expect("timer")
                        .is_none()
                );
            }
            drop(guard);
            for (table, id) in [
                ("FerrofinLiveTvTunerHosts", &tuner),
                ("FerrofinLiveTvListingProviders", &provider),
                ("FerrofinLiveTvSeriesTimers", &series),
                ("FerrofinLiveTvTimers", &timer_id),
            ] {
                let warns = logs.lines_at("WARN", id);
                assert_eq!(warns.len(), 1, "{table}: {warns:?}");
                assert!(warns[0].contains(table), "{warns:?}");
                assert_eq!(
                    logs.lines_at("DEBUG", id).len(),
                    1,
                    "{table} repeats at debug"
                );
            }
        })
        .await
        .expect("test timed out");
    }
}

/// Tests pinning the Live TV id derivation to the values a real Jellyfin
/// 10.11.8 produces.
///
/// The oracle is the lab pair's synthetic tuner: `channels.m3u` at
/// `/media/synth/livetv/channels.m3u` with two entries streaming from
/// `http://livetv-source:8000/live.ts?ch={1,2}`, and an XMLTV guide whose
/// `parity1` programmes start on the hour. The expected GUIDs below are the
/// bytes Jellyfin returned on `GET /LiveTv/Channels` and stored in its own
/// `BaseItems` table for that fixture — not values recomputed from this code.
#[cfg(test)]
mod id_derivation_tests {
    use super::{
        LIVE_TV_INTERNAL_VERSION, LIVE_TV_SERVICE_NAME, internal_channel_id, internal_program_id,
        new_item_id, to_lower_invariant,
    };
    // The channel identity itself moved to the M3U tuner host with the rest of
    // that backend; these vectors pin it wherever it lives.
    use crate::tuner_host::m3u_external_channel_id;

    const TUNER: &str = "/media/synth/livetv/channels.m3u";

    #[rstest::rstest]
    #[case(
        "http://livetv-source:8000/live.ts?ch=1",
        "m3u_5581ab8b17869acbac4cc454abc401683f2ed88fba54056b16c110c12038d26b",
        "d53240a11ffc0b1a0c165896a3bdf7f4"
    )]
    #[case(
        "http://livetv-source:8000/live.ts?ch=2",
        "m3u_5581ab8b17869acbac4cc454abc40168446c98aa7b6e4352b95b322018c4eebf",
        "dcd04dcd7fa074c3b728a1fa49acd10e"
    )]
    fn channel_ids_match_jellyfin(
        #[case] stream_url: &str,
        #[case] expected_external: &str,
        #[case] expected_guid: &str,
    ) {
        let external = m3u_external_channel_id(TUNER, stream_url);
        assert_eq!(external, expected_external);
        assert_eq!(
            internal_channel_id(&external).simple().to_string(),
            expected_guid
        );
    }

    /// `M3uParser` keys a channel on its stream URL, so two playlist entries
    /// that share a `tvg-id` stay distinct. Ferrofin used to key on the
    /// `tvg-id`, which collapsed them onto one primary key and aborted the
    /// whole lineup insert.
    #[test]
    fn same_tvg_id_different_stream_urls_stay_distinct() {
        let a = internal_channel_id(&m3u_external_channel_id(TUNER, "http://t/a.ts"));
        let b = internal_channel_id(&m3u_external_channel_id(TUNER, "http://t/b.ts"));
        assert_ne!(a, b);
    }

    /// The same stream URL served by two different tuners is two channels: the
    /// tuner URL is hashed into the prefix.
    #[test]
    fn same_stream_url_different_tuners_stay_distinct() {
        let a = m3u_external_channel_id("http://one/list.m3u", "http://t/a.ts");
        let b = m3u_external_channel_id("http://two/list.m3u", "http://t/a.ts");
        assert_ne!(a, b);
    }

    /// `{xmltvChannelId}_{StartDate:O}_{channelExternalId}`, hashed as a
    /// `LiveTvProgram`. The two expected GUIDs are the ones Jellyfin served as
    /// `CurrentProgram.Id` (03:00 airing) and stored in `BaseItems` (00:00
    /// airing) for the same fixture.
    #[rstest::rstest]
    #[case(
        "2026-08-30T03:00:00.0000000+00:00",
        "30126bace3d86b5ee0f00c270d105f86"
    )]
    #[case(
        "2026-08-30T00:00:00.0000000+00:00",
        "b68c76f67dc24315d7a00eb076ada809"
    )]
    fn program_ids_match_jellyfin(#[case] start: &str, #[case] expected_guid: &str) {
        let channel_external =
            m3u_external_channel_id(TUNER, "http://livetv-source:8000/live.ts?ch=1");
        let external = format!("parity1_{start}_{channel_external}");
        assert_eq!(
            internal_program_id(&external).simple().to_string(),
            expected_guid
        );
    }

    /// `ToLowerInvariant` is a simple, per-code-point fold. Rust's
    /// `str::to_lowercase` is a full, context-sensitive one, and the two
    /// disagree on exactly the inputs an XMLTV `channel` attribute can carry.
    /// ASCII — every id this fixture produces — is identical either way, which
    /// is why the pinned GUIDs above are unaffected and why nothing caught this.
    #[rstest::rstest]
    // Final sigma: `str::to_lowercase` yields U+03C2, the simple mapping U+03C3.
    #[case("\u{03A3}", "\u{03C3}")]
    #[case("\u{039F}\u{0394}\u{039F}\u{03A3}", "\u{03BF}\u{03B4}\u{03BF}\u{03C3}")]
    // A non-final sigma is unambiguous and both agree.
    #[case("\u{03A3}A", "\u{03C3}a")]
    // U+0130: the full mapping expands to two code points, the simple one does not.
    #[case("\u{0130}", "i")]
    // Turkish dotless i has no lowercase of its own; the invariant fold is identity.
    #[case("\u{0131}", "\u{0131}")]
    #[case("M3U_ABC", "m3u_abc")]
    fn to_lower_invariant_is_the_simple_fold(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(to_lower_invariant(input), expected);
        // The documented .NET guarantee: the fold never changes the code-point count.
        assert_eq!(
            to_lower_invariant(input).chars().count(),
            input.chars().count()
        );
    }

    /// The negative control for the above: `str::to_lowercase` really does
    /// derive a different programme GUID for a Greek channel id, so the helper
    /// is load-bearing and not decoration.
    #[test]
    fn a_final_sigma_channel_id_would_have_derived_a_different_guid() {
        let external = "\u{039F}\u{0394}\u{039F}\u{03A3}_2026-08-30T03:00:00.0000000+00:00_m3u_abc";
        let key = format!("{LIVE_TV_SERVICE_NAME}{external}{LIVE_TV_INTERNAL_VERSION}");
        assert_ne!(to_lower_invariant(&key), key.to_lowercase());
        assert_ne!(
            internal_program_id(external),
            new_item_id(crate::projection::PROGRAM_TYPE_NAME, &key.to_lowercase()),
        );
    }

    /// The hash input is case-folded only on the `"Emby"+id+"4"` half —
    /// `LibraryManager.GetNewItemIdInternal` prepends `type.FullName` *after*
    /// the `ToLowerInvariant()`. Folding the whole key instead is the mistake
    /// that silently produces plausible-but-wrong GUIDs.
    #[test]
    fn type_name_casing_is_load_bearing() {
        let external = "m3u_5581ab8b17869acbac4cc454abc401683f2ed88fba54056b16c110c12038d26b";
        let wrong = ferrofin_common::extensions::get_md5(
            &format!("MediaBrowser.Controller.LiveTv.LiveTvChannelEmby{external}4").to_lowercase(),
        );
        assert_ne!(wrong, internal_channel_id(external));
    }
}

/// Tests for the guide window and the recommendation ranking.
#[cfg(test)]
mod guide_window_tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::{FerrofinLiveTvManager, GuideProgramRow, recommendation_score, start_day};

    fn programme(start_hour: i64, dur_hours: i64) -> crate::xmltv::XmltvProgramme {
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        crate::xmltv::XmltvProgramme {
            start: Some((base + Duration::hours(start_hour)).fixed_offset()),
            stop: Some((base + Duration::hours(start_hour + dur_hours)).fixed_offset()),
            ..crate::xmltv::XmltvProgramme::default()
        }
    }

    /// The window is `[now - 1h, now - 1h + GuideDays)` and keeps anything
    /// *overlapping* it, so the airing already in progress survives and the one
    /// that ended exactly at the boundary does not.
    #[test]
    fn window_keeps_overlapping_airings_only() {
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let start = base + Duration::minutes(30);
        let end = start + Duration::days(7);

        // 23:00-00:00 the previous day ended before the window opened.
        assert!(!FerrofinLiveTvManager::in_guide_window(
            &programme(-1, 1),
            start,
            end
        ));
        // 00:00-01:00 is in progress at 00:30.
        assert!(FerrofinLiveTvManager::in_guide_window(
            &programme(0, 1),
            start,
            end
        ));
        // The airing starting exactly `GuideDays` on is inside; the next is not.
        assert!(FerrofinLiveTvManager::in_guide_window(
            &programme(7 * 24, 1),
            start,
            end
        ));
        assert!(!FerrofinLiveTvManager::in_guide_window(
            &programme(7 * 24 + 1, 1),
            start,
            end
        ));
    }

    /// On the lab fixture that filter must retain exactly what Jellyfin
    /// retained: 169 hourly airings per channel for a 7-day window.
    #[test]
    fn seven_day_window_retains_the_jellyfin_row_count() {
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let start = base + Duration::minutes(30);
        let end = start + Duration::days(7);
        let kept = (-24..24 * 400)
            .filter(|h| FerrofinLiveTvManager::in_guide_window(&programme(*h, 1), start, end))
            .count();
        assert_eq!(kept, 169);
    }

    /// A programme with no `start` cannot be placed, so it is not silently
    /// dropped by the window.
    #[test]
    fn undated_airings_survive_the_window() {
        let base = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        assert!(FerrofinLiveTvManager::in_guide_window(
            &crate::xmltv::XmltvProgramme::default(),
            base,
            base + Duration::days(7)
        ));
    }

    fn row(is_live: bool, is_series: bool, is_repeat: bool) -> GuideProgramRow {
        GuideProgramRow {
            is_live,
            is_series,
            is_repeat,
            start_date: "2026-08-30T03:00:00.0000000Z".to_owned(),
            ..GuideProgramRow::default()
        }
    }

    /// Port check against `LiveTvManager.GetRecommendationScore`: +1 live,
    /// +1 non-repeat series, ±2 like/dislike, +3 favourite, +PlayCount.
    #[test]
    fn recommendation_score_matches_the_c_sharp_arithmetic() {
        assert_eq!(recommendation_score(&row(false, false, false), None), 0);
        assert_eq!(recommendation_score(&row(true, false, false), None), 1);
        assert_eq!(recommendation_score(&row(true, true, false), None), 2);
        // A repeat gets no series bonus.
        assert_eq!(recommendation_score(&row(true, true, true), None), 1);
        // Channel user-data: liked, favourite, three plays.
        assert_eq!(
            recommendation_score(&row(false, false, false), Some(&(Some(true), true, 3))),
            8
        );
        // Disliked and not favourited subtracts.
        assert_eq!(
            recommendation_score(&row(false, false, false), Some(&(Some(false), false, 0))),
            -2
        );
    }

    /// `OrderBy(i => i.StartDate.Date)` groups by calendar day, not instant.
    #[test]
    fn start_day_is_the_calendar_date() {
        assert_eq!(start_day("2026-08-30T23:00:00.0000000Z"), "2026-08-30");
        assert_eq!(
            start_day("2026-08-30T01:00:00.0000000Z"),
            start_day("2026-08-30T23:00:00.0000000Z")
        );
    }
}
