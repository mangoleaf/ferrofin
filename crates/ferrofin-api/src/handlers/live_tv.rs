//! `LiveTvController` — Ferrofin's M3U-tuner + XMLTV-guide Live TV.
//!
//! The read/query surface (info, channels, programs, recommended) is served from
//! the real [`LiveTvManager`] when the composition root has wired it; until then
//! (or when nothing is configured) it returns the honest empty/disabled state so
//! the web UI still works. The configuration surface — add/delete a tuner host or
//! listing provider — and the single-resource `{id}` lookups are backed by the
//! manager too; adding a source triggers a guide refresh so channels/programmes
//! populate immediately.
//!
//! The DVR surface (recordings, timers, series timers) is backed by the manager
//! too: timers and series timers persist and list/get/update/cancel, and
//! recordings list/get/delete. The recording *capture* engine (a scheduler that
//! records a channel to disk when a timer fires) is a further increment.
//!
//! The three program queries (`GET`/`POST /LiveTv/Programs` and
//! `/LiveTv/Programs/Recommended`) bind the contract's whole filter set — channel
//! ids, the start/end date window, the airing/kind flags, genres, paging and
//! sort — and carry it into the `InternalItemsQuery` the manager seam takes; see
//! [`query_programs`]. Two divergences are deliberate and documented at their
//! handlers: `GET /LiveTv/Channels` cannot page (its seam method takes no
//! query), and `Programs/Recommended` does not apply upstream's per-user
//! recommendation *score* re-ordering (it needs channel user-data the seam does
//! not expose).

use axum::extract::{Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use uuid::Uuid;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{BaseItemDto, NameIdPair, NameValuePair, SortOrder};
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::{
    ChannelMappingOptionsDto, GuideInfo, ItemSortBy, ListingsProviderInfo, LiveTvInfo,
    SeriesTimerInfoDto, TimerInfoDto, TunerChannelMapping, TunerHostInfo,
};
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_csv_uuids, parse_pipe_strings};
use crate::handlers::streaming::serve_static_file;
use crate::state::AppState;

/// `GET /LiveTv/Info` — top-level Live TV status.
///
/// Port of `LiveTvController.GetLiveTvInfo`. Reports the configured services (a
/// single M3U/XMLTV service once a tuner host exists), or disabled when none.
async fn get_live_tv_info(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<LiveTvInfo>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(Json(m.get_live_tv_info().await?)),
        None => Ok(Json(LiveTvInfo::default())),
    }
}

/// Number of days of guide data the window spans, forward from now.
///
/// Mirrors Jellyfin's `LiveTvOptions.GuideDays` fallback (7). This is a
/// candidate configuration value (valid range 1..=14); it is hardcoded here
/// until Live TV options are surfaced in Ferrofin's config.
const GUIDE_DAYS_DEFAULT: i64 = 7;

/// `GET /LiveTv/GuideInfo` — the guide's date range.
///
/// Port of `LiveTvController.GetGuideInfo`. Returns a now-relative window
/// spanning [`GUIDE_DAYS_DEFAULT`] days forward from the current instant.
async fn get_guide_info(RequireAuth(_auth): RequireAuth) -> Json<GuideInfo> {
    let start = Utc::now();
    let end = start + chrono::Duration::days(GUIDE_DAYS_DEFAULT);
    Json(GuideInfo {
        start_date: start,
        end_date: end,
    })
}

/// `GET /LiveTv/Channels` — the user's Live TV channels.
///
/// Port of `LiveTvController.GetLiveTvChannels`, minus its filters: the
/// contract's `startIndex`/`limit`/`userId`/`is*` parameters are accepted and
/// ignored, because `LiveTvManager::get_channels` takes only a [`DtoOptions`] —
/// there is no query on that seam to carry them (unlike
/// `LiveTvManager::get_programs`, which takes an [`InternalItemsQuery`]).
/// Honouring them needs the trait signature widened in `ferrofin-traits` +
/// `ferrofin-livetv`; paging here instead would move filtering into the HTTP
/// layer and report a total record count the manager never computed. The whole
/// channel list is returned meanwhile.
async fn get_channels(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(Json(m.get_channels(&DtoOptions::default()).await?)),
        None => Ok(Json(QueryResult::default())),
    }
}

/// `GET /LiveTv/Channels/{channelId}` — a single channel.
///
/// Port of `LiveTvController.GetChannel`. `404` when the channel is unknown.
async fn get_channel(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let Some(m) = state.live_tv.as_ref() else {
        return Err(ApiError::NotFound("channel".into()));
    };
    m.get_channel(channel_id, &DtoOptions::default())
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("channel".into()))
}

/// The query parameters honoured by `GET /LiveTv/Programs`.
///
/// One field per `GetLiveTvPrograms` parameter in the vendored contract.
/// Multi-value parameters arrive as the raw delimited string — Jellyfin binds
/// them with the `CommaDelimited`/`PipeDelimited` model binders — and are split
/// in [`ProgramsQuery::into_parts`]. `#[serde(default)]` on the container makes
/// every parameter optional, as the contract marks none required.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[allow(clippy::struct_excessive_bools)] // one field per contract parameter
struct ProgramsQuery {
    /// Comma-delimited channel ids to return guide information for.
    channel_ids: Option<String>,
    /// The target user; defaults to the authenticated caller when absent.
    user_id: Option<Uuid>,
    /// The minimum programme start date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    min_start_date: Option<DateTime<Utc>>,
    /// Filter by programmes that have finished airing.
    has_aired: Option<bool>,
    /// Filter by programmes airing right now.
    is_airing: Option<bool>,
    /// The maximum programme start date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    max_start_date: Option<DateTime<Utc>>,
    /// The minimum programme end date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    min_end_date: Option<DateTime<Utc>>,
    /// The maximum programme end date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    max_end_date: Option<DateTime<Utc>>,
    /// Filter for movies.
    is_movie: Option<bool>,
    /// Filter for series.
    is_series: Option<bool>,
    /// Filter for news.
    is_news: Option<bool>,
    /// Filter for kids' programmes.
    is_kids: Option<bool>,
    /// Filter for sports.
    is_sports: Option<bool>,
    /// The index of the first record to return.
    start_index: Option<i32>,
    /// The maximum number of records to return.
    limit: Option<i32>,
    /// Comma-delimited sort columns.
    sort_by: Option<String>,
    /// Comma-delimited sort orders, paired positionally with `sort_by`.
    sort_order: Option<String>,
    /// Pipe-delimited genre names.
    genres: Option<String>,
    /// Comma-delimited genre ids.
    genre_ids: Option<String>,
    /// Whether image information is included.
    enable_images: Option<bool>,
    /// The maximum number of images returned per image type.
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    enable_image_types: Option<String>,
    /// Whether user data is included.
    enable_user_data: Option<bool>,
    /// Filter to the programmes a series timer records.
    series_timer_id: Option<String>,
    /// Filter to the programmes of one library series.
    library_series_id: Option<Uuid>,
    /// Comma-delimited additional DTO fields.
    fields: Option<String>,
    /// Whether the total record count is computed (contract default `true`).
    enable_total_record_count: Option<bool>,
}

impl ProgramsQuery {
    /// Splits the bound parameters into the query filters and the DTO options,
    /// parsing the delimited multi-value parameters. A malformed id is a `400`.
    fn into_parts(self) -> Result<(ProgramFilters, DtoOptions), ApiError> {
        let filters = ProgramFilters {
            channel_ids: parse_csv_uuids(self.channel_ids.as_deref())?,
            min_start_date: self.min_start_date,
            max_start_date: self.max_start_date,
            min_end_date: self.min_end_date,
            max_end_date: self.max_end_date,
            has_aired: self.has_aired,
            is_airing: self.is_airing,
            is_movie: self.is_movie,
            is_series: self.is_series,
            is_news: self.is_news,
            is_kids: self.is_kids,
            is_sports: self.is_sports,
            start_index: self.start_index,
            limit: self.limit,
            order_by: pair_order_by(
                parse_csv_enums_lenient(self.sort_by.as_deref()),
                &parse_csv_enums_lenient(self.sort_order.as_deref()),
            ),
            genres: parse_pipe_strings(self.genres.as_deref()),
            genre_ids: parse_csv_uuids(self.genre_ids.as_deref())?,
            series_timer_id: self.series_timer_id,
            library_series_id: self.library_series_id,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
        };
        let options = program_dto_options(
            parse_csv_enums_lenient(self.fields.as_deref()),
            self.enable_images,
            self.image_type_limit,
            parse_csv_enums_lenient(self.enable_image_types.as_deref()),
            self.enable_user_data,
        );
        Ok((filters, options))
    }
}

/// The `POST /LiveTv/Programs` request body — port of `GetProgramsDto`.
///
/// Every array member is nullable in the vendored schema and upstream coalesces
/// each to empty (`body.ChannelIds ?? []`), so they bind as `Option<Vec<_>>`
/// and collapse the same way.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // one field per contract property
struct GetProgramsDto {
    /// The channels to return guide information for.
    channel_ids: Option<Vec<Uuid>>,
    /// The target user. Unlike the query-string form this does *not* fall back
    /// to the authenticated caller (upstream reads the body id only).
    user_id: Option<Uuid>,
    /// The minimum programme start date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    min_start_date: Option<DateTime<Utc>>,
    /// Filter by programmes that have finished airing.
    has_aired: Option<bool>,
    /// Filter by programmes airing right now.
    is_airing: Option<bool>,
    /// The maximum programme start date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    max_start_date: Option<DateTime<Utc>>,
    /// The minimum programme end date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    min_end_date: Option<DateTime<Utc>>,
    /// The maximum programme end date.
    #[serde(deserialize_with = "deserialize_optional_date_time")]
    max_end_date: Option<DateTime<Utc>>,
    /// Filter for movies.
    is_movie: Option<bool>,
    /// Filter for series.
    is_series: Option<bool>,
    /// Filter for news.
    is_news: Option<bool>,
    /// Filter for kids' programmes.
    is_kids: Option<bool>,
    /// Filter for sports.
    is_sports: Option<bool>,
    /// The index of the first record to return.
    start_index: Option<i32>,
    /// The maximum number of records to return.
    limit: Option<i32>,
    /// The sort columns.
    sort_by: Option<Vec<ItemSortBy>>,
    /// The sort orders, paired positionally with `SortBy`.
    sort_order: Option<Vec<SortOrder>>,
    /// The genre names to return guide information for.
    genres: Option<Vec<String>>,
    /// The genre ids to return guide information for.
    genre_ids: Option<Vec<Uuid>>,
    /// Whether image information is included.
    enable_images: Option<bool>,
    /// The maximum number of images returned per image type.
    image_type_limit: Option<i32>,
    /// The image types to include.
    enable_image_types: Option<Vec<ImageType>>,
    /// Whether user data is included.
    enable_user_data: Option<bool>,
    /// Filter to the programmes a series timer records.
    series_timer_id: Option<String>,
    /// Filter to the programmes of one library series.
    library_series_id: Option<Uuid>,
    /// Additional DTO fields.
    fields: Option<Vec<ItemFields>>,
    /// Whether the total record count is computed (schema default `true`).
    enable_total_record_count: Option<bool>,
}

impl GetProgramsDto {
    /// Splits the body into the query filters and the DTO options. Nothing can
    /// fail here — JSON already carries the arrays typed.
    fn into_parts(self) -> (ProgramFilters, DtoOptions) {
        let filters = ProgramFilters {
            channel_ids: self.channel_ids.unwrap_or_default(),
            min_start_date: self.min_start_date,
            max_start_date: self.max_start_date,
            min_end_date: self.min_end_date,
            max_end_date: self.max_end_date,
            has_aired: self.has_aired,
            is_airing: self.is_airing,
            is_movie: self.is_movie,
            is_series: self.is_series,
            is_news: self.is_news,
            is_kids: self.is_kids,
            is_sports: self.is_sports,
            start_index: self.start_index,
            limit: self.limit,
            order_by: pair_order_by(
                self.sort_by.unwrap_or_default(),
                &self.sort_order.unwrap_or_default(),
            ),
            genres: self.genres.unwrap_or_default(),
            genre_ids: self.genre_ids.unwrap_or_default(),
            series_timer_id: self.series_timer_id,
            library_series_id: self.library_series_id,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
        };
        let options = program_dto_options(
            self.fields.unwrap_or_default(),
            self.enable_images,
            self.image_type_limit,
            self.enable_image_types.unwrap_or_default(),
            self.enable_user_data,
        );
        (filters, options)
    }
}

/// The query parameters honoured by `GET /LiveTv/Programs/Recommended`.
///
/// A strict subset of [`ProgramsQuery`]: `GetRecommendedPrograms` takes no
/// channel, date-window or series-timer filters — the "recommended" set is
/// scoped by the airing flags instead.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[allow(clippy::struct_excessive_bools)] // one field per contract parameter
struct RecommendedProgramsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    start_index: Option<i32>,
    /// The maximum number of records to return.
    limit: Option<i32>,
    /// Filter by programmes airing right now — the "On Now" row's filter.
    is_airing: Option<bool>,
    /// Filter by programmes that have finished airing.
    has_aired: Option<bool>,
    /// Filter for series.
    is_series: Option<bool>,
    /// Filter for movies.
    is_movie: Option<bool>,
    /// Filter for news.
    is_news: Option<bool>,
    /// Filter for kids' programmes.
    is_kids: Option<bool>,
    /// Filter for sports.
    is_sports: Option<bool>,
    /// Whether image information is included.
    enable_images: Option<bool>,
    /// The maximum number of images returned per image type.
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    enable_image_types: Option<String>,
    /// Comma-delimited genre ids.
    genre_ids: Option<String>,
    /// Comma-delimited additional DTO fields.
    fields: Option<String>,
    /// Whether user data is included.
    enable_user_data: Option<bool>,
    /// Whether the total record count is computed (contract default `true`).
    enable_total_record_count: Option<bool>,
}

impl RecommendedProgramsQuery {
    /// Splits the bound parameters into the query filters and the DTO options.
    /// A malformed genre id is a `400`.
    fn into_parts(self) -> Result<(ProgramFilters, DtoOptions), ApiError> {
        let filters = ProgramFilters {
            is_airing: self.is_airing,
            has_aired: self.has_aired,
            is_series: self.is_series,
            is_movie: self.is_movie,
            is_news: self.is_news,
            is_kids: self.is_kids,
            is_sports: self.is_sports,
            start_index: self.start_index,
            limit: self.limit,
            genre_ids: parse_csv_uuids(self.genre_ids.as_deref())?,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
            ..ProgramFilters::default()
        };
        let options = program_dto_options(
            parse_csv_enums_lenient(self.fields.as_deref()),
            self.enable_images,
            self.image_type_limit,
            parse_csv_enums_lenient(self.enable_image_types.as_deref()),
            self.enable_user_data,
        );
        Ok((filters, options))
    }
}

/// A program query in its parsed, delimiter-free form.
///
/// Both request shapes — the query string (`GET`) and the `GetProgramsDto` body
/// (`POST`) — normalize into this before [`query_programs`] builds the manager
/// query, which is what keeps the two forms from drifting apart (upstream
/// likewise assembles the identical `InternalItemsQuery` in `GetLiveTvPrograms`
/// and `GetPrograms`).
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // one field per contract filter
struct ProgramFilters {
    /// Restrict to these channels.
    channel_ids: Vec<Uuid>,
    /// The minimum programme start date.
    min_start_date: Option<DateTime<Utc>>,
    /// The maximum programme start date.
    max_start_date: Option<DateTime<Utc>>,
    /// The minimum programme end date.
    min_end_date: Option<DateTime<Utc>>,
    /// The maximum programme end date.
    max_end_date: Option<DateTime<Utc>>,
    /// Restrict to programmes that have finished airing.
    has_aired: Option<bool>,
    /// Restrict to programmes airing right now.
    is_airing: Option<bool>,
    /// Restrict to movies.
    is_movie: Option<bool>,
    /// Restrict to series.
    is_series: Option<bool>,
    /// Restrict to news.
    is_news: Option<bool>,
    /// Restrict to kids' programmes.
    is_kids: Option<bool>,
    /// Restrict to sports.
    is_sports: Option<bool>,
    /// The index of the first record to return.
    start_index: Option<i32>,
    /// The maximum number of records to return.
    limit: Option<i32>,
    /// The sort columns paired with their orders.
    order_by: Vec<(ItemSortBy, SortOrder)>,
    /// Restrict to these genre names.
    genres: Vec<String>,
    /// Restrict to these genre ids.
    genre_ids: Vec<Uuid>,
    /// Restrict to the programmes a series timer records.
    series_timer_id: Option<String>,
    /// Restrict to the programmes of one library series.
    library_series_id: Option<Uuid>,
    /// Whether the total record count is computed.
    enable_total_record_count: bool,
}

/// Pairs sort columns with sort orders.
///
/// Mirrors `RequestHelpers.GetOrderBy`: each column takes the order at its own
/// index, and columns beyond the supplied orders are padded with the **first**
/// requested order (ascending when none was supplied).
fn pair_order_by(columns: Vec<ItemSortBy>, orders: &[SortOrder]) -> Vec<(ItemSortBy, SortOrder)> {
    columns
        .into_iter()
        .enumerate()
        .map(|(i, column)| {
            let order = orders
                .get(i)
                .or_else(|| orders.first())
                .copied()
                .unwrap_or(SortOrder::Ascending);
            (column, order)
        })
        .collect()
}

/// Builds the [`DtoOptions`] for a program query.
///
/// Mirrors C# `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(...)`:
/// images default on, the per-type image limit falls back to Jellyfin's
/// unbounded default, and an explicit `enableImageTypes` narrows the type set
/// (leaving the set empty would suppress every `ImageTags` entry).
fn program_dto_options(
    fields: Vec<ItemFields>,
    enable_images: Option<bool>,
    image_type_limit: Option<i32>,
    enable_image_types: Vec<ImageType>,
    enable_user_data: Option<bool>,
) -> DtoOptions {
    let mut options = DtoOptions {
        fields,
        enable_images: enable_images.unwrap_or(true),
        image_type_limit: image_type_limit.unwrap_or(i32::MAX),
        enable_user_data: enable_user_data.unwrap_or(true),
        ..DtoOptions::default()
    };
    if !enable_image_types.is_empty() {
        options.image_types = enable_image_types;
    }
    options
}

/// Deserializes an optional `date-time` parameter.
///
/// RFC 3339 first (what every Jellyfin client sends), then the offset-less
/// forms ASP.NET's `DateTime` model binder also accepts, read as UTC. An
/// unparseable value fails the bind — a `400`, as it is upstream.
fn deserialize_optional_date_time<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let raw = Option::<String>::deserialize(deserializer)?;
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    parse_date_time(raw.trim())
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid date-time {raw:?}")))
}

/// Parses one `date-time` value: RFC 3339, else an offset-less date-time, else
/// a bare date — the latter two read as UTC.
fn parse_date_time(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .map(|date| date.and_time(chrono::NaiveTime::MIN).and_utc())
}

/// `GET /LiveTv/Programs` — EPG programs (query-string form).
///
/// Port of `LiveTvController.GetLiveTvPrograms`: every filter the contract
/// defines — channel ids, the start/end date window, the airing/kind flags,
/// genres, the series-timer and library-series scopes, paging and sort — is
/// bound and carried into the query. `userId` falls back to the authenticated
/// caller (C# `RequestHelpers.GetUserId`).
async fn get_programs(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ProgramsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let (filters, options) = query.into_parts()?;
    Ok(Json(query_programs(&state, user, filters, &options).await?))
}

/// `POST /LiveTv/Programs` — EPG programs (request-body form).
///
/// Port of `LiveTvController.GetPrograms`. The `GetProgramsDto` body carries the
/// same filter set as the query-string form and is honoured field for field;
/// the one deliberate difference is `UserId`, which upstream does **not** fall
/// back to the authenticated caller here.
async fn post_programs(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(body): Json<GetProgramsDto>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = match body.user_id.filter(|id| !id.is_nil()) {
        Some(id) => resolve_user_opt(&state, &auth, Some(id)).await?,
        None => None,
    };
    let (filters, options) = body.into_parts();
    Ok(Json(query_programs(&state, user, filters, &options).await?))
}

/// `GET /LiveTv/Programs/Recommended` — "On Now" / recommended programs.
///
/// Port of `LiveTvController.GetRecommendedPrograms`. Upstream's
/// `LiveTvManager.GetRecommendedProgramsAsync` is **not** merely a filtered
/// program list:
///
/// - when `isAiring` is anything but `true` it *is* `GetPrograms` — the same
///   query, so this handler takes the same path;
/// - when `isAiring` is `true` it additionally re-orders the airing programmes
///   by a per-user recommendation score (channel liked / favourited / play
///   count, plus live and non-repeat bonuses), over-fetching
///   `max(limit * 4, 200)` rows to rank within.
///
/// That score needs per-user *channel* user-data, which Ferrofin's
/// [`LiveTvManager`] seam does not expose (no user-data method, and its M3U
/// channels carry none), so the ranking step is an accepted divergence: airing
/// programmes come back in start-date order — Jellyfin's own primary sort key —
/// and the over-fetch is deliberately not done, since pulling four times the
/// requested rows only to return them unranked would break `limit`. The filters
/// themselves (`isAiring`, `hasAired`, the kind flags, `genreIds`, paging) are
/// honoured.
async fn get_recommended_programs(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<RecommendedProgramsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let (filters, options) = query.into_parts()?;
    Ok(Json(query_programs(&state, user, filters, &options).await?))
}

/// `GET /LiveTv/Programs/{programId}` — a single programme.
///
/// Port of `LiveTvController.GetProgram`. `404` when the programme is unknown.
async fn get_program(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(program_id): Path<Uuid>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let Some(m) = state.live_tv.as_ref() else {
        return Err(ApiError::NotFound("program".into()));
    };
    m.get_program(program_id, &DtoOptions::default())
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("program".into()))
}

/// Shared program query used by the GET/POST/Recommended program endpoints.
///
/// Carries the parsed [`ProgramFilters`] into the [`InternalItemsQuery`] the
/// Live TV manager seam takes. The start-date-ascending fallback mirrors
/// `LiveTvManager.GetPrograms` ("unless something else was specified, order by
/// start date to take advantage of a specialized index").
async fn query_programs(
    state: &AppState,
    user: Option<UserEntity>,
    filters: ProgramFilters,
    options: &DtoOptions,
) -> Result<QueryResult<BaseItemDto>, ApiError> {
    let Some(manager) = state.live_tv.as_ref() else {
        return Ok(QueryResult::default());
    };
    let library_series_id = filters.library_series_id;
    let mut query = InternalItemsQuery {
        user,
        channel_ids: filters.channel_ids,
        min_start_date: filters.min_start_date,
        max_start_date: filters.max_start_date,
        min_end_date: filters.min_end_date,
        max_end_date: filters.max_end_date,
        has_aired: filters.has_aired,
        is_airing: filters.is_airing,
        is_movie: filters.is_movie,
        is_series: filters.is_series,
        is_news: filters.is_news,
        is_kids: filters.is_kids,
        is_sports: filters.is_sports,
        start_index: filters.start_index,
        limit: filters.limit,
        order_by: filters.order_by,
        genres: filters.genres,
        genre_ids: filters.genre_ids,
        series_timer_id: filters.series_timer_id,
        enable_total_record_count: filters.enable_total_record_count,
        ..InternalItemsQuery::default()
    };
    if query.order_by.is_empty() {
        query.order_by = vec![(ItemSortBy::StartDate, SortOrder::Ascending)];
    }
    // `librarySeriesId` narrows the guide to the airings of one library series —
    // port of the controller's `GetItemById<Series>` block, which forces
    // `IsSeries` and matches on the series' name (an id that is not a series
    // leaves the name unset, exactly as the C# null-check does).
    if let Some(series_id) = library_series_id.filter(|id| !id.is_nil()) {
        query.is_series = Some(true);
        if let Some(series) = state.library.get_item_by_id(series_id).await?
            && series.type_.ends_with("Series")
        {
            query.name = series.name;
        }
    }
    Ok(manager.get_programs(&query, options).await?)
}

/// `POST /LiveTv/TunerHosts` — add (or update) an M3U tuner host.
///
/// Port of `LiveTvController.AddTunerHost`. Saves the host and refreshes the
/// guide so its channels populate immediately; returns the stored host.
async fn add_tuner_host(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(info): Json<TunerHostInfo>,
) -> Result<Json<TunerHostInfo>, ApiError> {
    let m = live_tv(&state)?;
    let saved = m.save_tuner_host(info).await?;
    m.refresh_guide().await?;
    Ok(Json(saved))
}

/// `DELETE /LiveTv/TunerHosts?id=` — remove a tuner host (and its channels).
///
/// Port of `LiveTvController.DeleteTunerHost`.
async fn delete_tuner_host(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(q): Query<IdQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.delete_tuner_host(&q.id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /LiveTv/ListingProviders` — add (or update) an XMLTV listing provider.
///
/// Port of `LiveTvController.AddListingProvider`. Saves the provider and
/// refreshes the guide; the `pw`/`validateListings`/`validateLogin` query flags
/// are not used by the XMLTV backend.
async fn add_listing_provider(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(info): Json<ListingsProviderInfo>,
) -> Result<Json<ListingsProviderInfo>, ApiError> {
    let m = live_tv(&state)?;
    let saved = m.save_listing_provider(info).await?;
    m.refresh_guide().await?;
    Ok(Json(saved))
}

/// `DELETE /LiveTv/ListingProviders?id=` — remove a listing provider.
///
/// Port of `LiveTvController.DeleteListingProvider`.
async fn delete_listing_provider(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(q): Query<IdQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.delete_listing_provider(&q.id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /LiveTv/Tuners/{tunerId}/Reset` — reset a tuner.
///
/// Port of `LiveTvController.ResetTuner`. M3U tuners are stateless, so this is a
/// successful no-op.
async fn reset_tuner(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(tuner_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.reset_tuner(&tuner_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Returns the wired Live TV manager, or `501` when Live TV is not configured in
/// this build (the composition root did not wire a manager).
fn live_tv(
    state: &AppState,
) -> Result<&std::sync::Arc<dyn ferrofin_traits::stubs::LiveTvManager>, ApiError> {
    state.live_tv.as_ref().ok_or(ApiError::NotImplemented)
}

/// The `?id=` query for the delete endpoints.
#[derive(Debug, Default, serde::Deserialize)]
struct IdQuery {
    /// The id of the tuner host / listing provider to delete.
    #[serde(default)]
    id: String,
}

/// `GET /LiveTv/Recordings` — DVR recordings.
async fn get_recordings(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(Json(m.get_recordings().await?)),
        None => Ok(Json(QueryResult::default())),
    }
}

/// `GET /LiveTv/Recordings/{recordingId}` — a single recording (`404` if absent).
async fn get_recording(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(recording_id): Path<Uuid>,
) -> Result<Json<BaseItemDto>, ApiError> {
    live_tv(&state)?
        .get_recording(recording_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("recording".into()))
}

/// `DELETE /LiveTv/Recordings/{recordingId}` — delete a recording + its file.
async fn delete_recording(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(recording_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.delete_recording(recording_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /LiveTv/Recordings/Folders` — recording folders (not modelled → empty).
async fn get_recording_folders(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Groups` — recording groups (deprecated; empty).
async fn get_recording_groups(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Groups/{groupId}` — a single recording group.
///
/// Port of `LiveTvController.GetRecordingGroup`: recording groups are an obsolete
/// concept (the list endpoint returns empty), so no group is ever resolvable and
/// this always reports `404` — the faithful outcome.
async fn get_recording_group(
    RequireAuth(_auth): RequireAuth,
    Path(_group_id): Path<Uuid>,
) -> Result<Json<BaseItemDto>, ApiError> {
    Err(ApiError::NotFound("recording group".into()))
}

/// `GET /LiveTv/ListingProviders/SchedulesDirect/Countries` — Schedules Direct
/// country list.
///
/// Port of `LiveTvController.GetSchedulesDirectCountries`: the raw JSON document
/// Schedules Direct serves at `available/countries` (no SD account involved),
/// passed through as `application/json` exactly as upstream's `File(stream, …)`
/// does. The manager serves it from Jellyfin's memory + on-disk
/// (`{cache}/sd-countries.json`, 7-day TTL) cache and fetches on a miss; an
/// upstream failure is a `500` (upstream `EnsureSuccessStatusCode` throws).
async fn get_schedules_direct_countries(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Response, ApiError> {
    let bytes = live_tv(&state)?.get_schedules_direct_countries().await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}

/// `GET /LiveTv/LiveRecordings/{recordingId}/stream` — stream a recording file.
///
/// Port of `LiveTvController.GetLiveRecordingFile`: resolves the recording's
/// captured file path and serves it (HTTP Range supported). `404` when the
/// recording is unknown or has no file on disk yet — the faithful result until
/// the capture engine (a later Live TV increment) writes recordings.
async fn get_live_recording_stream(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(recording_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let Ok(id) = Uuid::parse_str(&recording_id) else {
        return Err(ApiError::NotFound("recording".into()));
    };
    match live_tv(&state)?.get_recording_path(id).await? {
        Some(path) => serve_static_file(&path, request).await,
        None => Err(ApiError::NotFound("recording".into())),
    }
}

/// `GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}` — serve a buffered
/// live-stream file.
///
/// Port of `LiveTvController.GetLiveStreamFile`. Jellyfin serves the on-disk file
/// a tuner buffers a live stream into; Ferrofin direct-plays each M3U channel from
/// its source URL (see `LiveTvManager::get_channel_stream_url`) and buffers
/// nothing to disk, so there is no such file to serve and this reports `404` —
/// the faithful result for a stream id that has no buffered file.
async fn get_live_stream_file(
    RequireAuth(_auth): RequireAuth,
    Path((_stream_id, _container)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Err(ApiError::NotFound("live stream file".into()))
}

/// The `POST /LiveTv/ChannelMappings` request body.
///
/// Port of `SetChannelMappingDto`: a tuner channel mapped to a listings-provider
/// channel, for the given listings provider.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_field_names)] // mirrors the vendored `SetChannelMappingDto`
struct SetChannelMappingDto {
    /// The listings provider id the mapping belongs to.
    #[serde(default)]
    provider_id: String,
    /// The tuner channel id being mapped.
    #[serde(default)]
    tuner_channel_id: String,
    /// The provider channel id it maps to.
    #[serde(default)]
    provider_channel_id: String,
}

/// `POST /LiveTv/ChannelMappings` — map a tuner channel to a provider channel.
///
/// Port of `LiveTvController.SetChannelMapping` → `SetChannelMapping`: upserts the
/// `tunerChannelId -> providerChannelId` pair into the listings provider's
/// `ChannelMappings` and persists it (the guide match honors the mapping on the
/// next refresh). Returns the resulting [`TunerChannelMapping`]. `404` when the
/// provider id is unknown.
async fn set_channel_mapping(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(dto): Json<SetChannelMappingDto>,
) -> Result<Json<TunerChannelMapping>, ApiError> {
    let manager = live_tv(&state)?;
    let mut provider = manager
        .get_listing_providers()
        .await?
        .into_iter()
        .find(|p| p.id.as_deref() == Some(dto.provider_id.as_str()))
        .ok_or_else(|| ApiError::NotFound("listing provider".into()))?;

    // Upsert the mapping (keyed on the tuner channel id).
    if let Some(existing) = provider
        .channel_mappings
        .iter_mut()
        .find(|m| m.name.as_deref() == Some(dto.tuner_channel_id.as_str()))
    {
        existing.value = Some(dto.provider_channel_id.clone());
    } else {
        provider.channel_mappings.push(NameValuePair {
            name: Some(dto.tuner_channel_id.clone()),
            value: Some(dto.provider_channel_id.clone()),
        });
    }
    manager.save_listing_provider(provider).await?;

    Ok(Json(TunerChannelMapping {
        name: Some(dto.tuner_channel_id.clone()),
        id: Some(dto.tuner_channel_id),
        provider_channel_id: Some(dto.provider_channel_id),
        provider_channel_name: None,
    }))
}

/// `GET /LiveTv/Recordings/Series` — series recordings (deprecated; empty).
async fn get_recordings_series(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Timers` — pending recording timers.
async fn get_timers(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<TimerInfoDto>>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(Json(QueryResult::from_items(m.get_timers().await?))),
        None => Ok(Json(QueryResult::default())),
    }
}

/// `GET /LiveTv/Timers/{timerId}` — a single timer (`404` if absent).
async fn get_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
) -> Result<Json<TimerInfoDto>, ApiError> {
    live_tv(&state)?
        .get_timer(&timer_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("timer".into()))
}

/// `POST /LiveTv/Timers` — create a recording timer.
async fn create_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(timer): Json<TimerInfoDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    let program_id = timer.base.program_id.clone();
    let id = live_tv(&state)?.create_timer(timer).await?;
    notify_timer_event(
        &state,
        ferrofin_model::session::SessionMessageType::TimerCreated,
        &id,
        program_id.as_deref(),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Pushes a timer lifecycle message (`TimerCreated` / `TimerCancelled` /
/// `SeriesTimerCreated` / `SeriesTimerCancelled`) to every signed-in session,
/// carrying the C# `TimerEventInfo` shape (`Id` + optional `ProgramId`) — the
/// push Jellyfin's `RecordingNotifier` sends so clients refresh their
/// recording views. Best-effort: delivery must not fail the request.
async fn notify_timer_event(
    state: &AppState,
    message_type: ferrofin_model::session::SessionMessageType,
    timer_id: &str,
    program_id: Option<&str>,
) {
    let mut data = serde_json::json!({ "Id": timer_id });
    if let Some(program_id) = program_id {
        data["ProgramId"] = serde_json::Value::String(program_id.to_owned());
    }
    let _ = state
        .sessions
        .send_message_to_all_sessions(message_type, &data.to_string())
        .await;
}

/// `POST /LiveTv/Timers/{timerId}` — update a recording timer.
async fn update_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
    Json(timer): Json<TimerInfoDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.update_timer(&timer_id, timer).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `DELETE /LiveTv/Timers/{timerId}` — cancel a recording timer.
async fn cancel_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.cancel_timer(&timer_id).await?;
    notify_timer_event(
        &state,
        ferrofin_model::session::SessionMessageType::TimerCancelled,
        &timer_id,
        None,
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /LiveTv/Timers/Defaults` — default values for a new timer.
async fn get_default_timer(RequireAuth(_auth): RequireAuth) -> Json<SeriesTimerInfoDto> {
    Json(SeriesTimerInfoDto::default())
}

/// `GET /LiveTv/SeriesTimers` — recurring (series) timers.
async fn get_series_timers(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<SeriesTimerInfoDto>>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(Json(QueryResult::from_items(m.get_series_timers().await?))),
        None => Ok(Json(QueryResult::default())),
    }
}

/// `GET /LiveTv/SeriesTimers/{timerId}` — a single series timer (`404` if absent).
async fn get_series_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
) -> Result<Json<SeriesTimerInfoDto>, ApiError> {
    live_tv(&state)?
        .get_series_timer(&timer_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("series timer".into()))
}

/// `POST /LiveTv/SeriesTimers` — create a series timer.
async fn create_series_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(timer): Json<SeriesTimerInfoDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    let program_id = timer.base.program_id.clone();
    let id = live_tv(&state)?.create_series_timer(timer).await?;
    notify_timer_event(
        &state,
        ferrofin_model::session::SessionMessageType::SeriesTimerCreated,
        &id,
        program_id.as_deref(),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /LiveTv/SeriesTimers/{timerId}` — update a series timer.
async fn update_series_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
    Json(timer): Json<SeriesTimerInfoDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?
        .update_series_timer(&timer_id, timer)
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `DELETE /LiveTv/SeriesTimers/{timerId}` — cancel a series timer.
async fn cancel_series_timer(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(timer_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.cancel_series_timer(&timer_id).await?;
    notify_timer_event(
        &state,
        ferrofin_model::session::SessionMessageType::SeriesTimerCancelled,
        &timer_id,
        None,
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /LiveTv/ChannelMappingOptions` — channel-mapping options.
async fn get_channel_mapping_options(
    RequireAdmin(_auth): RequireAdmin,
) -> Json<ChannelMappingOptionsDto> {
    Json(ChannelMappingOptionsDto::default())
}

/// `GET /LiveTv/ListingProviders/Default` — default listing-provider config.
async fn get_default_listing_provider(
    RequireAuth(_auth): RequireAuth,
) -> Json<ListingsProviderInfo> {
    Json(ListingsProviderInfo::default())
}

/// `GET /LiveTv/ListingProviders/Lineups` — available lineups (none → empty).
async fn get_lineups(RequireAuth(_auth): RequireAuth) -> Json<Vec<NameIdPair>> {
    Json(Vec::new())
}

/// `GET /LiveTv/TunerHosts/Types` — supported tuner-host types.
///
/// Port of `LiveTvController.GetTunerHostTypes`. Ferrofin ships the M3U backend.
async fn get_tuner_host_types(RequireAuth(_auth): RequireAuth) -> Json<Vec<NameIdPair>> {
    Json(vec![NameIdPair {
        name: Some("M3U Tuner".to_owned()),
        id: Some("m3u".to_owned()),
    }])
}

/// `GET /LiveTv/Tuners/Discover` — auto-discovered tuner devices (none → empty).
async fn discover_tuners(RequireAdmin(_auth): RequireAdmin) -> Json<Vec<TunerHostInfo>> {
    Json(Vec::new())
}

/// Registers the Live TV surface onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/LiveTv/Info", get(get_live_tv_info))
        .route("/LiveTv/GuideInfo", get(get_guide_info))
        .route("/LiveTv/Channels", get(get_channels))
        .route("/LiveTv/Channels/{channelId}", get(get_channel))
        .route("/LiveTv/Programs", get(get_programs).post(post_programs))
        .route("/LiveTv/Programs/{programId}", get(get_program))
        .route(
            "/LiveTv/Programs/Recommended",
            get(get_recommended_programs),
        )
        .route("/LiveTv/Recordings", get(get_recordings))
        .route(
            "/LiveTv/Recordings/{recordingId}",
            get(get_recording).delete(delete_recording),
        )
        .route("/LiveTv/Recordings/Folders", get(get_recording_folders))
        .route("/LiveTv/Recordings/Groups", get(get_recording_groups))
        .route(
            "/LiveTv/Recordings/Groups/{groupId}",
            get(get_recording_group),
        )
        .route("/LiveTv/Recordings/Series", get(get_recordings_series))
        .route(
            "/LiveTv/LiveRecordings/{recordingId}/stream",
            get(get_live_recording_stream),
        )
        .route(
            "/LiveTv/LiveStreamFiles/{streamId}/{container}",
            get(get_live_stream_file),
        )
        .route("/LiveTv/Timers", get(get_timers).post(create_timer))
        .route(
            "/LiveTv/Timers/{timerId}",
            get(get_timer).post(update_timer).delete(cancel_timer),
        )
        .route("/LiveTv/Timers/Defaults", get(get_default_timer))
        .route(
            "/LiveTv/SeriesTimers",
            get(get_series_timers).post(create_series_timer),
        )
        .route(
            "/LiveTv/SeriesTimers/{timerId}",
            get(get_series_timer)
                .post(update_series_timer)
                .delete(cancel_series_timer),
        )
        .route(
            "/LiveTv/ChannelMappingOptions",
            get(get_channel_mapping_options),
        )
        .route(
            "/LiveTv/ListingProviders",
            post(add_listing_provider).delete(delete_listing_provider),
        )
        .route(
            "/LiveTv/ListingProviders/Default",
            get(get_default_listing_provider),
        )
        .route(
            "/LiveTv/ListingProviders/SchedulesDirect/Countries",
            get(get_schedules_direct_countries),
        )
        .route("/LiveTv/ListingProviders/Lineups", get(get_lineups))
        .route("/LiveTv/ChannelMappings", post(set_channel_mapping))
        .route(
            "/LiveTv/TunerHosts",
            post(add_tuner_host).delete(delete_tuner_host),
        )
        .route("/LiveTv/TunerHosts/Types", get(get_tuner_host_types))
        .route("/LiveTv/Tuners/Discover", get(discover_tuners))
        .route("/LiveTv/Tuners/Discvover", get(discover_tuners))
        .route("/LiveTv/Tuners/{tunerId}/Reset", post(reset_tuner))
}

#[cfg(test)]
mod tests {
    use ferrofin_traits::options::AuthorizationInfo;

    use super::*;
    use crate::test_support::fake_state;

    /// A `RequireAuth` for an authenticated (default) caller.
    fn auth() -> RequireAuth {
        RequireAuth(AuthorizationInfo::default())
    }

    /// The elevated caller for the tuner/listing-provider routes, which are
    /// `RequiresElevation` upstream. Constructing the extractor directly
    /// bypasses the policy check by design — these tests exercise the handler
    /// body, and the gate itself is pinned end to end in
    /// `apps/ferrofin-server/tests/elevation.rs`.
    fn admin_auth() -> RequireAdmin {
        RequireAdmin(AuthorizationInfo::default())
    }

    #[tokio::test]
    async fn info_is_disabled_when_no_manager() {
        let info = get_live_tv_info(State(fake_state()), auth())
            .await
            .unwrap()
            .0;
        assert!(!info.is_enabled);
        assert!(info.services.is_empty());
    }

    #[tokio::test]
    async fn query_ops_empty_when_no_manager() {
        let state = fake_state();
        assert_eq!(
            get_channels(State(state.clone()), auth())
                .await
                .unwrap()
                .0
                .total_record_count,
            0
        );
        assert_eq!(
            get_programs(
                State(state.clone()),
                auth(),
                Query(ProgramsQuery::default())
            )
            .await
            .unwrap()
            .0
            .total_record_count,
            0
        );
        assert_eq!(
            post_programs(
                State(state.clone()),
                auth(),
                Json(GetProgramsDto::default())
            )
            .await
            .unwrap()
            .0
            .total_record_count,
            0
        );
        assert_eq!(
            get_recommended_programs(
                State(state),
                auth(),
                Query(RecommendedProgramsQuery::default())
            )
            .await
            .unwrap()
            .0
            .total_record_count,
            0
        );
    }

    #[tokio::test]
    async fn mutations_501_when_no_manager() {
        let state = fake_state();
        let err = add_tuner_host(
            State(state.clone()),
            admin_auth(),
            Json(TunerHostInfo::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
        let err = get_channel(State(state), auth(), Path(Uuid::nil()))
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn defaults_and_lists() {
        let _ = get_guide_info(auth()).await;
        let _ = get_default_timer(auth()).await;
        let _ = get_channel_mapping_options(admin_auth()).await;
        let _ = get_default_listing_provider(auth()).await;
        assert!(get_lineups(auth()).await.0.is_empty());
        assert_eq!(get_tuner_host_types(auth()).await.0.len(), 1);
        assert!(discover_tuners(admin_auth()).await.0.is_empty());
        let state = fake_state();
        assert!(
            get_recordings(State(state.clone()), auth())
                .await
                .unwrap()
                .0
                .items
                .is_empty()
        );
        assert!(get_recording_folders(auth()).await.0.items.is_empty());
        assert!(get_recording_groups(auth()).await.0.items.is_empty());
        assert!(get_recordings_series(auth()).await.0.items.is_empty());
        assert_eq!(
            get_timers(State(state.clone()), auth())
                .await
                .unwrap()
                .0
                .total_record_count,
            0
        );
        assert_eq!(
            get_series_timers(State(state), auth())
                .await
                .unwrap()
                .0
                .total_record_count,
            0
        );
    }

    // ---- the 5 previously-stubbed routes ---------------------------------

    #[tokio::test]
    async fn guide_info_is_now_relative_seven_day_window() {
        let before = chrono::Utc::now();
        let info = get_guide_info(auth()).await.0;
        let after = chrono::Utc::now();
        // Start is "now" (bracketed by the call), not the epoch/default.
        assert!(info.start_date >= before && info.start_date <= after);
        // End is exactly GUIDE_DAYS_DEFAULT days past start.
        assert_eq!(
            info.end_date - info.start_date,
            chrono::Duration::days(GUIDE_DAYS_DEFAULT)
        );
    }

    #[tokio::test]
    async fn default_listing_provider_seeds_categories() {
        let p = get_default_listing_provider(auth()).await.0;
        assert!(p.enable_all_tuners);
        assert_eq!(p.movie_categories.unwrap(), ["movie"]);
        assert!(p.news_categories.is_some());
    }

    #[tokio::test]
    async fn schedules_direct_countries_passes_the_manager_bytes_through_as_json() {
        let doc = br#"{"North America":[{"fullName":"United States","shortName":"USA"}]}"#;
        let state = fake_state().with_live_tv(std::sync::Arc::new(FakeLiveTv {
            countries: Some(doc.to_vec()),
            ..FakeLiveTv::default()
        }));
        let resp = get_schedules_direct_countries(State(state), admin_auth())
            .await
            .expect("countries");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            &body[..],
            doc,
            "the SD document is passed through untouched"
        );
    }

    #[tokio::test]
    async fn schedules_direct_countries_maps_an_upstream_failure_to_500() {
        let state = fake_state().with_live_tv(std::sync::Arc::new(FakeLiveTv::default()));
        let err = get_schedules_direct_countries(State(state), admin_auth())
            .await
            .expect_err("upstream failure");
        assert_eq!(err.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn schedules_direct_countries_501_without_manager() {
        let err = get_schedules_direct_countries(State(fake_state()), admin_auth())
            .await
            .expect_err("no manager wired");
        assert_eq!(err.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn recording_group_and_live_stream_file_are_404() {
        let group = get_recording_group(auth(), Path(Uuid::nil()))
            .await
            .unwrap_err();
        assert_eq!(group.status(), axum::http::StatusCode::NOT_FOUND);
        let file = get_live_stream_file(auth(), Path(("s1".into(), "mp4".into())))
            .await
            .unwrap_err();
        assert_eq!(file.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manager_backed_ops_501_without_manager() {
        let state = fake_state();
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        let rec = get_live_recording_stream(
            State(state.clone()),
            auth(),
            Path(Uuid::nil().to_string()),
            req,
        )
        .await
        .unwrap_err();
        assert_eq!(rec.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
        let map = set_channel_mapping(
            State(state),
            admin_auth(),
            Json(SetChannelMappingDto {
                provider_id: "p".into(),
                tuner_channel_id: "t".into(),
                provider_channel_id: "c".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(map.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
    }

    /// A minimal [`LiveTvManager`] backing only the methods the handlers under
    /// test touch; everything else is unreachable in these tests.
    ///
    /// `programs_query`/`programs_options` record what the program handlers
    /// handed the seam — that recording is the assertion target, so the filters
    /// are proven to leave the handler rather than being inferred from a body
    /// the fake itself made up.
    #[derive(Default)]
    struct FakeLiveTv {
        providers: std::sync::Mutex<Vec<ListingsProviderInfo>>,
        recording_path: Option<String>,
        programs_query: std::sync::Mutex<Option<InternalItemsQuery>>,
        programs_options: std::sync::Mutex<Option<DtoOptions>>,
        /// The Schedules Direct country document; `None` models an upstream
        /// fetch failure.
        countries: Option<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl ferrofin_traits::stubs::LiveTvManager for FakeLiveTv {
        async fn get_listing_providers(
            &self,
        ) -> Result<Vec<ListingsProviderInfo>, ferrofin_traits::error::ServiceError> {
            Ok(self.providers.lock().unwrap().clone())
        }
        async fn save_listing_provider(
            &self,
            info: ListingsProviderInfo,
        ) -> Result<ListingsProviderInfo, ferrofin_traits::error::ServiceError> {
            let mut g = self.providers.lock().unwrap();
            *g = vec![info.clone()];
            Ok(info)
        }
        async fn get_recording_path(
            &self,
            _id: Uuid,
        ) -> Result<Option<String>, ferrofin_traits::error::ServiceError> {
            Ok(self.recording_path.clone())
        }
        async fn get_schedules_direct_countries(
            &self,
        ) -> Result<Vec<u8>, ferrofin_traits::error::ServiceError> {
            self.countries.clone().ok_or_else(|| {
                ferrofin_traits::error::ServiceError::backend("schedulesdirect.org: 503")
            })
        }
        async fn get_live_tv_info(
            &self,
        ) -> Result<LiveTvInfo, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_tuner_hosts(
            &self,
        ) -> Result<Vec<TunerHostInfo>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn save_tuner_host(
            &self,
            _info: TunerHostInfo,
        ) -> Result<TunerHostInfo, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_tuner_host(
            &self,
            _id: &str,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_listing_provider(
            &self,
            _id: &str,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channels(
            &self,
            _options: &DtoOptions,
        ) -> Result<QueryResult<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channel(
            &self,
            _id: Uuid,
            _options: &DtoOptions,
        ) -> Result<Option<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_programs(
            &self,
            query: &InternalItemsQuery,
            options: &DtoOptions,
        ) -> Result<QueryResult<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            *self.programs_query.lock().unwrap() = Some(query.clone());
            *self.programs_options.lock().unwrap() = Some(options.clone());
            Ok(QueryResult::from_items(vec![BaseItemDto::default()]))
        }
        async fn get_program(
            &self,
            _id: Uuid,
            _options: &DtoOptions,
        ) -> Result<Option<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn reset_tuner(&self, _id: &str) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn refresh_guide(&self) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channel_stream_url(
            &self,
            _id: Uuid,
        ) -> Result<Option<String>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_timers(
            &self,
        ) -> Result<Vec<TimerInfoDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_timer(
            &self,
            _id: &str,
        ) -> Result<Option<TimerInfoDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn create_timer(
            &self,
            _timer: TimerInfoDto,
        ) -> Result<String, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn update_timer(
            &self,
            _id: &str,
            _timer: TimerInfoDto,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn cancel_timer(
            &self,
            _id: &str,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_series_timers(
            &self,
        ) -> Result<Vec<SeriesTimerInfoDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_series_timer(
            &self,
            _id: &str,
        ) -> Result<Option<SeriesTimerInfoDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn create_series_timer(
            &self,
            _timer: SeriesTimerInfoDto,
        ) -> Result<String, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn update_series_timer(
            &self,
            _id: &str,
            _timer: SeriesTimerInfoDto,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn cancel_series_timer(
            &self,
            _id: &str,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_recordings(
            &self,
        ) -> Result<QueryResult<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_recording(
            &self,
            _id: Uuid,
        ) -> Result<Option<BaseItemDto>, ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_recording(
            &self,
            _id: Uuid,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn set_channel_mapping_upserts_and_returns() {
        let provider = ListingsProviderInfo {
            id: Some("prov1".into()),
            ..ListingsProviderInfo::default()
        };
        let fake = std::sync::Arc::new(FakeLiveTv {
            providers: std::sync::Mutex::new(vec![provider]),
            recording_path: None,
            ..FakeLiveTv::default()
        });
        let state = fake_state().with_live_tv(fake.clone());
        let mapping = set_channel_mapping(
            State(state),
            admin_auth(),
            Json(SetChannelMappingDto {
                provider_id: "prov1".into(),
                tuner_channel_id: "10".into(),
                provider_channel_id: "HBO".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(mapping.id.as_deref(), Some("10"));
        assert_eq!(mapping.provider_channel_id.as_deref(), Some("HBO"));
        // The mapping was persisted onto the provider.
        let saved = fake.providers.lock().unwrap()[0].channel_mappings.clone();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name.as_deref(), Some("10"));
        assert_eq!(saved[0].value.as_deref(), Some("HBO"));
    }

    #[tokio::test]
    async fn set_channel_mapping_unknown_provider_is_404() {
        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake);
        let err = set_channel_mapping(
            State(state),
            admin_auth(),
            Json(SetChannelMappingDto {
                provider_id: "missing".into(),
                tuner_channel_id: "1".into(),
                provider_channel_id: "2".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // ---- program queries carry the contract's filters --------------------

    /// Binds `uri`'s query string exactly as axum would, runs
    /// `GET /LiveTv/Programs`, and returns the query the handler handed the
    /// manager.
    async fn recorded_programs_query(uri: &str) -> InternalItemsQuery {
        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake.clone());
        let uri: axum::http::Uri = uri.parse().expect("uri");
        let query = Query::<ProgramsQuery>::try_from_uri(&uri).expect("query binds");
        let _ = get_programs(State(state), auth(), query).await.expect("ok");
        let recorded = fake.programs_query.lock().unwrap().clone();
        recorded.expect("the manager was called")
    }

    /// The same, for `GET /LiveTv/Programs/Recommended`.
    async fn recorded_recommended_query(uri: &str) -> InternalItemsQuery {
        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake.clone());
        let uri: axum::http::Uri = uri.parse().expect("uri");
        let query = Query::<RecommendedProgramsQuery>::try_from_uri(&uri).expect("query binds");
        let _ = get_recommended_programs(State(state), auth(), query)
            .await
            .expect("ok");
        let recorded = fake.programs_query.lock().unwrap().clone();
        recorded.expect("the manager was called")
    }

    #[tokio::test]
    async fn programs_query_carries_every_contract_filter() {
        let channel = Uuid::from_u128(11);
        let genre = Uuid::from_u128(22);
        // Every tri-state flag gets a *different* value so a cross-wiring cannot
        // round-trip clean, and each of the four dates is a distinct instant.
        let uri = format!(
            "/LiveTv/Programs?channelIds={channel}\
             &minStartDate=2026-08-19T10:00:00Z&maxStartDate=2026-08-19T11:00:00Z\
             &minEndDate=2026-08-19T12:00:00Z&maxEndDate=2026-08-19T13:00:00.500Z\
             &isAiring=true&hasAired=false&isMovie=true&isSeries=false&isNews=true\
             &isKids=false&isSports=true&startIndex=5&limit=2\
             &sortBy=StartDate,Name&sortOrder=Descending&genres=News%7CSport\
             &genreIds={genre}&seriesTimerId=st-1&enableTotalRecordCount=false"
        );
        let query = recorded_programs_query(&uri).await;

        assert_eq!(query.channel_ids, vec![channel]);
        assert_eq!(
            query.min_start_date,
            Some("2026-08-19T10:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(
            query.max_start_date,
            Some("2026-08-19T11:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(
            query.min_end_date,
            Some("2026-08-19T12:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(
            query.max_end_date,
            Some("2026-08-19T13:00:00.5Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(query.is_airing, Some(true));
        assert_eq!(query.has_aired, Some(false));
        assert_eq!(query.is_movie, Some(true));
        assert_eq!(query.is_series, Some(false));
        assert_eq!(query.is_news, Some(true));
        assert_eq!(query.is_kids, Some(false));
        assert_eq!(query.is_sports, Some(true));
        assert_eq!(query.start_index, Some(5));
        assert_eq!(query.limit, Some(2));
        assert_eq!(query.genres, vec!["News".to_owned(), "Sport".to_owned()]);
        assert_eq!(query.genre_ids, vec![genre]);
        assert_eq!(query.series_timer_id.as_deref(), Some("st-1"));
        assert!(!query.enable_total_record_count);
        // `RequestHelpers.GetOrderBy` pads the second column with the first
        // requested order.
        assert_eq!(
            query.order_by,
            vec![
                (ItemSortBy::StartDate, SortOrder::Descending),
                (ItemSortBy::Name, SortOrder::Descending),
            ]
        );
    }

    #[tokio::test]
    async fn programs_query_defaults_to_unfiltered_start_date_order() {
        let query = recorded_programs_query("/LiveTv/Programs").await;
        assert!(query.channel_ids.is_empty());
        assert!(query.min_start_date.is_none());
        assert!(query.limit.is_none());
        // The manager's "unless something else was specified" fallback.
        assert_eq!(
            query.order_by,
            vec![(ItemSortBy::StartDate, SortOrder::Ascending)]
        );
        // The contract's default for this one is `true`.
        assert!(query.enable_total_record_count);
    }

    #[tokio::test]
    async fn programs_query_rejects_a_malformed_date_and_id() {
        let bad_date: axum::http::Uri = "/LiveTv/Programs?minStartDate=whenever".parse().unwrap();
        assert!(Query::<ProgramsQuery>::try_from_uri(&bad_date).is_err());
        // A bare date and an offset-less date-time both bind (ASP.NET's
        // `DateTime` binder accepts them), read as UTC.
        let lenient: axum::http::Uri =
            "/LiveTv/Programs?minStartDate=2026-08-19&maxEndDate=2026-08-19T13:00:00"
                .parse()
                .unwrap();
        let bound = Query::<ProgramsQuery>::try_from_uri(&lenient).expect("binds");
        assert_eq!(
            bound.0.min_start_date,
            Some("2026-08-19T00:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(
            bound.0.max_end_date,
            Some("2026-08-19T13:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        // A malformed channel id is a 400 from the handler, not a silent drop.
        let bad_id: axum::http::Uri = "/LiveTv/Programs?channelIds=nope".parse().unwrap();
        let query = Query::<ProgramsQuery>::try_from_uri(&bad_id).expect("binds");
        let state = fake_state().with_live_tv(std::sync::Arc::new(FakeLiveTv::default()));
        let err = get_programs(State(state), auth(), query).await.unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_programs_body_filters_are_honoured() {
        let channel = Uuid::from_u128(33);
        let genre = Uuid::from_u128(44);
        let body: GetProgramsDto = serde_json::from_value(serde_json::json!({
            "ChannelIds": [channel],
            "MinStartDate": "2026-08-19T10:00:00.000Z",
            "MaxEndDate": "2026-08-19T12:00:00.000Z",
            "IsAiring": true,
            "HasAired": false,
            "StartIndex": 7,
            "Limit": 3,
            "Genres": ["Drama"],
            "GenreIds": [genre],
            "SortBy": ["Name"],
            "SortOrder": ["Descending"],
            "SeriesTimerId": "st-2",
            "EnableTotalRecordCount": false,
            // Nullable arrays must collapse to empty, not fail the bind.
            "Fields": null,
            "EnableImageTypes": null
        }))
        .expect("body binds");

        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake.clone());
        let _ = post_programs(State(state), auth(), Json(body))
            .await
            .expect("ok");
        let query = fake.programs_query.lock().unwrap().clone().expect("called");

        assert_eq!(query.channel_ids, vec![channel]);
        assert_eq!(
            query.min_start_date,
            Some("2026-08-19T10:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(
            query.max_end_date,
            Some("2026-08-19T12:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(query.is_airing, Some(true));
        assert_eq!(query.has_aired, Some(false));
        assert_eq!(query.start_index, Some(7));
        assert_eq!(query.limit, Some(3));
        assert_eq!(query.genres, vec!["Drama".to_owned()]);
        assert_eq!(query.genre_ids, vec![genre]);
        assert_eq!(query.series_timer_id.as_deref(), Some("st-2"));
        assert!(!query.enable_total_record_count);
        assert_eq!(
            query.order_by,
            vec![(ItemSortBy::Name, SortOrder::Descending)]
        );
    }

    #[tokio::test]
    async fn recommended_programs_is_not_the_unfiltered_program_list() {
        let genre = Uuid::from_u128(55);
        let uri = format!(
            "/LiveTv/Programs/Recommended?isAiring=true&hasAired=false&limit=4&startIndex=1\
             &isMovie=false&isSeries=true&isNews=false&isKids=true&isSports=false\
             &genreIds={genre}&enableTotalRecordCount=false"
        );
        let query = recorded_recommended_query(&uri).await;

        assert_eq!(query.is_airing, Some(true));
        assert_eq!(query.has_aired, Some(false));
        assert_eq!(query.limit, Some(4));
        assert_eq!(query.start_index, Some(1));
        assert_eq!(query.is_movie, Some(false));
        assert_eq!(query.is_series, Some(true));
        assert_eq!(query.is_news, Some(false));
        assert_eq!(query.is_kids, Some(true));
        assert_eq!(query.is_sports, Some(false));
        assert_eq!(query.genre_ids, vec![genre]);
        assert!(!query.enable_total_record_count);
        // The regression this pins: "Recommended" used to issue the *same*
        // unfiltered query as the plain program list.
        assert_ne!(query, recorded_programs_query("/LiveTv/Programs").await);
    }

    #[tokio::test]
    async fn program_dto_options_follow_the_image_and_field_parameters() {
        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake.clone());
        let uri: axum::http::Uri =
            "/LiveTv/Programs?fields=Overview&enableImageTypes=Thumb&imageTypeLimit=1&enableUserData=false"
                .parse()
                .expect("uri");
        let query = Query::<ProgramsQuery>::try_from_uri(&uri).expect("query binds");
        let _ = get_programs(State(state), auth(), query).await.expect("ok");
        let options = fake
            .programs_options
            .lock()
            .unwrap()
            .clone()
            .expect("called");
        assert_eq!(options.fields, vec![ItemFields::Overview]);
        assert_eq!(options.image_types, vec![ImageType::Thumb]);
        assert_eq!(options.image_type_limit, 1);
        assert!(!options.enable_user_data);
        assert!(options.enable_images);
    }

    #[tokio::test]
    async fn live_recording_stream_404_when_no_file() {
        let fake = std::sync::Arc::new(FakeLiveTv::default());
        let state = fake_state().with_live_tv(fake);
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        let err =
            get_live_recording_stream(State(state), auth(), Path(Uuid::new_v4().to_string()), req)
                .await
                .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
