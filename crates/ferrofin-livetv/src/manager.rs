//! The real [`LiveTvManager`] over the SQLite channel/guide cache.
//!
//! Configuration (tuner hosts, listing providers) is stored verbatim as JSON so
//! reads round-trip the DTO. `refresh_guide` fetches each tuner host (M3U) and
//! listing provider (XMLTV), rewrites `FerrofinLiveTvChannels`/`FerrofinLiveTvPrograms`, and
//! binds programmes to channels by the tuner `tvg-id` / XMLTV `channel id`.
//! Channels and programmes are surfaced to clients as `BaseItemDto`s.

use std::collections::{HashMap, HashSet};
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
use ferrofin_model::dto::SortOrder;
use ferrofin_model::dto::{
    BaseItemDto, MediaSourceInfo, MediaSourceType, NameIdPair, NameValuePair,
};
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_model::live_tv::{
    ChannelMappingOptionsDto, ChannelType, ItemSortBy, ListingsProviderInfo, LiveTvInfo,
    LiveTvServiceInfo, LiveTvServiceStatus, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
    TunerChannelMapping, TunerHostInfo,
};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::{LiveStreamFile, LiveTvChannelQuery, LiveTvManager};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dvr::{ActiveRecording, RecorderKind, RecordingInput, TimerRecordingInfo};
use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::m3u::parse_m3u;
use crate::mapping::{
    EpgChannel, EpgChannelData, TunerChannel, epg_channel_for_tuner_channel,
    eq_ordinal_ignore_case, is_listing_provider_enabled_for_tuner, m3u_channel_id,
    tuner_channel_mapping,
};
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

/// Namespace for deriving stable channel UUIDs (v5) from `tuner-host|tvg-id`.
const CHANNEL_NS: Uuid = Uuid::from_u128(0x6c74_7663_6861_6e6e_656c_735f_6e73_3031);
/// Namespace for deriving stable programme UUIDs (v5) from `channel|start`.
const PROGRAM_NS: Uuid = Uuid::from_u128(0x6c74_7670_726f_6772_616d_735f_6e73_3031);

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

/// The `ON CONFLICT` tail of the channel upsert: a channel already in the
/// lineup keeps its first-seen `DateCreated` (upstream stamps it only on a NEW
/// `LiveTvChannel`; a pre-migration NULL heals to the refresh instant via the
/// COALESCE) and takes the playlist's current word for everything else.
const CHANNEL_UPSERT_CONFLICT: &str = r#" ON CONFLICT("Id") DO UPDATE SET
                    "DateCreated"=COALESCE("FerrofinLiveTvChannels"."DateCreated", excluded."DateCreated"),
                    "TunerHostId"=excluded."TunerHostId","TvgId"=excluded."TvgId",
                    "Name"=excluded."Name","Number"=excluded."Number",
                    "ImageUrl"=excluded."ImageUrl","ChannelType"=excluded."ChannelType",
                    "StreamUrl"=excluded."StreamUrl","SortIndex"=excluded."SortIndex""#;

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
    /// cancels the in-flight refresh, we queue behind it, and queuing behind a
    /// lock that is waiting on a third-party HTTP source is how one wedged tuner
    /// URL stalls the whole subsystem.
    guide_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account-less Schedules Direct surface (country list), sharing the
    /// fetcher and caching under the application cache directory.
    schedules_direct: SchedulesDirect,
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
            // `{channelId}_{start:O}`. Upstream formats the file's own
            // DateTimeOffset (a `+0100` guide yields `…+01:00`); the parser here
            // normalizes to UTC, so the offset is always +00:00 — an accepted
            // divergence for an id no DTO field surfaces.
            external_id: prog.start.map(|start| {
                format!(
                    "{}_{}.{:07}+00:00",
                    prog.channel_id,
                    start.format("%Y-%m-%dT%H:%M:%S"),
                    start.timestamp_subsec_nanos() / 100
                )
            }),
            external_series_id,
        }
    }
}

impl FerrofinLiveTvManager {
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
        Self {
            db,
            fetcher,
            users: None,
            server_id,
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
            guide_lock: Arc::new(tokio::sync::Mutex::new(())),
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

    /// The wired DTO service, or the honest error when the composition root
    /// (or a test) never attached one.
    fn dto_service(&self) -> Result<&Arc<dyn DtoService>, ServiceError> {
        self.dto
            .get()
            .ok_or_else(|| ServiceError::Backend("live tv dto service not wired".to_owned()))
    }

    /// Refreshes the channel lineup for one tuner host from its M3U body,
    /// returning the ids it wrote for the caller's `CleanDatabase` pass.
    ///
    /// An UPSERT, not a delete-and-reinsert: `GuideManager.RefreshChannelsInternal`
    /// saves each channel it found and only removes the ones the pass did not
    /// re-emit, at the very end. Deleting first would cascade every programme of
    /// the tuner away (`FK_LiveTvChannels_TunerHosts_TunerHostId ON DELETE
    /// CASCADE` reaches them through the channel rows), so a client reading the
    /// guide mid-refresh would see it empty and briefly wrong.
    async fn replace_channels(
        &self,
        tuner_id: &str,
        m3u_body: &str,
    ) -> Result<Vec<String>, ServiceError> {
        let channels = parse_m3u(m3u_body);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        // `DateCreated` is bound for every row, but only a NEW channel keeps
        // it: the upsert's COALESCE holds an existing channel's first-seen
        // instant, the way `GuideManager.GetChannel` only stamps
        // `DateCreated = DateTime.UtcNow` on a NEW item.
        let now = datetime_to_db(Utc::now());
        let mut written = Vec::with_capacity(channels.len());

        // 10 columns per row; chunked multi-row insert instead of one round-trip
        // per channel.
        for (chunk_index, chunk) in channels.chunks(SQLITE_BIND_LIMIT / 10).enumerate() {
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
                r#"INSERT INTO "FerrofinLiveTvChannels"
                   ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType","StreamUrl","SortIndex","DateCreated") "#,
            );
            let base = chunk_index * (SQLITE_BIND_LIMIT / 10);
            qb.push_values(chunk.iter().enumerate(), |mut b, (offset, ch)| {
                let key = if ch.id.is_empty() { &ch.name } else { &ch.id };
                let id = guid_to_db(Uuid::new_v5(
                    &CHANNEL_NS,
                    format!("{tuner_id}|{key}").as_bytes(),
                ));
                let channel_type = if ch.is_radio { "Radio" } else { "Tv" };
                let date_created = now.clone();
                written.push(id.clone());
                b.push_bind(id)
                    .push_bind(tuner_id)
                    .push_bind(&ch.id)
                    .push_bind(&ch.name)
                    .push_bind(&ch.number)
                    .push_bind(&ch.logo)
                    .push_bind(channel_type)
                    .push_bind(&ch.url)
                    .push_bind(i64::try_from(base + offset).unwrap_or(i64::MAX))
                    .push_bind(date_created);
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

    /// The display name of a listings-provider backend, by its `Type`.
    ///
    /// Port of `ListingsManager.GetProvider`, which resolves the registered
    /// `IListingsProvider` for a type and throws `ResourceNotFoundException`
    /// when there is none. Ferrofin registers the XMLTV backend
    /// (`XmlTvListingsProvider.Name`/`.Type`); a Schedules Direct listings
    /// provider is an open work item, and until it is ported its type is
    /// genuinely unregistered here, which is the same 404 the C# gives for any
    /// type it has no provider for.
    fn listings_provider_name(provider_type: Option<&str>) -> Result<&'static str, ServiceError> {
        match provider_type {
            Some(t) if eq_ordinal_ignore_case(t, "xmltv") => Ok("XmlTV"),
            other => Err(ServiceError::not_found(format!(
                "Couldn't find provider of type {}",
                other.unwrap_or_default()
            ))),
        }
    }

    /// A listings provider's own channel list.
    ///
    /// Port of `XmlTvListingsProvider.GetChannels`: the guide document's
    /// `<channel>` elements, with the number falling back to the id when the
    /// document carries none (Ferrofin's XMLTV reader parses no channel number,
    /// so that fallback always applies) — the fallback matters because it is
    /// what feeds the by-number arm of the match ladder.
    async fn provider_channels(
        &self,
        info: &ListingsProviderInfo,
    ) -> Result<Vec<EpgChannel>, ServiceError> {
        Self::listings_provider_name(info.type_.as_deref())?;
        let path = info.path.clone().unwrap_or_default();
        let body = self.fetcher.fetch(&path).await?;
        Ok(parse_xmltv(&body)
            .channels
            .into_iter()
            .map(|c| EpgChannel {
                number: c.id.clone(),
                id: c.id,
                name: c.display_name,
            })
            .collect())
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
                    TunerChannel {
                        id: m3u_channel_id(url, &row.stream_url),
                        tuner_channel_id: row.tvg_id,
                        number: row.number,
                        name: row.name,
                        tuner_host_id: host_id.to_owned(),
                    },
                ));
            }
        }
        Ok(out)
    }

    /// Inserts programmes from an XMLTV body, binding each to every channel that
    /// takes its listings from the programme's guide channel, classified against
    /// the listings provider's category lists as
    /// `XmlTvListingsProvider.GetProgramInfo` does. Returns the ids it wrote, for
    /// the caller's `CleanDatabase` pass.
    ///
    /// The tuner-channel → guide-channel binding is
    /// `ListingsManager.GetEpgChannelFromTunerChannel`, so the provider's manual
    /// `ChannelMappings` actually move listings — the join is not the raw
    /// `tvg-id`.
    async fn insert_programs(
        &self,
        xmltv_body: &str,
        provider: &ListingsProviderInfo,
    ) -> Result<Vec<String>, ServiceError> {
        let guide = parse_xmltv(xmltv_body);
        let classes = CategoryClasses::from_provider(provider);

        // Which stored channel takes its listings from which guide channel —
        // the same match ladder the channel-mapping dialog renders.
        let epg_channels: Vec<EpgChannel> = guide
            .channels
            .iter()
            .map(|c| EpgChannel {
                number: c.id.clone(),
                id: c.id.clone(),
                name: c.display_name.clone(),
            })
            .collect();
        let epg = EpgChannelData::new(&epg_channels);
        let mut by_epg_channel: HashMap<String, Vec<String>> = HashMap::new();
        for (stored_id, tuner_channel) in self.channels_for_listings_provider(provider).await? {
            if let Some(matched) =
                epg_channel_for_tuner_channel(&provider.channel_mappings, &tuner_channel, &epg)
            {
                by_epg_channel
                    .entry(matched.id.clone())
                    .or_default()
                    .push(stored_id);
            }
        }

        // Flatten to one (channel, programme) row per binding, then insert in
        // chunked multi-row statements (25 columns per row) instead of one
        // round-trip per programme.
        let rows: Vec<_> = guide
            .programmes
            .iter()
            .flat_map(|prog| {
                let channel_ids = by_epg_channel
                    .get(&prog.channel_id)
                    .map_or(&[][..], Vec::as_slice);
                let start = opt_datetime_to_db(prog.start).unwrap_or_default();
                let end = opt_datetime_to_db(prog.stop);
                let genres = if prog.categories.is_empty() {
                    None
                } else {
                    serde_json::to_string(&prog.categories).ok()
                };
                let class = classes.classify(prog);
                channel_ids.iter().map(move |channel_id| {
                    let id = Uuid::new_v5(&PROGRAM_NS, format!("{channel_id}|{start}").as_bytes());
                    ProgramRow {
                        id: guid_to_db(id),
                        channel_id,
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
                    .push_bind(&class.external_id)
                    .push_bind(&class.external_series_id)
                    .push_bind(class.season_number)
                    .push_bind(class.episode_number)
                    .push_bind(&now);
            });
            qb.push(PROGRAM_UPSERT_CONFLICT);
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(rows.into_iter().map(|row| row.id).collect())
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
        // `TunerHostManager.SaveTunerHost`: the supplied id is honoured only
        // when it names a host that already exists (`Array.FindIndex(...,
        // OrdinalIgnoreCase)`); `index == -1 || IsNullOrWhiteSpace(info.Id)`
        // mints a fresh `Guid.NewGuid().ToString("N")` — 32 lowercase hex, no
        // dashes. The existing row's stored spelling wins on a case-differing
        // match, because Ferrofin's channel rows reference it by that string.
        let id = match info.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(wanted) => crate::guide_repository::existing_config_id(
                &self.db,
                crate::guide_repository::TUNER_HOSTS_TABLE,
                wanted,
            )
            .await?
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
            None => Uuid::new_v4().simple().to_string(),
        };
        info.id = Some(id.clone());
        if info.type_.is_none() {
            info.type_ = Some("m3u".to_owned());
        }
        let url = info.url.clone().unwrap_or_default();
        if url.is_empty() {
            return Err(ServiceError::InvalidInput(
                "tuner host Url is required".into(),
            ));
        }
        let data = serde_json::to_string(&info)
            .map_err(|e| LiveTvError::serialize("serialize tuner host", e))?;
        self.db
            .upsert_live_tv_tuner_host(&id, &url, info.type_.as_deref().unwrap_or("m3u"), &data)
            .await
            .map_err(ServiceError::from)?;
        self.tuner_flag.store(true, Ordering::Relaxed);
        Ok(info)
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
        // `ListingsManager.SaveListingProvider`, same rule as the tuner hosts:
        // an id that names no configured provider is replaced by a fresh
        // `Guid.NewGuid().ToString("N")`.
        let id = match info.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(wanted) => crate::guide_repository::existing_config_id(
                &self.db,
                crate::guide_repository::LISTING_PROVIDERS_TABLE,
                wanted,
            )
            .await?
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
            None => Uuid::new_v4().simple().to_string(),
        };
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
        // `ListingsManager.DeleteListingsProvider` filters OrdinalIgnoreCase.
        sqlx::query(
            r#"DELETE FROM "FerrofinLiveTvListingProviders" WHERE "Id" = ?1 COLLATE NOCASE"#,
        )
        .bind(id)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_lineups(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        _country: Option<&str>,
        _location: Option<&str>,
    ) -> Result<Vec<NameIdPair>, ServiceError> {
        // Port of `ListingsManager.GetLineups`. With a blank id the C# calls
        // `GetProvider(providerType).GetLineups(null, …)`: an unregistered type
        // is `ResourceNotFoundException` (404), and the xmltv backend
        // dereferences the null `info` inside `GetXml` and 500s. Ferrofin
        // answers 404 to both — there is no provider to read a document from,
        // and a NullReferenceException is not a contract.
        let Some(id) = provider_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(ServiceError::not_found(format!(
                "Couldn't find provider of type {}",
                provider_type.unwrap_or_default()
            )));
        };
        let info = self.listing_provider_by_id(id).await?;
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
        let provider_name = Self::listings_provider_name(info.type_.as_deref())?;
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
        let info = self.save_listing_provider(info).await?;

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
        remove_fields(&mut list_options);
        let entities: Vec<_> = page.iter().map(|r| channel_entity(r, parse_dt)).collect();
        let mut dtos = self
            .dto_service()?
            .get_base_item_dtos(&entities, &list_options, query.user.as_ref(), None, true)
            .await?;
        self.add_channel_info(&mut dtos, &page, &list_options, query.user.as_ref())
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
        // projection inside `add_channel_info` strips them.
        let entity = channel_entity(&row, parse_dt);
        let mut dtos = vec![
            self.dto_service()?
                .get_base_item_dto(&entity, options, user, None)
                .await?,
        ];
        self.add_channel_info(&mut dtos, std::slice::from_ref(&row), options, user)
            .await?;
        Ok(dtos.pop())
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
        // FETCH FIRST, OUTSIDE THE LOCK. `guide_lock` exists to keep two passes
        // from interleaving their WRITES (see the field doc); holding it across
        // the tuner/guide fetches would put unbounded third-party network I/O
        // inside it, so one wedged M3U source would stall every later refresh —
        // a failure mode upstream does not have, because its one scheduled task
        // is CANCELLED when the next run is queued rather than queued behind.
        //
        // `GuideManager.RefreshChannels` collects the ids this pass (re)wrote
        // and then drops everything else through `CleanDatabase` — but only
        // when nothing threw: its catch sets `cleanDatabase = false`, so a
        // tuner or guide that could not be read never empties the cache.
        let mut clean_database = true;
        let mut tuner_bodies: Vec<(String, String)> = Vec::new();
        for tuner in self.get_tuner_hosts().await? {
            let (Some(id), Some(url)) = (tuner.id.as_deref(), tuner.url.as_deref()) else {
                clean_database = false;
                continue;
            };
            match self.fetcher.fetch(url).await {
                Ok(body) => tuner_bodies.push((id.to_owned(), body)),
                Err(e) => {
                    clean_database = false;
                    tracing::warn!(%url, error = %e, "live tv: tuner fetch failed");
                }
            }
        }
        let mut guide_bodies: Vec<(ListingsProviderInfo, String)> = Vec::new();
        for provider in self.get_listing_providers().await? {
            let Some(path) = provider.path.as_deref() else {
                clean_database = false;
                continue;
            };
            match self.fetcher.fetch(path).await {
                Ok(body) => guide_bodies.push((provider.clone(), body)),
                Err(e) => {
                    clean_database = false;
                    tracing::warn!(%path, error = %e, "live tv: guide fetch failed");
                }
            }
        }

        // Now the write half, one pass at a time. Channels go in before
        // programmes because `insert_programs` joins against the stored lineup.
        let _guard = self.guide_lock.lock().await;
        let mut kept_channels: HashSet<String> = HashSet::new();
        for (id, body) in &tuner_bodies {
            kept_channels.extend(self.replace_channels(id, body).await?);
        }
        let mut kept_programs: HashSet<String> = HashSet::new();
        for (provider, body) in &guide_bodies {
            kept_programs.extend(self.insert_programs(body, provider).await?);
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
                .filter(|id| !kept_programs.contains(id))
                .collect();
            if !stale.is_empty() {
                crate::guide_repository::delete_programs(&self.db, &stale).await?;
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
        let Some((path, tuner_host_id)) =
            crate::guide_repository::channel_stream_source(&self.db, id).await?
        else {
            return Ok(Vec::new());
        };
        let tuner = self.tuner_host(&tuner_host_id).await?;
        let mut source = crate::stream::create_media_source_info(&path, &tuner);
        crate::stream::normalize(&mut source);
        Self::stamp_source(&mut source);
        // `LiveTvMediaSourceProvider.GetMediaSourcesInternal`: the open token is
        // `{item type}_{item id "N"}_{source id}`. The media-source manager
        // prefixes the provider key before it reaches a client.
        source.open_token = Some(format!(
            "{OPEN_TOKEN_ITEM_TYPE}{STREAM_ID_DELIMITER}{}{STREAM_ID_DELIMITER}{}",
            id.simple(),
            source.id.clone().unwrap_or_default()
        ));
        Ok(vec![source])
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
        let Some((path, tuner_host_id)) =
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

        let mut source = crate::stream::create_media_source_info(&path, &tuner);
        crate::stream::normalize(&mut source);
        Self::stamp_source(&mut source);

        let share = tuner.allow_stream_sharing
            && source.protocol == MediaProtocol::Http
            && !source.requires_looping
            && self.can_share_stream(&path, &source).await;

        let (unique_id, opened_at, alive, kind) = if share {
            let transcode_dir = self.transcode_dir()?;
            let base_url = self.local_api_url()?;
            let opened = crate::stream::open_shared_http_stream(
                self.tuner_source.as_ref(),
                &path,
                &source.required_http_headers,
                &transcode_dir,
            )
            .await?;
            // Every consumer now reads the one buffered copy, not the tuner
            // (C# `SharedHttpStream.Open`).
            source.path = Some(format!(
                "{base_url}/LiveTv/LiveStreamFiles/{}/stream.{LIVE_STREAM_BUFFER_CONTAINER}",
                opened.unique_id
            ));
            source.protocol = MediaProtocol::Http;
            source.container = Some(LIVE_STREAM_BUFFER_CONTAINER.to_owned());
            (
                opened.unique_id,
                opened.opened_at,
                opened.alive,
                LiveStreamKind::Shared {
                    temp_path: opened.temp_path,
                    task: opened.task,
                },
            )
        } else {
            // The pass-through stream: nothing is buffered and the media source
            // keeps the tuner URL (C# bare `LiveStream`).
            (
                Uuid::new_v4().simple().to_string(),
                Utc::now(),
                Arc::new(std::sync::atomic::AtomicBool::new(true)),
                LiveStreamKind::Direct,
            )
        };

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
        self.json_list(r#"SELECT "Data" FROM "FerrofinLiveTvTimers" ORDER BY "StartDate""#)
            .await
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
                self.persist_timer(&revived).await?;
                self.arm_timer(&revived);
                return Ok(existing_id);
            }
            return Err(ServiceError::InvalidInput(
                "A scheduled recording already exists for this program.".to_owned(),
            ));
        }

        // The id is always the server's to mint: C# overwrites whatever the
        // client posted with a fresh GUID.
        timer.base.id = Some(Uuid::new_v4().simple().to_string());
        timer.base.type_ = Some("Timer".to_owned());
        timer.base.service_name = Some(ferrofin_traits::stubs::LIVE_TV_SERVICE_NAME.to_owned());
        // `CopyProgramInfoToTimerInfo`: the guide is the authority on what this
        // timer is actually recording.
        if let Some(program) = self.timer_program_row(&timer).await? {
            copy_program_into_timer(&program, &mut timer);
        }
        let id = self.persist_timer(&timer).await?;
        self.arm_timer(&timer);
        Ok(id)
    }

    async fn update_timer(&self, id: &str, mut timer: TimerInfoDto) -> Result<(), ServiceError> {
        timer.base.id = Some(id.to_owned());
        self.persist_timer(&timer).await?;
        // C# `TimerManager.Update` re-arms the system timer behind it.
        self.arm_timer(&timer);
        Ok(())
    }

    async fn cancel_timer(&self, id: &str) -> Result<(), ServiceError> {
        // Port of `DefaultLiveTvService.CancelTimerInternal`: the timer goes to
        // `Cancelled`; a manual one (nothing scheduled it) is deleted outright,
        // and any capture it started stops.
        match self.get_timer(id).await? {
            Some(mut timer) => {
                timer.status = RecordingStatus::Cancelled;
                if timer
                    .series_timer_id
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(str::is_empty)
                {
                    self.delete_by_id(DELETE_TIMER_SQL, id).await?;
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

    async fn get_series_timers(&self) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
        self.json_list(r#"SELECT "Data" FROM "FerrofinLiveTvSeriesTimers" ORDER BY "Name""#)
            .await
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
        let id = ensure_id(&mut timer.base.id);
        let data = to_json(&timer)?;
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvSeriesTimers" ("Id","ChannelId","ProgramId","Name","Data")
               VALUES (?1,?2,?3,?4,?5)
               ON CONFLICT("Id") DO UPDATE SET
                 "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
                 "Name"=excluded."Name","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(guid_to_db(timer.base.channel_id))
        .bind(&timer.base.program_id)
        .bind(timer.base.name.clone().unwrap_or_default())
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(id)
    }

    async fn update_series_timer(
        &self,
        id: &str,
        mut timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        timer.base.id = Some(id.to_owned());
        self.create_series_timer(timer).await.map(|_| ())
    }

    async fn cancel_series_timer(&self, id: &str) -> Result<(), ServiceError> {
        // Drop the series timer and any timers it scheduled.
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvTimers" WHERE "SeriesTimerId" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
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
        // so a restart mid-schedule still records.
        for timer in self.get_timers().await? {
            self.arm_timer(&timer);
        }
        Ok(())
    }

    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
        self.schedules_direct.get_available_countries().await
    }
}

impl FerrofinLiveTvManager {
    /// Port of `LiveTvManager.AddChannelInfo`: the channel-only DTO fields
    /// (`Number`/`ChannelNumber`/`ChannelType`, the `ExternalServiceId`
    /// provider id), plus — when `options.add_current_program` — each
    /// channel's currently-airing programme, fetched with ONE query for the
    /// whole page and projected through the programme DTO path.
    async fn add_channel_info(
        &self,
        dtos: &mut [BaseItemDto],
        rows: &[ChannelRow],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<(), ServiceError> {
        for (dto, row) in dtos.iter_mut().zip(rows) {
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
        if !options.add_current_program || rows.is_empty() {
            return Ok(());
        }

        // One airing query for the page: `MaxStartDate = MinEndDate = now`,
        // `Limit = channel count`, start-date ascending.
        let now = Utc::now();
        let channel_ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
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

    /// Writes a timer through, DTO and promoted columns together.
    async fn persist_timer(&self, timer: &TimerInfoDto) -> Result<String, ServiceError> {
        crate::dvr_repository::upsert_timer(&self.db, timer).await
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
    async fn timer_program_row(
        &self,
        timer: &TimerInfoDto,
    ) -> Result<Option<GuideProgramRow>, ServiceError> {
        if let Some(id) = timer
            .base
            .program_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            && let Some(row) = self.program_row(id).await?
        {
            return Ok(Some(row));
        }
        let Some(external) = timer
            .base
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
                    channel_ids: vec![timer.base.channel_id],
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
        if let Ok(Some(program)) = self.timer_program_row(&timer).await {
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

/// Ensures a DTO id field is set, generating a fresh UUID when absent, and
/// returns it.
fn ensure_id(id: &mut Option<String>) -> String {
    let value = id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
    *id = Some(value.clone());
    value
}

/// Serializes a DVR DTO to its stored JSON.
fn to_json<T: Serialize>(value: &T) -> Result<String, ServiceError> {
    serde_json::to_string(value).map_err(|e| LiveTvError::serialize("serialize timer", e).into())
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

    use uuid::Uuid;

    use super::{DEFAULT_LIVE_STREAM_BUFFER_MS, GuideProgramRow, LiveTvPaths, live_tv_service_key};
    use ferrofin_model::live_tv::{
        BaseTimerInfoDto, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
    };

    use ferrofin_db::Database;
    use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
    use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
    use ferrofin_traits::stubs::LiveTvManager;

    use ferrofin_model::dto::SortOrder;
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

    const M3U: &str = "#EXTM3U\n\
        #EXTINF:-1 tvg-id=\"one.tv\" tvg-chno=\"1\",Channel One\nhttp://tuner/one\n\
        #EXTINF:-1 tvg-id=\"two.tv\" tvg-chno=\"2\",Channel Two\nhttp://tuner/two\n";
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
            .save_tuner_host(TunerHostInfo::default())
            .await
            .expect_err("no url");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::InvalidInput(_)
        ));
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
        sources.insert(
            "http://guide/xmltv.xml".to_owned(),
            CLASSIFIED_XMLTV.to_owned(),
        );
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
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
        sources.insert("http://guide/xmltv.xml".to_owned(), XMLTV.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;

        mgr.save_tuner_host(TunerHostInfo {
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

        let channels = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("chans");
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
        sources.insert("http://guide/ae.xml".to_owned(), XMLTV_AMP.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;

        mgr.save_tuner_host(TunerHostInfo {
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
            let day = 20 + p / 24 / 60 % 8;
            let hh = p / 60 % 24;
            let mm = p % 60;
            let _ = write!(
                xmltv,
                "<programme start=\"202607{day:02}{hh:02}{mm:02}00 +0000\" \
                 stop=\"202607{day:02}{hh:02}{mm:02}30 +0000\" channel=\"ch{ch}.tv\">\
                 <title>Show {p}</title></programme>"
            );
        }
        xmltv.push_str("</tv>");

        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), m3u);
        sources.insert("http://guide/xmltv.xml".to_owned(), xmltv);
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
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
            ("one.tv", -180, -120, "Aired", "News"),
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

    #[tokio::test]
    async fn series_timer_crud_and_cascade() {
        use ferrofin_model::live_tv::{BaseTimerInfoDto, SeriesTimerInfoDto, TimerInfoDto};
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let st = SeriesTimerInfoDto {
            base: BaseTimerInfoDto {
                channel_id: uuid::Uuid::new_v4(),
                name: Some("Every episode".to_owned()),
                ..BaseTimerInfoDto::default()
            },
            ..SeriesTimerInfoDto::default()
        };
        let st_id = mgr.create_series_timer(st).await.expect("create st");
        assert_eq!(mgr.get_series_timers().await.expect("list").len(), 1);

        // A timer that belongs to the series timer is removed when it's cancelled.
        let timer = TimerInfoDto {
            series_timer_id: Some(st_id.clone()),
            base: BaseTimerInfoDto {
                channel_id: uuid::Uuid::new_v4(),
                start_date: parse_dt("2026-07-25T06:00:00Z").unwrap(),
                end_date: parse_dt("2026-07-25T07:00:00Z").unwrap(),
                ..BaseTimerInfoDto::default()
            },
            ..TimerInfoDto::default()
        };
        mgr.create_timer(timer).await.expect("create timer");
        assert_eq!(mgr.get_timers().await.expect("t").len(), 1);

        mgr.cancel_series_timer(&st_id).await.expect("cancel st");
        assert!(mgr.get_series_timers().await.expect("l2").is_empty());
        assert!(
            mgr.get_timers().await.expect("t2").is_empty(),
            "cancelling a series timer drops its timers"
        );
    }

    // ---- Channel query/projection (plan A) --------------------------------

    /// The list path: `AddChannelInfo` fields land, the four detail fields are
    /// stripped (`RemoveFields`), and each channel carries its currently-airing
    /// programme from the one page-wide airing query.
    #[tokio::test]
    async fn channel_list_attaches_channel_info_and_current_program() {
        let mgr = manager_with_relative_guide().await;
        let channels = mgr
            .get_channels(&LiveTvChannelQuery::default(), &DtoOptions::default())
            .await
            .expect("channels");
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

        let channel = mgr
            .get_channel(id, None, &DtoOptions::default())
            .await
            .expect("get")
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
        sources.insert(
            "http://guide/xmltv.xml".to_owned(),
            CLASSIFIED_XMLTV.to_owned(),
        );
        let mgr = manager_with(FakeFetcher(sources)).await;
        mgr.save_tuner_host(TunerHostInfo {
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
        let mut cancelled = mgr
            .get_timer(&timer_id)
            .await
            .expect("get timer")
            .expect("timer");
        cancelled.status = RecordingStatus::Cancelled;
        mgr.update_timer(&timer_id, cancelled)
            .await
            .expect("update");
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
        sources.insert("http://guide/xmltv.xml".to_owned(), XMLTV.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;
        let tuner = mgr
            .save_tuner_host(TunerHostInfo {
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("tuner");
        let provider = mgr
            .save_listing_provider(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            })
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
        assert!(
            mgr.get_listing_providers()
                .await
                .expect("providers")
                .is_empty()
        );
        mgr.delete_tuner_host(&tuner_id.to_ascii_uppercase())
            .await
            .expect("delete tuner");
        assert!(mgr.get_tuner_hosts().await.expect("hosts").is_empty());
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
        assert!(
            mgr.get_listing_providers().await.expect("p")[0]
                .channel_mappings
                .is_empty()
        );
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
        assert!(
            mgr.get_listing_providers().await.expect("p")[0]
                .channel_mappings
                .is_empty()
        );
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
        mgr.save_tuner_host(TunerHostInfo {
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
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
        assert!(
            crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .is_empty()
        );
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
        sources.insert("http://guide/xmltv.xml".to_owned(), XMLTV.to_owned());
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
        let before = crate::guide_repository::channel_rows(&mgr.db)
            .await
            .expect("channels");
        assert_eq!(before.len(), 2);
        let kept_id = before[0].id.clone();
        assert!(
            !crate::guide_repository::all_program_ids(&mgr.db)
                .await
                .expect("ids")
                .is_empty()
        );

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
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/offline.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
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
}
