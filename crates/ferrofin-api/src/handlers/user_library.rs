//! `UserLibraryController` + the `ItemsController` user-data routes — per-user
//! play-state, favorites, ratings, and the item-scoped extra lists.
//!
//! Ports the portable user-data surface (Jellyfin's `Library` / `UserData`
//! tags):
//!
//! - `POST`/`DELETE /UserFavoriteItems/{itemId}` — mark / unmark a favourite.
//! - `POST`/`DELETE /UserItems/{itemId}/Rating` — set / clear the like rating.
//! - `GET`/`POST /UserItems/{itemId}/UserData` — read / write the item's
//!   [`UserItemDataDto`].
//! - `GET /Items/Root` — the user root folder.
//! - `GET /Items/{itemId}/LocalTrailers` — the item's trailer extras.
//! - `GET /Items/{itemId}/SpecialFeatures` — the item's display extras.
//! - `GET /Items/{itemId}/Intros` — pre-roll intros (empty; no intro provider
//!   is ported — Jellyfin returns none without a configured provider).
//! - `GET /Items/{itemId}/CriticReviews` — always an empty list (Jellyfin
//!   dropped critic-review providers; the route survives returning empty).
//!
//! Each favourite/rating handler resolves the item (the empty-guid fallback maps
//! to the user root folder, mirroring C# `itemId.IsEmpty()`), mutates the row via
//! the [`UserDataManager`](ferrofin_traits::library::UserDataManager), and returns
//! the refreshed [`UserItemDataDto`].
//!
//! Faithfulness notes / deferrals: the C# `AssertCanUpdateUser` policy gate on
//! the `UserData` reads/writes needs the un-ported per-user administration
//! policy; the portable seam authenticates the caller (`RequireAuth`) and scopes
//! to the resolved user. The person on-demand metadata refresh
//! (`RefreshItemOnDemandIfNeeded`) is the filesystem/OOP-tree slice deferred
//! elsewhere; `GET /Items/Latest` is the full `GetLatestMedia` port (the
//! grouping lives in the [`UserViewManager`](ferrofin_traits::library::UserViewManager)).

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::dto::{BaseItemDto, UpdateUserItemDataDto, UserItemDataDto};
use ferrofin_model::entities::ExtraType;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::handlers::items::{resolve_user, user_uuid};
use crate::handlers::session_ctx::notify_user_data_changed;
use crate::state::AppState;

/// The default `GET /Items/Latest` return limit (C# `limit = 20`).
const DEFAULT_LATEST_LIMIT: i32 = 20;

/// The extra types shown as "special features", matching C#
/// `BaseItem.DisplayExtraTypes`.
const DISPLAY_EXTRA_TYPES: &[ExtraType] = &[
    ExtraType::Unknown,
    ExtraType::BehindTheScenes,
    ExtraType::Clip,
    ExtraType::DeletedScene,
    ExtraType::Interview,
    ExtraType::Sample,
    ExtraType::Scene,
    ExtraType::Featurette,
    ExtraType::Short,
];

/// Query parameters carrying only the optional target `userId`.
///
/// `userId` is optional in the contract; when omitted it defaults to the
/// authenticated caller (Jellyfin's `RequestHelpers.GetUserId`).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserIdQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// Resolves the effective item for a user-data route: the empty guid maps to the
/// user root folder (C# `itemId.IsEmpty() ? GetUserRootFolder() : …`), any other
/// id to the addressed item. A missing item is reported as `None` so the caller
/// can map it to a `404`.
async fn resolve_item_id(state: &AppState, item_id: Uuid) -> Result<Option<Uuid>, ApiError> {
    if item_id.is_nil() {
        return Ok(state
            .library
            .get_user_root_folder()
            .await?
            .and_then(|item| Uuid::parse_str(&item.id).ok()));
    }
    Ok(state
        .library
        .get_item_by_id(item_id)
        .await?
        .and_then(|item| Uuid::parse_str(&item.id).ok()))
}

/// Saves `update` for the resolved user/item, then returns the refreshed DTO.
///
/// The shared body of the favourite / rating / user-data writes: mirror C#
/// `MarkFavorite` / `UpdateUserItemRatingInternal` (load → apply → save → read).
async fn save_and_return(
    state: &AppState,
    user_id: Uuid,
    item_id: Uuid,
    update: &UpdateUserItemDataDto,
) -> Result<Json<UserItemDataDto>, ApiError> {
    state
        .user_data
        .save_user_data(user_id, item_id, update)
        .await?;
    let dto = state
        .user_data
        .get_user_data_dto(item_id, user_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user data for item {item_id}")))?;
    notify_user_data_changed(state, user_id, &dto).await;
    Ok(Json(dto))
}

/// Sets — or clears — the user's like for an item, pushing the refreshed data
/// to the user's other sessions before returning it (the shared body of the
/// four rating routes).
async fn set_likes_and_notify(
    state: &AppState,
    user_id: Uuid,
    item_id: Uuid,
    likes: Option<bool>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let dto = state.user_data.set_likes(user_id, item_id, likes).await?;
    notify_user_data_changed(state, user_id, &dto).await;
    Ok(Json(dto))
}

/// Resolves the user and item for a user-data route, returning the two ids.
///
/// A missing user is a `404` (via [`resolve_user`]); a missing item is a `404`.
async fn resolve_user_and_item(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    user_id: Option<Uuid>,
    item_id: Uuid,
) -> Result<(Uuid, Uuid), ApiError> {
    let user = resolve_user(state, auth, user_id).await?;
    let user_uuid = user_uuid(&user)?;
    let resolved_item = resolve_item_id(state, item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    Ok((user_uuid, resolved_item))
}

/// `POST /UserFavoriteItems/{itemId}` — marks an item as a favourite.
///
/// Port of `UserLibraryController.MarkFavoriteItem`.
#[utoipa::path(
    post,
    path = "/UserFavoriteItems/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item marked as favorite (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn mark_favorite(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    set_favorite(&state, &auth, query.user_id, item_id, true).await
}

/// `DELETE /UserFavoriteItems/{itemId}` — unmarks an item as a favourite.
///
/// Port of `UserLibraryController.UnmarkFavoriteItem`.
#[utoipa::path(
    delete,
    path = "/UserFavoriteItems/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item unmarked as favorite (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn unmark_favorite(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    set_favorite(&state, &auth, query.user_id, item_id, false).await
}

/// `POST /Users/{userId}/FavoriteItems/{itemId}` — the legacy per-user favourite
/// route. jellyfin-web (apiclient) still calls this form; the current contract
/// route is `/UserFavoriteItems/{itemId}`. Same handler, `userId` from the path.
async fn mark_favorite_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    set_favorite(&state, &auth, Some(user_id), item_id, true).await
}

/// `DELETE /Users/{userId}/FavoriteItems/{itemId}` — legacy per-user unfavourite.
async fn unmark_favorite_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    set_favorite(&state, &auth, Some(user_id), item_id, false).await
}

/// `POST /Users/{userId}/Items/{itemId}/Rating` — legacy per-user like route.
async fn update_rating_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<RatingQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let (user_uuid, resolved_item) =
        resolve_user_and_item(&state, &auth, Some(user_id), item_id).await?;
    set_likes_and_notify(&state, user_uuid, resolved_item, query.likes).await
}

/// `DELETE /Users/{userId}/Items/{itemId}/Rating` — legacy per-user clear-like.
/// Clears the stored like (C# `Likes = null`) and returns the refreshed data.
async fn delete_rating_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let (user_uuid, resolved_item) =
        resolve_user_and_item(&state, &auth, Some(user_id), item_id).await?;
    set_likes_and_notify(&state, user_uuid, resolved_item, None).await
}

/// Shared favourite toggle for the mark/unmark handlers.
async fn set_favorite(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    user_id: Option<Uuid>,
    item_id: Uuid,
    is_favorite: bool,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let (user_uuid, resolved_item) = resolve_user_and_item(state, auth, user_id, item_id).await?;
    let update = UpdateUserItemDataDto {
        is_favorite: Some(is_favorite),
        ..UpdateUserItemDataDto::default()
    };
    save_and_return(state, user_uuid, resolved_item, &update).await
}

/// Query parameters for `POST /UserItems/{itemId}/Rating`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RatingQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Whether the rating is a "like"; absent clears the like.
    #[serde(default)]
    likes: Option<bool>,
}

/// `POST /UserItems/{itemId}/Rating` — sets the like rating for an item.
///
/// Port of `UserLibraryController.UpdateUserItemRating`.
#[utoipa::path(
    post,
    path = "/UserItems/{itemId}/Rating",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("likes" = Option<bool>, Query, description = "Whether this is a like")
    ),
    responses(
        (status = 200, description = "Rating updated (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn update_rating(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<RatingQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let (user_uuid, resolved_item) =
        resolve_user_and_item(&state, &auth, query.user_id, item_id).await?;
    // C# assigns `userData.Likes = likes` (clearing when the query param is
    // absent), so use the explicit like setter rather than the merge path.
    set_likes_and_notify(&state, user_uuid, resolved_item, query.likes).await
}

/// `DELETE /UserItems/{itemId}/Rating` — clears the like rating for an item.
///
/// Port of `UserLibraryController.DeleteUserItemRating` (`likes = null`).
#[utoipa::path(
    delete,
    path = "/UserItems/{itemId}/Rating",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Rating removed (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn delete_rating(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let (user_uuid, resolved_item) =
        resolve_user_and_item(&state, &auth, query.user_id, item_id).await?;
    // Persist the cleared like (C# saves `Likes = null`), then return the row.
    set_likes_and_notify(&state, user_uuid, resolved_item, None).await
}

/// `GET /UserItems/{itemId}/UserData` — reads an item's user data.
///
/// Port of `ItemsController.GetItemUserData`.
#[utoipa::path(
    get,
    path = "/UserItems/{itemId}/UserData",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "User data returned (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn get_item_user_data(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_uuid = user_uuid(&user)?;
    // C# `GetItemById<BaseItem>` requires a real item (no empty-guid fallback).
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let dto = state
        .user_data
        .get_user_data_dto(item_id, user_uuid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user data for item {item_id}")))?;
    Ok(Json(dto))
}

/// `POST /UserItems/{itemId}/UserData` — writes an item's user data.
///
/// Port of `ItemsController.UpdateItemUserData`.
#[utoipa::path(
    post,
    path = "/UserItems/{itemId}/UserData",
    params(("itemId" = String, Path, description = "The item id")),
    request_body = UpdateUserItemDataDto,
    responses(
        (status = 200, description = "User data updated (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn update_item_user_data(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
    JsonBody(update): JsonBody<UpdateUserItemDataDto>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_uuid = user_uuid(&user)?;
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    save_and_return(&state, user_uuid, item_id, &update).await
}

/// `GET /Items/Root` — the user root folder.
///
/// Port of `UserLibraryController.GetRootFolder`.
#[utoipa::path(
    get,
    path = "/Items/Root",
    responses(
        (status = 200, description = "Root folder returned (BaseItemDto)"),
        (status = 404, description = "User or root folder not found")
    ),
    tag = "ferrofin"
)]
async fn get_root_folder(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_user_root_folder()
        .await?
        .ok_or_else(|| ApiError::NotFound("user root folder".to_owned()))?;
    let options = DtoOptions::default();
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, Some(&user), None)
        .await?;
    Ok(Json(dto))
}

/// `GET /Items/{itemId}/LocalTrailers` — the item's local trailer extras.
///
/// Port of `UserLibraryController.GetLocalTrailers` (`GetExtras([Trailer])`).
#[utoipa::path(
    get,
    path = "/Items/{itemId}/LocalTrailers",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Local trailers returned (Vec<BaseItemDto>)"),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn get_local_trailers(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    get_extras(&state, &auth, query.user_id, item_id, &[ExtraType::Trailer]).await
}

/// `GET /Items/{itemId}/SpecialFeatures` — the item's display extras.
///
/// Port of `UserLibraryController.GetSpecialFeatures` (`GetExtras(user)` filtered
/// to `DisplayExtraTypes`).
#[utoipa::path(
    get,
    path = "/Items/{itemId}/SpecialFeatures",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Special features returned (Vec<BaseItemDto>)"),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn get_special_features(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    get_extras(&state, &auth, query.user_id, item_id, DISPLAY_EXTRA_TYPES).await
}

/// Shared extras query for the trailer / special-feature handlers: resolves the
/// user + owner item, then returns the owner's extras of the given types, sorted
/// by name (C# `GetExtras` orders by `SortName`).
async fn get_extras(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    user_id: Option<Uuid>,
    item_id: Uuid,
    extra_types: &[ExtraType],
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let user = resolve_user(state, auth, user_id).await?;
    let owner = resolve_item_id(state, item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let query = InternalItemsQuery {
        owner_ids: vec![owner],
        extra_types: extra_types.to_vec(),
        order_by: vec![(
            ferrofin_model::live_tv::ItemSortBy::SortName,
            ferrofin_model::dto::SortOrder::Ascending,
        )],
        ..InternalItemsQuery::default()
    };
    let items = state.library.get_item_list(&query).await?;
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, Some(&user), Some(owner), true)
        .await?;
    Ok(Json(dtos))
}

/// `GET /Items/{itemId}/Intros` — pre-roll intros before an item plays.
///
/// Port of `UserLibraryController.GetIntros`. No intro provider is ported (they
/// are plugin-supplied), so Jellyfin's default — an empty set — is returned; the
/// item/user are still resolved so a bogus id is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Intros",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Intros returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item or user not found")
    ),
    tag = "ferrofin"
)]
async fn get_intros(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    // Resolve the user + item so a missing one is a `404` (C# null-checks both).
    resolve_user_and_item(&state, &auth, query.user_id, item_id).await?;
    Ok(Json(QueryResult::from_items(Vec::new())))
}

/// Query parameters for `GET /Items/Latest`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatestQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Localizes the search to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to populate on
    /// each DTO. Absent/empty ⇒ the base DTO (matches Jellyfin's GetLatestMedia).
    #[serde(default)]
    fields: Option<String>,
    /// Comma-delimited [`BaseItemKind`](ferrofin_model::data::BaseItemKind) set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Filter by items that are played, or not.
    #[serde(default)]
    is_played: Option<bool>,
    /// Include image information in the output (default `true`).
    #[serde(default)]
    enable_images: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType)s to include.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Include user data (default `true`).
    #[serde(default)]
    enable_user_data: Option<bool>,
    /// Return item limit (default 20).
    #[serde(default)]
    limit: Option<i32>,
    /// Whether to group items into a parent container (default `true`).
    #[serde(default)]
    group_items: Option<bool>,
}

/// `GET /Items/Latest` — the user's newest media.
///
/// Port of `UserLibraryController.GetLatestMedia`. The
/// [`UserViewManager`](ferrofin_traits::library::UserViewManager) returns the
/// C# `(container, items)` tuples — one query across the user's views (or the
/// `parentId` folder), grouped by index container up to `limit` groups — and
/// this handler applies the controller's projection: a container with more
/// than one new item, or a `MusicAlbum` with any, is emitted in place of its
/// items with `ChildCount` = the number of new items; everything else is the
/// first item with `ChildCount` 0.
#[utoipa::path(
    get,
    path = "/Items/Latest",
    responses((status = 200, description = "Latest media returned (Vec<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_latest_media(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<LatestQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    use crate::handlers::query_parse::parse_csv_enums_lenient;

    let user = resolve_user(&state, &auth, query.user_id).await?;

    // C#: an unset `isPlayed` defaults to `false` when the user hides played
    // items from the "latest" rows.
    let is_played = query
        .is_played
        .or_else(|| user.hide_played_in_latest.then_some(false));
    let limit = query.limit.unwrap_or(DEFAULT_LATEST_LIMIT).max(0);

    // `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(enableImages,
    // enableUserData, imageTypeLimit, enableImageTypes)`.
    let options = crate::handlers::tv_shows::build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    );
    // The views the user excluded from "latest"
    // (`user.GetPreferenceValues<Guid>(PreferenceKind.LatestItemExcludes)`).
    let latest_item_excludes = state
        .users
        .get_user_dto(&user, None)
        .await?
        .configuration
        .map(|c| c.latest_items_excludes)
        .unwrap_or_default();
    let latest_query = ferrofin_traits::options::LatestItemsQuery {
        user: Some(user.clone()),
        parent_id: query.parent_id,
        include_item_types: parse_csv_enums_lenient(query.include_item_types.as_deref()),
        is_played,
        limit: Some(limit),
        group_items: query.group_items.unwrap_or(true),
        latest_item_excludes,
    };
    let groups = state
        .user_views
        .get_latest_items(&latest_query, &options)
        .await?;

    // The controller's tuple projection.
    let mut entities = Vec::with_capacity(groups.len());
    let mut child_counts = Vec::with_capacity(groups.len());
    for (container, mut items) in groups {
        let collapses = container.as_ref().is_some_and(|c| {
            items.len() > 1
                || type_name_matches(&c.type_, ferrofin_model::data::BaseItemKind::MusicAlbum)
        });
        match container {
            Some(container) if collapses => {
                child_counts.push(i32::try_from(items.len()).unwrap_or(i32::MAX));
                entities.push(container);
            }
            _ => {
                if items.is_empty() {
                    // A manager never yields an empty group (C# builds each
                    // tuple with its first item); skip rather than index.
                    continue;
                }
                child_counts.push(0);
                entities.push(items.swap_remove(0));
            }
        }
    }

    let mut dtos = state
        .dto
        .get_base_item_dtos(&entities, &options, Some(&user), None, true)
        .await?;
    for (dto, count) in dtos.iter_mut().zip(child_counts) {
        // `dto.ChildCount = childCount` on every row — 0 (serialized) for an
        // ungrouped item, the fetched-children count for a collapsed container.
        dto.child_count = Some(count);
    }
    Ok(Json(dtos))
}

/// Whether a stored type name matches a [`BaseItemKind`](ferrofin_model::data::BaseItemKind).
///
/// The stored `Type` column holds the full CLR type name (e.g.
/// `MediaBrowser.Controller.Entities.Movies.Movie`), so compare its last dotted
/// segment against the serde name of the kind (Jellyfin's `BaseItemKind` names).
pub(crate) fn type_name_matches(stored: &str, kind: ferrofin_model::data::BaseItemKind) -> bool {
    let short = stored.rsplit('.').next().unwrap_or(stored);
    // Compare against the serialized name in place: a borrowed `&str` against
    // the kind's name, no allocation of the stored name.
    matches!(serde_json::to_value(kind), Ok(serde_json::Value::String(name)) if name == short)
}

/// `GET /Items/{itemId}/CriticReviews` — critic reviews for an item.
///
/// Jellyfin removed critic-review providers; the route survives returning an
/// empty [`QueryResult`], which this port mirrors.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/CriticReviews",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Critic reviews (empty QueryResult)")),
    tag = "ferrofin"
)]
async fn get_critic_reviews(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(_item_id): Path<Uuid>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    Ok(Json(QueryResult::from_items(Vec::new())))
}

/// `GET /Users/{userId}/Items/Latest` — path-scoped form of `GET /Items/Latest`
/// (the home screen's "Latest …" rows). jellyfin-web's bundled apiclient calls
/// this form; it injects the path `userId` and forwards to [`get_latest_media`].
async fn get_latest_media_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    Query(mut query): Query<LatestQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_latest_media(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/Root` — path-scoped form of `GET /Items/Root`.
///
/// Like the other `/Users/{userId}/…` forms, this is absent from the 10.11
/// contract (upstream keeps it `[Obsolete]` + hidden from the OpenAPI doc) but
/// still served, and jellyfin-web's bundled `jellyfin-apiclient` still calls it.
async fn get_root_folder_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    get_root_folder(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}/Intros` — path-scoped form of
/// `GET /Items/{itemId}/Intros` (jellyfin-web calls it before every playback).
async fn get_intros_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    get_intros(state, auth, Path(item_id), Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}/LocalTrailers` — path-scoped form of
/// `GET /Items/{itemId}/LocalTrailers`.
async fn get_local_trailers_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    get_local_trailers(state, auth, Path(item_id), Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}/SpecialFeatures` — path-scoped form of
/// `GET /Items/{itemId}/SpecialFeatures`.
async fn get_special_features_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    get_special_features(state, auth, Path(item_id), Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}/UserData` — path-scoped form of
/// `GET /UserItems/{itemId}/UserData`.
async fn get_item_user_data_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    get_item_user_data(state, auth, Path(item_id), Query(query)).await
}

/// `POST /Users/{userId}/Items/{itemId}/UserData` — path-scoped form of
/// `POST /UserItems/{itemId}/UserData`.
async fn update_item_user_data_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    update: JsonBody<UpdateUserItemDataDto>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    update_item_user_data(state, auth, Path(item_id), Query(query), update).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/UserFavoriteItems/{itemId}",
            post(mark_favorite).delete(unmark_favorite),
        )
        // Legacy per-user routes still used by jellyfin-web's apiclient.
        .route(
            "/Users/{userId}/FavoriteItems/{itemId}",
            post(mark_favorite_for_user).delete(unmark_favorite_for_user),
        )
        .route(
            "/Users/{userId}/Items/{itemId}/Rating",
            post(update_rating_for_user).delete(delete_rating_for_user),
        )
        .route(
            "/Users/{userId}/Items/Latest",
            get(get_latest_media_for_user),
        )
        .route("/Users/{userId}/Items/Root", get(get_root_folder_for_user))
        .route(
            "/Users/{userId}/Items/{itemId}/Intros",
            get(get_intros_for_user),
        )
        .route(
            "/Users/{userId}/Items/{itemId}/LocalTrailers",
            get(get_local_trailers_for_user),
        )
        .route(
            "/Users/{userId}/Items/{itemId}/SpecialFeatures",
            get(get_special_features_for_user),
        )
        .route(
            "/Users/{userId}/Items/{itemId}/UserData",
            get(get_item_user_data_for_user).post(update_item_user_data_for_user),
        )
        .route(
            "/UserItems/{itemId}/Rating",
            post(update_rating).delete(delete_rating),
        )
        .route(
            "/UserItems/{itemId}/UserData",
            get(get_item_user_data).post(update_item_user_data),
        )
        .route("/Items/Root", get(get_root_folder))
        .route("/Items/Latest", get(get_latest_media))
        .route("/Items/{itemId}/LocalTrailers", get(get_local_trailers))
        .route("/Items/{itemId}/SpecialFeatures", get(get_special_features))
        .route("/Items/{itemId}/Intros", get(get_intros))
        .route("/Items/{itemId}/CriticReviews", get(get_critic_reviews))
}

#[cfg(test)]
mod tests {
    use super::type_name_matches;
    use ferrofin_model::data::BaseItemKind;

    #[test]
    fn type_filter_matches_full_clr_name() {
        // Stored `Type` is the full CLR name; the short kind name must still match.
        assert!(type_name_matches(
            "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
            BaseItemKind::MusicAlbum
        ));
        assert!(type_name_matches(
            "MediaBrowser.Controller.Entities.Movies.Movie",
            BaseItemKind::Movie
        ));
        assert!(!type_name_matches(
            "MediaBrowser.Controller.Entities.TV.Episode",
            BaseItemKind::Movie
        ));
    }
}
