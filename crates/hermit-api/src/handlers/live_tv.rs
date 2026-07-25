//! `LiveTvController` — the read surface of Jellyfin's Live TV.
//!
//! Hermit has no tuner/EPG/DVR backend (Live TV is a deferred subsystem — see
//! `brain/DEFERRED.md`). But a stock Jellyfin install with **no Live TV service
//! configured** does not 501 its browse endpoints: it reports Live TV as
//! *disabled* and returns empty channel/program/recording/timer listings. The
//! web UI depends on that — the home screen calls `GET /LiveTv/Programs/Recommended`
//! on load, and a `501` there surfaces as a console error and a broken tile.
//!
//! So these handlers return the honest **"nothing configured"** state — empty
//! `QueryResult`s and disabled/default DTOs — exactly like [`channels`] does for
//! the (also-empty) internet-channels subsystem. No manager trait: an
//! always-empty read surface has no behaviour to inject.
//!
//! ponytail: only the *read/query* ops are wired. Every **mutation** (create a
//! timer, add a tuner host / listing provider, reset a tuner, delete a
//! recording) and every **by-id lookup** stays on the shared `501` stub — the
//! browsing UI never reaches them (their parent lists are empty), and faking a
//! `204 No Content` success for "recording scheduled" when nothing was scheduled
//! would be a lie (CLAUDE.md: never fake a deferred subsystem). Un-defer path:
//! implement a real `LiveTvManager` + tuner/EPG backend, then promote those ops.
//!
//! [`channels`]: crate::handlers::channels

use axum::routing::get;
use axum::{Json, Router};
use hermit_model::dto::{BaseItemDto, NameIdPair};
use hermit_model::live_tv::{
    ChannelMappingOptionsDto, GuideInfo, ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto,
    TimerInfoDto, TunerHostInfo,
};
use hermit_model::querying::QueryResult;

use crate::auth::RequireAuth;
use crate::state::AppState;

/// `GET /LiveTv/Info` — top-level Live TV status.
///
/// Port of `LiveTvController.GetLiveTvInfo`. No service configured →
/// [`LiveTvInfo::default`] (`IsEnabled = false`, no services), which tells the
/// client to hide the Live TV section.
async fn get_live_tv_info(RequireAuth(_auth): RequireAuth) -> Json<LiveTvInfo> {
    Json(LiveTvInfo::default())
}

/// `GET /LiveTv/GuideInfo` — the guide's date range.
///
/// Port of `LiveTvController.GetGuideInfo`. No guide data → default range.
async fn get_guide_info(RequireAuth(_auth): RequireAuth) -> Json<GuideInfo> {
    Json(GuideInfo::default())
}

/// `GET /LiveTv/Channels` — the user's Live TV channels.
///
/// Port of `LiveTvController.GetLiveTvChannels`. No tuners → empty.
async fn get_channels(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Programs` — EPG programs (query-string form).
///
/// Port of `LiveTvController.GetLiveTvPrograms`. No guide → empty.
async fn get_programs(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `POST /LiveTv/Programs` — EPG programs (request-body form).
///
/// Port of `LiveTvController.GetPrograms`. Same empty result as the GET form;
/// the `GetProgramsDto` body is ignored because there is no guide data.
async fn post_programs(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Programs/Recommended` — "On Now" / recommended programs.
///
/// Port of `LiveTvController.GetRecommendedPrograms`. Called by the web home
/// screen on load; no guide → empty (the tile renders nothing rather than
/// erroring on a `501`).
async fn get_recommended_programs(
    RequireAuth(_auth): RequireAuth,
) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings` — DVR recordings.
///
/// Port of `LiveTvController.GetRecordings`. No DVR → empty.
async fn get_recordings(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Folders` — recording folders.
///
/// Port of `LiveTvController.GetRecordingFolders`. No DVR → empty.
async fn get_recording_folders(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Groups` — recording groups (deprecated in Jellyfin).
///
/// Port of `LiveTvController.GetRecordingGroups`. No DVR → empty.
async fn get_recording_groups(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Recordings/Series` — series recordings (deprecated in Jellyfin).
///
/// Port of `LiveTvController.GetRecordingsSeries`. No DVR → empty.
async fn get_recordings_series(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<BaseItemDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Timers` — pending recording timers.
///
/// Port of `LiveTvController.GetTimers`. No DVR → empty.
async fn get_timers(RequireAuth(_auth): RequireAuth) -> Json<QueryResult<TimerInfoDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/Timers/Defaults` — default values for a new timer.
///
/// Port of `LiveTvController.GetDefaultTimer`. Returns a default
/// [`SeriesTimerInfoDto`] (Jellyfin returns a fresh timer-defaults object).
async fn get_default_timer(RequireAuth(_auth): RequireAuth) -> Json<SeriesTimerInfoDto> {
    Json(SeriesTimerInfoDto::default())
}

/// `GET /LiveTv/SeriesTimers` — recurring (series) recording timers.
///
/// Port of `LiveTvController.GetSeriesTimers`. No DVR → empty.
async fn get_series_timers(
    RequireAuth(_auth): RequireAuth,
) -> Json<QueryResult<SeriesTimerInfoDto>> {
    Json(QueryResult::default())
}

/// `GET /LiveTv/ChannelMappingOptions` — channel-mapping options.
///
/// Port of `LiveTvController.GetChannelMappingOptions`. No provider → default.
async fn get_channel_mapping_options(
    RequireAuth(_auth): RequireAuth,
) -> Json<ChannelMappingOptionsDto> {
    Json(ChannelMappingOptionsDto::default())
}

/// `GET /LiveTv/ListingProviders/Default` — default listing-provider config.
///
/// Port of `LiveTvController.GetDefaultListingProvider`. Returns a fresh
/// [`ListingsProviderInfo`], matching Jellyfin's `new ListingsProviderInfo()`.
async fn get_default_listing_provider(
    RequireAuth(_auth): RequireAuth,
) -> Json<ListingsProviderInfo> {
    Json(ListingsProviderInfo::default())
}

/// `GET /LiveTv/ListingProviders/Lineups` — available lineups for a provider.
///
/// Port of `LiveTvController.GetLineups`. No provider configured → empty.
async fn get_lineups(RequireAuth(_auth): RequireAuth) -> Json<Vec<NameIdPair>> {
    Json(Vec::new())
}

/// `GET /LiveTv/TunerHosts/Types` — supported tuner-host types.
///
/// Port of `LiveTvController.GetTunerHostTypes`. No tuner backends → empty.
async fn get_tuner_host_types(RequireAuth(_auth): RequireAuth) -> Json<Vec<NameIdPair>> {
    Json(Vec::new())
}

/// `GET /LiveTv/Tuners/Discover` — auto-discovered tuner devices.
///
/// Port of `LiveTvController.DiscoverTuners`. No tuners on the network (nothing
/// to scan) → empty.
async fn discover_tuners(RequireAuth(_auth): RequireAuth) -> Json<Vec<TunerHostInfo>> {
    Json(Vec::new())
}

/// Registers the Live TV read surface onto `router`.
///
/// Only the empty-state read/query ops; mutations and by-id lookups stay on the
/// shared `501` stub (see the module docs).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/LiveTv/Info", get(get_live_tv_info))
        .route("/LiveTv/GuideInfo", get(get_guide_info))
        .route("/LiveTv/Channels", get(get_channels))
        .route("/LiveTv/Programs", get(get_programs).post(post_programs))
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
            "/LiveTv/ListingProviders/Default",
            get(get_default_listing_provider),
        )
        .route("/LiveTv/ListingProviders/Lineups", get(get_lineups))
        .route("/LiveTv/TunerHosts/Types", get(get_tuner_host_types))
        .route("/LiveTv/Tuners/Discover", get(discover_tuners))
        .route("/LiveTv/Tuners/Discvover", get(discover_tuners))
}

#[cfg(test)]
mod tests {
    use hermit_traits::options::AuthorizationInfo;

    use super::*;

    /// A `RequireAuth` for an authenticated (default) caller — lets the tests
    /// invoke handlers directly, past the extractor, to exercise their bodies.
    fn auth() -> RequireAuth {
        RequireAuth(AuthorizationInfo::default())
    }

    #[tokio::test]
    async fn info_is_disabled_and_empty() {
        let info = get_live_tv_info(auth()).await.0;
        assert!(!info.is_enabled);
        assert!(info.services.is_empty());
    }

    #[tokio::test]
    async fn all_query_ops_return_empty() {
        assert_eq!(get_channels(auth()).await.0.total_record_count, 0);
        assert_eq!(get_programs(auth()).await.0.total_record_count, 0);
        assert_eq!(post_programs(auth()).await.0.total_record_count, 0);
        assert_eq!(
            get_recommended_programs(auth()).await.0.total_record_count,
            0
        );
        assert_eq!(get_recordings(auth()).await.0.total_record_count, 0);
        assert_eq!(get_recording_folders(auth()).await.0.total_record_count, 0);
        assert_eq!(get_recording_groups(auth()).await.0.total_record_count, 0);
        assert_eq!(get_recordings_series(auth()).await.0.total_record_count, 0);
        assert_eq!(get_timers(auth()).await.0.total_record_count, 0);
        assert_eq!(get_series_timers(auth()).await.0.total_record_count, 0);
    }

    #[tokio::test]
    async fn singleton_and_list_ops_return_defaults() {
        // GuideInfo / timer defaults / mapping options / default provider: just
        // that the default DTO is produced without panicking.
        let _ = get_guide_info(auth()).await;
        let _ = get_default_timer(auth()).await;
        let _ = get_channel_mapping_options(auth()).await;
        let _ = get_default_listing_provider(auth()).await;
        assert!(get_lineups(auth()).await.0.is_empty());
        assert!(get_tuner_host_types(auth()).await.0.is_empty());
        assert!(discover_tuners(auth()).await.0.is_empty());
    }
}
