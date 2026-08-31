//! `ChannelsController` — internet/provider channels.
//!
//! Channels are contributed by channel *providers* (`IChannel`), which in Jellyfin
//! ship as .NET plugins. Neither the vendored 10.11.8 tree nor upstream master
//! contains a single `IChannel` implementation, so on a stock server no `Channel`
//! item can exist at all — and Ferrofin, which does not load .NET assemblies, is
//! in the same state permanently.
//!
//! That splits this controller in two, and the split is *measured*, not assumed:
//!
//! - The COLLECTION routes (`/Channels`, `/Channels/Features`,
//!   `/Channels/Items/Latest`) query for `Channel` items and legitimately find
//!   none, so both servers answer `200` with an empty result.
//! - The PER-CHANNEL routes (`/Channels/{channelId}/…`) do **not**. Upstream
//!   resolves the id first — `ChannelManager.GetChannel(id)` is
//!   `_libraryManager.GetItemById(id) as Channel`, which is `null` for every id
//!   — and hands that `null` straight to
//!   `ChannelManager.GetChannelProvider(channel)`, whose first statement is
//!   `ArgumentNullException.ThrowIfNull(channel)`
//!   (v10.11.8 `src/Jellyfin.LiveTv/Channels/ChannelManager.cs:1177`, master
//!   `:1176` — byte-identical). `ExceptionMiddleware.GetStatusCode` maps
//!   `ArgumentException => Status400BadRequest`, so **every** `channelId` is a
//!   `400` on a stock server, on both trees.
//!
//! CONTRACT NOTE (owner-visible, not an implementation detail): `400` is **not**
//! among the responses the vendored 10.11.8 spec declares for either per-channel
//! op — `.paths["/Channels/{channelId}/Features"].get.responses` and the `/Items`
//! one are both `[200, 401, 403, 503]`. The undeclared status is the *spec's*
//! divergence rather than ours: the C# reaches `400` by letting the
//! `ArgumentNullException` escape the action into `ExceptionMiddleware`, a path
//! the generated OpenAPI never sees, so the oracle emits it undeclared too and
//! the wire stays symmetric. `contract_superset` is unaffected — it gates the
//! route TABLE, not per-op response codes, and `400` is not `404`. Recorded here
//! because "the wire carries a status the vendored contract does not declare" is
//! the owner's call to keep or to raise upstream, never an agent's.
//!
//! Ferrofin used to answer `200` here — an empty item list, and a fabricated
//! `ChannelFeatures` echoing back whatever UUID was asked for. That told a client
//! "this channel exists and is empty" for a resource that cannot exist, and the
//! fabricated body was wrong on its own terms as well (`GetChannelFeaturesDto`
//! sets `CanFilter = !features.MaxPageSize.HasValue`, i.e. `true` for a null page
//! size, where `ChannelFeatures::default()` gives `false`). Both routes now
//! reject, matching the C# on both trees. Measured on the parity pair
//! 2026-08-30: before, F `200` vs J `400`; after, `400` on both.
//!
//! ponytail: no `ChannelManager` trait for a subsystem with no providers — add
//! one only if channel providers are ever introduced.
//!
//! Ports `ChannelsController`:
//! - `GET /Channels` — the user's channels (empty).
//! - `GET /Channels/Features` — every channel's features (empty list).
//! - `GET /Channels/{channelId}/Features` — one channel's features (400: no provider).
//! - `GET /Channels/{channelId}/Items` — a channel's items (400: no provider).
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
/// Port of `ChannelsController.GetChannelFeatures` →
/// `ChannelManager.GetChannelFeatures(Guid?)` (v10.11.8
/// `src/Jellyfin.LiveTv/Channels/ChannelManager.cs:545-556`, master `:543-554`):
/// `GetChannel(id)` is `null` with no `IChannel` provider registered, and
/// `GetChannelProvider(null)` throws `ArgumentNullException` before any
/// not-found check — `ExceptionMiddleware` maps that to `400`.
///
/// Nothing may be fabricated here: a feature set carrying the requested id would
/// claim a channel exists that no provider backs.
#[utoipa::path(
    get,
    path = "/Channels/{channelId}/Features",
    params(("channelId" = String, Path, description = "Channel id")),
    responses(
        (status = 200, description = "Channel features returned", body = ChannelFeatures),
        (status = 400, description = "No channel provider backs the id")
    ),
    tag = "ferrofin"
)]
async fn get_channel_features(
    RequireAuth(_auth): RequireAuth,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<ChannelFeatures>, ApiError> {
    Err(ApiError::BadRequest(format!(
        "no channel provider found for channel {channel_id}"
    )))
}

/// `GET /Channels/{channelId}/Items` — a channel's items.
///
/// Port of `ChannelsController.GetChannelItems` →
/// `ChannelManager.GetChannelItemsInternal` (v10.11.8
/// `src/Jellyfin.LiveTv/Channels/ChannelManager.cs:691-697`), which resolves
/// `GetChannel(query.ChannelIds[0])` and calls `GetChannelProvider(channel)`
/// *before* it queries anything. With no `IChannel` provider that channel is
/// `null`, so the same `ArgumentNullException.ThrowIfNull` fires and upstream
/// answers `400` for every id. An empty `QueryResult` would assert the channel
/// exists.
#[utoipa::path(
    get,
    path = "/Channels/{channelId}/Items",
    params(
        ("channelId" = String, Path, description = "Channel id"),
        ("userId" = Option<String>, Query, description = "User id.")
    ),
    responses(
        (status = 200, description = "Channel items returned (QueryResult<BaseItemDto>)"),
        (status = 400, description = "No channel provider backs the id")
    ),
    tag = "ferrofin"
)]
async fn get_channel_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(channel_id): Path<Uuid>,
    Query(query): Query<ChannelUserQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    // Upstream runs `RequestHelpers.GetUserId(User, userId)` on the very first
    // line of `GetChannelItems`, before the channel is resolved — so a
    // non-administrator naming another user's id is refused there *before* the
    // provider lookup throws. Both outcomes have to survive in that order.
    effective_user_id(&state, &auth, query.user_id).await?;
    Err(ApiError::BadRequest(format!(
        "no channel provider found for channel {channel_id}"
    )))
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
