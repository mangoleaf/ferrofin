//! `LiveTvController` — Hermit's M3U-tuner + XMLTV-guide Live TV.
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

use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;

use hermit_model::dto::{BaseItemDto, NameIdPair, NameValuePair};
use hermit_model::live_tv::{
    ChannelMappingOptionsDto, GuideInfo, ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto,
    TimerInfoDto, TunerChannelMapping, TunerHostInfo,
};
use hermit_model::querying::QueryResult;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};

use crate::auth::RequireAuth;
use crate::error::ApiError;
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
/// until Live TV options are surfaced in Hermit's config.
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
/// Port of `LiveTvController.GetLiveTvChannels`.
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

/// `GET /LiveTv/Programs` — EPG programs (query-string form).
///
/// Port of `LiveTvController.GetLiveTvPrograms`.
async fn get_programs(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    Ok(Json(query_programs(&state).await?))
}

/// `POST /LiveTv/Programs` — EPG programs (request-body form).
///
/// Port of `LiveTvController.GetPrograms`. The `GetProgramsDto` filter body is
/// not yet honored; returns the same set as the GET form.
async fn post_programs(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    Ok(Json(query_programs(&state).await?))
}

/// `GET /LiveTv/Programs/Recommended` — "On Now" / recommended programs.
///
/// Port of `LiveTvController.GetRecommendedPrograms`.
async fn get_recommended_programs(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    Ok(Json(query_programs(&state).await?))
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
async fn query_programs(state: &AppState) -> Result<QueryResult<BaseItemDto>, ApiError> {
    match state.live_tv.as_ref() {
        Some(m) => Ok(m
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await?),
        None => Ok(QueryResult::default()),
    }
}

/// `POST /LiveTv/TunerHosts` — add (or update) an M3U tuner host.
///
/// Port of `LiveTvController.AddTunerHost`. Saves the host and refreshes the
/// guide so its channels populate immediately; returns the stored host.
async fn add_tuner_host(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
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
    RequireAuth(_auth): RequireAuth,
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
    RequireAuth(_auth): RequireAuth,
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
    RequireAuth(_auth): RequireAuth,
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
    RequireAuth(_auth): RequireAuth,
    Path(tuner_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    live_tv(&state)?.reset_tuner(&tuner_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Returns the wired Live TV manager, or `501` when Live TV is not configured in
/// this build (the composition root did not wire a manager).
fn live_tv(
    state: &AppState,
) -> Result<&std::sync::Arc<dyn hermit_traits::stubs::LiveTvManager>, ApiError> {
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
/// Port of `LiveTvController.GetSchedulesDirectCountries`: Hermit's Live TV is
/// M3U + XMLTV, with no Schedules Direct provider, so the available-country set
/// is empty (faithful — Jellyfin streams SD's country JSON only when SD is
/// configured). Returned as a JSON array so the dashboard's SD setup page parses
/// it instead of erroring.
async fn get_schedules_direct_countries(
    RequireAuth(_auth): RequireAuth,
) -> Json<serde_json::Value> {
    Json(serde_json::json!([]))
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
/// a tuner buffers a live stream into; Hermit direct-plays each M3U channel from
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
    RequireAuth(_auth): RequireAuth,
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
        hermit_model::session::SessionMessageType::TimerCreated,
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
    message_type: hermit_model::session::SessionMessageType,
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
        hermit_model::session::SessionMessageType::TimerCancelled,
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
        hermit_model::session::SessionMessageType::SeriesTimerCreated,
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
        hermit_model::session::SessionMessageType::SeriesTimerCancelled,
        &timer_id,
        None,
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /LiveTv/ChannelMappingOptions` — channel-mapping options.
async fn get_channel_mapping_options(
    RequireAuth(_auth): RequireAuth,
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
/// Port of `LiveTvController.GetTunerHostTypes`. Hermit ships the M3U backend.
async fn get_tuner_host_types(RequireAuth(_auth): RequireAuth) -> Json<Vec<NameIdPair>> {
    Json(vec![NameIdPair {
        name: Some("M3U Tuner".to_owned()),
        id: Some("m3u".to_owned()),
    }])
}

/// `GET /LiveTv/Tuners/Discover` — auto-discovered tuner devices (none → empty).
async fn discover_tuners(RequireAuth(_auth): RequireAuth) -> Json<Vec<TunerHostInfo>> {
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
    use hermit_traits::options::AuthorizationInfo;

    use super::*;
    use crate::test_support::fake_state;

    /// A `RequireAuth` for an authenticated (default) caller.
    fn auth() -> RequireAuth {
        RequireAuth(AuthorizationInfo::default())
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
            get_programs(State(state.clone()), auth())
                .await
                .unwrap()
                .0
                .total_record_count,
            0
        );
        assert_eq!(
            post_programs(State(state.clone()), auth())
                .await
                .unwrap()
                .0
                .total_record_count,
            0
        );
        assert_eq!(
            get_recommended_programs(State(state), auth())
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
        let err = add_tuner_host(State(state.clone()), auth(), Json(TunerHostInfo::default()))
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
        let _ = get_channel_mapping_options(auth()).await;
        let _ = get_default_listing_provider(auth()).await;
        assert!(get_lineups(auth()).await.0.is_empty());
        assert_eq!(get_tuner_host_types(auth()).await.0.len(), 1);
        assert!(discover_tuners(auth()).await.0.is_empty());
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
    async fn schedules_direct_countries_is_empty() {
        assert_eq!(
            get_schedules_direct_countries(auth()).await.0,
            serde_json::json!([])
        );
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
            auth(),
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

    /// A minimal [`LiveTvManager`] backing only the three methods the new
    /// handlers touch; everything else is unreachable in these tests.
    #[derive(Default)]
    struct FakeLiveTv {
        providers: std::sync::Mutex<Vec<ListingsProviderInfo>>,
        recording_path: Option<String>,
    }

    #[async_trait::async_trait]
    impl hermit_traits::stubs::LiveTvManager for FakeLiveTv {
        async fn get_listing_providers(
            &self,
        ) -> Result<Vec<ListingsProviderInfo>, hermit_traits::error::ServiceError> {
            Ok(self.providers.lock().unwrap().clone())
        }
        async fn save_listing_provider(
            &self,
            info: ListingsProviderInfo,
        ) -> Result<ListingsProviderInfo, hermit_traits::error::ServiceError> {
            let mut g = self.providers.lock().unwrap();
            *g = vec![info.clone()];
            Ok(info)
        }
        async fn get_recording_path(
            &self,
            _id: Uuid,
        ) -> Result<Option<String>, hermit_traits::error::ServiceError> {
            Ok(self.recording_path.clone())
        }
        async fn get_live_tv_info(&self) -> Result<LiveTvInfo, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_tuner_hosts(
            &self,
        ) -> Result<Vec<TunerHostInfo>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn save_tuner_host(
            &self,
            _info: TunerHostInfo,
        ) -> Result<TunerHostInfo, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_tuner_host(
            &self,
            _id: &str,
        ) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_listing_provider(
            &self,
            _id: &str,
        ) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channels(
            &self,
            _options: &DtoOptions,
        ) -> Result<QueryResult<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channel(
            &self,
            _id: Uuid,
            _options: &DtoOptions,
        ) -> Result<Option<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_programs(
            &self,
            _query: &InternalItemsQuery,
            _options: &DtoOptions,
        ) -> Result<QueryResult<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_program(
            &self,
            _id: Uuid,
            _options: &DtoOptions,
        ) -> Result<Option<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn reset_tuner(&self, _id: &str) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn refresh_guide(&self) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_channel_stream_url(
            &self,
            _id: Uuid,
        ) -> Result<Option<String>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_timers(
            &self,
        ) -> Result<Vec<TimerInfoDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_timer(
            &self,
            _id: &str,
        ) -> Result<Option<TimerInfoDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn create_timer(
            &self,
            _timer: TimerInfoDto,
        ) -> Result<String, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn update_timer(
            &self,
            _id: &str,
            _timer: TimerInfoDto,
        ) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn cancel_timer(&self, _id: &str) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_series_timers(
            &self,
        ) -> Result<Vec<SeriesTimerInfoDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_series_timer(
            &self,
            _id: &str,
        ) -> Result<Option<SeriesTimerInfoDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn create_series_timer(
            &self,
            _timer: SeriesTimerInfoDto,
        ) -> Result<String, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn update_series_timer(
            &self,
            _id: &str,
            _timer: SeriesTimerInfoDto,
        ) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn cancel_series_timer(
            &self,
            _id: &str,
        ) -> Result<(), hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_recordings(
            &self,
        ) -> Result<QueryResult<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn get_recording(
            &self,
            _id: Uuid,
        ) -> Result<Option<BaseItemDto>, hermit_traits::error::ServiceError> {
            unimplemented!()
        }
        async fn delete_recording(
            &self,
            _id: Uuid,
        ) -> Result<(), hermit_traits::error::ServiceError> {
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
        });
        let state = fake_state().with_live_tv(fake.clone());
        let mapping = set_channel_mapping(
            State(state),
            auth(),
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
            auth(),
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
