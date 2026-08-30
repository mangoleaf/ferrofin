//! The real [`LiveTvManager`] over the SQLite channel/guide cache.
//!
//! Configuration (tuner hosts, listing providers) is stored verbatim as JSON so
//! reads round-trip the DTO. `refresh_guide` fetches each tuner host (M3U) and
//! listing provider (XMLTV), rewrites `FerrofinLiveTvChannels`/`FerrofinLiveTvPrograms`, and
//! binds programmes to channels by the tuner `tvg-id` / XMLTV `channel id`.
//! Channels and programmes are surfaced to clients as `BaseItemDto`s.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db, opt_datetime_to_db};
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{BaseItemDto, MediaSourceInfo, MediaSourceType};
use ferrofin_model::dto::{DayOfWeek, SortOrder};
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_model::live_tv::{
    ChannelType, GuideInfo, ItemSortBy, ListingsProviderInfo, LiveTvInfo, LiveTvServiceInfo,
    LiveTvServiceStatus, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserViewManager;
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::stubs::{LiveStreamFile, LiveTvChannelQuery, LiveTvManager};
use serde::de::DeserializeOwned;

use crate::dvr::{ActiveRecording, RecorderKind, RecordingInput, TimerRecordingInfo};
use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::projection::{
    ChannelRow, ProgramRow as GuideProgramRow, channel_entity, program_entity, remove_fields,
};
use crate::schedules_direct::SchedulesDirect;
use crate::stream::{LiveStreamHandle, LiveStreamKind, TunerStreamSource};
use crate::xmltv::parse_xmltv;

/// SQLite's conservative default bind-parameter limit (`SQLITE_MAX_VARIABLE_NUMBER`
/// is 999 before 3.32, 32766 after); multi-row inserts chunk to stay under it.
const SQLITE_BIND_LIMIT: usize = 999;

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

/// The columns a guide list read returns, joined to the owning channel for
/// `ChannelName`. Held apart from the filters so the `WHERE`/`ORDER`/`LIMIT`
/// builders can be shared with the total-record count.
const PROGRAM_SELECT: &str = r#"SELECT p."Id",p."ChannelId",p."StartDate",p."EndDate",p."Title",p."EpisodeTitle",
                      p."Overview",p."Genres",p."ProductionYear",p."OfficialRating",p."IsNew",
                      p."IsRepeat",p."IsPremiere",p."IsMovie",p."IsSeries",p."IsNews",p."IsKids",
                      p."IsSports",p."IsLive",p."ExternalId",p."ExternalSeriesId",
                      p."SeasonNumber",p."EpisodeNumber",p."DateCreated",
                      c."Name" AS "ChannelName",c."Number" AS "ChannelNumber",
                      c."ChannelType" AS "ChannelMediaKind"
               FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId""#;

/// [`PROGRAM_SELECT`]'s `FROM`/`JOIN` counting instead of selecting, so the
/// total-record count runs the identical filter set.
const PROGRAM_COUNT: &str = r#"SELECT COUNT(*)
               FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId""#;

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
                    "SeasonNumber"=excluded."SeasonNumber","EpisodeNumber"=excluded."EpisodeNumber""#;

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
    pub transcode_dir: PathBuf,
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
    fetcher: Arc<dyn SourceFetcher>,
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
    /// The API base URL a live stream's buffered file is served from — C#
    /// `IServerApplicationHost.GetApiUrlForLocalAccess()`. An `Arc<OnceLock<_>>`
    /// because the composition root builds the application host *after* this
    /// manager, and every clone must see the value once it lands.
    local_api_url: Arc<OnceLock<String>>,
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
    /// The media encoder, for the encoded recorder's ffmpeg. Absent in tests
    /// and on a server with no ffmpeg, where only the direct recorder runs.
    encoder: Option<Arc<dyn MediaEncoder>>,
    /// Serializes "join or open", so two clients tuning the same channel at
    /// once cannot both miss the join and both dial the tuner.
    ///
    /// Port of `MediaSourceManager._liveStreamLocker` (an
    /// `AsyncNonKeyedLocker(1)` around the whole of `OpenLiveStreamInternal`);
    /// a `tokio::sync::Mutex` because this one *is* held across awaits.
    open_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account-less Schedules Direct surface (country list), sharing the
    /// fetcher and caching under the application cache directory.
    schedules_direct: SchedulesDirect,
    /// The `BaseItems` seam the guide refresh mirrors the channel lineup
    /// through, and the Live TV `UserView` the rows parent to.
    ///
    /// A `OnceLock` for the same composition-root cycle the `dto` field breaks:
    /// the item store and the user-view manager are both built after this
    /// manager. Absent in unit tests, where the guide refresh simply writes no
    /// item rows — the same state a server has before its first guide refresh.
    item_store: OnceLock<LiveTvItemStore>,
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

impl std::fmt::Debug for FerrofinLiveTvManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLiveTvManager")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// One `(channel, programme)` binding ready to insert.
struct ProgramRow<'a> {
    id: String,
    channel_id: &'a String,
    /// `{xmltvChannelId}_{start:O}_{channelExternalId}` — the listings-facing id
    /// the item GUID is derived from.
    external_id: Option<String>,
    start: String,
    end: Option<String>,
    genres: Option<String>,
    class: ProgramClass,
    prog: &'a crate::xmltv::XmltvProgramme,
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
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        let schedules_direct = SchedulesDirect::new(Arc::clone(&fetcher), cache_dir);
        // The same two hosts, in the same order, that
        // `LiveTvServiceCollectionExtensions.AddLiveTvServices` registers
        // (v10.11.8 LiveTvServiceCollectionExtensions.cs:41-42): HDHomeRun
        // then M3U. The ORDER of registration is not the advertised order —
        // `GetTunerHostTypes` sorts by name — but keeping it identical means a
        // future first-match rule cannot silently diverge.
        let hosts: Vec<Arc<dyn crate::tuner_host::TunerHost>> = vec![
            Arc::new(crate::hdhomerun::HdHomerunHost::new(Arc::clone(&fetcher))),
            Arc::new(crate::tuner_host::M3uTunerHost::new(Arc::clone(&fetcher))),
        ];
        Self {
            db,
            fetcher,
            hosts,
            users: None,
            server_id,
            item_store: OnceLock::new(),
            live_tv_view: OnceLock::new(),
            tuner_flag: Arc::new(AtomicBool::new(false)),
            dto: OnceLock::new(),
            paths: LiveTvPaths::default(),
            tuner_source: Arc::new(crate::stream::ReqwestTunerSource::new()),
            local_api_url: Arc::new(OnceLock::new()),
            live_streams: Arc::new(Mutex::new(HashMap::new())),
            active_recordings: Arc::new(Mutex::new(HashMap::new())),
            armed_timers: Arc::new(Mutex::new(HashMap::new())),
            retry_counts: Arc::new(Mutex::new(HashMap::new())),
            encoder: None,
            open_lock: Arc::new(tokio::sync::Mutex::new(())),
            schedules_direct,
        }
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

    /// Publishes the API base URL a live stream's buffered file is served from
    /// (C# `GetApiUrlForLocalAccess()`), e.g. `http://127.0.0.1:8096`.
    ///
    /// The composition root calls this once the application host exists; a
    /// second call is ignored.
    pub fn set_local_api_url(&self, url: impl Into<String>) {
        let _ = self.local_api_url.set(url.into());
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
        let entities: Vec<_> = rows
            .iter()
            .map(|row| channel_entity(row, parse_dt, view_id))
            .collect();
        if !entities.is_empty() {
            store.persistence.save_items(&entities).await?;
        }
        // `CleanDatabase`: the stored channel items the refreshed lineup no
        // longer names. A lineup that came back empty because every tuner was
        // unreachable must NOT wipe the channels — `BaseTunerHost.GetChannels`
        // falls back to its cache rather than reporting none, and
        // `replace_channels` is likewise never reached on a failed fetch.
        if entities.is_empty() {
            return Ok(());
        }
        let keep: std::collections::HashSet<String> =
            entities.iter().map(|e| e.id.to_uppercase()).collect();
        let stored = store
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::LiveTvChannel],
                ..InternalItemsQuery::default()
            })
            .await?;
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

    /// The wired DTO service, or the honest error when the composition root
    /// (or a test) never attached one.
    fn dto_service(&self) -> Result<&Arc<dyn DtoService>, ServiceError> {
        self.dto
            .get()
            .ok_or_else(|| ServiceError::Backend("live tv dto service not wired".to_owned()))
    }

    /// Rewrites the channel lineup for one tuner host from the lineup its
    /// [`TunerHost`] produced, in a transaction (deleting the old channels
    /// cascades away their programmes).
    ///
    /// The lineup arrives already built, so the identity of a channel is the
    /// HOST's business — `m3u_{md5}{md5}` for a playlist, `hdhr_{GuideNumber}`
    /// for an HDHomeRun — exactly as `ChannelInfo.Id` is upstream.
    async fn replace_channels(
        &self,
        tuner_id: &str,
        channels: &[crate::tuner_host::TunerChannel],
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        // A channel already in the lineup keeps its first-seen instant across
        // refreshes, the way `GuideManager.GetChannel` only stamps
        // `DateCreated = DateTime.UtcNow` on a NEW item.
        let existing = crate::guide_repository::existing_channel_dates(&mut tx, tuner_id).await?;
        let now = datetime_to_db(Utc::now());

        sqlx::query(r#"DELETE FROM "FerrofinLiveTvChannels" WHERE "TunerHostId" = ?1"#)
            .bind(tuner_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // 14 columns per row; chunked multi-row insert instead of one round-trip
        // per channel.
        for (chunk_index, chunk) in channels.chunks(SQLITE_BIND_LIMIT / 14).enumerate() {
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
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
                let date_created = existing
                    .get(&id)
                    .and_then(Clone::clone)
                    .unwrap_or_else(|| now.clone());
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
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)
    }

    /// Every channel keyed by its `tvg-id`, as `(channel GUID, external id)`.
    ///
    /// The external id rides along because upstream builds a programme's id
    /// from the *channel's* external id (`ListingsManager.GetProgramsAsync`,
    /// v10.11.8 ListingsManager.cs:151-155), not from its item GUID, and a
    /// `tvg-id` can legitimately be shared by more than one channel.
    async fn channels_by_tvg_id(
        &self,
    ) -> Result<HashMap<String, Vec<(String, String)>>, ServiceError> {
        let rows = sqlx::query(r#"SELECT "Id","TvgId","ExternalId" FROM "FerrofinLiveTvChannels""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let mut by_tvg: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in rows {
            let id: String = row.get("Id");
            let tvg: String = row.get("TvgId");
            let ext: String = row.get("ExternalId");
            by_tvg.entry(tvg).or_default().push((id, ext));
        }
        Ok(by_tvg)
    }

    /// The channels one XMLTV document speaks for — every channel bound to a
    /// `<channel>` or `<programme>` `tvg-id` it carries.
    ///
    /// These are the channels the refresh is authoritative for, and so the ones
    /// [`Self::clean_programs`] may replace. Scoping matters: with two listings
    /// providers configured, the second document must not delete the first's
    /// airings.
    fn channels_covered_by(
        guide: &crate::xmltv::Xmltv,
        by_tvg: &HashMap<String, Vec<(String, String)>>,
    ) -> Vec<String> {
        guide
            .channels
            .iter()
            .map(|c| c.id.clone())
            .chain(guide.programmes.iter().map(|p| p.channel_id.clone()))
            .filter_map(|tvg| by_tvg.get(&tvg))
            .flatten()
            .map(|(id, _)| id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
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
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
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

            let mut qb: QueryBuilder<'_, Sqlite> =
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

    /// Inserts programmes from an XMLTV body, binding each to every channel whose
    /// `TvgId` matches the programme's `channel` attribute, classified against the
    /// listings provider's category lists as `XmlTvListingsProvider.GetProgramInfo`
    /// does.
    async fn insert_programs(
        &self,
        xmltv_body: &str,
        provider: &ListingsProviderInfo,
        window: (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<(), ServiceError> {
        let guide = parse_xmltv(xmltv_body);
        let classes = CategoryClasses::from_provider(provider);
        let (window_start, window_end) = window;

        let by_tvg = self.channels_by_tvg_id().await?;

        let touched = Self::channels_covered_by(&guide, &by_tvg);

        // Flatten to one (channel, programme) row per binding, then insert in
        // chunked multi-row statements (25 columns per row) instead of one
        // round-trip per programme.
        let rows: Vec<_> = guide
            .programmes
            .iter()
            .filter(|prog| Self::in_guide_window(prog, window_start, window_end))
            .flat_map(|prog| {
                let channels = by_tvg.get(&prog.channel_id).map_or(&[][..], Vec::as_slice);
                let start = opt_datetime_to_db(prog.start.map(|d| d.with_timezone(&Utc)))
                    .unwrap_or_default();
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
                        id: guid_to_db(id),
                        channel_id,
                        external_id: external_id.clone(),
                        start: start.clone(),
                        end: end.clone(),
                        genres: genres.clone(),
                        class: class.clone(),
                        prog,
                    }
                })
            })
            .collect();

        let now = datetime_to_db(Utc::now());
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        let existing_dates = Self::clean_programs(&mut tx, &touched).await?;

        for chunk in rows.chunks(SQLITE_BIND_LIMIT / 26) {
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
                r#"INSERT INTO "FerrofinLiveTvPrograms"
                   ("Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview",
                    "Genres","ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere",
                    "IsRepeat","OfficialRating","IsMovie","IsSeries","IsNews","IsKids",
                    "IsSports","IsLive","ExternalId","ExternalSeriesId","SeasonNumber",
                    "EpisodeNumber","DateCreated") "#,
            );
            qb.push_values(chunk, |mut b, row| {
                let prog = row.prog;
                let class = &row.class;
                b.push_bind(&row.id)
                    .push_bind(row.channel_id)
                    .push_bind(&row.start)
                    .push_bind(&row.end)
                    .push_bind(&prog.title)
                    .push_bind(class.episode_title.as_deref())
                    .push_bind(&prog.desc)
                    .push_bind(&row.genres)
                    .push_bind(&prog.icon)
                    .push_bind(prog.year)
                    .push_bind(&prog.episode_num)
                    .push_bind(i32::from(prog.is_new))
                    .push_bind(i32::from(prog.is_premiere))
                    .push_bind(i32::from(class.is_repeat))
                    .push_bind(&prog.rating)
                    .push_bind(i32::from(class.is_movie))
                    .push_bind(i32::from(class.is_series))
                    .push_bind(i32::from(class.is_news))
                    .push_bind(i32::from(class.is_kids))
                    .push_bind(i32::from(class.is_sports))
                    .push_bind(0_i32)
                    .push_bind(&row.external_id)
                    .push_bind(&class.external_series_id)
                    .push_bind(class.season_number)
                    .push_bind(class.episode_number)
                    .push_bind(
                        existing_dates
                            .get(&row.id)
                            .and_then(Clone::clone)
                            .unwrap_or_else(|| now.clone()),
                    );
            });
            qb.push(PROGRAM_UPSERT_CONFLICT);
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
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
        let rows = sqlx::query(r#"SELECT "Data" FROM "FerrofinLiveTvTunerHosts" ORDER BY "Id""#)
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
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
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

        let id = info
            .id
            .clone()
            .filter(|s| !s.is_empty())
            // `TunerHostManager.SaveTunerHost` / `ListingsManager.SaveListingProvider`
            // mint the id as `Guid.NewGuid().ToString("N")` — 32 lowercase hex
            // digits, no dashes. `guid_to_db`'s uppercase-dashed DB form is a
            // storage detail; these ids go out over the wire on
            // `GET /System/Configuration/livetv`.
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
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
        self.db
            .upsert_live_tv_tuner_host(&id, &url, &type_id, &data)
            .await
            .map_err(ServiceError::from)?;
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
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvTunerHosts" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
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
        let rows =
            sqlx::query(r#"SELECT "Data" FROM "FerrofinLiveTvListingProviders" ORDER BY "Id""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
            .collect())
    }

    async fn save_listing_provider(
        &self,
        mut info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        let id = info
            .id
            .clone()
            .filter(|s| !s.is_empty())
            // `TunerHostManager.SaveTunerHost` / `ListingsManager.SaveListingProvider`
            // mint the id as `Guid.NewGuid().ToString("N")` — 32 lowercase hex
            // digits, no dashes. `guid_to_db`'s uppercase-dashed DB form is a
            // storage detail; these ids go out over the wire on
            // `GET /System/Configuration/livetv`.
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
        info.id = Some(id.clone());
        if info.type_.is_none() {
            info.type_ = Some("xmltv".to_owned());
        }
        let path = info.path.clone().unwrap_or_default();
        if path.is_empty() {
            return Err(ServiceError::InvalidInput(
                "listing provider Path is required".into(),
            ));
        }
        let data = serde_json::to_string(&info)
            .map_err(|e| LiveTvError::serialize("serialize listing provider", e))?;
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvListingProviders" ("Id","Type","Path","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Type"=excluded."Type","Path"=excluded."Path","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(info.type_.as_deref().unwrap_or("xmltv"))
        .bind(&path)
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(info)
    }

    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvListingProviders" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
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
        // `SeriesTimerId` scopes the guide to one series timer's airings by the
        // timer's `SeriesId`. Ferrofin's stored `SeriesTimerInfoDto` carries no
        // series id, so the scope can never be built — and upstream is explicit
        // about that case: "Better to return nothing than every program in the
        // database" (`LiveTvManager.GetPrograms`), which is exactly what
        // returning the unscoped guide would do.
        if query
            .series_timer_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        {
            return Ok(QueryResult::default());
        }

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
            let mut cb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(PROGRAM_COUNT);
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

    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        // M3U tuners are stateless HTTP streams — there is nothing to reset.
        Ok(())
    }

    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        for tuner in self.get_tuner_hosts().await? {
            let Some(id) = tuner.id.clone() else {
                continue;
            };
            // `GuideManager.RefreshChannelsInternal` asks the tuner-host
            // manager for the channels, which fans out over the registered
            // hosts (`TunerHostManager`/`BaseTunerHost.GetChannels`). Dispatch
            // on the row's own `Type`: feeding an HDHomeRun's URL to the M3U
            // parser produced an empty lineup and no error at all.
            let type_id = tuner.type_.as_deref().unwrap_or_default();
            let Some(host) = crate::tuner_host::find_host(&self.hosts, type_id) else {
                tracing::warn!(
                    tuner = %id,
                    type_id,
                    "live tv: no tuner host is registered for this type; its channels are skipped"
                );
                continue;
            };
            match host.get_channels(&tuner).await {
                Ok(channels) => self.replace_channels(&id, &channels).await?,
                Err(e) => {
                    // `BaseTunerHost.GetChannels` logs and falls back to its
                    // channel cache rather than failing the whole refresh, so
                    // one unreachable tuner must not lose the others' guide.
                    tracing::warn!(
                        tuner = %id,
                        url = tuner.url.as_deref().unwrap_or_default(),
                        error = %e,
                        "live tv: tuner lineup fetch failed"
                    );
                }
            }
        }
        // The lineup is settled; mirror it into `BaseItems` before the guide
        // goes in, exactly as `RefreshChannelsInternal` stores each channel
        // item (and cleans the departed ones) before it fetches programmes.
        self.sync_channel_items().await?;
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
        for provider in self.get_listing_providers().await? {
            let Some(path) = provider.path.as_deref() else {
                continue;
            };
            match self.fetcher.fetch(path).await {
                Ok(body) => self.insert_programs(&body, &provider, window).await?,
                Err(e) => tracing::warn!(%path, error = %e, "live tv: guide fetch failed"),
            }
        }
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
        self.json_get(
            r#"SELECT "Data" FROM "FerrofinLiveTvTimers" WHERE "Id" = ?1"#,
            id,
        )
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
            .json_list(r#"SELECT "Data" FROM "FerrofinLiveTvSeriesTimers""#)
            .await?;
        sort_series_timers(&mut timers, query);
        Ok(timers)
    }

    async fn get_series_timer(&self, id: &str) -> Result<Option<SeriesTimerInfoDto>, ServiceError> {
        self.json_get(
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
            let base_url = self.local_api_url()?;
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
        let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(PROGRAM_SELECT);
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
        let entities: Vec<_> = rows.iter().map(|r| program_entity(r, parse_dt)).collect();
        let mut dtos = self
            .dto_service()?
            .get_base_item_dtos(&entities, options, user, None, true)
            .await?;
        Self::add_info_to_program_dto(&mut dtos, rows, &options.fields);
        self.add_recording_info(&mut dtos, rows).await?;
        Ok(dtos)
    }

    /// Port of `LiveTvManager.AddInfoToProgramDto`: the airing fields the DTO
    /// service does not project (`StartDate`, `EpisodeTitle`, the true-only
    /// flags), and — only when the `ChannelInfo`/`ChannelImage` field was
    /// requested — the owning channel's name/number/media type.
    fn add_info_to_program_dto(
        dtos: &mut [BaseItemDto],
        rows: &[GuideProgramRow],
        fields: &[ItemFields],
    ) {
        let has_channel_info = fields.contains(&ItemFields::ChannelInfo);
        let has_channel_image = fields.contains(&ItemFields::ChannelImage);
        for (dto, row) in dtos.iter_mut().zip(rows) {
            dto.start_date = parse_dt(&row.start_date);
            dto.episode_title.clone_from(&row.episode_title);
            // C# `dto.IsNews |= program.IsNews` on a null `bool?`: a false flag
            // stays null and is never serialized, so only true flags appear.
            let flag = |on: bool| on.then_some(true);
            dto.is_repeat = dto.is_repeat.or(flag(row.is_repeat));
            dto.is_movie = dto.is_movie.or(flag(row.is_movie));
            dto.is_series = dto.is_series.or(flag(row.is_series));
            dto.is_sports = dto.is_sports.or(flag(row.is_sports));
            dto.is_live = dto.is_live.or(flag(row.is_live));
            dto.is_news = dto.is_news.or(flag(row.is_news));
            dto.is_kids = dto.is_kids.or(flag(row.is_kids));
            dto.is_premiere = dto.is_premiere.or(flag(row.is_premiere));
            if has_channel_info || has_channel_image {
                dto.channel_name = Some(row.channel_name.clone());
                dto.media_type = if row.channel_media_kind == "Radio" {
                    MediaType::Audio
                } else {
                    MediaType::Video
                };
                dto.channel_number.clone_from(&row.channel_number);
                // `ChannelPrimaryImageTag` needs a channel primary image; the
                // guide cache stores only the remote logo URL, which carries
                // no local image tag.
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
            let Some(external_id) = row.external_id.as_deref() else {
                continue;
            };
            let timer = timers.iter().find(|t| {
                t.base
                    .program_id
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(external_id))
            });
            if let Some(timer) = timer {
                if !matches!(
                    timer.status,
                    RecordingStatus::Cancelled | RecordingStatus::Error
                ) {
                    dto.timer_id.clone_from(&timer.base.id);
                    dto.status = Some(recording_status_name(timer.status).to_owned());
                }
                if let Some(series_timer_id) =
                    timer.series_timer_id.as_deref().filter(|s| !s.is_empty())
                {
                    dto.series_timer_id = Some(series_timer_id.to_owned());
                }
            }
        }
        Ok(())
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
    /// saved (which is also what upstream falls back to on a corrupt store).
    async fn live_tv_options(&self) -> LiveTvOptions {
        if self.paths.options_file.as_os_str().is_empty() {
            return LiveTvOptions::default();
        }
        let Ok(body) = tokio::fs::read_to_string(&self.paths.options_file).await else {
            return LiveTvOptions::default();
        };
        serde_json::from_str(&body).unwrap_or_default()
    }

    /// Every stored timer, whatever its status — C# `TimerManager.GetAll()`,
    /// which is what the restart path re-arms from. The trait's `get_timers`
    /// is the FILTERED view on top of this.
    async fn all_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        self.json_list(r#"SELECT "Data" FROM "FerrofinLiveTvTimers" ORDER BY "StartDate""#)
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
            .map(|(dto, external_id, series_id)| StoredSeriesTimer {
                dto,
                external_id,
                series_id,
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

    /// Port of `DefaultLiveTvService.UpdateTimersForSeriesTimer` (v10.11.8
    /// DefaultLiveTvService.cs:709-800): expand the series timer over the guide,
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
        // scheduled more than once records only the first of them.
        for mut loser in duplicate_showings_to_cancel(&enabled) {
            loser.status = RecordingStatus::Cancelled;
            self.persist_timer(&loser).await?;
            self.arm_timer(&loser);
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
        crate::dvr_repository::timer_for_program(
            &self.db,
            program_id,
            timer.base.external_program_id.as_deref(),
        )
        .await
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
        let manager = self.clone();
        let fire_id = id.clone();
        tracing::info!(
            timer_id = id,
            name = timer.base.name.as_deref().unwrap_or_default(),
            in_seconds = delay.as_secs(),
            "live tv: recording timer armed"
        );

        // Disarming sets a flag the waiting task reads; it never aborts the
        // task. A `JoinHandle::abort` would be a live hazard: by the time
        // `cancel_timer` runs, the task may already BE the capture, and killing
        // it there would strand the active recording, leave the tuner open and
        // freeze the row at `InProgress`. Cancelling a capture is
        // `cancel_recording`'s job (C# `RecordingsManager.CancelRecording`);
        // this only stops one that has not started.
        //
        // The flag is registered BEFORE the task exists, so a zero-delay timer
        // cannot fire before it is observable.
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.armed_timers_lock().insert(id, Arc::clone(&cancelled));
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            manager.on_timer_fired(fire_id).await;
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
        // The system timer is one-shot: once it has fired it is no longer
        // armed, and — crucially — cancelling the recording timer from here on
        // must stop the CAPTURE (`RecordingsManager.CancelRecording`) rather
        // than abort the task that is running it. Dropping the handle detaches
        // this task instead of aborting it.
        self.armed_timers_lock().remove(&timer_id);

        let Ok(Some(timer)) = self.get_timer(&timer_id).await else {
            return;
        };
        let mut info = TimerRecordingInfo::from_timer(&timer);
        if let Ok(Some(program)) = self.program_row_for_timer(&timer.base).await {
            apply_program_to_recording_info(&program, &mut info);
        }

        if info.recording_end_date() <= Utc::now() {
            tracing::warn!(
                timer_id,
                "live tv: the recording timer fired but the programme has already ended"
            );
            let _ = self.delete_by_id(DELETE_TIMER_SQL, &timer_id).await;
            return;
        }
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
        let (target, _series_path) =
            crate::dvr::recording_path(&info, &options, &self.paths.data_dir);
        let target = {
            let active: Vec<ActiveRecording> =
                self.active_recordings_lock().values().cloned().collect();
            crate::dvr::ensure_file_unique(&target, &info.id, &active)
        };

        let capture = self.capture(&mut timer, &info, &target).await;
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
        let started = self.start_capture(timer, info, target, &opened).await;
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

    /// The tuner host with this id, or its defaults when the row has gone (the
    /// channel outlives a deleted host only in a torn state, and upstream's
    /// `TunerHostInfo` defaults are the same "nothing configured" answer).
    async fn tuner_host(&self, id: &str) -> Result<TunerHostInfo, ServiceError> {
        Ok(self
            .get_tuner_hosts()
            .await?
            .into_iter()
            .find(|t| t.id.as_deref() == Some(id))
            .unwrap_or_default())
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
        let opened = match kind {
            crate::tuner_host::ChannelStreamKind::LegacyUdp(plan) => {
                let transcode_dir = self.transcode_dir()?;
                Some(
                    crate::hdhomerun::udp_stream::open_legacy_udp_stream(
                        plan,
                        &transcode_dir,
                        rand_lockkey(),
                    )
                    .await?,
                )
            }
            crate::tuner_host::ChannelStreamKind::Http => {
                let share = tuner.allow_stream_sharing
                    && source.protocol == MediaProtocol::Http
                    && !source.requires_looping
                    && self.can_share_stream(path, source).await;
                if share {
                    let transcode_dir = self.transcode_dir()?;
                    Some(
                        crate::stream::open_shared_http_stream(
                            self.tuner_source.as_ref(),
                            path,
                            &source.required_http_headers,
                            &transcode_dir,
                        )
                        .await?,
                    )
                } else {
                    None
                }
            }
        };

        let Some(opened) = opened else {
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
        let base_url = self.local_api_url()?;
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
        let in_use = open
            .values()
            .filter(|entry| {
                entry.is_sharing() && entry.tuner_host_id.as_deref() == Some(tuner_host_id)
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

    /// The directory a shared live stream's buffer is written to.
    fn transcode_dir(&self) -> Result<PathBuf, ServiceError> {
        if self.paths.transcode_dir.as_os_str().is_empty() {
            return Err(ServiceError::Backend(
                "live tv transcode path not wired".to_owned(),
            ));
        }
        Ok(self.paths.transcode_dir.clone())
    }

    /// The API base URL a live stream's buffered file is served from.
    fn local_api_url(&self) -> Result<&str, ServiceError> {
        self.local_api_url
            .get()
            .map(String::as_str)
            .ok_or_else(|| ServiceError::Backend("live tv local api url not wired".to_owned()))
    }

    /// Reads a JSON `Data` column across all rows of `sql`, deserializing each.
    async fn json_list<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>, ServiceError> {
        let rows = sqlx::query(sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
            .collect())
    }

    /// Reads and deserializes a single JSON `Data` column by id.
    async fn json_get<T: DeserializeOwned>(
        &self,
        sql: &str,
        id: &str,
    ) -> Result<Option<T>, ServiceError> {
        let data: Option<String> = sqlx::query_scalar(sql)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
    }

    /// Runs a `DELETE … WHERE "Id" = ?1` statement.
    async fn delete_by_id(&self, sql: &str, id: &str) -> Result<(), ServiceError> {
        sqlx::query(sql)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

/// Emits `WHERE` for the first predicate of a query and `AND` for each one
/// after it.
fn push_separator(qb: &mut QueryBuilder<'_, Sqlite>, first: &mut bool) {
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
    qb: &mut QueryBuilder<'_, Sqlite>,
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
            // Accepted divergence: upstream writes `StartDate > now && EndDate
            // < now`, a contradiction that can never match a programme, so
            // `isAiring=false` upstream returns nothing at all. Ferrofin reads
            // it as the negation that was plainly meant.
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
        // Upstream compares against the item's CleanName; the guide cache has
        // no cleaned column, so this is a case-insensitive title match.
        push_separator(qb, &mut first);
        qb.push(r#"LOWER(p."Title") = "#)
            .push_bind(name.to_lowercase());
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
        // matches any one of the requested genres.
        push_separator(qb, &mut first);
        qb.push(r#"EXISTS (SELECT 1 FROM json_each(p."Genres") WHERE LOWER("value") IN ("#);
        let mut list = qb.separated(",");
        for genre in &query.genres {
            list.push_bind(genre.to_lowercase());
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
fn push_program_order(qb: &mut QueryBuilder<'_, Sqlite>, order_by: &[(ItemSortBy, SortOrder)]) {
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
fn push_program_paging(qb: &mut QueryBuilder<'_, Sqlite>, limit: Option<i32>, start_index: i32) {
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
fn recording_status_name(status: RecordingStatus) -> &'static str {
    match status {
        RecordingStatus::New => "New",
        RecordingStatus::InProgress => "InProgress",
        RecordingStatus::Completed => "Completed",
        RecordingStatus::Cancelled => "Cancelled",
        RecordingStatus::ConflictedOk => "ConflictedOk",
        RecordingStatus::ConflictedNotOk => "ConflictedNotOk",
        RecordingStatus::Error => "Error",
    }
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
/// Port of `XmlTvListingsProvider.GetProgramInfo`'s `ShowId` block (v10.11.8
/// XmlTvListingsProvider.cs:186-213), bugs included: the season and episode
/// branches REPLACE `uniqueString` rather than appending to it, so a programme
/// with an episode number hashes `"-{episode}"` alone. Ported verbatim, because
/// this value decides which showings a series timer drops.
///
/// Two knowing gaps, both open work items rather than choices:
///   * upstream's `else` arm takes the listings' own `program.ProgramId` when the
///     guide supplies one (`<episode-num system="dd_progid">`); Ferrofin's XMLTV
///     reader does not capture that element yet, so the MD5 arm always runs. A
///     guide carrying dd_progid would group differently — porting it means
///     reading the element in `crate::xmltv` and carrying it to here.
///   * this derives the value from the stored guide row instead of persisting it
///     at ingest as `GuideManager` does. The inputs are all on the row, so the
///     value is identical; it is a column Ferrofin does not need.
fn program_show_id(program: &GuideProgramRow) -> String {
    let mut unique = format!(
        "{}{}",
        program.title,
        program.episode_title.as_deref().unwrap_or_default()
    );
    if let Some(season) = program.season_number {
        unique = format!("-{season}");
    }
    if let Some(episode) = program.episode_number {
        unique = format!("-{episode}");
    }
    let mut show_id = ferrofin_common::extensions::get_md5(&unique)
        .simple()
        .to_string();
    // "If we don't have valid episode info, assume it's a unique program,
    // otherwise recordings might be skipped."
    if program.is_series && !program.is_repeat && program.episode_number.unwrap_or(0) == 0 {
        let start = parse_dt(&program.start_date).unwrap_or_else(Utc::now);
        show_id.push_str(&dotnet_ticks(start).to_string());
    }
    show_id
}

/// A UTC instant as .NET `DateTime.Ticks`: 100-nanosecond intervals since
/// `0001-01-01T00:00:00`, which is what `ShowId`'s uniqueness suffix appends.
fn dotnet_ticks(at: DateTime<Utc>) -> i64 {
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
/// `HandleDuplicateShowIds` (v10.11.8 DefaultLiveTvService.cs:671-707): timers
/// are grouped by `ShowId`, a blank group is skipped, a group of one is skipped,
/// a group whose key ends `"0000"` is skipped (upstream's guard for a
/// SchedulesDirect show id with no sub-key, jellyfin/jellyfin#5856), and in what
/// is left every showing after the first is cancelled.
///
/// TODO(work item, not a deferral): upstream orders the group
/// `OrderByDescending(t => GetLiveTvChannel(t).IsHD).ThenBy(t => t.StartDate)`,
/// so an HD showing beats an earlier SD one. Ferrofin's Live TV has no `IsHD`
/// at all — the M3U parser does not read a channel's HD flag and no table stores
/// one — so only the `StartDate` half is ported and the earliest showing always
/// wins. Closing it means carrying HD through `crate::m3u` → the channel row →
/// here. It can only differ on a lineup mixing HD and SD feeds of one channel.
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

    use ferrofin_db::Database;
    use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
    use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
    use ferrofin_traits::stubs::LiveTvManager;

    use ferrofin_model::dto::{DayOfWeek, SortOrder};
    use ferrofin_model::live_tv::ItemSortBy;

    use super::{FerrofinLiveTvManager, SourceFetcher, parse_dt};

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

    async fn manager_with(fetcher: FakeFetcher) -> FerrofinLiveTvManager {
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        FerrofinLiveTvManager::new(
            db,
            std::sync::Arc::new(fetcher),
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_dto(Arc::new(FakeDto))
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
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
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
            Arc::new(FakeFetcher(HashMap::new())),
            "srv".to_owned(),
            std::env::temp_dir().join("ferrofin-livetv-manager-tests"),
        )
        .with_users(users);
        // `IsLiveTvEnabled`: the permission alone is not enough — a tuner host must exist.
        assert!(
            mgr.get_live_tv_info()
                .await
                .expect("info")
                .enabled_users
                .is_empty()
        );
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
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
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
        assert!(mgr.get_tuner_hosts().await.expect("list2").is_empty());
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
        assert!(!source.supports_direct_play);
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
            .save_listing_provider(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            })
            .await
            .expect("save");
        assert_eq!(saved.type_.as_deref(), Some("xmltv"));
        let id = saved.id.clone().expect("id");
        assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 1);
        mgr.delete_listing_provider(&id).await.expect("delete");
        assert!(mgr.get_listing_providers().await.expect("list2").is_empty());
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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            news_categories: None,
            movie_categories: None,
            kids_categories: None,
            sports_categories: None,
            ..ListingsProviderInfo::default()
        })
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

    #[tokio::test]
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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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

        // Deleting the tuner host cascades its channels (and their programmes).
        let host_id = mgr
            .get_tuner_hosts()
            .await
            .expect("h")
            .pop()
            .unwrap()
            .id
            .unwrap();
        mgr.delete_tuner_host(&host_id).await.expect("del host");
        assert_eq!(
            mgr.get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
                .await
                .expect("c3")
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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/ae.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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
        use std::fmt::Write as _;
        let now = chrono::Utc::now();
        let mut xml = String::from(
            "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
        );
        for (channel, from, to, title, genre) in [
            // Already over, but inside the refresh's one-hour lead-in — an
            // airing that ended more than an hour ago is not in the guide
            // window at all, so upstream would not have it either and neither
            // does Ferrofin.
            ("one.tv", -55, -45, "Aired", "News"),
            ("one.tv", -30, 30, "Now Playing", "Drama"),
            ("one.tv", 120, 180, "Later", "Comedy"),
            ("two.tv", -10, 50, "Two Now", "News"),
        ] {
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
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), relative_guide());
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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
        let view = Uuid::from_u128(0x2b2b_ca16_aacc_8a14_d53a_11bb_829e_afa5);
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;
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
            items,
            Arc::new(FixedLiveTvView(view)),
        );
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.refresh_guide().await.expect("refresh");

        let lineup = channel_ids(&mgr).await;
        assert_eq!(lineup.len(), 2);
        let stored = stored_channel_items(&mgr).await;
        assert_eq!(
            stored.len(),
            2,
            "every channel in the lineup is an item row"
        );
        let parent = ferrofin_db::store::guid_to_db(view);
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
        assert_eq!(stored_channel_items(&mgr).await.len(), 3);

        mgr.refresh_guide().await.expect("second refresh");
        let after = stored_channel_items(&mgr).await;
        assert_eq!(after.len(), 2, "the departed channel item was cleaned");
        assert!(
            !after
                .iter()
                .any(|(id, ..)| Uuid::parse_str(id).ok() == Some(ghost))
        );
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
                transcode_dir: transcode_dir.to_path_buf(),
                ..LiveTvPaths::default()
            });
        mgr.set_local_api_url("http://127.0.0.1:8096");
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
        assert!(
            mgr.get_channel_media_sources(Uuid::from_u128(0xdead))
                .await
                .expect("sources")
                .is_empty()
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
                transcode_dir: root.join("transcodes"),
                data_dir: root.join("data"),
                options_file: root.join("named").join("livetv.json"),
            });
        mgr.set_local_api_url("http://127.0.0.1:8096");
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
                transcode_dir: root.join("transcodes"),
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
        assert!(!id.is_empty());
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
        assert!(!again.is_empty());
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
        assert!(
            mgr.get_timers_matching(&ferrofin_model::live_tv::TimerQuery {
                is_active: Some(true),
                ..ferrofin_model::live_tv::TimerQuery::default()
            })
            .await
            .expect("timers")
            .is_empty()
        );
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
        // The recording settled rather than being stuck mid-flight.
        let in_progress = mgr
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
        assert!(
            in_progress.items.is_empty(),
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

        // Accepted divergence: upstream's `isAiring=false` predicate is a
        // contradiction that matches nothing; Ferrofin reads it as "not on now".
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
        assert!(!id.is_empty());
        let timers = mgr.get_timers().await.expect("list");
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].base.channel_id, ch);
        assert_eq!(timers[0].base.name.as_deref(), Some("Record the news"));

        let got = mgr.get_timer(&id).await.expect("get").expect("some");
        assert_eq!(got.base.id.as_deref(), Some(id.as_str()));

        mgr.cancel_timer(&id).await.expect("cancel");
        assert!(mgr.get_timers().await.expect("list2").is_empty());
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
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), series_guide());
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
            type_: Some("m3u".to_owned()),
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
        .await
        .expect("provider");
        mgr.refresh_guide().await.expect("refresh");
        mgr
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
        assert!(all_series_timers(&mgr).await.is_empty());
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
        assert!(all_series_timers(&mgr).await.is_empty());
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
        assert!(duplicate_showings_to_cancel(&[showing("aa", 0)]).is_empty());
        // …a blank ShowId groups nothing (upstream skips the empty key)…
        assert!(duplicate_showings_to_cancel(&[showing("", 0), showing("", 60)]).is_empty());
        // …and neither does one ending "0000" (jellyfin/jellyfin#5856).
        assert!(
            duplicate_showings_to_cancel(&[showing("ab0000", 0), showing("ab0000", 60)]).is_empty()
        );
        // Different programmes never suppress each other.
        assert!(duplicate_showings_to_cancel(&[showing("aa", 0), showing("bb", 60)]).is_empty());
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
        assert!(!kept.is_empty());
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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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
        assert!(
            names(
                with(LiveTvChannelQuery {
                    is_news: Some(true),
                    ..LiveTvChannelQuery::default()
                })
                .await
            )
            .is_empty()
        );
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

    /// `seriesTimerId` cannot be scoped (Ferrofin's series timers carry no
    /// series id), so upstream's "better to return nothing than every program
    /// in the database" branch applies — but a blank value must NOT blank the
    /// guide, or an empty `?seriesTimerId=` query param would.
    #[tokio::test]
    async fn series_timer_scope_returns_nothing_and_ignores_a_blank_value() {
        let mgr = manager_with_relative_guide().await;
        let scoped = mgr
            .get_programs(
                &InternalItemsQuery {
                    series_timer_id: Some("st-x".to_owned()),
                    ..InternalItemsQuery::default()
                },
                &DtoOptions::default(),
            )
            .await
            .expect("scoped");
        assert!(scoped.items.is_empty());
        assert_eq!(scoped.total_record_count, 0);
        assert_eq!(scoped.start_index, 0);

        for blank in ["", "   "] {
            let unscoped = mgr
                .get_programs(
                    &InternalItemsQuery {
                        series_timer_id: Some(blank.to_owned()),
                        ..InternalItemsQuery::default()
                    },
                    &DtoOptions::default(),
                )
                .await
                .expect("blank");
            assert_eq!(
                unscoped.items.len(),
                4,
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
