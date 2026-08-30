//! `ChannelsController` — internet/provider channels.
//!
//! Channels are contributed by channel *providers*, which in Jellyfin ship as
//! plugins. Ferrofin has no built-in channel providers, so — exactly like a stock
//! Jellyfin install with none registered — every channel query resolves to an
//! empty result and there are no channel features to report. These handlers
//! therefore return empty `QueryResult`s / `ChannelFeatures` directly.
//!
//! ponytail: no `ChannelManager` trait for an always-empty subsystem — add one
//! only if channel providers are ever introduced.
//!
//! Ports `ChannelsController`:
//! - `GET /Channels` — the user's channels (empty).
//! - `GET /Channels/Features` — every channel's features (empty list).
//! - `GET /Channels/{channelId}/Features` — one channel's features (default).
//! - `GET /Channels/{channelId}/Items` — a channel's items (empty).
//! - `GET /Channels/Items/Latest` — latest items across channels (empty).

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::channels::ChannelFeatures;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::effective_user_id;
use crate::state::AppState;

/// The `userId` query parameter the three user-scoped channel routes accept.
///
/// The result is empty either way, but the parameter is **not** inert: upstream
/// runs `RequestHelpers.GetUserId` on it before it ever reaches the channel
/// manager, so naming another user's id as a non-administrator is a `403` there
/// and must be one here. Dropping the parameter on the floor turned that refusal
/// into a `200`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelUserQuery {
    /// Optional target user; defaults to the authenticated caller.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// `GET /Channels` — the authenticated user's channels.
///
/// Port of `ChannelsController.GetChannels`. No channel providers → empty.
#[utoipa::path(
    get,
    path = "/Channels",
    params(("userId" = Option<String>, Query, description = "User id.")),
    // Body schema omitted: `BaseItemDto` recurses without bound in the derived
    // `utoipa::ToSchema` (a `ferrofin-model` DTO defect) — see `items::get_items`.
    responses((status = 200, description = "Channels returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_channels(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ChannelUserQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    effective_user_id(&state, &auth, query.user_id).await?;
    Ok(Json(QueryResult::default()))
}

/// `GET /Channels/Features` — features for every channel.
///
/// Port of `ChannelsController.GetAllChannelFeatures`. No channels → empty list.
#[utoipa::path(
    get,
    path = "/Channels/Features",
    responses((status = 200, description = "Channel features returned", body = [ChannelFeatures])),
    tag = "ferrofin"
)]
async fn get_all_channel_features(RequireAuth(_auth): RequireAuth) -> Json<Vec<ChannelFeatures>> {
    Json(Vec::new())
}

/// `GET /Channels/{channelId}/Features` — one channel's features.
///
/// Port of `ChannelsController.GetChannelFeatures`. No provider backs the id, so
/// a default feature set (carrying the requested id) is returned.
#[utoipa::path(
    get,
    path = "/Channels/{channelId}/Features",
    params(("channelId" = String, Path, description = "Channel id")),
    responses((status = 200, description = "Channel features returned", body = ChannelFeatures)),
    tag = "ferrofin"
)]
async fn get_channel_features(
    RequireAuth(_auth): RequireAuth,
    Path(channel_id): Path<Uuid>,
) -> Json<ChannelFeatures> {
    Json(ChannelFeatures {
        id: channel_id,
        ..ChannelFeatures::default()
    })
}

/// `GET /Channels/{channelId}/Items` — a channel's items.
///
/// Port of `ChannelsController.GetChannelItems`. No channels → empty.
#[utoipa::path(
    get,
    path = "/Channels/{channelId}/Items",
    params(
        ("channelId" = String, Path, description = "Channel id"),
        ("userId" = Option<String>, Query, description = "User id.")
    ),
    responses((status = 200, description = "Channel items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_channel_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(_channel_id): Path<Uuid>,
    Query(query): Query<ChannelUserQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    effective_user_id(&state, &auth, query.user_id).await?;
    Ok(Json(QueryResult::default()))
}

/// `GET /Channels/Items/Latest` — latest items across all channels.
///
/// Port of `ChannelsController.GetLatestChannelItems`. No channels → empty.
#[utoipa::path(
    get,
    path = "/Channels/Items/Latest",
    params(("userId" = Option<String>, Query, description = "User id.")),
    responses((status = 200, description = "Latest channel items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_latest_channel_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ChannelUserQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    effective_user_id(&state, &auth, query.user_id).await?;
    Ok(Json(QueryResult::default()))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Channels", get(get_channels))
        .route("/Channels/Features", get(get_all_channel_features))
        .route("/Channels/{channelId}/Features", get(get_channel_features))
        .route("/Channels/{channelId}/Items", get(get_channel_items))
        .route("/Channels/Items/Latest", get(get_latest_channel_items))
}
