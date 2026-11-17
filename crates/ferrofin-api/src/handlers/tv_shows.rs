//! `TvShowsController` — the "Next Up" queue, upcoming episodes, and a series'
//! seasons/episodes under the `/Shows` path. The `/Shows/{itemId}/Similar`
//! alias of the similar-items surface lives in [`super::similar`].
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
//!
//! Season filters and aired special positioning use persisted season identities,
//! presentation keys and episode metadata, with user visibility applied first.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::{
    BaseItemEntity, aired_episode_order, is_virtual_location, presentation_key, relation_id,
};
use ferrofin_db::entities::users::UserEntity;
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
use crate::extract::{DotnetGuid, Query};
use crate::handlers::items::{resolve_user, resolve_user_opt, user_uuid};
use crate::handlers::query_parse::parse_csv_enums_lenient;
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

/// One episode of the series: its aired position, its row, and the season it
/// resolves to, as an index into the request's seasons.
type SeasonEpisodeRow = (
    ferrofin_model::entities_media::AiredEpisodeOrder,
    BaseItemEntity,
    Option<usize>,
);

/// `FilterEpisodesBySeason` for `season`, in aired order: the indexes into
/// `episodes` of the ones it lists. An episode's season is its index into
/// `seasons`.
fn season_episode_rows(
    episodes: &[SeasonEpisodeRow],
    seasons: &[BaseItemEntity],
    season: &BaseItemEntity,
    include_specials: bool,
    require_ancestry: bool,
) -> Vec<usize> {
    let number = season.index_number;
    let season_key = presentation_key(season);
    let support_specials = include_specials && number.is_some_and(|number| number != 0);
    let mut matches: Vec<_> = episodes
        .iter()
        .enumerate()
        .filter(|(_, (order, item, item_season))| {
            let item_season = item_season.and_then(|index| seasons.get(index));
            let same_season = item_season
                .is_some_and(|known| presentation_key(known).eq_ignore_ascii_case(season_key));
            if require_ancestry && !same_season {
                return false;
            }
            let current = if support_specials {
                order
                    .airs_after_season
                    .or(order.airs_before_season)
                    .or(item.parent_index_number)
            } else {
                item.parent_index_number
            };
            if current.is_some() && number.is_some() && current == number {
                return true;
            }
            if current.is_none() && number.is_none() && is_virtual_location(season) {
                return item_season.is_none_or(is_virtual_location);
            }
            same_season
        })
        .collect();
    if number != Some(0) {
        // `LibraryManager.Sort`'s LINQ `OrderBy`: stable, and the aired-order
        // comparer is not a total order, on which `sort_by` may panic.
        ferrofin_util::stable_sort::stable_sort_by(
            &mut matches,
            |(_, (left, _, _)), (_, (right, _, _))| left.compare(right),
        );
    }
    // Stable input SortName order remains for season zero and comparator ties.
    matches.into_iter().map(|(index, _)| index).collect()
}

/// Moves the rows at `indexes` out of `episodes`, in that order. An index
/// is taken once; [`season_episode_rows`] and the all-seasons dedup never
/// repeat one.
fn take_episode_rows(episodes: Vec<SeasonEpisodeRow>, indexes: Vec<usize>) -> Vec<BaseItemEntity> {
    let mut rows: Vec<_> = episodes.into_iter().map(|(_, row, _)| Some(row)).collect();
    indexes
        .into_iter()
        .filter_map(|index| {
            let row = rows.get_mut(index).and_then(Option::take);
            debug_assert!(row.is_some(), "episode row {index} missing or listed twice");
            row
        })
        .collect()
}

/// The query parameters honoured by `GET /Shows/NextUp`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextUpParams {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    series_id: Option<Uuid>,
    /// Localizes the search to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_date_time::deserialize_optional_date_time"
    )]
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
    for scope in [query.parent_id, query.series_id] {
        crate::handlers::items::require_scope_id_visible(&state, &auth, scope, Some(&user)).await?;
    }
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    crate::handlers::items::require_scope_id_visible(
        &state,
        &auth,
        query.parent_id,
        user.as_deref(),
    )
    .await?;
    let options = Arc::new(build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    ));

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
        dto_options: Arc::clone(&options),
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }

    let items = state.library.get_item_list(&internal).await?;
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user.as_deref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(query.start_index, Some(total), dtos)))
}

/// The query parameters honoured by `GET /Shows/{seriesId}/Episodes`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodesParams {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Comma-delimited additional fields.
    #[serde(default)]
    fields: Option<String>,
    /// Filter by season number.
    #[serde(default)]
    season: Option<i32>,
    /// Filter by season id.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    season_id: Option<Uuid>,
    /// Filter by items that are missing episodes or not.
    #[serde(default)]
    is_missing: Option<bool>,
    /// Return items that are siblings of a supplied item.
    ///
    /// Applied after season/start-item filtering and before paging.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    adjacent_to: Option<Uuid>,
    /// Skip through the list until a given item is found.
    ///
    /// Alternate versions remap to their stored primary episode before slicing.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::nullable_enum"
    )]
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
#[allow(clippy::too_many_lines)] // source season resolution, positioning and request filters
async fn get_episodes(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(DotnetGuid(series_id)): Path<DotnetGuid>,
    Query(query): Query<EpisodesParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let options = Arc::new(build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    ));
    // Source admits missing rows before applying the explicit request filter.
    let include_missing =
        auth.is_api_key || user.as_ref().is_some_and(|u| u.display_missing_episodes);
    let include_specials = state
        .config
        .configuration()
        .await?
        .display_specials_within_seasons;
    let (series_key, selected_season, actual_series_id) = if let Some(season_id) = query.season_id {
        let (season, chain) = state
            .library
            .get_item_by_id_for_user_with_chain(season_id, user.as_deref())
            .await?
            .filter(|(item, _)| item.type_.ends_with("Season"))
            .ok_or_else(|| ApiError::NotFound(format!("No season exists with Id {season_id}")))?;
        // Season.GetEpisodes follows its actual Series, even when the route's
        // series id names another row. An orphan season has no episodes.
        let actual_series = relation_id(season.series_id.as_deref())
            .or_else(|| relation_id(season.parent_id.as_deref()));
        let Some(actual_series) = actual_series else {
            return Ok(Json(QueryResult::new(
                query.start_index,
                Some(0),
                Vec::new(),
            )));
        };
        // The season's visibility check read the chain above it, the series
        // among it: read the series only when the chain does not hold it, as
        // when the check read none (a user-less request) (gl-982).
        let series = match chain
            .into_iter()
            .find(|row| Uuid::parse_str(&row.id).ok() == Some(actual_series))
        {
            Some(series) => Some(series),
            None => state.library.get_item_by_id(actual_series).await?,
        };
        let Some(series) = series.filter(|item| item.type_.ends_with("Series")) else {
            return Ok(Json(QueryResult::new(
                query.start_index,
                Some(0),
                Vec::new(),
            )));
        };
        let key = presentation_key(&series).to_owned();
        (key, Some(season), actual_series)
    } else {
        let series = state
            .library
            .get_item_by_id_for_user(series_id, user.as_deref())
            .await?
            .filter(|item| item.type_.ends_with("Series"))
            .ok_or_else(|| ApiError::NotFound("Series not found".to_owned()))?;
        (presentation_key(&series).to_owned(), None, series_id)
    };
    // Fetch visible rows in source SortName order. Parse aired metadata once, then
    // apply the same in-memory comparator used by NextUp before filtering/paging.
    // The selection reads a few columns of every row; the page's full rows are
    // read after it (gl-1259).
    let rows = state
        .library
        .get_episode_order_rows(&InternalItemsQuery {
            user: user.clone(),
            series_presentation_unique_key: Some(series_key),
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            is_missing: (!include_missing).then_some(false),
            dto_options: Arc::clone(&options),
            ..Default::default()
        })
        .await?;
    // The series' seasons, then any season an episode names that the query
    // did not return. Episodes refer to them by index, so no row is copied
    // per episode (gl-1003).
    let mut seasons = Vec::new();
    let mut episode_items = Vec::new();
    for row in rows {
        if row.type_.ends_with("Season") {
            seasons.push(row);
        } else if row.type_.ends_with("Episode") {
            episode_items.push(row);
        }
    }
    let selected = if query.season_id.is_some() {
        selected_season
    } else if let Some(number) = query.season {
        let Some(season) = seasons
            .iter()
            .find(|season| season.index_number == Some(i64::from(number)))
            .cloned()
        else {
            return Ok(Json(QueryResult::new(
                query.start_index,
                Some(0),
                Vec::new(),
            )));
        };
        Some(season)
    } else {
        None
    };
    if let Some(season) = &selected
        && !seasons
            .iter()
            .any(|known| known.id.eq_ignore_ascii_case(&season.id))
    {
        seasons.push(season.clone());
    }
    // The seasons listed when none is selected: those read for an episode
    // alone follow them.
    let listed = seasons.len();
    let mut known_seasons: std::collections::HashMap<Uuid, Option<usize>> = seasons
        .iter()
        .enumerate()
        .filter_map(|(index, season)| Uuid::parse_str(&season.id).ok().map(|id| (id, Some(index))))
        .collect();
    let mut episode_rows = Vec::with_capacity(episode_items.len());
    for row in episode_items {
        let season_id =
            relation_id(row.season_id.as_deref()).or_else(|| relation_id(row.parent_id.as_deref()));
        let episode_season = if let Some(id) = season_id {
            if let Some(season) = known_seasons.get(&id) {
                *season
            } else {
                let season = state
                    .library
                    .get_item_by_id(id)
                    .await?
                    .filter(|item| item.type_.ends_with("Season"))
                    .map(|season| {
                        seasons.push(season);
                        seasons.len() - 1
                    });
                known_seasons.insert(id, season);
                season
            }
        } else {
            None
        };
        // Episode.FindSeasonId also resolves episodes stored directly under
        // the series by their physical season number when SeasonId is empty.
        let episode_season = episode_season.or_else(|| {
            (relation_id(row.parent_id.as_deref()) == Some(actual_series_id))
                .then(|| {
                    seasons[..listed]
                        .iter()
                        .position(|season| season.index_number == row.parent_index_number)
                })
                .flatten()
        });
        episode_rows.push((aired_episode_order(&row), row, episode_season));
    }
    let listed_rows = if let Some(season) = selected {
        season_episode_rows(
            &episode_rows,
            &seasons,
            &season,
            include_specials,
            !include_specials,
        )
    } else {
        // Series.GetEpisodes keeps the last appearance of a positioned special,
        // placing it in its aired season rather than duplicating season zero.
        let mut flattened = Vec::new();
        for season in &seasons[..listed] {
            flattened.extend(season_episode_rows(
                &episode_rows,
                &seasons,
                season,
                include_specials,
                false,
            ));
        }
        let mut seen = std::collections::HashSet::new();
        flattened.reverse();
        flattened.retain(|&index| seen.insert(episode_rows[index].1.id.as_str()));
        flattened.reverse();
        flattened
    };
    let mut episodes = take_episode_rows(episode_rows, listed_rows);
    if let Some(missing) = query.is_missing {
        episodes.retain(|item| is_virtual_location(item) == missing);
    }
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
        let start_item_id = state
            .library
            .get_item_by_id(start_item_id)
            .await?
            .and_then(|item| item.primary_version_id)
            .and_then(|id| Uuid::parse_str(&id).ok())
            .unwrap_or(start_item_id);
        let start = ferrofin_db::store::guid_to_db(start_item_id);
        match episodes.iter().position(|e| e.id == start) {
            Some(pos) => drop(episodes.drain(..pos)),
            None => episodes.clear(),
        }
    }

    if let Some(adjacent) = query.adjacent_to.filter(|id| !id.is_nil()) {
        if let Some(index) = episodes
            .iter()
            .position(|episode| Uuid::parse_str(&episode.id).ok() == Some(adjacent))
        {
            let end = (index + 2).min(episodes.len());
            episodes = episodes.drain(index.saturating_sub(1)..end).collect();
        } else {
            episodes.clear();
        }
    }
    let mut ids: Vec<_> = episodes
        .iter()
        .filter_map(|episode| Uuid::parse_str(&episode.id).ok())
        .collect();
    if query.sort_by == Some(ItemSortBy::Random) && !ids.is_empty() {
        // Use the repository's existing randomized item order after source filters.
        ids = state
            .library
            .get_item_ids(&InternalItemsQuery {
                user: user.clone(),
                item_ids: ids,
                include_item_types: vec![BaseItemKind::Episode],
                order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
                dto_options: Arc::clone(&options),
                ..Default::default()
            })
            .await?;
    }
    let total = i32::try_from(ids.len()).unwrap_or(i32::MAX);
    let ids = paginate(ids, query.start_index, query.limit);
    // The page's full rows, by id alone: the listing above already applied
    // the user's access and the source filters.
    let mut full: std::collections::HashMap<_, _> = if ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        state
            .library
            .get_item_list(&InternalItemsQuery {
                item_ids: ids.clone(),
                include_item_types: vec![BaseItemKind::Episode],
                dto_options: Arc::clone(&options),
                ..Default::default()
            })
            .await?
            .into_iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect()
    };
    let page: Vec<_> = ids.iter().filter_map(|id| full.remove(id)).collect();
    let dtos = state
        .dto
        .get_base_item_dtos(&page, &options, user.as_deref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(query.start_index, Some(total), dtos)))
}

/// The query parameters honoured by `GET /Shows/{seriesId}/Seasons`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeasonsParams {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    /// Return the supplied episode and its adjacent siblings.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    Path(DotnetGuid(series_id)): Path<DotnetGuid>,
    Query(query): Query<SeasonsParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let series = state
        .library
        .get_item_by_id_for_user(series_id, user.as_deref())
        .await?
        .filter(|i| i.type_.ends_with("Series"))
        .ok_or_else(|| ApiError::NotFound(format!("series {series_id}")))?;
    let series_key = presentation_key(&series).to_owned();
    let options = Arc::new(build_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
        query.enable_user_data,
    ));

    let internal = InternalItemsQuery {
        user: user.clone(),
        series_presentation_unique_key: Some(series_key),
        include_item_types: vec![BaseItemKind::Season],
        order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
        is_special_season: query.is_special_season,
        is_missing: season_is_missing(user.as_deref(), query.is_missing),
        adjacent_to: query.adjacent_to,
        dto_options: Arc::clone(&options),
        ..InternalItemsQuery::default()
    };

    let seasons = state.library.get_item_list(&internal).await?;
    let total = i32::try_from(seasons.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&seasons, &options, user.as_deref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(None, Some(total), dtos)))
}

/// The seasons query's `IsMissing`: the request's `isMissing`, then C#'s
/// `SetSeasonQueryOptions` (v12.2 Series.cs:241-253), which runs after the
/// controller sets it and forces `false` for a user who does not display
/// missing episodes.
fn season_is_missing(user: Option<&UserEntity>, requested: Option<bool>) -> Option<bool> {
    match user {
        Some(user) if !user.display_missing_episodes => Some(false),
        _ => requested,
    }
}

/// Applies C#'s `ApplyPaging`: skip `start_index`, then take `limit`.
fn paginate<T>(items: Vec<T>, start_index: Option<i32>, limit: Option<i32>) -> Vec<T> {
    if start_index.is_none() && limit.is_none() {
        return items;
    }
    let start = start_index
        .and_then(|s| usize::try_from(s).ok())
        .unwrap_or(0);
    let mut page: Vec<T> = items.into_iter().skip(start).collect();
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
}
crate::query::query_parameters! {
    NextUpParams {
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Shows/NextUp")];
    UpcomingParams {
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Shows/Upcoming")];
    EpisodesParams {
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Shows/{seriesId}/Episodes")];
    SeasonsParams {
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Shows/{seriesId}/Seasons")];
}

#[cfg(test)]
mod episode_season_tests {
    use super::{SeasonEpisodeRow, season_episode_rows};
    use ferrofin_db::entities::base_items::{BaseItemEntity, aired_episode_order};

    fn season(number: Option<i64>, key: &str, physical: bool) -> BaseItemEntity {
        BaseItemEntity {
            type_: "Season".to_owned(),
            index_number: number,
            presentation_unique_key: Some(key.to_owned()),
            path: physical.then(|| "/media/season".to_owned()),
            ..Default::default()
        }
    }
    /// An episode whose season is `parent`, an index into the test's seasons.
    fn row(
        name: &str,
        number: Option<i64>,
        index: i64,
        air: &serde_json::Value,
        parent: Option<usize>,
    ) -> SeasonEpisodeRow {
        let item = BaseItemEntity {
            name: Some(name.to_owned()),
            type_: "Episode".to_owned(),
            parent_index_number: number,
            index_number: Some(index),
            data: Some(air.to_string()),
            path: Some("/media/episode.mkv".to_owned()),
            ..Default::default()
        };
        (aired_episode_order(&item), item, parent)
    }
    /// The names of the episodes [`season_episode_rows`] lists for `season`.
    fn listed(
        rows: &[SeasonEpisodeRow],
        seasons: &[BaseItemEntity],
        season: &BaseItemEntity,
        include_specials: bool,
        require_ancestry: bool,
    ) -> Vec<String> {
        season_episode_rows(rows, seasons, season, include_specials, require_ancestry)
            .into_iter()
            .map(|index| rows[index].1.name.clone().unwrap())
            .collect()
    }
    #[test]
    fn season_special_policy_and_aired_order_preserve_the_physical_specials_season() {
        let seasons = [season(Some(0), "zero", true), season(Some(1), "one", true)];
        let [zero, one] = [&seasons[0], &seasons[1]];
        // Input is SortName order; special's title deliberately sorts last.
        let rows = vec![
            row("One", Some(1), 1, &serde_json::json!({}), Some(1)),
            row("Two", Some(1), 2, &serde_json::json!({}), Some(1)),
            row(
                "Z Special",
                Some(0),
                4,
                &serde_json::json!({"AirsBeforeSeasonNumber":1,"AirsBeforeEpisodeNumber":2}),
                Some(0),
            ),
        ];
        assert_eq!(listed(&rows, &seasons, one, false, true), ["One", "Two"]);
        assert_eq!(
            listed(&rows, &seasons, one, true, false),
            ["One", "Z Special", "Two"]
        );
        assert_eq!(listed(&rows, &seasons, zero, true, false), ["Z Special"]);
        assert_eq!(listed(&rows, &seasons, one, false, false), ["One", "Two"]);
    }
    #[test]
    fn unknown_virtual_season_and_presentation_fallback_follow_source_rules() {
        let unknown = season(None, "unknown", false);
        let seasons = [
            season(Some(8), "elsewhere", false),
            season(Some(8), "physical", true),
            season(Some(99), "UNKNOWN", true),
        ];
        let rows = vec![
            row("No parent", None, 1, &serde_json::json!({}), None),
            row("Virtual parent", None, 2, &serde_json::json!({}), Some(0)),
            row("Physical parent", None, 3, &serde_json::json!({}), Some(1)),
            row("Key fallback", Some(8), 4, &serde_json::json!({}), Some(2)),
        ];
        assert_eq!(
            listed(&rows, &seasons, &unknown, true, false),
            ["No parent", "Virtual parent", "Key fallback"]
        );
        assert_eq!(
            listed(&rows, &seasons, &unknown, false, true),
            ["Key fallback"]
        );
        let physical_unknown = season(None, "unknown", true);
        assert_eq!(
            listed(&rows, &seasons, &physical_unknown, true, false),
            ["Key fallback"]
        );
    }
    #[test]
    fn specials_zero_keeps_sort_name_and_negative_seasons_use_airs_after_precedence() {
        let seasons = [season(Some(0), "zero", true)];
        let zero = &seasons[0];
        let negative = season(Some(-1), "negative", true);
        let rows = vec![
            row(
                "Alpha",
                Some(0),
                7,
                &serde_json::json!({"AirsAfterSeasonNumber":-1,"AirsBeforeSeasonNumber":2}),
                Some(0),
            ),
            row(
                "Beta",
                Some(0),
                1,
                &serde_json::json!({"AirsBeforeSeasonNumber":-1}),
                Some(0),
            ),
        ];
        assert_eq!(
            listed(&rows, &seasons, zero, true, false),
            ["Alpha", "Beta"]
        );
        assert_eq!(
            listed(&rows, &seasons, &negative, true, false),
            ["Beta", "Alpha"]
        );
        let found = listed(&rows, &seasons, &negative, false, true);
        assert!(found.is_empty(), "{found:?}");
    }
    #[test]
    fn regular_episode_aired_season_numbers_are_used_and_bad_data_is_neutral() {
        let one = season(Some(1), "one", true);
        let seasons = [season(Some(2), "two", true)];
        let rows = vec![row(
            "Moved",
            Some(2),
            1,
            &serde_json::json!({"AirsAfterSeasonNumber":1,"AirsBeforeSeasonNumber":3}),
            Some(0),
        )];
        assert_eq!(listed(&rows, &seasons, &one, true, false), ["Moved"]);
        let found = listed(&rows, &seasons, &one, false, true);
        assert!(found.is_empty(), "{found:?}");
        let item = BaseItemEntity {
            parent_index_number: Some(3),
            data: Some("broken".to_owned()),
            ..Default::default()
        };
        assert_eq!(aired_episode_order(&item).airs_before_season, None);
        assert_eq!(aired_episode_order(&item).season, Some(3));
    }
}

#[cfg(test)]
mod season_missing_tests {
    use super::season_is_missing;
    use crate::test_support::fake_user_entity;
    use rstest::rstest;

    /// `GetSeasons` sets `IsMissing = isMissing`, then `SetSeasonQueryOptions`
    /// forces `false` for a user who hides missing episodes: their setting
    /// wins over the request. A user who displays them, or no user, keeps the
    /// request's value.
    #[rstest]
    #[case(Some(false), Some(true), Some(false))]
    #[case(Some(false), Some(false), Some(false))]
    #[case(Some(false), None, Some(false))]
    #[case(Some(true), Some(true), Some(true))]
    #[case(Some(true), Some(false), Some(false))]
    #[case(Some(true), None, None)]
    #[case(None, Some(true), Some(true))]
    #[case(None, None, None)]
    fn the_user_setting_wins_over_is_missing(
        #[case] display_missing: Option<bool>,
        #[case] requested: Option<bool>,
        #[case] expected: Option<bool>,
    ) {
        let user = display_missing.map(|display| {
            let mut user = fake_user_entity(uuid::Uuid::from_u128(1), "viewer");
            user.display_missing_episodes = display;
            user
        });
        assert_eq!(season_is_missing(user.as_ref(), requested), expected);
    }
}
