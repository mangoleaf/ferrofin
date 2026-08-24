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
use ferrofin_model::dto::SortOrder;
use ferrofin_model::dto::{BaseItemDto, MediaSourceInfo, MediaSourceType};
use ferrofin_model::live_tv::{
    ChannelType, ItemSortBy, ListingsProviderInfo, LiveTvInfo, LiveTvServiceInfo,
    LiveTvServiceStatus, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::{LiveStreamFile, LiveTvChannelQuery, LiveTvManager};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::m3u::parse_m3u;
use crate::projection::{
    ChannelRow, ProgramRow as GuideProgramRow, channel_entity, program_entity, remove_fields,
};
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
    /// Serializes "join or open", so two clients tuning the same channel at
    /// once cannot both miss the join and both dial the tuner.
    ///
    /// Port of `MediaSourceManager._liveStreamLocker` (an
    /// `AsyncNonKeyedLocker(1)` around the whole of `OpenLiveStreamInternal`);
    /// a `tokio::sync::Mutex` because this one *is* held across awaits.
    open_lock: Arc<tokio::sync::Mutex<()>>,
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
    /// Creates the manager over the given database and source fetcher.
    #[must_use]
    pub fn new(db: Database, fetcher: Arc<dyn SourceFetcher>, server_id: String) -> Self {
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
            open_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
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

    /// Rewrites the channel lineup for one tuner host from its M3U body, in a
    /// transaction (deleting the old channels cascades away their programmes).
    async fn replace_channels(&self, tuner_id: &str, m3u_body: &str) -> Result<(), ServiceError> {
        let channels = parse_m3u(m3u_body);
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
                let date_created = existing
                    .get(&id)
                    .and_then(Clone::clone)
                    .unwrap_or_else(|| now.clone());
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
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)
    }

    /// Inserts programmes from an XMLTV body, binding each to every channel whose
    /// `TvgId` matches the programme's `channel` attribute, classified against the
    /// listings provider's category lists as `XmlTvListingsProvider.GetProgramInfo`
    /// does.
    async fn insert_programs(
        &self,
        xmltv_body: &str,
        provider: &ListingsProviderInfo,
    ) -> Result<(), ServiceError> {
        let guide = parse_xmltv(xmltv_body);
        let classes = CategoryClasses::from_provider(provider);

        // Map each tvg-id to the channel UUIDs that carry it.
        let rows = sqlx::query(r#"SELECT "Id","TvgId" FROM "FerrofinLiveTvChannels""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let mut by_tvg: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let id: String = row.get("Id");
            let tvg: String = row.get("TvgId");
            by_tvg.entry(tvg).or_default().push(id);
        }

        // Flatten to one (channel, programme) row per binding, then insert in
        // chunked multi-row statements (25 columns per row) instead of one
        // round-trip per programme.
        let rows: Vec<_> = guide
            .programmes
            .iter()
            .flat_map(|prog| {
                let channel_ids = by_tvg.get(&prog.channel_id).map_or(&[][..], Vec::as_slice);
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
        let id = info
            .id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
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
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id","Url","Type","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Url"=excluded."Url","Type"=excluded."Type","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(&url)
        .bind(info.type_.as_deref().unwrap_or("m3u"))
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        self.tuner_flag.store(true, Ordering::Relaxed);
        Ok(info)
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
            .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
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

    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        // M3U tuners are stateless HTTP streams — there is nothing to reset.
        Ok(())
    }

    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        for tuner in self.get_tuner_hosts().await? {
            let (Some(id), Some(url)) = (tuner.id.as_deref(), tuner.url.as_deref()) else {
                continue;
            };
            match self.fetcher.fetch(url).await {
                Ok(body) => self.replace_channels(id, &body).await?,
                Err(e) => tracing::warn!(%url, error = %e, "live tv: tuner fetch failed"),
            }
        }
        for provider in self.get_listing_providers().await? {
            let Some(path) = provider.path.as_deref() else {
                continue;
            };
            match self.fetcher.fetch(path).await {
                Ok(body) => self.insert_programs(&body, &provider).await?,
                Err(e) => tracing::warn!(%path, error = %e, "live tv: guide fetch failed"),
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
        let id = ensure_id(&mut timer.base.id);
        let data = to_json(&timer)?;
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvTimers"
               ("Id","ChannelId","ProgramId","SeriesTimerId","Name","StartDate","EndDate","Status",
                "PrePaddingSeconds","PostPaddingSeconds","Data")
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
               ON CONFLICT("Id") DO UPDATE SET
                 "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
                 "SeriesTimerId"=excluded."SeriesTimerId","Name"=excluded."Name",
                 "StartDate"=excluded."StartDate","EndDate"=excluded."EndDate",
                 "Status"=excluded."Status","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(guid_to_db(timer.base.channel_id))
        .bind(&timer.base.program_id)
        .bind(&timer.series_timer_id)
        .bind(timer.base.name.clone().unwrap_or_default())
        .bind(datetime_to_db(timer.base.start_date))
        .bind(datetime_to_db(timer.base.end_date))
        .bind(recording_status_name(timer.status))
        .bind(timer.base.pre_padding_seconds)
        .bind(timer.base.post_padding_seconds)
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(id)
    }

    async fn update_timer(&self, id: &str, mut timer: TimerInfoDto) -> Result<(), ServiceError> {
        timer.base.id = Some(id.to_owned());
        self.create_timer(timer).await.map(|_| ())
    }

    async fn cancel_timer(&self, id: &str) -> Result<(), ServiceError> {
        self.delete_by_id(r#"DELETE FROM "FerrofinLiveTvTimers" WHERE "Id" = ?1"#, id)
            .await
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
        let rows = sqlx::query(
            r#"SELECT "Id","Name","Overview","StartDate","EndDate","Status","ChannelId"
               FROM "FerrofinLiveTvRecordings" ORDER BY "StartDate" DESC"#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let items: Vec<BaseItemDto> = rows.iter().map(|r| self.recording_dto(r)).collect();
        Ok(QueryResult::from_items(items))
    }

    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError> {
        let row = sqlx::query(
            r#"SELECT "Id","Name","Overview","StartDate","EndDate","Status","ChannelId"
               FROM "FerrofinLiveTvRecordings" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| self.recording_dto(&r)))
    }

    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let path: Option<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "FerrofinLiveTvRecordings" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?
                .flatten();
        // Only report a path that actually points at a captured file.
        Ok(path.filter(|p| !p.is_empty()))
    }

    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError> {
        // Remove the file first (best-effort), then the row.
        let path: Option<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "FerrofinLiveTvRecordings" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?
                .flatten();
        if let Some(path) = path {
            let _ = tokio::fs::remove_file(&path).await;
        }
        self.delete_by_id(
            r#"DELETE FROM "FerrofinLiveTvRecordings" WHERE "Id" = ?1"#,
            &guid_to_db(id),
        )
        .await
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

    /// Maps a recording row to a `BaseItemDto` (`Type = "Recording"`).
    fn recording_dto(&self, r: &sqlx::sqlite::SqliteRow) -> BaseItemDto {
        let id = Uuid::parse_str(&r.get::<String, _>("Id")).unwrap_or_default();
        BaseItemDto {
            id,
            server_id: Some(self.server_id.clone()),
            name: Some(r.get::<String, _>("Name")),
            type_: BaseItemKind::Recording,
            channel_id: Uuid::parse_str(&r.get::<String, _>("ChannelId")).ok(),
            media_type: MediaType::Video,
            overview: r.get::<Option<String>, _>("Overview"),
            start_date: parse_dt(r.get::<String, _>("StartDate").as_str()),
            end_date: r
                .get::<Option<String>, _>("EndDate")
                .as_deref()
                .and_then(parse_dt),
            status: r.get::<Option<String>, _>("Status"),
            is_folder: Some(false),
            ..BaseItemDto::default()
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

    use super::{DEFAULT_LIVE_STREAM_BUFFER_MS, LiveTvPaths, live_tv_service_key};

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

    async fn manager_with(fetcher: FakeFetcher) -> FerrofinLiveTvManager {
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        FerrofinLiveTvManager::new(db, std::sync::Arc::new(fetcher), "srv".to_owned())
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

        let mgr =
            FerrofinLiveTvManager::new(db, Arc::new(FakeFetcher(HashMap::new())), "srv".to_owned())
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
                    start_date: parse_dt("2026-07-25T06:00:00Z").unwrap(),
                    end_date: parse_dt("2026-07-25T07:00:00Z").unwrap(),
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
}
