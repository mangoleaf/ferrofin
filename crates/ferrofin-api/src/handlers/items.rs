//! `ItemsController` / `UserLibraryController` / `LibraryController` — item
//! queries, counts, ancestors, and deletion under the `/Items` path.
//!
//! Ports the portable `/Items` surface:
//!
//! - `GET  /Items` — a paged, filtered, user-scoped query over the library,
//!   projected to [`BaseItemDto`]s and wrapped in a [`QueryResult`]. The wide
//!   Jellyfin `GetItems` query is mapped onto [`InternalItemsQuery`]; the
//!   persistence layer (`translate_query`) applies every filter.
//! - `GET  /Items/{itemId}` — a single item by id.
//! - `DELETE /Items/{itemId}` — deletes one item (and its subtree).
//! - `DELETE /Items` — deletes several items by id.
//! - `GET  /Items/Counts` — per-kind item counts for a (favourite-filtered) query.
//! - `GET  /Items/{itemId}/Ancestors` — the item's parents, nearest first.
//!
//! The remote/metadata-provider slices of these controllers (search providers,
//! box-set collapsing that needs the un-ported `Folder` OOP tree) are applied by
//! the persistence layer where portable and otherwise left to later waves; the
//! handler maps the request faithfully onto the query struct.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, ItemCounts};
use ferrofin_model::entities::ImageType;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::options::{AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::query_parse::{
    parse_csv_enums_lenient, parse_csv_strings, parse_csv_uuids, parse_pipe_strings,
};
use crate::state::AppState;

/// Resolves a user-addressed item before returning data or performing a side effect.
/// Invisible and missing IDs deliberately have the same response.
pub(crate) async fn require_visible_item(
    state: &AppState,
    item_id: Uuid,
    user: Option<&UserEntity>,
) -> Result<BaseItemEntity, ApiError> {
    state
        .library
        .get_item_by_id_for_user(item_id, user)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))
}

/// The effective user id for a request, porting C#
/// `RequestHelpers.GetUserId` (v10.11.8 `Jellyfin.Api/Helpers/RequestHelpers.cs`
/// lines 67-85) in both of its halves:
///
/// 1. An absent **or all-zero** `user_id` is "not provided" — upstream tests
///    `userId.IsNullOrEmpty()` and `Guid.Empty` is empty — so it falls back to
///    the authenticated caller. Rejecting a nil made
///    `?userId=00000000-0000-0000-0000-000000000000` a `400` here against a
///    `200` upstream, a shape jellyfin-web sends whenever it has no user id in
///    hand yet.
/// 2. Naming **another** user's id requires the administrator role: upstream
///    throws `SecurityException("Forbidden")`, which `ExceptionMiddleware`
///    (`:129`) maps to `403`. Without it any authenticated account could read
///    another account's user-scoped data by passing its guid.
///
/// The role lookup only runs when the caller actually named a different user
/// (jellyfin-web always sends its own id), so the common path adds no work. An
/// API-key caller is elevated upstream and is elevated here.
///
/// This is the **single** implementation of the rule for the whole crate: any
/// handler that accepts a `userId` from the request must route it through here
/// (directly, or via [`resolve_user`] / [`resolve_user_opt`] /
/// [`resolve_user_or_api_key`]) rather than
/// hand-rolling a `user_id.unwrap_or(caller)` fallback, which silently drops
/// half 2 and lets any authenticated account act as any other.
pub(crate) async fn effective_user_id(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<Uuid, ApiError> {
    let authenticated = auth.user_id();
    let Some(requested) = user_id.filter(|id| !id.is_nil()) else {
        return Ok(authenticated);
    };
    if requested == authenticated || auth.is_api_key {
        return Ok(requested);
    }
    let elevated = match &auth.user {
        Some(user) => super::users::is_administrator(state, user).await?,
        None => false,
    };
    if elevated {
        Ok(requested)
    } else {
        Err(ApiError::Forbidden("Forbidden".to_owned()))
    }
}

/// Resolves the effective user for a request: the explicit `user_id` query
/// parameter when present, otherwise the authenticated caller.
///
/// The id itself comes from [`effective_user_id`] (the full
/// `RequestHelpers.GetUserId` port, nil-fallback *and* the administrator gate).
/// A caller with neither an explicit id nor an authenticated user (a nil
/// effective id) is a `400`, as `GetUserById(Guid.Empty)` throws
/// `ArgumentException`; an id that resolves to no account is a `404`.
pub(crate) async fn resolve_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<UserEntity, ApiError> {
    let effective = effective_user_id(state, auth, user_id).await?;
    if effective.is_nil() {
        return Err(ApiError::BadRequest("no user for request".to_owned()));
    }
    load_user(state, auth, effective).await
}

/// Resolves the effective user for an action that also serves an API key with
/// no user: v12.2 `ItemsController.GetItems` (`ItemsController.cs:263-274`).
///
/// An API key that names no `userId` has no user (`GetUserId` falls back to
/// the key's empty claim), so the action runs with none: unscoped, with no
/// per-user filters and no `UserData`. Any other caller without a user is a
/// `400`, upstream's rule; no request reaches that arm today, because the
/// default authorization policy already refuses a token that is neither a
/// key nor a user (`403`). An id naming no account is a `404`, as
/// [`resolve_user`].
pub(crate) async fn resolve_user_or_api_key(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<Option<UserEntity>, ApiError> {
    let effective = effective_user_id(state, auth, user_id).await?;
    if effective.is_nil() {
        return if auth.is_api_key {
            Ok(None)
        } else {
            Err(ApiError::BadRequest("userId is required".to_owned()))
        };
    }
    load_user(state, auth, effective).await.map(Some)
}

/// The account `effective` names: the caller's own row when it is the caller,
/// else a lookup, a `404` when no account has that id.
async fn load_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    effective: Uuid,
) -> Result<UserEntity, ApiError> {
    // The auth layer already loaded the caller's row — reuse it instead of
    // re-fetching the identical user on every user-scoped request.
    if effective == auth.user_id()
        && let Some(user) = &auth.user
    {
        return Ok(user.clone());
    }
    state
        .users
        .get_user_by_id(effective)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {effective}")))
}

/// The resolved user's id as a [`Uuid`].
///
/// [`UserEntity::id`] holds the hyphenated `Guid` text the row was written with
/// (`guid_to_db`), so this is exactly the id the lookup used. A row whose id
/// does not parse means a corrupt `Users` table — a state upstream cannot even
/// express, because C# `User.Id` *is* a `Guid`. Report it as a backend failure
/// (`500`) rather than degrading to the nil GUID, which would silently scope the
/// request to *no* user: empty user data, no parental filtering, and a `UserId`
/// no client can act on.
pub(crate) fn user_uuid(user: &UserEntity) -> Result<Uuid, ApiError> {
    Uuid::parse_str(&user.id).map_err(|_| {
        ApiError::Service(ferrofin_traits::ServiceError::Backend(
            "stored user id is not a guid".to_owned(),
        ))
    })
}

/// Resolves the effective user *optionally*: like [`resolve_user`] but a nil
/// effective id yields [`None`] rather than a `400`.
///
/// Mirrors the controllers (counts/ancestors/delete/instant-mix) that accept an
/// API-key caller with no user — C# runs `RequestHelpers.GetUserId` FIRST and
/// only then `userId.IsNullOrEmpty() ? null : GetUserById(...)`
/// (`InstantMixController.cs:82-85`), so the same nil-falls-back-to-caller rule
/// applies here and `None` is reached only when there is genuinely no user at
/// all. Resolving the id through [`effective_user_id`] instead of
/// `user_id.unwrap_or(caller)` is load-bearing: an explicit all-zero `userId`
/// used to yield `None` on this path and strip `UserData` off every DTO the
/// instant-mix routes returned, against a fully populated `UserData` upstream.
pub(crate) async fn resolve_user_opt(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<Option<UserEntity>, ApiError> {
    let effective = effective_user_id(state, auth, user_id).await?;
    if effective.is_nil() {
        return Ok(None);
    }
    // Same reuse as `resolve_user`: the auth layer already holds this row.
    if effective == auth.user_id()
        && let Some(user) = &auth.user
    {
        return Ok(Some(user.clone()));
    }
    Ok(state.users.get_user_by_id(effective).await?)
}

/// The query parameters honoured by `GET /Items`.
///
/// Every field is optional. Comma/pipe-delimited multi-value parameters arrive as
/// a raw [`String`] and are split + parsed in [`get_items`]; this keeps the serde
/// surface simple while matching Jellyfin's `CommaDelimited`/`PipeDelimited`
/// model binders.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The index of the first item to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of items to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Whether to search descendants recursively.
    #[serde(default)]
    recursive: Option<bool>,
    /// Override collection collapsing for this folder browse.
    #[serde(default)]
    collapse_box_set_items: Option<bool>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`](ferrofin_model::data::BaseItemKind) set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited item kinds to exclude.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited [`MediaType`](ferrofin_model::data::MediaType) set.
    #[serde(default)]
    media_types: Option<String>,
    /// Comma-delimited [`ItemSortBy`](ferrofin_model::live_tv::ItemSortBy) columns.
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited [`SortOrder`](ferrofin_model::dto::SortOrder) directions.
    #[serde(default)]
    sort_order: Option<String>,
    /// Comma-delimited [`ItemFilter`](ferrofin_model::querying::ItemFilter) flags.
    #[serde(default)]
    filters: Option<String>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType)s an
    /// item must have — Wholphin's genre grid asks for one `Backdrop` item per
    /// genre and crashes on an item without one.
    #[serde(default)]
    image_types: Option<String>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to populate
    /// on each returned DTO (e.g. `Path`, `Genres`). Absent/empty ⇒ the base DTO.
    #[serde(default)]
    fields: Option<String>,
    /// Comma-delimited explicit item ids to fetch.
    #[serde(default)]
    ids: Option<String>,
    /// Comma-delimited item ids to exclude.
    #[serde(default)]
    exclude_item_ids: Option<String>,
    /// Pipe-delimited genre names.
    #[serde(default)]
    genres: Option<String>,
    /// Pipe-delimited tags.
    #[serde(default)]
    tags: Option<String>,
    /// Pipe-delimited official ratings.
    #[serde(default)]
    official_ratings: Option<String>,
    /// Comma-delimited production years.
    #[serde(default)]
    years: Option<String>,
    /// Comma-delimited genre ids.
    #[serde(default)]
    genre_ids: Option<String>,
    /// Comma-delimited studio ids.
    #[serde(default)]
    studio_ids: Option<String>,
    /// Comma-delimited person ids.
    #[serde(default)]
    person_ids: Option<String>,
    /// Comma-delimited artist ids.
    #[serde(default)]
    artist_ids: Option<String>,
    /// Comma-delimited artist ids to exclude.
    #[serde(default)]
    exclude_artist_ids: Option<String>,
    /// Comma-delimited album artist ids — jellyfin-web's artist page sends
    /// this for its Albums section.
    #[serde(default)]
    album_artist_ids: Option<String>,
    /// Comma-delimited contributing artist ids — the artist page's
    /// "Appears On" section.
    #[serde(default)]
    contributing_artist_ids: Option<String>,
    /// Comma-delimited album ids.
    #[serde(default)]
    album_ids: Option<String>,
    /// Pipe-delimited album names. The first album of each name replaces
    /// `albumIds`.
    #[serde(default)]
    albums: Option<String>,
    /// Pipe-delimited artist names. Their artists replace `artistIds`.
    #[serde(default)]
    artists: Option<String>,
    /// Pipe-delimited studio names. Their studios replace `studioIds`.
    #[serde(default)]
    studios: Option<String>,
    /// Restrict to favourited items.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Restrict to played / unplayed items.
    #[serde(default)]
    is_played: Option<bool>,
    /// Restrict to movies.
    #[serde(default)]
    is_movie: Option<bool>,
    /// Restrict to series.
    #[serde(default)]
    is_series: Option<bool>,
    /// Restrict to 4K items.
    #[serde(default, rename = "is4K")]
    is_4k: Option<bool>,
    /// Restrict to HD items (jellyfin-web's filter dialog sends `IsHD`; keys
    /// bind ignoring case).
    #[serde(default)]
    is_hd: Option<bool>,
    /// Restrict to 3D items (jellyfin-web sends `Is3D` → `is3D`).
    #[serde(default, rename = "is3D")]
    is_3d: Option<bool>,
    /// Restrict to missing (virtual) episodes, or exclude them.
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_missing: Option<bool>,
    /// Restrict to episodes not aired yet, or exclude them.
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_unaired: Option<bool>,
    /// Restrict to Live TV news programs (or exclude them).
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_news: Option<bool>,
    /// Restrict to Live TV kids programs (or exclude them).
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_kids: Option<bool>,
    /// Restrict to Live TV sports programs (or exclude them).
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_sports: Option<bool>,
    /// The minimum width of the item. The four dimension bounds bind as
    /// plain integers: an empty value is a 400, where ASP.NET binds null.
    #[serde(default)]
    min_width: Option<i32>,
    /// The maximum width of the item.
    #[serde(default)]
    max_width: Option<i32>,
    /// The minimum height of the item.
    #[serde(default)]
    min_height: Option<i32>,
    /// The maximum height of the item.
    #[serde(default)]
    max_height: Option<i32>,
    /// The minimum last-saved date, bound as `ItemsController`'s `DateTime`
    /// and converted to UTC.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_date_time::deserialize_optional_date_time"
    )]
    min_date_last_saved: Option<DateTime<Utc>>,
    /// The minimum last-saved date of the caller's user data, bound and
    /// converted like `minDateLastSaved`.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_date_time::deserialize_optional_date_time"
    )]
    min_date_last_saved_for_user: Option<DateTime<Utc>>,
    /// Restrict to metadata-locked (or unlocked) items (`IsLocked`).
    #[serde(default)]
    is_locked: Option<bool>,
    /// Restrict to items with (or without) an overview.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_overview: Option<bool>,
    /// Restrict to items with (or without) an IMDb provider id.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_imdb_id: Option<bool>,
    /// Restrict to items with (or without) a TMDb provider id.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_tmdb_id: Option<bool>,
    /// Restrict to items with (or without) a TVDb provider id.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_tvdb_id: Option<bool>,
    /// Restrict to items with (or without) a custom or official parental rating.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_parental_rating: Option<bool>,
    /// Restrict to items with (or without) an official rating; a folder counts
    /// as rated when an item below it is.
    #[serde(default, deserialize_with = "http_presence_filter")]
    has_official_rating: Option<bool>,
    /// The lowest official rating to return, resolved to a parental-rating
    /// score (`LocalizationManager.GetRatingScore`); unrated items stay in.
    #[serde(default)]
    min_official_rating: Option<String>,
    /// The highest official rating to return, resolved like
    /// `minOfficialRating`. It can lower the user's parental limit, never
    /// raise it.
    #[serde(default)]
    max_official_rating: Option<String>,
    /// Comma-delimited [`SeriesStatus`](ferrofin_model::entities::SeriesStatus)
    /// set — jellyfin-web's TV Status filter.
    #[serde(default)]
    series_status: Option<String>,
    /// Comma-delimited person types (`Actor`, `Director`, …) a `person` or
    /// `personIds` credit must have.
    #[serde(default)]
    person_types: Option<String>,
    /// Restrict to placeholder items (an offline disc stub), or exclude them;
    /// a folder counts as one when an item below it is.
    #[serde(default, deserialize_with = "http_presence_filter")]
    is_place_holder: Option<bool>,
    /// Comma-delimited [`VideoType`](ferrofin_model::entities::VideoType) set
    /// (`BluRay`, `Dvd`, `Iso`).
    #[serde(default)]
    video_types: Option<String>,
    /// Restrict to items with (or without) subtitle streams.
    #[serde(default)]
    has_subtitles: Option<bool>,
    /// Restrict to items with (or without) a local trailer extra.
    #[serde(default)]
    has_trailer: Option<bool>,
    /// Restrict to items with (or without) a special-feature extra.
    #[serde(default)]
    has_special_feature: Option<bool>,
    /// Restrict to items with (or without) a theme song extra.
    #[serde(default)]
    has_theme_song: Option<bool>,
    /// Restrict to items with (or without) a theme video extra.
    #[serde(default)]
    has_theme_video: Option<bool>,
    /// Exact index number.
    #[serde(default)]
    index_number: Option<i32>,
    /// Exact parent index number.
    #[serde(default)]
    parent_index_number: Option<i32>,
    /// Minimum community rating.
    #[serde(default)]
    min_community_rating: Option<f64>,
    /// Minimum critic rating.
    #[serde(default)]
    min_critic_rating: Option<f64>,
    /// Inclusive lower premiere-date bound, normalized to UTC.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_date_time::deserialize_optional_date_time"
    )]
    min_premiere_date: Option<DateTime<Utc>>,
    /// Inclusive upper premiere-date bound, normalized to UTC.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_date_time::deserialize_optional_date_time"
    )]
    max_premiere_date: Option<DateTime<Utc>>,
    /// Return this item and its immediate neighbours after sorting, before paging.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    adjacent_to: Option<Uuid>,
    /// Restrict to items whose name starts with this value.
    #[serde(default)]
    name_starts_with: Option<String>,
    /// Restrict to items whose name sorts at or after this value.
    #[serde(default)]
    name_starts_with_or_greater: Option<String>,
    /// Restrict to items whose name sorts before this value.
    #[serde(default)]
    name_less_than: Option<String>,
    /// A single person name filter.
    #[serde(default)]
    person: Option<String>,
    /// Comma-delimited [`LocationType`](ferrofin_model::entities::LocationType)
    /// set to include. Drives `IsVirtualItem` — see
    /// [`apply_location_types`](crate::handlers::query_parse::apply_location_types).
    #[serde(default)]
    location_types: Option<String>,
    /// Comma-delimited `LocationType` set to exclude.
    #[serde(default)]
    exclude_location_types: Option<String>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
    /// Include image information in the output (C# default `true`).
    #[serde(default)]
    enable_images: Option<bool>,
    /// Include user data (unset ⇒ the `DtoOptions` default, `true`).
    #[serde(default)]
    enable_user_data: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType)s to
    /// include. Empty ⇒ every type, as upstream.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Comma-delimited audio-stream languages (12.1 `audioLanguages`): keep an
    /// item with an audio stream in any of them — see
    /// [`parse_csv_strings`] for the binding.
    #[serde(default)]
    audio_languages: Option<String>,
    /// Comma-delimited subtitle-stream languages (12.1 `subtitleLanguages`).
    #[serde(default)]
    subtitle_languages: Option<String>,
}

impl ItemsQuery {
    /// `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(enableImages,
    /// enableUserData, imageTypeLimit, enableImageTypes)` (v12
    /// ItemsController.cs:283-284, DtoExtensions.cs:57-82).
    ///
    /// Lenient field parse: clients still send deprecated field names, which
    /// Jellyfin drops rather than 400ing; absent ⇒ empty ⇒ the base DTO.
    fn dto_options(&self) -> DtoOptions {
        super::by_name::additional_dto_options(
            self.fields.as_deref(),
            self.enable_images,
            self.enable_user_data,
            self.image_type_limit,
            self.enable_image_types.as_deref(),
        )
    }
}

/// ASP.NET's string binder converts empty/whitespace query strings to null.
/// Keep nonblank values byte-for-byte: leading/trailing spaces are part of the
/// requested name boundary, not text to trim. Internal typed queries bypass
/// this HTTP binding and retain their own source filter semantics.
fn http_name_boundary(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

/// ASP.NET nullable booleans accept case-insensitive, trimmed true/false and
/// bind empty or whitespace-only values to null; malformed values fail with 400.
fn http_presence_filter<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let raw = Option::<String>::deserialize(deserializer)?;
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if raw.trim().eq_ignore_ascii_case("true") {
        Ok(Some(true))
    } else if raw.trim().eq_ignore_ascii_case("false") {
        Ok(Some(false))
    } else {
        Err(serde::de::Error::custom(format!("invalid boolean {raw:?}")))
    }
}

/// `GET /Items` — a paged, filtered, user-scoped library query.
///
/// Port of `ItemsController.GetItems` (v12 ItemsController.cs:83-350). The
/// wide query is mapped onto [`InternalItemsQuery`] and every persistable
/// filter is honoured by `translate_query`; the controller-side branches
/// live here: the `UserRootFolder` children answer for an unfiltered,
/// non-recursive, id-less request (`user_root_children`) while a filtered one
/// runs recursively (Jellyfin 12's `HasFilters` dispatch), the library browse
/// defaults ([`apply_library_browse_defaults`]), the BoxSet/Playlist re-rooting
/// (`redirect_container_browse`), `ApplyFilters`, `LocationTypes` →
/// `IsVirtualItem`, the `AllowedTags` ⇒ `Tags` field rule, and
/// `AddAdditionalDtoOptions`. A non-recursive `parentId` browse of a box-set
/// or playlist surfaces its manual `LinkedChildren` members (the SQL merge
/// that mirrors C# `Folder.GetChildren`; see `translate_query`).
#[utoipa::path(
    get,
    path = "/Items",
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "Items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_or_api_key(&state, &auth, query.user_id).await?;

    let mut internal = items_query_options(&query, user)?;
    // `query.MinParentalRating = _localization.GetRatingScore(minOfficialRating)`
    // and the same for the maximum, each only when the parameter is not blank
    // (v12.2 ItemsController.cs:515-525). The repository applies the stricter
    // of the requested maximum and the user's own limit, so a request can
    // lower a restricted user's ceiling but not raise it: upstream lets the
    // request replace the limit `SetUser` copied from the user's policy, and an
    // unrecognized rating there clears it.
    internal.min_parental_rating =
        official_rating_score(&state, query.min_official_rating.as_deref());
    internal.max_parental_rating =
        official_rating_score(&state, query.max_official_rating.as_deref());
    resolve_name_filters(&state, &query, &mut internal).await?;
    // C# `ItemsController.GetItems` (ItemsController.cs:307, 525-529) answers a
    // non-recursive, id-less request whose parent resolves to the
    // `UserRootFolder` with `folder.GetChildren(user, true)` — not with a query.
    // The controller owns that decision; the repository applies it only if the
    // parent really is the user root (an ABSENT `parentId` is, per
    // `LibraryManager.GetParentItem(null, userId)`).
    internal.user_root_children = !internal.recursive && internal.item_ids.is_empty();
    // Explicit ids and text searches always return the requested titles.
    if !internal.item_ids.is_empty()
        || internal
            .search_term
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        internal.collapse_box_set_items = Some(false);
    }
    apply_media_stream_filter_rules(&mut internal);
    // `IsVirtualItem` is driven ONLY by these two parameters on this route
    // (v10.11.8 Jellyfin.Api/Controllers/ItemsController.cs:437-447) — there is
    // no `isVirtualItem` query parameter to set it directly.
    crate::handlers::query_parse::apply_location_types(
        &mut internal.is_virtual_item,
        query.location_types.as_deref(),
        query.exclude_location_types.as_deref(),
    );
    // C# `ApplyFilters` translates the `filters` flag set onto the tri-state
    // fields, rejecting contradictory pairs with a `400`. Token parsing is
    // lenient + case-insensitive like ASP.NET's binder: jellyfin-web sends
    // `Filters=IsUnPlayed` (sic), and upstream drops unknown tokens.
    let filters = parse_csv_enums_lenient(query.filters.as_deref());
    internal
        .apply_filters(&filters)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // A BoxSet/Playlist-typed browse under a normal library is re-rooted: box
    // sets live outside the library tree, so the parent becomes a
    // linked-child-ancestor constraint instead (C# ItemsController's
    // `linkedChildAncestorIds` redirect). Without this, the library's
    // Collections tab can never list anything.
    let mut library = None;
    let (parent_type, folder_policy) =
        check_parent_visibility(&state, &auth, &mut internal, &mut library).await?;
    internal.parent_type = parent_type;
    // Reuse the already-loaded parent for SourceType and UserView dispatch;
    // only source consumers that call Folder's policy inherit global flags.
    internal.apply_folder_display_options = folder_policy;
    redirect_container_browse(&state, &mut internal).await;
    // After the re-root, as upstream: it decides on the requested types and
    // these rewrites on the folder that results.
    list_playlists_folder_as_playlists(&mut internal);
    apply_library_browse_defaults(&mut internal, library, query.recursive);
    // The default order sees the final query: the resolved `artists` names and
    // the rewritten types. The search order then leads with `SortName`.
    apply_albums_by_artist_order(&mut internal);
    order_search_by_sort_name(&mut internal);
    // `|| query.HasFilters` (v12.2 ItemsController.cs:592) with
    // `UserRootFolder.GetItemsInternal` (UserRootFolder.cs:77): at the user
    // root only an unfiltered request is the plain listing of the user's
    // libraries; a filtered one runs recursively.
    if internal.item_ids.is_empty()
        && (internal.parent_id.is_nil()
            || internal.parent_type == Some(BaseItemKind::UserRootFolder))
        && internal.has_filters()
    {
        internal.recursive = true;
        internal.user_root_children = false;
    }
    if !internal.item_ids.is_empty() {
        // ItemsController clears query.Parent immediately before GetItems.
        // Keep any linked-child ancestor constraint established by the
        // BoxSet/Playlist redirect, but bypass parent/view child selection.
        internal.set_parent(None);
        internal.user_root_children = false;
    }

    let mut result = state.library.query_items(&internal).await?;
    sort_items_by_request(&internal, &mut result.items);
    // The requested `Fields` plus the four image/user-data toggles (an
    // `EnableImageTypes=Primary,Backdrop` browse must not answer with a
    // `Logo` tag, and `ImageTypeLimit=1` caps the backdrops).
    let mut options = query.dto_options();
    // `if (user.GetPreference(AllowedTags).Length != 0 && !fields.Contains(Tags))
    //  fields = [.. fields, Tags];` (v12 ItemsController.cs:276-281): a user
    // whose policy whitelists tags always gets each row's `Tags`.
    if let Some(user) = internal.user.as_ref()
        && whitelists_tags(&state, &auth, user).await?
        && !options.contains_field(ItemFields::Tags)
    {
        options.fields.push(ItemFields::Tags);
    }
    // A playlist's or box set's own listing is its linked children
    // (`Folder.GetItemsInternal` → `GetChildren` → `GetLinkedChildren(user)`),
    // which upstream resolves with every field loaded; a recursive listing is
    // the repository query, loaded with the client's fields.
    let linked_children = !internal.recursive
        && internal.item_ids.is_empty()
        && matches!(
            internal.parent_type,
            Some(BaseItemKind::BoxSet | BaseItemKind::Playlist)
        );
    let dtos = if linked_children {
        state
            .dto
            .get_linked_children_dtos(&result.items, &options, internal.user.as_ref())
            .await?
    } else {
        state
            .dto
            .get_base_item_dtos(&result.items, &options, internal.user.as_ref(), None, true)
            .await?
    };
    Ok(Json(QueryResult::new(
        Some(result.start_index),
        Some(result.total_record_count),
        dtos,
    )))
}

/// Whether `user`'s policy names any `AllowedTags`.
///
/// The caller's own policy came with the request, so a browse needs no user
/// DTO for it. `user` can be another account, one an administrator or an API
/// key named with `userId`: its policy is read, never the caller's.
async fn whitelists_tags(
    state: &AppState,
    auth: &AuthorizationInfo,
    user: &UserEntity,
) -> Result<bool, ApiError> {
    if auth
        .user
        .as_ref()
        .is_some_and(|caller| caller.id == user.id)
        && let Some(policy) = &auth.user_policy
    {
        return Ok(!policy.allowed_tags.is_empty());
    }
    Ok(state
        .users
        .get_user_dto(user, None)
        .await?
        .policy
        .is_some_and(|policy| !policy.allowed_tags.is_empty()))
}

/// `if (collectionType == CollectionType.playlists) { recursive = true;
/// includeItemTypes = [Playlist]; }` (v12.2 ItemsController.cs:321-325): a
/// playlists folder, or the Playlists view that displays one, lists its
/// playlists recursively, so the user's post-filter (only playlists they may
/// open) applies whatever the request asked for.
///
/// Every playlist the user may open, wherever it is: Jellyfin 12.2 lists a
/// playlist file inside a music album in its Playlists view (measured, for
/// the plain `parentId=<view>` browse jellyfin-web sends and the
/// `IncludeItemTypes=Playlist` one), not only the playlists the folder holds.
/// So the listing is re-rooted at the user root, as the typed browse is
/// ([`redirect_container_browse`]). With no user (an API key without
/// `userId`, [`resolve_user_or_api_key`]) upstream lists the folder's own
/// children only, and so does this.
fn list_playlists_folder_as_playlists(internal: &mut InternalItemsQuery) {
    if matches!(
        internal.parent_type,
        Some(BaseItemKind::PlaylistsFolder | BaseItemKind::ManualPlaylistsFolder)
    ) {
        internal.recursive = true;
        internal.include_item_types = vec![BaseItemKind::Playlist];
        internal.user_root_children = false;
        if internal.user.is_some() {
            reroot_to_user_root(internal);
        }
    }
}

/// A library (`ICollectionFolder`) parent's defaults (v12.2
/// ItemsController.cs:326-340): a `boxsets` library browsed with no item
/// types lists box sets, and an item-typed browse that did not choose
/// `recursive` lists the library's contents recursively rather than just its
/// immediate children. `library` is `Some(is_boxsets)` only when the requested
/// parent is itself a library ([`library_is_boxsets`]); a view that delegates
/// to one is not, and a re-rooted browse no longer has a library parent.
fn apply_library_browse_defaults(
    internal: &mut InternalItemsQuery,
    library: Option<bool>,
    requested_recursive: Option<bool>,
) {
    let Some(is_boxsets) = library else {
        return;
    };
    if internal.parent_id.is_nil() {
        return;
    }
    if internal.include_item_types.is_empty() && is_boxsets {
        internal.include_item_types = vec![BaseItemKind::BoxSet];
    }
    if !internal.include_item_types.is_empty() && requested_recursive.is_none() {
        internal.recursive = true;
        internal.user_root_children = false;
    }
}

/// "Apply default sorting if none requested" (v12.2 ItemsController.cs:579-586):
/// an artist's albums, asked for with no `sortBy`, come newest first, then by
/// name. `sortBy=Default` is a requested sort and keeps its own order. With an
/// order set, an explicit-ids page keeps it rather than the requested id order
/// ([`sort_items_by_request`]), as upstream's `Folder.GetItems` does.
fn apply_albums_by_artist_order(internal: &mut InternalItemsQuery) {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    if internal.order_by.is_empty()
        && !internal.artist_ids.is_empty()
        && internal.include_item_types == [BaseItemKind::MusicAlbum]
    {
        internal.order_by = vec![
            (ItemSortBy::ProductionYear, SortOrder::Descending),
            (ItemSortBy::SortName, SortOrder::Ascending),
        ];
    }
}

/// Leads a searched query's sort keys with `SortName` ascending.
///
/// v12.2 `ItemsController.GetItems` (ItemsController.cs:596-600) orders a
/// searched page by the search providers' score, then `SortName`, with a
/// stable in-memory sort of what `folder.GetItems` returned in the caller's
/// order — so the caller's `sortBy` only orders rows tied on both. The
/// repository's search branch (v12.2 `ApplyOrder`) emits the relevance rank,
/// then these keys (the leading `SortName` earning its `Name` tiebreaker), so
/// this reproduces that order with the relevance rank standing in for the
/// provider score until those providers are ported (gl-450). The extra `Name`
/// term only reorders rows tied on `SortName`. Upstream falls back to `ApplyOrder` alone when no provider
/// returns a candidate; that fallback belongs to the same port.
pub(crate) fn order_search_by_sort_name(internal: &mut InternalItemsQuery) {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    if internal
        .search_term
        .as_deref()
        .is_some_and(|term| !term.trim().is_empty())
    {
        internal
            .order_by
            .insert(0, (ItemSortBy::SortName, SortOrder::Ascending));
    }
}

/// Puts an explicit-ids page back in the order its ids were requested.
///
/// v12.2 `Folder.GetItems` (Folder.cs:1065-1083), where `GetItems` sends
/// explicit ids (ItemsController.cs:591-595): with no `OrderBy` and more than
/// one id, the repository's page is ordered by `Array.IndexOf(ItemIds, id)`,
/// a stable sort in which a row whose id was not asked for (`-1`) comes first.
/// Only the page is reordered; the start index and total stay as they are.
/// `order_by` is the request's, so `sortBy=Default` still counts as a sort
/// (the repository drops it later). A search already leads its keys with
/// `SortName` ([`order_search_by_sort_name`]), standing in for the score sort
/// upstream applies after this one, so it keeps that order.
pub(crate) fn sort_items_by_request(query: &InternalItemsQuery, items: &mut [BaseItemEntity]) {
    if !query.order_by.is_empty() || query.item_ids.len() <= 1 {
        return;
    }
    items.sort_by_cached_key(|row| {
        Uuid::parse_str(&row.id)
            .ok()
            .and_then(|id| ferrofin_util::read_only_list_extension::index_of(&query.item_ids, &id))
    });
}

/// The name filters replace their id lists (v12.2 ItemsController.cs:527-577):
/// `artists` replaces `artistIds`, `albums` replaces `albumIds` and `studios`
/// replaces `studioIds`. They resolve without writing a row, so an unknown name
/// matches nothing; an absent or blank list leaves the id list as it is.
async fn resolve_name_filters(
    state: &AppState,
    query: &ItemsQuery,
    internal: &mut InternalItemsQuery,
) -> Result<(), ApiError> {
    use super::by_name::{resolve_album_ids, resolve_by_name_ids};
    if let Some(ids) =
        resolve_by_name_ids(state, BaseItemKind::MusicArtist, query.artists.as_deref()).await?
    {
        internal.artist_ids = ids;
    }
    if let Some(ids) = resolve_album_ids(state, query.albums.as_deref()).await? {
        internal.album_ids = ids;
    }
    if let Some(ids) =
        resolve_by_name_ids(state, BaseItemKind::Studio, query.studios.as_deref()).await?
    {
        internal.studio_ids = ids;
    }
    Ok(())
}

/// An official rating parameter as a parental-rating score:
/// `!string.IsNullOrWhiteSpace(rating) ? _localization.GetRatingScore(rating)`
/// with no country code, so `None` for a blank or unrecognized rating.
fn official_rating_score(
    state: &AppState,
    rating: Option<&str>,
) -> Option<ferrofin_model::entities_media::ParentalRatingScore> {
    rating
        .filter(|rating| !rating.trim().is_empty())
        .and_then(|rating| state.localization.get_rating_score(rating, None))
}

/// Convert HTTP query fields once, retaining the resolved user for DTO projection.
fn items_query_options(
    query: &ItemsQuery,
    user: Option<UserEntity>,
) -> Result<InternalItemsQuery, ApiError> {
    Ok(InternalItemsQuery {
        user,
        parent_id: query.parent_id.unwrap_or_default(),
        start_index: query.start_index,
        limit: query.limit,
        recursive: query.recursive.unwrap_or(false),
        collapse_box_set_items: query.collapse_box_set_items,
        apply_folder_display_options: true,
        search_term: query.search_term.clone(),
        include_item_types: parse_csv_enums_lenient(query.include_item_types.as_deref()),
        exclude_item_types: parse_csv_enums_lenient(query.exclude_item_types.as_deref()),
        media_types: parse_csv_enums_lenient(query.media_types.as_deref()),
        image_types: parse_csv_enums_lenient(query.image_types.as_deref()),
        order_by: parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref()),
        item_ids: parse_csv_uuids(query.ids.as_deref())?,
        exclude_item_ids: parse_csv_uuids(query.exclude_item_ids.as_deref())?,
        genres: parse_pipe_strings(query.genres.as_deref()),
        tags: parse_pipe_strings(query.tags.as_deref()),
        official_ratings: parse_pipe_strings(query.official_ratings.as_deref()),
        years: parse_csv_i32(query.years.as_deref())?,
        genre_ids: parse_csv_uuids(query.genre_ids.as_deref())?,
        studio_ids: parse_csv_uuids(query.studio_ids.as_deref())?,
        person_ids: parse_csv_uuids(query.person_ids.as_deref())?,
        artist_ids: parse_csv_uuids(query.artist_ids.as_deref())?,
        exclude_artist_ids: parse_csv_uuids(query.exclude_artist_ids.as_deref())?,
        album_artist_ids: parse_csv_uuids(query.album_artist_ids.as_deref())?,
        contributing_artist_ids: parse_csv_uuids(query.contributing_artist_ids.as_deref())?,
        album_ids: parse_csv_uuids(query.album_ids.as_deref())?,
        is_favorite: query.is_favorite,
        is_played: query.is_played,
        is_movie: query.is_movie,
        is_series: query.is_series,
        is_4k: query.is_4k,
        is_hd: query.is_hd,
        is_3d: query.is_3d,
        is_missing: query.is_missing,
        is_unaired: query.is_unaired,
        is_news: query.is_news,
        is_kids: query.is_kids,
        is_sports: query.is_sports,
        min_width: query.min_width,
        max_width: query.max_width,
        min_height: query.min_height,
        max_height: query.max_height,
        min_date_last_saved: query.min_date_last_saved,
        min_date_last_saved_for_user: query.min_date_last_saved_for_user,
        is_locked: query.is_locked,
        has_overview: query.has_overview,
        has_imdb_id: query.has_imdb_id,
        has_tmdb_id: query.has_tmdb_id,
        has_tvdb_id: query.has_tvdb_id,
        has_parental_rating: query.has_parental_rating,
        has_official_rating: query.has_official_rating,
        series_statuses: parse_csv_enums_lenient(query.series_status.as_deref()),
        person_types: parse_csv_strings(query.person_types.as_deref()),
        is_place_holder: query.is_place_holder,
        video_types: parse_csv_enums_lenient(query.video_types.as_deref()),
        has_subtitles: query.has_subtitles,
        has_trailer: query.has_trailer,
        has_special_feature: query.has_special_feature,
        has_theme_song: query.has_theme_song,
        has_theme_video: query.has_theme_video,
        index_number: query.index_number,
        parent_index_number: query.parent_index_number,
        min_community_rating: query.min_community_rating,
        min_critic_rating: query.min_critic_rating,
        min_premiere_date: query.min_premiere_date,
        max_premiere_date: query.max_premiere_date,
        adjacent_to: query.adjacent_to,
        name_starts_with: http_name_boundary(query.name_starts_with.as_deref()),
        name_starts_with_or_greater: http_name_boundary(
            query.name_starts_with_or_greater.as_deref(),
        ),
        name_less_than: http_name_boundary(query.name_less_than.as_deref()),
        person: query.person.clone(),
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        audio_languages: parse_csv_strings(query.audio_languages.as_deref()),
        subtitle_languages: parse_csv_strings(query.subtitle_languages.as_deref()),
        ..InternalItemsQuery::default()
    })
}

/// The media-stream filter rules `ItemsController.GetItems` applies before
/// `ApplyFilters` (v12 ItemsController.cs:462-487).
///
/// A specific subtitle language already implies subtitles exist, so
/// `hasSubtitles=true` is dropped beside it; `hasSubtitles=false` contradicts
/// it, so the language list is dropped instead. Every filter that reads
/// media streams then sets `IncludeOwnedItems`, "to include alternative and
/// linked versions" — the alternate versions and owned non-extras a general
/// query hides stay in, and the `PresentationUniqueKey` grouping folds them
/// back onto their primary.
pub(crate) fn apply_media_stream_filter_rules(internal: &mut InternalItemsQuery) {
    if !internal.subtitle_languages.is_empty() {
        match internal.has_subtitles {
            Some(true) => internal.has_subtitles = None,
            Some(false) => internal.subtitle_languages.clear(),
            None => {}
        }
    }
    if internal.has_subtitles.is_some()
        || !internal.subtitle_languages.is_empty()
        || !internal.audio_languages.is_empty()
        || internal.is_3d.is_some()
        || internal.is_hd.is_some()
        || internal.is_4k.is_some()
        || !internal.video_types.is_empty()
    {
        internal.include_owned_items = true;
    }
}

/// Query parameters for `GET /Items/{itemId}`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// `GET /Items/{itemId}` — a single item by id.
///
/// Port of `UserLibraryController.GetItem`. A missing item (or user) is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item returned (BaseItemDto)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = if item_id.is_nil() {
        state
            .library
            .get_user_root_folder()
            .await?
            .ok_or_else(|| ApiError::NotFound("user root folder".into()))?
    } else {
        let item = require_visible_item(&state, item_id, Some(&user)).await?;
        // A refresh that finished rewrote the row: answer with what it found.
        // Upstream refreshes the object it already checked and builds the DTO
        // from it, so the access check is not asked again.
        if refresh_on_demand_if_needed(&state, item_id, &item).await? {
            state.library.get_item_by_id(item_id).await?.unwrap_or(item)
        } else {
            item
        }
    };
    // A single-item fetch backs a detail page, so return the full DTO (overview,
    // genres, people, studios, tags, …) — the field-gated data jellyfin-web's
    // detail view needs. Port of `UserLibraryController.GetItem`, which builds a
    // `DtoOptions` with all fields.
    let options = DtoOptions::with_all_fields(true);
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, Some(&user), None)
        .await?;
    Ok(Json(dto))
}

/// `UserLibraryController.RefreshOnDemandTimeout`: how long a page view waits
/// for a person's on-demand refresh.
const REFRESH_ON_DEMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long ago a person must have been refreshed for its page view to
/// refresh it again (C# `(DateTime.UtcNow - item.DateLastRefreshed).TotalDays
/// < 3` skips it).
const REFRESH_ON_DEMAND_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(3);

/// The persons whose on-demand refresh is running. A page view of one of
/// them starts no second refresh; upstream's would run its own beside it.
static PERSON_REFRESHES: Mutex<BTreeSet<Uuid>> = Mutex::new(BTreeSet::new());

/// A person's place in [`PERSON_REFRESHES`], given up when its refresh ends.
struct PersonRefresh(Uuid);

impl PersonRefresh {
    /// Claims `item_id`, or `None` while a refresh of it is running.
    fn claim(item_id: Uuid) -> Option<Self> {
        let mut running = PERSON_REFRESHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // `then`, not `then_some`: a claim built and dropped here, as an
        // eager one is when the person is taken, would lock the set again.
        running.insert(item_id).then(|| Self(item_id))
    }
}

impl Drop for PersonRefresh {
    fn drop(&mut self) {
        PERSON_REFRESHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Port of `UserLibraryController.RefreshOnDemandIfNeeded`: a person shown
/// without an overview or a primary image, and not refreshed for three days,
/// is refreshed in full (metadata and images, saved) before its page is
/// answered, for at most [`REFRESH_ON_DEMAND_TIMEOUT`].
///
/// The refresh runs on a task of its own, which the page only waits for.
/// Upstream cancels the refresh when the timeout fires or the client hangs
/// up, and on the timeout queues a new one at high priority. Dropping the
/// refresh part way here could leave its image files written without their
/// rows, or its row saved without its provider ids, so a page that stops
/// waiting leaves the refresh to finish instead, with no second one queued.
/// While it runs, another view of the person is answered at once, without
/// starting another ([`PERSON_REFRESHES`]). The task logs a failure itself
/// only when no page is left to report it.
///
/// Returns whether the refresh finished in time, so the caller re-reads the
/// row it rewrote.
async fn refresh_on_demand_if_needed(
    state: &AppState,
    item_id: Uuid,
    item: &BaseItemEntity,
) -> Result<bool, ApiError> {
    if BaseItemKind::from_stored_type_name(&item.type_) != Some(BaseItemKind::Person) {
        return Ok(false);
    }
    let refreshed_lately = item
        .date_last_refreshed
        .is_some_and(|at| Utc::now() - at < REFRESH_ON_DEMAND_AFTER);
    if refreshed_lately {
        return Ok(false);
    }
    let has_overview = item
        .overview
        .as_deref()
        .is_some_and(|overview| !overview.trim().is_empty());
    let has_metadata = has_overview
        && state
            .library
            .get_item_images(item_id)
            .await?
            .iter()
            .any(|image| image.image_type == ImageType::Primary);
    if has_metadata {
        return Ok(false);
    }
    let Some(claim) = PersonRefresh::claim(item_id) else {
        return Ok(false);
    };
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        force_save: true,
        ..MetadataRefreshOptions::default()
    };
    let providers = Arc::clone(&state.providers);
    let (outcome, waiting) = tokio::sync::oneshot::channel();
    // Background work starts its own trace (LOGGING.md §4).
    let span = tracing::info_span!(
        parent: None,
        "person_refresh",
        item_id = %item_id,
        trigger = "item_page"
    );
    tokio::spawn(tracing::Instrument::instrument(
        async move {
            let _claim = claim;
            let refreshed = providers.refresh_single_item(item_id, &options).await;
            // A provider's failure is counted inside the refresh, not
            // returned, so an error here is the store's: the queued
            // refresh's `warn!` ("queued metadata refresh failed").
            if let Err(Err(error)) = outcome.send(refreshed) {
                tracing::warn!(%error, "on-demand person refresh failed");
            }
        },
        span,
    ));
    match tokio::time::timeout(REFRESH_ON_DEMAND_TIMEOUT, waiting).await {
        Ok(Ok(refreshed)) => {
            refreshed?;
            Ok(true)
        }
        Ok(Err(_ended)) => Err(ApiError::Service(
            ferrofin_traits::error::ServiceError::Backend("person refresh task panicked".into()),
        )),
        Err(_elapsed) => {
            tracing::debug!(
                %item_id,
                "person refresh still running; answering the page without it"
            );
            Ok(false)
        }
    }
}

/// `LibraryController.DeleteItem`'s gate (`LibraryController.cs:386-389` at
/// master `96bca6f0bd`, and `:432-435` in `DeleteItems`): `if (user is not
/// null && !item.CanDelete(user)) return Unauthorized("Unauthorized access")`.
///
/// `CanDelete(user)` is asked of the DTO service — the very value it puts in
/// the DTO's `CanDelete`, which is what jellyfin-web shows its "Delete" entry
/// by — so the menu and the endpoint cannot disagree. Its inputs are the
/// user's "Allow media deletion" / "Allow media deletion from" settings
/// (`EnableContentDeletion` / `EnableContentDeletionFromFolders`), collection
/// management for a collection, and ownership for a playlist. An API key has
/// no user and is not asked, as upstream.
async fn require_can_delete(
    state: &AppState,
    auth: &AuthorizationInfo,
    item: &BaseItemEntity,
) -> Result<(), ApiError> {
    let Some(user) = auth.user.as_ref() else {
        return Ok(());
    };
    if state.dto.can_delete(item, user).await? {
        Ok(())
    } else {
        Err(ApiError::Unauthorized("Unauthorized access".to_owned()))
    }
}

/// What both delete endpoints ask of the library: Jellyfin's
/// `new DeleteOptions { DeleteFileLocation = true }` — the item's files go
/// with it.
fn delete_with_files() -> DeleteOptions {
    DeleteOptions {
        delete_file_location: true,
        ..DeleteOptions::default()
    }
}

/// `LibraryController.DeleteItem`'s body for one id, after the user check:
/// resolve the item as the user (`404`), ask `CanDelete(user)` (`401`),
/// delete it with its files.
///
/// Resolve visibility before CanDelete: a hidden item is indistinguishable
/// from a missing one, even when the caller may delete from every library
/// (`_libraryManager.GetItemById<BaseItem>(itemId, user)`,
/// `LibraryController.cs:380,425`).
async fn delete_one(state: &AppState, auth: &AuthorizationInfo, id: Uuid) -> Result<(), ApiError> {
    let item = state
        .library
        .get_item_by_id_for_user(id, auth.user.as_ref())
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {id}")))?;
    require_can_delete(state, auth, &item).await?;
    state.library.delete_item(id, &delete_with_files()).await?;
    // A recording is this item upstream, so `/LiveTv/Recordings` ends with it.
    // The delete has happened, so a failure here does not undo that answer.
    let path = item.path.as_deref().filter(|path| !path.is_empty());
    if let (Some(live_tv), Some(path)) = (state.live_tv.as_ref(), path)
        && let Err(error) = live_tv.forget_recordings(path).await
    {
        tracing::warn!(item_id = %id, %error, "could not forget a deleted item's recordings");
    }
    Ok(())
}

/// Runs a delete request's work to completion even if the client goes away.
///
/// axum drops a handler's future when the connection closes, which could
/// stop a delete between its files and its rows, or half way through its
/// paths. The row delete itself is one transaction, but the `LibraryChanged`
/// notice that follows its commit would be skipped, and a
/// `DELETE /Items?ids=` would stop between two items. Upstream's controller
/// does not observe `RequestAborted` here, so its delete finishes; the work
/// runs on its own task, which the request only awaits.
async fn run_to_completion<F>(work: F) -> Result<(), ApiError>
where
    F: std::future::Future<Output = Result<(), ApiError>> + Send + 'static,
{
    // The request awaits the task, so its logs and spans stay under the
    // request's (LOGGING.md §4: a spawned future carries its span explicitly).
    tokio::spawn(tracing::Instrument::in_current_span(work))
        .await
        .map_err(|e| {
            ApiError::Service(ferrofin_traits::error::ServiceError::Backend(format!(
                "delete task failed: {e}"
            )))
        })?
}

/// `DELETE /Items/{itemId}` — deletes one item from the library and the
/// filesystem.
///
/// Port of `LibraryController.DeleteItem`. A token whose user no longer
/// exists, or a missing or hidden item, is a `404`; a user who may not
/// delete the item (`CanDelete(user)`) is a `401`. Otherwise the item,
/// everything under it and its files are deleted (`LibraryManager.DeleteItem`
/// with `DeleteFileLocation = true`) and the handler returns `204`. When the
/// item's first path cannot be deleted — a read-only mount — the request
/// fails (`500`) with its rows in place. The work finishes even if the
/// client disconnects.
#[utoipa::path(
    delete,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Item deleted"),
        (status = 401, description = "Unauthorized access"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn delete_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // `if (user is null && !isApiKey) return NotFound();`
    if auth.user.is_none() && !auth.is_api_key {
        return Err(ApiError::NotFound(format!("user {}", auth.user_id())));
    }
    run_to_completion(async move { delete_one(&state, &auth, item_id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `DELETE /Items`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteItemsQuery {
    /// Comma-delimited ids of the items to delete.
    #[serde(default)]
    ids: Option<String>,
}

/// `DELETE /Items` — deletes several items by id, with their files.
///
/// Port of `LibraryController.DeleteItems`, which is **per item, in order**,
/// not all-or-nothing: each id is resolved (`404` if missing), checked
/// (`401` if the user may not delete it) and deleted before the next is
/// looked at, so the ids before the first refused or missing one are
/// already deleted when the request fails. The loop finishes even if the
/// client disconnects, as upstream's does.
#[utoipa::path(
    delete,
    path = "/Items",
    responses(
        (status = 204, description = "Items deleted"),
        (status = 401, description = "Unauthorized access"),
        (status = 404, description = "An item was not found")
    ),
    tag = "ferrofin"
)]
async fn delete_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<DeleteItemsQuery>,
) -> Result<StatusCode, ApiError> {
    // `if (!isApiKey && user is null) return Unauthorized("Unauthorized access");`
    if auth.user.is_none() && !auth.is_api_key {
        return Err(ApiError::Unauthorized("Unauthorized access".to_owned()));
    }
    let ids = parse_csv_uuids(query.ids.as_deref())?;
    run_to_completion(async move {
        for id in ids {
            delete_one(&state, &auth, id).await?;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `GET /Items/Counts`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountsQuery {
    /// Optional user whose library the counts are scoped to.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Optional favourite-only filter.
    #[serde(default)]
    is_favorite: Option<bool>,
}

/// `GET /Items/Counts` — per-kind item counts.
///
/// Port of `LibraryController.GetItemCounts`.
#[utoipa::path(
    get,
    path = "/Items/Counts",
    responses((status = 200, description = "Item counts returned (ItemCounts)", body = ItemCounts)),
    tag = "ferrofin"
)]
async fn get_item_counts(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<CountsQuery>,
) -> Result<Json<ItemCounts>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let internal = InternalItemsQuery {
        recursive: true,
        is_virtual_item: Some(false),
        is_favorite: query.is_favorite,
        user,
        ..InternalItemsQuery::default()
    };
    let counts = state.library.get_item_counts(&internal).await?;
    Ok(Json(counts))
}

/// Query parameters for `GET /Items/{itemId}/Ancestors`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AncestorsQuery {
    /// Optional user to scope visibility and attach user data.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// The stored `BaseItems.Type` short name of a row (`AggregateFolder`,
/// `CollectionFolder`, …).
fn short_type(item: &BaseItemEntity) -> &str {
    item.type_.rsplit('.').next().unwrap_or(&item.type_)
}

/// The children of the `UserRootFolder` that are the aggregate's VIRTUAL
/// children rather than its own rows — the plug-in folders
/// `LibraryManager.CreateRootFolder` registers with `AddVirtualChild`
/// (v12.2 LibraryManager.cs:1146, the only call site: the playlists folder).
///
/// Named as a set because that is what it is in the C#: they are members of the
/// candidate list `TranslateParentItem` searches, and their
/// `BaseItem.PhysicalLocations` is `[Path]` (BaseItem.cs:450-461), so the search
/// matches such a folder against ITSELF.
const AGGREGATE_VIRTUAL_CHILD_TYPES: [&str; 3] = [
    "PlaylistsFolder",
    "ManualPlaylistsFolder",
    "BasePluginFolder",
];

/// Port of `LibraryController.TranslateParentItem` (LibraryController.cs:959-966):
///
/// ```text
/// item.GetParent() is AggregateFolder
///     ? _libraryManager.GetUserRootFolder().GetChildren(user, true)
///         .FirstOrDefault(i => i.PhysicalLocations.Contains(item.Path))
///     : item
/// ```
///
/// A hop whose parent is NOT the aggregate passes through untouched. A hop under
/// the aggregate is looked up in the user root's children — and `GetChildren`
/// there is the concatenated set (`UserRootFolder.cs:96-102`), so the candidates
/// are BOTH groups:
///
/// 1. the user root's own children, the `CollectionFolder` views, whose
///    `PhysicalLocations` are the library's configured locations — a physical
///    library root under the aggregate (as in a database scanned by Jellyfin) is
///    therefore shown as the view containing it;
/// 2. the aggregate's virtual children, whose `PhysicalLocations` is `[Path]` —
///    such a folder matches itself, so the walk continues THROUGH it. Ferrofin
///    used to leave that group out of the candidate set entirely, and a
///    playlist's ancestor chain came back EMPTY where Jellyfin answers
///    `[Playlists, root]`.
///
/// The two groups are disjoint by construction (a library location is never the
/// plug-in folder's own `{data}/playlists` path), so group 2 is tested first —
/// it needs no query.
///
/// `None` is the C# `FirstOrDefault` miss, which ends the walk.
async fn translate_parent_item(
    state: &AppState,
    item: &BaseItemEntity,
    grandparent: Option<&BaseItemEntity>,
) -> Result<Option<BaseItemEntity>, ApiError> {
    if grandparent.is_none_or(|g| short_type(g) != "AggregateFolder") {
        return Ok(Some(item.clone()));
    }
    let Some(path) = item.path.as_deref() else {
        return Ok(None);
    };
    // Candidate group 2: `PhysicalLocations == [Path]`, so `Contains(item.Path)`
    // is true of the item itself.
    if AGGREGATE_VIRTUAL_CHILD_TYPES.contains(&short_type(item)) {
        return Ok(Some(item.clone()));
    }
    // Candidate group 1: the `CollectionFolder` views.
    let folders = state.virtual_folders.library_folders().await?;
    // `GetChildren(user, true).FirstOrDefault(…)`: the first by `SortName`,
    // not by the listing, which comes in the file system's order.
    let Some(view_id) = folders
        .iter()
        .filter(|vf| vf.locations.iter().any(|loc| loc == path))
        .min_by_key(|vf| {
            ferrofin_util::sort_name::create_sort_name(vf.name.as_deref().unwrap_or_default())
        })
        .and_then(|vf| vf.item_id.as_deref())
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return Ok(None);
    };
    Ok(state.library.get_item_by_id(view_id).await?)
}

/// `GET /Items/{itemId}/Ancestors` — an item's parents, nearest first.
///
/// Port of `LibraryController.GetAncestors`: walks `GetParent()` from the
/// item, translating each hop through [`translate_parent_item`] when a user
/// is in scope (a physical root becomes the user's view, whose own parent is
/// the `UserRootFolder`), and stops at the first hop that translates to
/// nothing. A missing item is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Ancestors",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Ancestors returned (Vec<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_ancestors(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<AncestorsQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    require_visible_item(&state, item_id, user.as_ref()).await?;
    let mut chain = state
        .library
        .get_ancestors(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let mut ancestors: Vec<BaseItemEntity> = Vec::with_capacity(chain.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut index = 0;
    while let Some(parent) = chain.get(index) {
        let parent = if user.is_some() {
            match translate_parent_item(&state, parent, chain.get(index + 1)).await? {
                Some(translated) => translated,
                None => break,
            }
        } else {
            parent.clone()
        };
        if !seen.insert(parent.id.to_ascii_uppercase()) {
            break;
        }
        // A translated hop re-roots the walk: the view's parents (the user
        // root), not the physical folder's (the aggregate root), come next.
        let translated = !parent.id.eq_ignore_ascii_case(&chain[index].id);
        let translated_id = Uuid::parse_str(&parent.id).ok();
        ancestors.push(parent);
        if translated && let Some(id) = translated_id {
            chain = state.library.get_ancestors(id).await?.unwrap_or_default();
            index = 0;
        } else {
            index += 1;
        }
    }
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&ancestors, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(dtos))
}

/// Query parameters for `GET /UserItems/Resume`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The index of the first item to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of items to return.
    #[serde(default)]
    limit: Option<i32>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`MediaType`](ferrofin_model::data::MediaType) set.
    #[serde(default)]
    media_types: Option<String>,
    /// Comma-delimited kinds to exclude.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited kinds to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to
    /// populate on each row. Absent/empty ⇒ the base DTO.
    #[serde(default)]
    fields: Option<String>,
    /// Include image information in the output (C# default `true`).
    #[serde(default)]
    enable_images: Option<bool>,
    /// Include user data (unset ⇒ the `DtoOptions` default, `true`).
    #[serde(default)]
    enable_user_data: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType)s to
    /// include. Empty ⇒ every type, as upstream.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether to exclude the items the user's active sessions are playing
    /// (C# default `false`).
    #[serde(default)]
    exclude_active_sessions: Option<bool>,
}

impl ResumeQuery {
    /// `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(enableImages,
    /// enableUserData, imageTypeLimit, enableImageTypes)` (v12
    /// ItemsController.cs:947-949).
    fn dto_options(&self) -> DtoOptions {
        super::by_name::additional_dto_options(
            self.fields.as_deref(),
            self.enable_images,
            self.enable_user_data,
            self.image_type_limit,
            self.enable_image_types.as_deref(),
        )
    }
}

/// The ids of everything the user's active sessions are playing, each
/// expanded to every version of the video (v12 ItemsController.cs:963-976):
///
/// ```text
/// _sessionManager.Sessions
///     .Where(s => s.UserId.Equals(requestUserId) && s.NowPlayingItem is not null)
///     .SelectMany(s => GetItemById(s.NowPlayingItem.Id) is Video video
///         ? video.GetAllVersions().Select(v => v.Id) : [s.NowPlayingItem.Id])
/// ```
///
/// `NowPlayingItem.Id` is the displayed/primary id, but the resume query
/// surfaces the actually-played alternate version's own id, so an in-progress
/// alternate must be excluded too. `GetAllVersions()` spans the whole group
/// in both directions (Video.cs:845-884: the primary, its alternates, and —
/// for an alternate — the primary's other alternates). Ferrofin's alternates
/// hang off the primary through `PrimaryVersionId`, so each playing id is
/// first lifted to its group's primary, and the group is the primaries plus
/// every alternate under them.
async fn active_session_item_ids(state: &AppState, user_id: Uuid) -> Result<Vec<Uuid>, ApiError> {
    // C# reads the raw `_sessionManager.Sessions` list, which the summaries
    // are: no caller-visibility gate, no permission reads and no session DTO,
    // and the `s.UserId.Equals(requestUserId)` filter below is the
    // controller's own.
    let sessions = state.sessions.get_session_summaries().await?;
    let mut playing: Vec<Uuid> = sessions
        .iter()
        .filter(|s| s.user_id == user_id)
        .filter_map(|s| s.now_playing_item_id)
        .collect();
    playing.sort_unstable();
    playing.dedup();
    if playing.is_empty() {
        return Ok(playing);
    }
    // Up: an alternate's group root is its primary.
    let mut roots = playing.clone();
    for id in &playing {
        if let Some(primary) = state
            .library
            .get_item_by_id(*id)
            .await?
            .and_then(|row| row.primary_version_id)
            .and_then(|p| Uuid::parse_str(&p).ok())
            .filter(|p| !p.is_nil())
        {
            roots.push(primary);
        }
    }
    roots.sort_unstable();
    roots.dedup();
    // Down: every alternate under those primaries.
    let alternates = state
        .media_sources
        .get_alternate_versions_batch(&roots)
        .await?;
    let mut ids = roots;
    ids.extend(
        alternates
            .values()
            .flatten()
            .filter_map(|alt| Uuid::parse_str(&alt.id).ok()),
    );
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// `GET /UserItems/Resume` — the user's resumable (in-progress) items.
///
/// Port of `ItemsController.GetResumeItems` (v12 ItemsController.cs:920-1005):
/// date-played-descending, resumable, non-virtual, recursive, owned items
/// included, box-set collapsing off. An unscoped request (`parentId` empty)
/// with `LatestItemExcludes` set is confined to the user's remaining root
/// folders through `AncestorIds`, and `excludeActiveSessions` drops whatever
/// the user's sessions are currently playing (every version of it).
#[utoipa::path(
    get,
    path = "/UserItems/Resume",
    responses((status = 200, description = "Resumable items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_resume_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ResumeQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    let options = query.dto_options();

    // `var excludeFolderIds = user.GetPreferenceValues<Guid>(LatestItemExcludes);
    //  if (parentIdGuid.IsEmpty() && excludeFolderIds.Length > 0)
    //      ancestorIds = GetUserRootFolder().GetChildren(user, true)
    //          .Where(i => i is Folder).Where(i => !excludeFolderIds.Contains(i.Id))
    //          .Select(i => i.Id).ToArray();` (:952-961)
    let mut ancestor_ids: Vec<Uuid> = Vec::new();
    if query.parent_id.is_none_or(|id| id.is_nil()) {
        let exclude_folder_ids = state
            .users
            .get_user_dto(&user, None)
            .await?
            .configuration
            .map(|c| c.latest_items_excludes)
            .unwrap_or_default();
        if !exclude_folder_ids.is_empty() {
            // `GetUserRootFolder().GetChildren(user, true)` — the user-root
            // children the USER can see (`Folder.IsVisible`: blocked-/enabled-
            // folders), which is the repository's `user_root_children` branch
            // (the same query `tv_series_manager` runs for NextUp's parents).
            // `/Library/MediaFolders`' seam is the unfiltered list and would
            // let a restricted user resume into a library they cannot see.
            let children = state
                .library
                .query_items(&InternalItemsQuery {
                    user: Some(user.clone()),
                    user_root_children: true,
                    include_item_types: vec![
                        BaseItemKind::CollectionFolder,
                        BaseItemKind::UserView,
                        BaseItemKind::PlaylistsFolder,
                        BaseItemKind::ManualPlaylistsFolder,
                    ],
                    ..InternalItemsQuery::default()
                })
                .await?
                .items;
            ancestor_ids = children
                .iter()
                .filter(|folder| folder.is_folder)
                .filter_map(|folder| Uuid::parse_str(&folder.id).ok())
                .filter(|id| !exclude_folder_ids.contains(id))
                .collect();
        }
    }
    let exclude_item_ids = if query.exclude_active_sessions.unwrap_or(false) {
        active_session_item_ids(&state, user_id).await?
    } else {
        Vec::new()
    };

    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let mut internal = InternalItemsQuery {
        user: Some(user),
        start_index: query.start_index,
        limit: query.limit,
        recursive: true,
        is_resumable: Some(true),
        is_virtual_item: Some(false),
        collapse_box_set_items: Some(false),
        include_owned_items: true,
        search_term: query.search_term.clone(),
        media_types: parse_csv_enums_lenient(query.media_types.as_deref()),
        include_item_types: parse_csv_enums_lenient(query.include_item_types.as_deref()),
        exclude_item_types: parse_csv_enums_lenient(query.exclude_item_types.as_deref()),
        order_by: vec![(ItemSortBy::DatePlayed, SortOrder::Descending)],
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        ancestor_ids,
        exclude_item_ids,
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }

    let result = state.library.query_items(&internal).await?;
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, internal.user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(
        Some(result.start_index),
        Some(result.total_record_count),
        dtos,
    )))
}

/// Parses a comma-delimited list of `i32` values (Jellyfin's `years`).
fn parse_csv_i32(raw: Option<&str>) -> Result<Vec<i32>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i32>()
                .map_err(|_| ApiError::BadRequest(format!("invalid integer {s:?}")))
        })
        .collect()
}

/// Builds the `order_by` list from parallel `sort_by`/`sort_order` lists.
///
/// Mirrors `RequestHelpers.GetOrderBy`: each sort column is paired with the
/// order at the same index, falling back to the last supplied order (then
/// ascending) when fewer orders than columns are given.
pub(crate) fn parse_order_by(
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Vec<(
    ferrofin_model::live_tv::ItemSortBy,
    ferrofin_model::dto::SortOrder,
)> {
    use ferrofin_model::dto::SortOrder;
    let columns: Vec<ferrofin_model::live_tv::ItemSortBy> = parse_csv_enums_lenient(sort_by);
    let orders: Vec<SortOrder> = parse_csv_enums_lenient(sort_order);
    columns
        .into_iter()
        .enumerate()
        .map(|(i, column)| {
            let order = orders
                .get(i)
                // C# RequestHelpers.GetOrderBy pads missing orders with the
                // FIRST requested order, not the last.
                .or_else(|| orders.first())
                .copied()
                .unwrap_or(SortOrder::Ascending);
            (column, order)
        })
        .collect()
}

/// Whether a parent row is itself a container of the requested kind, so the
/// browse is a plain direct-children read and must NOT be re-rooted (C#
/// `item is not BoxSet/Playlist`, plus the playlists library itself).
///
/// Takes the row's **stored** short type name. That is the trap this guards:
/// 10.11.8 persists the playlists plugin folder as
/// `Emby.Server.Implementations.Playlists.PlaylistsFolder`, while
/// `ManualPlaylistsFolder` is only `PlaylistsFolder.GetClientTypeName()` — and
/// the spelling an older Ferrofin wrote into the column. Both have to be
/// recognised, or the Playlists-library browse jellyfin-web issues
/// (`GET /Items?parentId=<playlistsFolder>&includeItemTypes=Playlist`) falls
/// through to `linked_child_ancestor_ids` re-rooting on exactly the databases
/// that spell it the upstream way.
fn is_direct_children_browse(short_type_name: &str) -> bool {
    matches!(
        short_type_name,
        "BoxSet" | "Playlist" | "PlaylistsFolder" | "ManualPlaylistsFolder"
    )
}

/// ItemsController uses IsVisible on its explicit parent, not standalone
/// lookup. It returns 401 for a hidden parent, and API keys bypass this gate.
/// Container-only browses re-root to UserRootFolder before the upstream check.
/// Whether the parent is itself a library, and a `boxsets` one, is left in
/// `library` for the library browse defaults ([`apply_library_browse_defaults`]).
async fn check_parent_visibility(
    state: &AppState,
    auth: &AuthorizationInfo,
    query: &mut InternalItemsQuery,
    library: &mut Option<bool>,
) -> Result<(Option<BaseItemKind>, bool), ApiError> {
    use ferrofin_model::data::CollectionType;

    if query.parent_id.is_nil() {
        return Ok((None, true));
    }
    let parent = state
        .library
        .get_item_by_id(query.parent_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("Invalid parent id: {}", query.parent_id)))?;
    let kind = parent.type_.rsplit('.').next().unwrap_or(&parent.type_);
    let rerooted = matches!(
        query.include_item_types.as_slice(),
        [BaseItemKind::BoxSet | BaseItemKind::Playlist]
    ) && !matches!(kind, "BoxSet" | "Playlist");
    if !auth.is_api_key
        && kind != "UserRootFolder"
        && !rerooted
        && let Some(user) = query.user.as_ref()
        && !state.library.is_item_visible(&parent, user).await?
    {
        return Err(ApiError::Unauthorized("Unauthorized access".into()));
    }
    let kind_from = |row: &ferrofin_db::entities::base_items::BaseItemEntity| {
        serde_json::from_value(serde_json::Value::String(
            row.type_
                .rsplit('.')
                .next()
                .unwrap_or(&row.type_)
                .to_owned(),
        ))
        .ok()
    };
    // Folder.GetItems dispatches explicit IDs straight to the repository,
    // before GetItemsInternal/UserViewBuilder can alter kinds or recursion.
    // The controller still checks the actual parent before that dispatch,
    // then clears query.Parent after the container redirect below.
    if !query.item_ids.is_empty() {
        return Ok((None, false));
    }
    let is_channel = |row: &ferrofin_db::entities::base_items::BaseItemEntity| {
        row.type_.rsplit('.').next() == Some("Channel")
            || row
                .channel_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some_and(|id| !id.is_nil())
    };
    if kind != "UserView" {
        *library = library_is_boxsets(&parent);
        return Ok((kind_from(&parent), !is_channel(&parent)));
    }

    // UserViewBuilder's movie/TV recursive base views call QueryRecursive;
    // folders/direct repository/subnavigation builders do not auto-collapse.
    // ViewType belongs to the parent row already read for access, including
    // adopted numeric values; it is never a new client preference.
    let (view_type, parent_id) = user_view_browse_context(&parent);
    let direct = user_view_uses_direct_items(view_type);
    if direct {
        // These simple direct builders (`GetMovieMovies`, `GetTvSeries`,
        // `GetBooks`) set their kinds and recursive mode regardless of the
        // request, then forward explicit collapse to SQL. Their automatic
        // global grouping policy is intentionally absent.
        if let Some(kinds) = match view_type {
            Some(CollectionType::Moviemovies) => Some(vec![BaseItemKind::Movie]),
            Some(CollectionType::Tvshowseries) => Some(vec![BaseItemKind::Series]),
            Some(CollectionType::books) => Some(vec![BaseItemKind::Book, BaseItemKind::AudioBook]),
            _ => None,
        } {
            query.include_item_types = kinds;
            query.recursive = true;
            query.user_root_children = false;
        }
        return Ok((kind_from(&parent), false));
    }
    if matches!(
        view_type,
        Some(CollectionType::movies | CollectionType::tvshows)
    ) {
        if !query.recursive {
            return Ok((kind_from(&parent), false));
        }
        query.folder_query_recursive = true;
        if query.include_item_types.is_empty() {
            query.include_item_types = if view_type == Some(CollectionType::movies) {
                vec![BaseItemKind::Movie]
            } else {
                vec![
                    BaseItemKind::Series,
                    BaseItemKind::Season,
                    BaseItemKind::Episode,
                ]
            };
        }
        if let Some(parent_id) = parent_id
            && let Some(query_parent) = state.library.get_item_by_id(parent_id).await?
        {
            return Ok((kind_from(&query_parent), true));
        }
        return Ok((kind_from(&parent), true));
    }
    // The default builder delegates to its actual display/relational parent
    // when one exists; an unparented UserView uses GetResult over media-folder
    // children and has no automatic grouping policy.
    if let Some(parent_id) = parent_id
        && let Some(query_parent) = state.library.get_item_by_id(parent_id).await?
    {
        if !query.recursive || kind_from(&query_parent) == Some(BaseItemKind::Season) {
            // A default view delegates to that Season's GetItemsInternal even
            // for recursive requests; its aired selection needs the real row.
            query.parent_id = parent_id;
            query.user_root_children = false;
        }
        return Ok((kind_from(&query_parent), !is_channel(&query_parent)));
    }
    Ok((kind_from(&parent), false))
}

/// Read UserView dispatch data from the parent already loaded for visibility.
fn user_view_browse_context(
    parent: &BaseItemEntity,
) -> (Option<ferrofin_model::data::CollectionType>, Option<Uuid>) {
    use ferrofin_model::data::CollectionType;
    let data: serde_json::Value = parent
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    let view_type = data
        .get("ViewType")
        .cloned()
        .and_then(|value| serde_json::from_value::<CollectionType>(value).ok());
    let parent_id = data
        .get("DisplayParentId")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .filter(|id| !id.is_nil())
        .or_else(|| {
            parent
                .parent_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|id| !id.is_nil())
        });
    (view_type, parent_id)
}

/// When the requested parent is itself a library (`ICollectionFolder`),
/// whether it is a `boxsets` one, from its `Data.CollectionType` read
/// case-insensitively as the other readers do; `None` for any other row.
fn library_is_boxsets(parent: &BaseItemEntity) -> Option<bool> {
    (parent.type_.rsplit('.').next() == Some("CollectionFolder")).then(|| {
        parent
            .data
            .as_deref()
            .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .and_then(|data| {
                data.get("CollectionType")?
                    .as_str()
                    .map(|kind| kind.eq_ignore_ascii_case("boxsets"))
            })
            .unwrap_or(false)
    })
}

/// These UserView builders use direct item selection rather than Folder policy.
fn user_view_uses_direct_items(view_type: Option<ferrofin_model::data::CollectionType>) -> bool {
    use ferrofin_model::data::CollectionType;
    matches!(
        view_type,
        Some(
            CollectionType::folders
                | CollectionType::books
                | CollectionType::Tvshowseries
                | CollectionType::Tvgenres
                | CollectionType::Tvgenre
                | CollectionType::Tvlatest
                | CollectionType::Tvnextup
                | CollectionType::Tvresume
                | CollectionType::Tvfavoriteseries
                | CollectionType::Tvfavoriteepisodes
                | CollectionType::Movielatest
                | CollectionType::Movieresume
                | CollectionType::Moviemovies
                | CollectionType::Moviecollection
                | CollectionType::Moviefavorites
                | CollectionType::Moviegenres
                | CollectionType::Moviegenre
        )
    )
}

/// Re-roots a BoxSet/Playlist-typed browse onto the user root (port of
/// `ItemsController.GetItems`'s `linkedChildAncestorIds` block): from a normal
/// library parent with a linked-child-ancestor constraint, from the Playlists
/// view without one (its collection type is the target's own).
///
/// Applies only when the request is for exactly `[BoxSet]` or `[Playlist]`,
/// a parent is set, and that parent is neither itself a box set/playlist (nor,
/// for a playlist browse, the playlists folder) nor the matching `boxsets`
/// library. A re-rooted query takes the user root's folder policy. Best-effort:
/// an unresolvable parent leaves the query untouched (the plain parent scoping
/// then returns empty, as before).
async fn redirect_container_browse(
    state: &AppState,
    internal: &mut ferrofin_traits::options::InternalItemsQuery,
) {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::CollectionTypeOptions;

    // Playlists have no `CollectionTypeOptions` variant (upstream's options
    // enum has none either — the playlists "library" is the manual playlists
    // folder, caught by the container check below), so its target type is None.
    let target_collection_type = match internal.include_item_types.as_slice() {
        [BaseItemKind::BoxSet] => Some(CollectionTypeOptions::boxsets),
        [BaseItemKind::Playlist] => None,
        _ => return,
    };
    if internal.parent_id.is_nil() {
        return;
    }
    let Ok(Some(parent)) = state.library.get_item_by_id(internal.parent_id).await else {
        return;
    };
    // The parent is itself a container of the requested kind → a direct
    // children browse, no re-rooting (C# `item is not BoxSet/Playlist`).
    let short = parent.type_.rsplit('.').next().unwrap_or(&parent.type_);
    // The playlists folder only answers a playlist browse; a box-set browse
    // of it re-roots like any other parent.
    if is_direct_children_browse(short)
        && (target_collection_type.is_none() || matches!(short, "BoxSet" | "Playlist"))
    {
        return;
    }
    // The Playlists view's collection type is `playlists`, the target's own,
    // so a playlist browse of it re-roots with no linked-ancestor constraint.
    if target_collection_type.is_none()
        && short == "UserView"
        && user_view_browse_context(&parent).0
            == Some(ferrofin_model::data::CollectionType::playlists)
    {
        reroot_to_user_root(internal);
        return;
    }
    // A browse of the boxsets/playlists library itself keeps plain parent
    // scoping (C# `itemCollectionType != targetCollectionType`).
    let parent_collection_type =
        state
            .virtual_folders
            .library_folders()
            .await
            .ok()
            .and_then(|folders| {
                folders
                    .iter()
                    .find(|vf| {
                        vf.item_id
                            .as_deref()
                            .is_some_and(|id| id.eq_ignore_ascii_case(&parent.id))
                    })
                    .and_then(|vf| vf.collection_type)
            });
    if target_collection_type.is_some() && parent_collection_type == target_collection_type {
        return;
    }
    internal.linked_child_ancestor_ids = vec![internal.parent_id];
    reroot_to_user_root(internal);
}

/// The re-rooted folder is the user root, which always takes Folder's policy
/// (and with it the per-user container post-filter), whatever the original
/// parent's was.
fn reroot_to_user_root(internal: &mut ferrofin_traits::options::InternalItemsQuery) {
    internal.set_parent(None);
    internal.apply_folder_display_options = true;
}

/// Registers this controller's real routes onto `router`.
///
/// The bare `/Items/{itemId}` slot carries `GET`/`DELETE` (this controller) and
/// `POST` (the item-update controller) on one shared `MethodRouter`, since axum
/// rejects a duplicate method+path registered across two `route` calls.
/// `GET /Users/{userId}/Items` — the path-scoped form of `GET /Items`.
///
/// These `/Users/{userId}/Items…` forms aren't in the 10.11 OpenAPI contract
/// (which prefers `/Items?userId=`), but jellyfin-web's bundled `jellyfin-apiclient`
/// still calls them for core screens (home rows, the item/metadata views), so they
/// are required — a `404` here breaks those screens. Each injects the path
/// `userId` into the query and forwards to the query-scoped handler.
async fn get_items_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    Query(mut query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    // The obsolete route passes `audioLanguages: []` and
    // `subtitleLanguages: []` to `GetItems` (v12 ItemsController.cs:896-897):
    // it never filters by stream language.
    query.audio_languages = None;
    query.subtitle_languages = None;
    get_items(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/Resume` — path-scoped form of `GET /UserItems/Resume`
/// (the home screen's "Continue watching" row).
async fn get_resume_items_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    Query(mut query): Query<ResumeQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_resume_items(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}` — path-scoped form of `GET /Items/{itemId}`
/// (item detail + the library metadata editor).
async fn get_item_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    Query(mut query): Query<ItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    query.user_id = Some(user_id);
    get_item(state, auth, Path(item_id), Query(query)).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items", get(get_items).delete(delete_items))
        .route("/Users/{userId}/Items", get(get_items_for_user))
        .route(
            "/Users/{userId}/Items/Resume",
            get(get_resume_items_for_user),
        )
        .route("/Users/{userId}/Items/{itemId}", get(get_item_for_user))
        .route("/Items/Counts", get(get_item_counts))
        .route("/UserItems/Resume", get(get_resume_items))
        .route(
            "/Items/{itemId}",
            get(get_item)
                .delete(delete_item)
                .post(super::item_update::update_item),
        )
        .route("/Items/{itemId}/Ancestors", get(get_ancestors))
}
crate::query::query_parameters! {
    ItemsQuery {
        "includeItemTypes" => ',',
        "excludeItemTypes" => ',',
        "mediaTypes" => ',',
        "sortBy" => ',',
        "sortOrder" => ',',
        "filters" => ',',
        "imageTypes" => ',',
        "fields" => ',',
        "ids" => ',',
        "excludeItemIds" => ',',
        "genres" => '|',
        "tags" => '|',
        "officialRatings" => '|',
        "years" => ',',
        "genreIds" => ',',
        "studioIds" => ',',
        "personIds" => ',',
        "artistIds" => ',',
        "excludeArtistIds" => ',',
        "albumArtistIds" => ',',
        "contributingArtistIds" => ',',
        "albumIds" => ',',
        "videoTypes" => ',',
        "locationTypes" => ',',
        "excludeLocationTypes" => ',',
        "enableImageTypes" => ',',
        "audioLanguages" => ',',
        "subtitleLanguages" => ',',
        "seriesStatus" => ',',
        "personTypes" => ',',
        "albums" => '|',
        "artists" => '|',
        "studios" => '|',
    } => [("get", "/Items")];
    ItemQuery {} => [("get", "/Items/{itemId}")];
    DeleteItemsQuery {
        "ids" => ',',
    } => [("delete", "/Items")];
    CountsQuery {} => [("get", "/Items/Counts")];
    AncestorsQuery {} => [("get", "/Items/{itemId}/Ancestors")];
    ResumeQuery {
        "mediaTypes" => ',',
        "excludeItemTypes" => ',',
        "includeItemTypes" => ',',
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/UserItems/Resume")];
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The whitelist that counts is the one of the account the query is for.
    /// The request's policy is the caller's, so it answers only for the
    /// caller: an administrator or an API key naming `userId` gets that
    /// account's, in either direction.
    #[tokio::test]
    async fn the_tag_whitelist_comes_from_the_account_the_query_is_for() {
        use crate::test_support::{authed_fake_state_with_policy, fake_user_entity};
        use ferrofin_model::users::UserPolicy;

        let whitelist = || UserPolicy {
            allowed_tags: vec!["kids".to_owned()],
            ..UserPolicy::default()
        };
        let caller = fake_user_entity(Uuid::from_u128(1), "caller");
        let other = fake_user_entity(Uuid::from_u128(2), "other");
        let auth = |policy: Option<UserPolicy>| AuthorizationInfo {
            user: Some(caller.clone()),
            user_policy: policy.map(std::sync::Arc::new),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        };
        let answer = |stored: UserPolicy, auth: AuthorizationInfo, user: UserEntity| async move {
            whitelists_tags(&authed_fake_state_with_policy(stored), &auth, &user)
                .await
                .expect("no lookup fails")
        };

        // The caller: the request's policy, whatever the store would say.
        assert!(
            answer(
                UserPolicy::default(),
                auth(Some(whitelist())),
                caller.clone()
            )
            .await
        );
        assert!(
            !answer(
                whitelist(),
                auth(Some(UserPolicy::default())),
                caller.clone()
            )
            .await
        );
        // Another account: its stored policy, whatever the caller's says.
        assert!(
            !answer(
                UserPolicy::default(),
                auth(Some(whitelist())),
                other.clone()
            )
            .await
        );
        assert!(
            answer(
                whitelist(),
                auth(Some(UserPolicy::default())),
                other.clone()
            )
            .await
        );
        // No policy on the request (an alternate auth service, an API key).
        assert!(answer(whitelist(), auth(None), caller.clone()).await);
        let api_key = AuthorizationInfo {
            is_api_key: true,
            is_authenticated: true,
            ..AuthorizationInfo::default()
        };
        assert!(answer(whitelist(), api_key.clone(), other.clone()).await);
        assert!(!answer(UserPolicy::default(), api_key, other).await);
    }

    /// A browse under either spelling of the playlists folder becomes a
    /// recursive `[Playlist]` listing from the user root (so it is
    /// post-filtered per user); any other parent is left as asked.
    #[test]
    fn a_playlists_folder_browse_lists_its_playlists_recursively() {
        for (parent, rewritten) in [
            (Some(BaseItemKind::PlaylistsFolder), true),
            (Some(BaseItemKind::ManualPlaylistsFolder), true),
            (Some(BaseItemKind::CollectionFolder), false),
            (None, false),
        ] {
            let parent_id = Uuid::new_v4();
            let mut query = InternalItemsQuery {
                user: Some(crate::test_support::sample_user()),
                parent_id,
                parent_type: parent,
                include_item_types: vec![BaseItemKind::Audio],
                user_root_children: true,
                ..InternalItemsQuery::default()
            };
            list_playlists_folder_as_playlists(&mut query);
            assert_eq!(
                (
                    query.recursive,
                    query.include_item_types == [BaseItemKind::Playlist],
                    query.user_root_children,
                    // Every playlist the user may open, not only the folder's.
                    query.parent_id.is_nil(),
                ),
                (rewritten, rewritten, !rewritten, rewritten),
                "{parent:?}"
            );
            // With no user the folder keeps its own playlists only.
            let mut anonymous = InternalItemsQuery {
                parent_id,
                parent_type: parent,
                ..InternalItemsQuery::default()
            };
            list_playlists_folder_as_playlists(&mut anonymous);
            assert_eq!(anonymous.parent_id, parent_id, "{parent:?}");
        }
    }

    /// Both stored spellings of the playlists plugin folder are a direct-children
    /// browse. `PlaylistsFolder` is the FQN 10.11.8 persists;
    /// `ManualPlaylistsFolder` is `GetClientTypeName()`, which an older Ferrofin
    /// wrote into the column. Recognising only the latter re-rooted the
    /// Playlists-library browse on a fresh or adopted database.
    #[test]
    fn both_playlists_folder_spellings_are_a_direct_children_browse() {
        for short in [
            "BoxSet",
            "Playlist",
            "PlaylistsFolder",
            "ManualPlaylistsFolder",
        ] {
            assert!(is_direct_children_browse(short), "{short}");
        }
        // An ordinary library parent is what the re-rooting exists for.
        for short in ["CollectionFolder", "Folder", "UserRootFolder", "Series"] {
            assert!(!is_direct_children_browse(short), "{short}");
        }
    }

    /// The query keys [`ItemsQuery`] binds: the field list serde hands
    /// `deserialize_struct`, captured by a deserializer that reads nothing.
    fn items_query_keys() -> &'static [&'static str] {
        struct Keys<'a>(&'a mut &'static [&'static str]);
        impl<'de> serde::Deserializer<'de> for Keys<'_> {
            type Error = serde::de::value::Error;
            fn deserialize_any<V: serde::de::Visitor<'de>>(
                self,
                _: V,
            ) -> Result<V::Value, Self::Error> {
                Err(serde::de::Error::custom("not a struct"))
            }
            fn deserialize_struct<V: serde::de::Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                _: V,
            ) -> Result<V::Value, Self::Error> {
                *self.0 = fields;
                Err(serde::de::Error::custom("keys captured"))
            }
            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
                byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
                enum identifier ignored_any
            }
        }
        let mut keys: &'static [&'static str] = &[];
        let _ = <ItemsQuery as serde::Deserialize>::deserialize(Keys(&mut keys));
        keys
    }

    /// `GET /Items` parameters in the vendored contracts that [`ItemsQuery`]
    /// does not bind yet. Each is an open port from `ItemsController.GetItems`
    /// — bind it, honour it, and delete it here; the guard below fails on a
    /// stale entry, so this list can only shrink.
    const UNBOUND_ITEMS_PARAMETERS: &[&str] = &[];

    /// Every `GET /Items` parameter in the vendored contracts — 10.11.8, the
    /// route gate's pin, and 12.1.0, the newest vendored Jellyfin spec (whose additions,
    /// `audioLanguages` and `subtitleLanguages`, are bound) — is bound by
    /// [`ItemsQuery`] or listed in [`UNBOUND_ITEMS_PARAMETERS`]. serde drops an unknown query key
    /// without a word, so a missing field is a filter the client asked for and
    /// never got: `albumArtistIds` was one, and every artist page listed every
    /// album in the library.
    #[test]
    fn items_query_binds_every_contract_parameter() {
        let mut contract = BTreeSet::new();
        for (version, raw) in [
            (
                "10.11.8",
                include_str!("../../../../contracts/jellyfin-openapi-10.11.8.json"),
            ),
            (
                "12.1.0",
                include_str!("../../../../contracts/jellyfin-openapi-12.1.0.json"),
            ),
        ] {
            let spec: serde_json::Value = serde_json::from_str(raw).expect("contract parses");
            let parameters = spec["paths"]["/Items"]["get"]["parameters"]
                .as_array()
                .unwrap_or_else(|| panic!("{version}: GET /Items lists its parameters"));
            contract.extend(
                parameters
                    .iter()
                    .filter_map(|p| p["name"].as_str().map(str::to_owned)),
            );
        }
        let keys = items_query_keys();
        assert!(keys.contains(&"albumArtistIds"), "keys captured: {keys:?}");
        let unbound: BTreeSet<&str> = contract
            .iter()
            .map(String::as_str)
            .filter(|name| !keys.contains(name))
            .collect();
        let listed: BTreeSet<&str> = UNBOUND_ITEMS_PARAMETERS.iter().copied().collect();
        assert_eq!(
            unbound, listed,
            "left: the contract parameters ItemsQuery drops; right: the open-port list. \
             A newly dropped parameter must be bound, not listed; a bound one must leave the list"
        );
    }

    /// `audioLanguages` / `subtitleLanguages` bind as comma-delimited string
    /// collections (v12 ItemsController.cs:258-259, 452-453); a repeated key
    /// keeps every value through the declared collection.
    #[test]
    fn items_query_binds_the_stream_language_filters() {
        let bind = |q: &str| {
            let Query(query) =
                Query::<ItemsQuery>::try_from_uri(&format!("/Items?{q}").parse().expect("uri"))
                    .expect(q);
            items_query_options(&query, Some(crate::test_support::sample_user())).expect("maps")
        };
        let internal = bind("audioLanguages=eng,ger&subtitleLanguages=fre");
        assert_eq!(internal.audio_languages, ["eng", "ger"]);
        assert_eq!(internal.subtitle_languages, ["fre"]);
        let internal = bind("audioLanguages=eng&AudioLanguages=ger");
        assert_eq!(internal.audio_languages, ["eng", "ger"]);
        assert!(
            internal.subtitle_languages.is_empty(),
            "{:?}",
            internal.subtitle_languages
        );
    }

    /// The filters `TranslateQuery` already applies reach it from the URL
    /// (v12 `ItemsController.GetItems`: `IsMissing`, `IsUnaired`, `IsNews`,
    /// `IsKids`, `IsSports`, the four dimension bounds, and the two
    /// `MinDateLastSaved` bounds `?.ToUniversalTime()`).
    #[test]
    fn items_query_binds_the_translated_filters() {
        let Query(query) = Query::<ItemsQuery>::try_from_uri(
            &"/Items?isMissing=false&isUnaired=true&isNews=true&isKids=false&isSports=true\
              &minWidth=640&maxWidth=1920&minHeight=360&maxHeight=1080\
              &minDateLastSaved=2026-01-02T03:04:05Z&minDateLastSavedForUser=2026-02-03T04:05:06Z"
                .parse()
                .expect("uri"),
        )
        .expect("binds");
        let internal =
            items_query_options(&query, Some(crate::test_support::sample_user())).expect("maps");
        assert_eq!(
            (internal.is_missing, internal.is_unaired),
            (Some(false), Some(true))
        );
        assert_eq!(
            (internal.is_news, internal.is_kids, internal.is_sports),
            (Some(true), Some(false), Some(true))
        );
        assert_eq!(
            (
                internal.min_width,
                internal.max_width,
                internal.min_height,
                internal.max_height
            ),
            (Some(640), Some(1920), Some(360), Some(1080))
        );
        assert_eq!(
            internal.min_date_last_saved.map(|d| d.to_rfc3339()),
            Some("2026-01-02T03:04:05+00:00".to_owned())
        );
        assert_eq!(
            internal
                .min_date_last_saved_for_user
                .map(|d| d.to_rfc3339()),
            Some("2026-02-03T04:05:06+00:00".to_owned())
        );
    }

    /// ASP.NET binding of the new filters: a bool binds trimmed and ignoring
    /// case, an empty one is null, a malformed one is a 400; an offset date
    /// is converted to UTC.
    #[test]
    fn the_translated_filters_bind_like_asp_net() {
        let bind = |q: &str| {
            Query::<ItemsQuery>::try_from_uri(&format!("/Items?{q}").parse().expect("uri"))
                .map(|Query(query)| query)
        };
        let query = bind("isMissing=%20FALSE%20&isUnaired=&isKids=True").expect("binds");
        assert_eq!(
            (query.is_missing, query.is_unaired, query.is_kids),
            (Some(false), None, Some(true))
        );
        assert!(bind("isSports=maybe").is_err());
        assert!(bind("minWidth=wide").is_err());
        let query = bind("minDateLastSaved=2026-01-02T05:04:05%2B02:00").expect("binds");
        assert_eq!(
            query.min_date_last_saved.map(|d| d.to_rfc3339()),
            Some("2026-01-02T03:04:05+00:00".to_owned())
        );
    }

    /// v12 ItemsController.cs:462-474: a language list beside `hasSubtitles`.
    #[test]
    fn subtitle_languages_and_has_subtitles_resolve_like_get_items() {
        let mut both = InternalItemsQuery {
            subtitle_languages: vec!["ger".to_owned()],
            has_subtitles: Some(true),
            ..InternalItemsQuery::default()
        };
        apply_media_stream_filter_rules(&mut both);
        assert_eq!(both.has_subtitles, None);
        assert_eq!(both.subtitle_languages, ["ger"]);
        assert!(both.include_owned_items);

        let mut contradicting = InternalItemsQuery {
            subtitle_languages: vec!["ger".to_owned()],
            has_subtitles: Some(false),
            ..InternalItemsQuery::default()
        };
        apply_media_stream_filter_rules(&mut contradicting);
        assert_eq!(contradicting.has_subtitles, Some(false));
        assert!(
            contradicting.subtitle_languages.is_empty(),
            "{:?}",
            contradicting.subtitle_languages
        );
        assert!(contradicting.include_owned_items);
    }

    /// v12 ItemsController.cs:476-487: every media-stream filter includes the
    /// alternate and linked versions; nothing else does.
    #[test]
    fn media_stream_filters_include_owned_items() {
        let includes_owned = |mut query: InternalItemsQuery| {
            apply_media_stream_filter_rules(&mut query);
            query.include_owned_items
        };
        let base = InternalItemsQuery::default;
        assert!(includes_owned(InternalItemsQuery {
            audio_languages: vec!["eng".to_owned()],
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            subtitle_languages: vec!["eng".to_owned()],
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            has_subtitles: Some(false),
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            is_3d: Some(false),
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            is_hd: Some(true),
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            is_4k: Some(true),
            ..base()
        }));
        assert!(includes_owned(InternalItemsQuery {
            video_types: vec![ferrofin_model::entities::VideoType::BluRay],
            ..base()
        }));
        let mut plain = InternalItemsQuery {
            is_favorite: Some(true),
            recursive: true,
            ..InternalItemsQuery::default()
        };
        apply_media_stream_filter_rules(&mut plain);
        assert!(!plain.include_owned_items);
    }

    /// `[FromQuery] bool? isLocked` → `IsLocked = isLocked`
    /// (`ItemsController.cs:243,400`).
    #[test]
    fn items_query_reads_is_locked() {
        let q: ItemsQuery = serde_urlencoded::from_str("isLocked=true").expect("parses");
        assert_eq!(q.is_locked, Some(true));
        let q: ItemsQuery = serde_urlencoded::from_str("recursive=true").expect("parses");
        assert_eq!(q.is_locked, None);
    }

    // GET /Items must honour the camelCase `fields` param (the OpenAPI contract's casing, what
    // jellyfin-web sends) and map it onto DtoOptions. Regression for the bug where the handler
    // hardcoded `with_all_fields(false)` and had no `fields` member, dropping every requested
    // field (e.g. Path) — which made item DTOs diverge from Jellyfin.
    #[test]
    fn items_query_fields_maps_onto_dto_options() {
        let q: ItemsQuery =
            serde_urlencoded::from_str("recursive=true&fields=Path,Genres").expect("parses");
        assert_eq!(q.fields.as_deref(), Some("Path,Genres"));
        let options = q.dto_options();
        assert!(options.contains_field(ItemFields::Path));
        assert!(options.contains_field(ItemFields::Genres));

        // No `fields` ⇒ the base DTO (empty field set), matching Jellyfin's default GetItems.
        let base: ItemsQuery = serde_urlencoded::from_str("recursive=true").expect("parses");
        let base_opts = base.dto_options();
        assert!(!base_opts.contains_field(ItemFields::Path));
    }

    /// `AddAdditionalDtoOptions(enableImages, enableUserData, imageTypeLimit,
    /// enableImageTypes)` (v12 DtoExtensions.cs:57-82) on `GET /Items` and
    /// `GET /UserItems/Resume`: the four toggles reach the projection, and the
    /// defaults are upstream's (images on, every type, no limit, user data on).
    #[test]
    fn items_and_resume_queries_honour_the_image_and_user_data_toggles() {
        use ferrofin_model::entities::ImageType;

        let raw = "fields=PrimaryImageAspectRatio&imageTypeLimit=1&enableImageTypes=Primary,Backdrop&enableUserData=false&enableImages=true";
        let items: ItemsQuery = serde_urlencoded::from_str(raw).expect("parses");
        let resume: ResumeQuery = serde_urlencoded::from_str(raw).expect("parses");
        for options in [items.dto_options(), resume.dto_options()] {
            assert!(options.contains_field(ItemFields::PrimaryImageAspectRatio));
            assert_eq!(options.image_type_limit, 1);
            assert_eq!(
                options.image_types,
                [ImageType::Primary, ImageType::Backdrop]
            );
            assert!(!options.enable_user_data);
            assert!(options.enable_images);
            // `GetImageLimit(Logo)` is 0 for a type outside the list, so no
            // `ImageTags.Logo` can come back on this browse.
            assert_eq!(options.image_limit(ImageType::Logo), 0);
            assert_eq!(options.image_limit(ImageType::Backdrop), 1);
        }

        // Defaults.
        let items: ItemsQuery = serde_urlencoded::from_str("recursive=true").expect("parses");
        let resume: ResumeQuery = serde_urlencoded::from_str("").expect("parses");
        for options in [items.dto_options(), resume.dto_options()] {
            assert!(options.enable_images);
            assert!(options.enable_user_data);
            assert_eq!(options.image_type_limit, i32::MAX);
            assert!(options.image_limit(ImageType::Logo) > 0);
            assert!(options.fields.is_empty(), "no fields ⇒ the base DTO");
        }

        // `enableImages=false` switches every tag off.
        let items: ItemsQuery = serde_urlencoded::from_str("enableImages=false").expect("parses");
        assert!(!items.dto_options().enable_images);
        assert_eq!(items.dto_options().image_limit(ImageType::Primary), 0);

        // `excludeActiveSessions` parses (default off).
        let resume: ResumeQuery =
            serde_urlencoded::from_str("excludeActiveSessions=true").expect("parses");
        assert_eq!(resume.exclude_active_sessions, Some(true));
    }

    /// The albums-by-artist default is a sort, so an explicit-ids page of
    /// that shape keeps the repository's year order (v12.2 Folder.cs:1072-1080
    /// reorders by id only with no `OrderBy`).
    #[test]
    fn an_albums_by_artist_default_order_keeps_an_ids_page_in_that_order() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        let (a, b) = (Uuid::from_u128(0xA), Uuid::from_u128(0xB));
        let mut query = InternalItemsQuery {
            item_ids: vec![a, b],
            artist_ids: vec![Uuid::from_u128(0xA7)],
            include_item_types: vec![BaseItemKind::MusicAlbum],
            ..InternalItemsQuery::default()
        };
        apply_albums_by_artist_order(&mut query);
        assert_eq!(
            query.order_by,
            [
                (ItemSortBy::ProductionYear, SortOrder::Descending),
                (ItemSortBy::SortName, SortOrder::Ascending),
            ]
        );
        let mut rows: Vec<BaseItemEntity> = [b, a]
            .into_iter()
            .map(|id| BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(id),
                ..BaseItemEntity::default()
            })
            .collect();
        sort_items_by_request(&query, &mut rows);
        assert_eq!(
            rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>(),
            [b, a].map(ferrofin_db::store::guid_to_db),
            "the repository's year order stands"
        );
    }

    /// `SortItemsByRequest` (v12.2 Folder.cs:1065-1083): an unsorted request
    /// for more than one id answers in the request's order, stably, a row by
    /// the first position of its id, and a row whose id was not asked for
    /// (`Array.IndexOf` = -1) first. Any requested sort, `Default` included,
    /// and a single id keep the repository's order.
    #[test]
    fn sort_items_by_request_restores_the_requested_id_order() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        let [a, b, c, unasked] = [1, 2, 3, 4].map(Uuid::from_u128);
        let sorted = |item_ids: &[Uuid], order_by: &[(ItemSortBy, SortOrder)], rows: &[Uuid]| {
            let query = InternalItemsQuery {
                item_ids: item_ids.to_vec(),
                order_by: order_by.to_vec(),
                ..InternalItemsQuery::default()
            };
            let mut items: Vec<BaseItemEntity> = rows
                .iter()
                .map(|id| crate::test_support::minimal_base_item(*id, "Item", "Movie"))
                .collect();
            sort_items_by_request(&query, &mut items);
            items
                .iter()
                .map(|row| Uuid::parse_str(&row.id).expect("id"))
                .collect::<Vec<_>>()
        };
        // The repository's `SortName` order becomes the request's.
        assert_eq!(sorted(&[c, a, b], &[], &[a, b, c]), [c, a, b]);
        for order_by in [ItemSortBy::Default, ItemSortBy::SortName] {
            assert_eq!(
                sorted(&[c, a, b], &[(order_by, SortOrder::Ascending)], &[a, b, c]),
                [a, b, c],
                "{order_by:?}"
            );
        }
        // `ItemIds.Length > 1`: a single id is left as the repository had it.
        assert_eq!(sorted(&[a], &[], &[a, unasked]), [a, unasked]);
        // A repeated id sorts by its first position.
        assert_eq!(sorted(&[c, a, c, b], &[], &[a, b, c]), [c, a, b]);
        // Unasked rows lead, and rows tied on a position keep their order.
        assert_eq!(
            sorted(&[c, a], &[], &[a, c, unasked, b]),
            [unasked, b, c, a]
        );
    }

    /// The delete's task runs inside the request's span, so its logs and
    /// spans stay under `http_request` (LOGGING.md §4).
    #[tokio::test]
    async fn the_delete_task_runs_in_the_requests_span() {
        use tracing::Instrument as _;
        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
        let span = tracing::info_span!("http_request");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = std::sync::Arc::clone(&seen);
        run_to_completion(async move {
            *record.lock().expect("lock") = tracing::Span::current()
                .metadata()
                .map(tracing::Metadata::name);
            Ok(())
        })
        .instrument(span)
        .await
        .expect("ran");
        assert_eq!(*seen.lock().expect("lock"), Some("http_request"));
    }
}
