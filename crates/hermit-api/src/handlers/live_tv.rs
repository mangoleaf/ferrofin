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
//! The DVR surface (recordings, timers, series timers) has no backend yet and
//! returns empty listings; its mutations stay on the shared `501` stub.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use uuid::Uuid;

use hermit_model::dto::{BaseItemDto, NameIdPair};
use hermit_model::live_tv::{
    ChannelMappingOptionsDto, GuideInfo, ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto,
    TimerInfoDto, TunerHostInfo,
};
use hermit_model::querying::QueryResult;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};

use crate::auth::RequireAuth;
use crate::error::ApiError;
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

/// `GET /LiveTv/GuideInfo` — the guide's date range.
///
/// Port of `LiveTvController.GetGuideInfo`.
async fn get_guide_info(RequireAuth(_auth): RequireAuth) -> Json<GuideInfo> {
    Json(GuideInfo::default())
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

/// `GET /LiveTv/Recordings` — DVR recordings (no DVR backend → empty).
async fn get_recordings(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Folders` — recording folders (no DVR → empty).
async fn get_recording_folders(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Groups` — recording groups (deprecated; empty).
async fn get_recording_groups(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Series` — series recordings (deprecated; empty).
async fn get_recordings_series(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Timers` — pending recording timers (no DVR → empty).
async fn get_timers(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<TimerInfoDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Timers/Defaults` — default values for a new timer.
async fn get_default_timer(RequireAuth(_auth): RequireAuth) -> Json<SeriesTimerInfoDto> {
    Json(SeriesTimerInfoDto::default())
}

/// `GET /LiveTv/SeriesTimers` — recurring (series) timers (no DVR → empty).
async fn get_series_timers(
    RequireAuth(_auth): RequireAuth,
) -> Json<QueryResult<SeriesTimerInfoDto>> {
    Json(QueryResult::default())
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
        .route("/LiveTv/Recordings/Folders", get(get_recording_folders))
        .route("/LiveTv/Recordings/Groups", get(get_recording_groups))
        .route("/LiveTv/Recordings/Series", get(get_recordings_series))
        .route("/LiveTv/Timers", get(get_timers))
        .route("/LiveTv/Timers/Defaults", get(get_default_timer))
        .route("/LiveTv/SeriesTimers", get(get_series_timers))
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
        .route("/LiveTv/ListingProviders/Lineups", get(get_lineups))
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
        assert!(get_recordings(auth()).await.0.items.is_empty());
        assert!(get_recording_folders(auth()).await.0.items.is_empty());
        assert!(get_recording_groups(auth()).await.0.items.is_empty());
        assert!(get_recordings_series(auth()).await.0.items.is_empty());
        assert_eq!(get_timers(auth()).await.0.total_record_count, 0);
        assert_eq!(get_series_timers(auth()).await.0.total_record_count, 0);
    }
}
