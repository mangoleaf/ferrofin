//! `TvShowsController` — the "Next Up" queue, upcoming episodes, and a series'
//! seasons/episodes under the `/Shows` path, plus the `/Shows/{itemId}/Similar`
//! alias of the similar-items surface.
//!
//! Ports the portable `/Shows` surface:
//!
//! - `GET /Shows/NextUp` — a user's "Next Up" episode queue, delegated to the
//!   [`TvSeriesManager`](ferrofin_traits::tv::TvSeriesManager) seam (which runs the
//!   per-series next-up algorithm through the `NextUpService`).
//! - `GET /Shows/Upcoming` — episodes premiering on or after yesterday, ordered
//!   by premiere date then sort name.
//! - `GET /Shows/{seriesId}/Episodes` — a series' episodes, optionally scoped to
//!   one season (by season id or season number).
//! - `GET /Shows/{seriesId}/Seasons` — a series' seasons.
//! - `GET /Shows/{itemId}/Similar` — items similar to a show, delegated to the
//!   [`SimilarItemsManager`](ferrofin_traits::library::SimilarItemsManager) seam.
//!
//! The C# controller walks the un-ported `Series`/`Season`/`Episode` OOP tree
//! (`series.GetEpisodes` / `series.GetSeasons` / `season.GetEpisodes`); the
//! portable equivalent queries `BaseItems` by the series' presentation key
//! (`SeriesPresentationUniqueKey`) plus the season filters, exactly as the C#
//! entity methods build their `InternalItemsQuery`. Two OOP-tree-only refinements
//! are documented deferrals: the `adjacentTo` sibling filter
//! (`UserViewBuilder.FilterForAdjacency`) and the `startItemId`
//! alternate-version primary-episode remap, neither of which is persistable
//! without the reconstructed domain tree.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::tv::NextUpQuery;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::{resolve_user, resolve_user_opt, user_uuid};
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_csv_uuids};
use crate::state::AppState;

/// Builds a [`DtoOptions`] from the request's `fields` / image parameters.
///
/// Mirrors C# `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(...)`:
/// the parsed `fields` list rides through, `enable_images` defaults on (C#'s
/// `enableImages ?? true`), the image-type limit falls back to Jellyfin's
/// unbounded default, and any explicit `enableImageTypes` narrow the set.
pub(crate) fn build_dto_options(
    fields: Option<&str>,
    enable_images: Option<bool>,
    image_type_limit: Option<i32>,
    enable_image_types: Option<&str>,
    enable_user_data: Option<bool>,
) -> DtoOptions {
    let requested_types: Vec<ImageType> = parse_csv_enums_lenient(enable_image_types);
    let mut options = DtoOptions {
        // Lenient: clients still send deprecated ItemFields (e.g. BasicSyncInfo);
        // Jellyfin drops unknowns rather than 400-ing the request.
        fields: parse_csv_enums_lenient(fields),
        enable_images: enable_images.unwrap_or(true),
        image_type_limit: image_type_limit.unwrap_or(i32::MAX),
        enable_user_data: enable_user_data.unwrap_or(true),
        // `..default()` seeds `image_types` with *every* type — Jellyfin's
        // `DtoOptions` constructor default. `GetImageLimit` only returns a
        // non-zero limit for types in that list, so leaving it empty (the old
        // behaviour) suppressed all `ImageTags` on the Seasons/Episodes lists.
        ..DtoOptions::default()
    };
    // Narrow to the client's requested types only when it actually asked.
    if !requested_types.is_empty() {
        options.image_types = requested_types;
    }
    options
}

/// The presentation unique key of a series row: its explicit
/// `PresentationUniqueKey` when set, else its id (mirrors
/// `Series.GetPresentationUniqueKey()` / `GetUniqueSeriesKey`).
fn series_presentation_key(series: &BaseItemEntity) -> String {
    series
        .presentation_unique_key
        .clone()
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| series.id.clone())
}

/// The query parameters honoured by `GET /Shows/NextUp`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextUpParams {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Comma-delimited additional [`ItemFields`](ferrofin_model::querying::ItemFields).
    #[serde(default)]
    fields: Option<String>,
    /// Restrict to a single series.
    #[serde(default)]
    series_id: Option<Uuid>,
    /// Localizes the search to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Whether image information is included.
    #[serde(default)]
    enable_images: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether user data is included.
    #[serde(default)]
    enable_user_data: Option<bool>,
    /// Only consider episodes aired on or after this cutoff.
    #[serde(default)]
    next_up_date_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
    /// Whether to include resumable (partially-watched) episodes (defaults `true`).
    #[serde(default)]
    enable_resumable: Option<bool>,
    /// Whether to include already-watched episodes for rewatching (defaults `false`).
    #[serde(default)]
    enable_rewatching: Option<bool>,
}

/// `GET /Shows/NextUp` — a user's "Next Up" episode queue.
///
/// Port of `TvShowsController.GetNextUp`. Delegates to the
/// [`TvSeriesManager`](ferrofin_traits::tv::TvSeriesManager), which runs the
/// per-series next-up algorithm and paginates the result.
#[utoipa::path(
    get,
    path = "/Shows/NextUp",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Next-up episodes returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_next_up(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<NextUpParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    let options = build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    );

    let next_up_query = NextUpQuery {
        user_id,
        parent_id: query.parent_id,
        series_id: query.series_id,
        start_index: query.start_index,
        limit: query.limit,
        enable_image_types: options.image_types.clone(),
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        next_up_date_cutoff: query.next_up_date_cutoff,
        enable_resumable: query.enable_resumable.unwrap_or(true),
        enable_rewatching: query.enable_rewatching.unwrap_or(false),
    };

    let result = state
        .tv_series
        .get_next_up(&next_up_query, &options)
        .await?;
    Ok(Json(result))
}

/// The query parameters honoured by `GET /Shows/Upcoming`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpcomingParams {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Comma-delimited additional fields.
    #[serde(default)]
    fields: Option<String>,
    /// Localizes the search to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Whether image information is included.
    #[serde(default)]
    enable_images: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether user data is included.
    #[serde(default)]
    enable_user_data: Option<bool>,
}

/// `GET /Shows/Upcoming` — episodes premiering on or after yesterday.
///
/// Port of `TvShowsController.GetUpcomingEpisodes`. The C# cutoff is
/// `DateTime.UtcNow.Date.AddDays(-1)`; the query is recursive over `Episode`s,
/// ordered by premiere date then sort name.
#[utoipa::path(
    get,
    path = "/Shows/Upcoming",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Upcoming episodes returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_upcoming_episodes(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UpcomingParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let options = build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    );

    // C# `DateTime.UtcNow.Date.AddDays(-1)` — midnight yesterday, UTC.
    let min_premiere_date = (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc());

    let mut internal = InternalItemsQuery {
        user: user.clone(),
        include_item_types: vec![BaseItemKind::Episode],
        order_by: vec![
            (ItemSortBy::PremiereDate, SortOrder::Ascending),
            (ItemSortBy::SortName, SortOrder::Ascending),
        ],
        min_premiere_date,
        start_index: query.start_index,
        limit: query.limit,
        recursive: true,
        dto_options: options.clone(),
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }

    let items = state.library.get_item_list(&internal).await?;
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(query.start_index, Some(total), dtos)))
}

/// The query parameters honoured by `GET /Shows/{seriesId}/Episodes`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodesParams {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Comma-delimited additional fields.
    #[serde(default)]
    fields: Option<String>,
    /// Filter by season number.
    #[serde(default)]
    season: Option<i32>,
    /// Filter by season id.
    #[serde(default)]
    season_id: Option<Uuid>,
    /// Filter by items that are missing episodes or not.
    #[serde(default)]
    is_missing: Option<bool>,
    /// Return items that are siblings of a supplied item.
    ///
    /// Accepted for wire compatibility but not yet applied: the
    /// `UserViewBuilder.FilterForAdjacency` sibling filter needs the un-ported
    /// domain tree (documented deferral).
    #[serde(default)]
    #[allow(dead_code)]
    adjacent_to: Option<Uuid>,
    /// Skip through the list until a given item is found.
    ///
    /// Accepted for wire compatibility but not yet applied: the alternate-version
    /// primary-episode remap needs the un-ported domain tree (documented deferral).
    #[serde(default)]
    #[allow(dead_code)]
    start_item_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Whether image information is included.
    #[serde(default)]
    enable_images: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether user data is included.
    #[serde(default)]
    enable_user_data: Option<bool>,
    /// Sort order override (only `Random` is honoured, matching C#).
    #[serde(default)]
    sort_by: Option<ItemSortBy>,
}

/// `GET /Shows/{seriesId}/Episodes` — a series' episodes.
///
/// Port of `TvShowsController.GetEpisodes`. The season-id / season-number /
/// all-episodes branches mirror C#, but instead of walking the OOP tree the
/// episodes are queried from `BaseItems` by the series' presentation key plus the
/// season filter, ordered by aired-episode order.
#[utoipa::path(
    get,
    path = "/Shows/{itemId}/Episodes",
    params(("itemId" = String, Path, description = "The series id")),
    responses(
        (status = 200, description = "Episodes returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Series or season not found")
    ),
    tag = "ferrofin"
)]
async fn get_episodes(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(series_id): Path<Uuid>,
    Query(query): Query<EpisodesParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let options = build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    );
    // C# includes missing episodes only when the user opts in (or an API key).
    let include_missing = user.as_ref().is_some_and(|u| u.display_missing_episodes);

    // Resolve the series key and the effective season-number filter.
    let (series_key, season_number): (String, Option<i32>) = if let Some(season_id) =
        query.season_id
    {
        // Season id supplied — the item must be a season.
        let season = state
            .library
            .get_item_by_id(season_id)
            .await?
            .filter(|i| i.type_.ends_with("Season"))
            .ok_or_else(|| ApiError::NotFound(format!("No season exists with Id {season_id}")))?;
        let key = season
            .series_presentation_unique_key
            .clone()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| series_presentation_key(&season));
        let number = season.index_number.and_then(|n| i32::try_from(n).ok());
        (key, number)
    } else {
        // Series id supplied — the item must be a series.
        let series = state
            .library
            .get_item_by_id(series_id)
            .await?
            .filter(|i| i.type_.ends_with("Series"))
            .ok_or_else(|| ApiError::NotFound("Series not found".to_owned()))?;
        (series_presentation_key(&series), query.season)
    };

    // C# orders by aired-episode order, except `sortBy == Random` which shuffles;
    // the persistence layer implements `Random` ordering directly.
    let order_by = if query.sort_by == Some(ItemSortBy::Random) {
        vec![(ItemSortBy::Random, SortOrder::Ascending)]
    } else {
        vec![(ItemSortBy::AiredEpisodeOrder, SortOrder::Ascending)]
    };
    let mut internal = InternalItemsQuery {
        user: user.clone(),
        series_presentation_unique_key: Some(series_key),
        include_item_types: vec![BaseItemKind::Episode],
        order_by,
        dto_options: options.clone(),
        ..InternalItemsQuery::default()
    };
    // C#: episodes are filtered to the season by its aired index number.
    internal.parent_index_number = season_number;
    if !include_missing {
        internal.is_missing = Some(false);
    }
    // C# applies `isMissing` as an explicit after-the-fact filter when supplied.
    if let Some(is_missing) = query.is_missing {
        internal.is_missing = Some(is_missing);
    }

    let mut episodes = state.library.get_item_list(&internal).await?;

    // `startItemId`: return the run of episodes from that item onward, so a client
    // playing "from this episode" queues the right slice. Port of C#
    // `episodes.SkipWhile(i => i.Id != startItemId)` — drop everything before the
    // match; if the item isn't in this list, the skip consumes all (empty).
    if let Some(start_item_id) = query.start_item_id {
        // Stored ids are UPPERCASE-hyphenated (`guid_to_db`); `Uuid::to_string()`
        // is lowercase and can never match — the compare-in-stored-form rule.
        // (The old lowercase compare cleared EVERY episode list, which
        // jellyfin-web's episode playback path reported as "Unable to find a
        // valid media source to play".)
        let start = ferrofin_db::store::guid_to_db(start_item_id);
        match episodes.iter().position(|e| e.id == start) {
            Some(pos) => drop(episodes.drain(..pos)),
            None => episodes.clear(),
        }
    }

    let total = i32::try_from(episodes.len()).unwrap_or(i32::MAX);
    let page = paginate(episodes, query.start_index, query.limit);
    let dtos = state
        .dto
        .get_base_item_dtos(&page, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(query.start_index, Some(total), dtos)))
}

/// The query parameters honoured by `GET /Shows/{seriesId}/Seasons`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeasonsParams {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Comma-delimited additional fields.
    #[serde(default)]
    fields: Option<String>,
    /// Filter by special season.
    #[serde(default)]
    is_special_season: Option<bool>,
    /// Filter by items that are missing episodes or not.
    #[serde(default)]
    is_missing: Option<bool>,
    /// Return items that are siblings of a supplied item (deferred).
    #[serde(default)]
    adjacent_to: Option<Uuid>,
    /// Whether image information is included.
    #[serde(default)]
    enable_images: Option<bool>,
    /// The max number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to include.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether user data is included.
    #[serde(default)]
    enable_user_data: Option<bool>,
}

/// `GET /Shows/{seriesId}/Seasons` — a series' seasons.
///
/// Port of `TvShowsController.GetSeasons`. Queries `BaseItems` for the series'
/// `Season` children by presentation key, ordered by sort name; a missing series
/// is a `404`.
#[utoipa::path(
    get,
    path = "/Shows/{itemId}/Seasons",
    params(("itemId" = String, Path, description = "The series id")),
    responses(
        (status = 200, description = "Seasons returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Series not found")
    ),
    tag = "ferrofin"
)]
async fn get_seasons(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(series_id): Path<Uuid>,
    Query(query): Query<SeasonsParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let series = state
        .library
        .get_item_by_id(series_id)
        .await?
        .filter(|i| i.type_.ends_with("Series"))
        .ok_or_else(|| ApiError::NotFound(format!("series {series_id}")))?;
    let series_key = series_presentation_key(&series);
    let options = build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    );

    // C# `SetSeasonQueryOptions`: also drop missing seasons unless the user
    // opts in; an explicit `isMissing`/`isSpecialSeason` narrows further.
    let include_missing = user.as_ref().is_some_and(|u| u.display_missing_episodes);
    let internal = InternalItemsQuery {
        user: user.clone(),
        series_presentation_unique_key: Some(series_key),
        include_item_types: vec![BaseItemKind::Season],
        order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
        is_special_season: query.is_special_season,
        is_missing: query
            .is_missing
            .or_else(|| (!include_missing).then_some(false)),
        adjacent_to: query.adjacent_to,
        dto_options: options.clone(),
        ..InternalItemsQuery::default()
    };

    let seasons = state.library.get_item_list(&internal).await?;
    let total = i32::try_from(seasons.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&seasons, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(None, Some(total), dtos)))
}

/// The query parameters honoured by `GET /Shows/{itemId}/Similar`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimilarParams {
    /// Comma-delimited artist ids to exclude.
    #[serde(default)]
    exclude_artist_ids: Option<String>,
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Comma-delimited additional fields.
    #[serde(default)]
    fields: Option<String>,
}

/// `GET /Shows/{itemId}/Similar` — items similar to a show.
///
/// Port of `LibraryController.GetSimilarItems` (the `GetSimilarShows` route).
/// Delegates to the
/// [`SimilarItemsManager`](ferrofin_traits::library::SimilarItemsManager), whose
/// `get_similar_items` answers empty for an `Episode` or a by-name seed other
/// than a `MusicArtist` (the C# controller guard) and otherwise runs the
/// providers with the user's access and the per-kind filter set.
#[utoipa::path(
    get,
    path = "/Shows/{itemId}/Similar",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Similar items returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_similar_shows(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<SimilarParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let user_id = user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok());
    let exclude_artist_ids = parse_csv_uuids(query.exclude_artist_ids.as_deref())?;
    let options = build_dto_options(query.fields.as_deref(), None, None, None, None);

    // Same C# seed resolution as the other five aliases: a nil id falls back to
    // the root folder, and an id that resolves to nothing is a `404`.
    let Some(seed_id) = crate::handlers::similar::resolve_similar_seed(&state, item_id).await?
    else {
        return Ok(Json(QueryResult::new(Some(0), Some(0), Vec::new())));
    };
    let items = state
        .similar_items
        .get_similar_items(seed_id, &exclude_artist_ids, user_id, &options, query.limit)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(Some(0), Some(total), dtos)))
}

/// Applies C#'s `ApplyPaging`: skip `start_index`, then take `limit`.
fn paginate(
    items: Vec<BaseItemEntity>,
    start_index: Option<i32>,
    limit: Option<i32>,
) -> Vec<BaseItemEntity> {
    if start_index.is_none() && limit.is_none() {
        return items;
    }
    let start = start_index
        .and_then(|s| usize::try_from(s).ok())
        .unwrap_or(0);
    let mut page: Vec<BaseItemEntity> = items.into_iter().skip(start).collect();
    if let Some(limit) = limit
        && limit >= 0
    {
        page.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    page
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Shows/NextUp", get(get_next_up))
        .route("/Shows/Upcoming", get(get_upcoming_episodes))
        .route("/Shows/{itemId}/Episodes", get(get_episodes))
        .route("/Shows/{itemId}/Seasons", get(get_seasons))
        .route("/Shows/{itemId}/Similar", get(get_similar_shows))
}
