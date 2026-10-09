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
//! Season filters and aired special positioning use persisted season identities,
//! presentation keys and episode metadata, with user visibility applied first.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::{ItemFields, QueryResult};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::tv::NextUpQuery;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
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

fn nonempty_relation_id(value: Option<&str>) -> Option<Uuid> {
    value
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|id| !id.is_nil())
}

fn virtual_location(item: &BaseItemEntity) -> bool {
    // LocationType is derived from Path and SourceType, not IsVirtualItem.
    // Channel items without a path are remote; LiveTV overrides the base key.
    let kind = item.type_.rsplit('.').next().unwrap_or(&item.type_);
    let channel = kind == "Channel"
        || (!matches!(
            kind,
            "LiveTvChannel" | "TvChannel" | "LiveTvProgram" | "TvProgram"
        ) && nonempty_relation_id(item.channel_id.as_deref()).is_some());
    item.path.as_deref().is_none_or(str::is_empty) && !channel
}

fn episode_air_order(item: &BaseItemEntity) -> ferrofin_model::entities_media::AiredEpisodeOrder {
    let data = item
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok());
    let number = |key| {
        data.as_ref()
            .and_then(|data| data.get(key))
            .and_then(serde_json::Value::as_i64)
    };
    ferrofin_model::entities_media::AiredEpisodeOrder {
        season: item.parent_index_number,
        episode: item.index_number,
        premiere: item.premiere_date,
        airs_before_season: number("AirsBeforeSeasonNumber"),
        airs_after_season: number("AirsAfterSeasonNumber"),
        airs_before_episode: number("AirsBeforeEpisodeNumber"),
    }
}

type SeasonEpisodeRow = (
    ferrofin_model::entities_media::AiredEpisodeOrder,
    BaseItemEntity,
    Option<BaseItemEntity>,
);

fn season_episode_rows(
    episodes: &[SeasonEpisodeRow],
    season: &BaseItemEntity,
    include_specials: bool,
    require_ancestry: bool,
) -> Vec<BaseItemEntity> {
    let number = season.index_number;
    let season_key = series_presentation_key(season);
    let support_specials = include_specials && number.is_some_and(|number| number != 0);
    let mut matches: Vec<_> = episodes
        .iter()
        .filter(|(order, item, item_season)| {
            let same_season = item_season.as_ref().is_some_and(|known| {
                series_presentation_key(known).eq_ignore_ascii_case(&season_key)
            });
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
            if current.is_none() && number.is_none() && virtual_location(season) {
                return item_season.as_ref().is_none_or(virtual_location);
            }
            same_season
        })
        .collect();
    if number != Some(0) {
        matches.sort_by(|(left, _, _), (right, _, _)| left.compare(right));
    }
    // Stable input SortName order remains for season zero and comparator ties.
    matches
        .into_iter()
        .map(|(_, item, _)| item.clone())
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
#[allow(clippy::too_many_lines)] // source season resolution, positioning and request filters
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
    // Source admits missing rows before applying the explicit request filter.
    let include_missing =
        auth.is_api_key || user.as_ref().is_some_and(|u| u.display_missing_episodes);
    let include_specials = state
        .config
        .configuration()
        .await?
        .display_specials_within_seasons;
    let (series_key, selected_season, actual_series_id) = if let Some(season_id) = query.season_id {
        let season = state
            .library
            .get_item_by_id_for_user(season_id, user.as_ref())
            .await?
            .filter(|item| item.type_.ends_with("Season"))
            .ok_or_else(|| ApiError::NotFound(format!("No season exists with Id {season_id}")))?;
        // Season.GetEpisodes follows its actual Series, even when the route's
        // series id names another row. An orphan season has no episodes.
        let actual_series = nonempty_relation_id(season.series_id.as_deref())
            .or_else(|| nonempty_relation_id(season.parent_id.as_deref()));
        let Some(actual_series) = actual_series else {
            return Ok(Json(QueryResult::new(
                query.start_index,
                Some(0),
                Vec::new(),
            )));
        };
        let Some(series) = state
            .library
            .get_item_by_id(actual_series)
            .await?
            .filter(|item| item.type_.ends_with("Series"))
        else {
            return Ok(Json(QueryResult::new(
                query.start_index,
                Some(0),
                Vec::new(),
            )));
        };
        let key = series_presentation_key(&series);
        (key, Some(season), actual_series)
    } else {
        let series = state
            .library
            .get_item_by_id_for_user(series_id, user.as_ref())
            .await?
            .filter(|item| item.type_.ends_with("Series"))
            .ok_or_else(|| ApiError::NotFound("Series not found".to_owned()))?;
        (series_presentation_key(&series), None, series_id)
    };
    // Fetch visible rows in source SortName order. Parse aired metadata once, then
    // apply the same in-memory comparator used by NextUp before filtering/paging.
    let rows = state
        .library
        .get_item_list(&InternalItemsQuery {
            user: user.clone(),
            series_presentation_unique_key: Some(series_key),
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            is_missing: (!include_missing).then_some(false),
            dto_options: options.clone(),
            ..Default::default()
        })
        .await?;
    let mut seasons: Vec<_> = rows
        .iter()
        .filter(|item| item.type_.ends_with("Season"))
        .cloned()
        .collect();
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
    let mut known_seasons: std::collections::HashMap<Uuid, Option<BaseItemEntity>> = seasons
        .iter()
        .filter_map(|season| {
            Uuid::parse_str(&season.id)
                .ok()
                .map(|id| (id, Some(season.clone())))
        })
        .collect();
    let mut episode_rows = Vec::new();
    for row in rows
        .into_iter()
        .filter(|item| item.type_.ends_with("Episode"))
    {
        let season_id = nonempty_relation_id(row.season_id.as_deref())
            .or_else(|| nonempty_relation_id(row.parent_id.as_deref()));
        let episode_season = if let Some(id) = season_id {
            if let Some(season) = known_seasons.get(&id) {
                season.clone()
            } else {
                let season = state
                    .library
                    .get_item_by_id(id)
                    .await?
                    .filter(|item| item.type_.ends_with("Season"));
                known_seasons.insert(id, season.clone());
                season
            }
        } else {
            None
        };
        // Episode.FindSeasonId also resolves episodes stored directly under
        // the series by their physical season number when SeasonId is empty.
        let episode_season = episode_season.or_else(|| {
            (nonempty_relation_id(row.parent_id.as_deref()) == Some(actual_series_id))
                .then(|| {
                    seasons
                        .iter()
                        .find(|season| season.index_number == row.parent_index_number)
                        .cloned()
                })
                .flatten()
        });
        episode_rows.push((episode_air_order(&row), row, episode_season));
    }
    let mut episodes = if let Some(season) = selected {
        season_episode_rows(&episode_rows, &season, include_specials, !include_specials)
    } else {
        // Series.GetEpisodes keeps the last appearance of a positioned special,
        // placing it in its aired season rather than duplicating season zero.
        let mut flattened = Vec::new();
        for season in &seasons {
            flattened.extend(season_episode_rows(
                &episode_rows,
                season,
                include_specials,
                false,
            ));
        }
        let mut seen = std::collections::HashSet::new();
        flattened.reverse();
        flattened.retain(|item| seen.insert(item.id.clone()));
        flattened.reverse();
        flattened
    };
    if let Some(missing) = query.is_missing {
        episodes.retain(|item| virtual_location(item) == missing);
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
    if query.sort_by == Some(ItemSortBy::Random) {
        // Use the repository's existing randomized item order after source filters.
        let ids: Vec<_> = episodes
            .iter()
            .filter_map(|episode| Uuid::parse_str(&episode.id).ok())
            .collect();
        if !ids.is_empty() {
            let rows = state
                .library
                .get_item_list(&InternalItemsQuery {
                    user: user.clone(),
                    item_ids: ids,
                    include_item_types: vec![BaseItemKind::Episode],
                    order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
                    dto_options: options.clone(),
                    ..Default::default()
                })
                .await?;
            episodes = rows;
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
    Path(series_id): Path<Uuid>,
    Query(query): Query<SeasonsParams>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let series = state
        .library
        .get_item_by_id_for_user(series_id, user.as_ref())
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    let mut options = build_dto_options(query.fields.as_deref(), None, None, None, None);
    // `SimilarItemsManager.GetSimilarItemsAsync` forces `ProviderIds` into the
    // options the controller then projects with (v12
    // SimilarItemsManager.cs:108-112) — same as the five aliases in
    // `handlers::similar`.
    if !options.contains_field(ItemFields::ProviderIds) {
        options.fields.push(ItemFields::ProviderIds);
    }

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
    SimilarParams {
        "excludeArtistIds" => ',',
        "fields" => ',',
    } => [("get", "/Shows/{itemId}/Similar")];
}

#[cfg(test)]
mod episode_season_tests {
    use super::{episode_air_order, season_episode_rows, virtual_location};
    use ferrofin_db::entities::base_items::BaseItemEntity;

    fn season(number: Option<i64>, key: &str, physical: bool) -> BaseItemEntity {
        BaseItemEntity {
            type_: "Season".to_owned(),
            index_number: number,
            presentation_unique_key: Some(key.to_owned()),
            path: physical.then(|| "/media/season".to_owned()),
            ..Default::default()
        }
    }
    fn row(
        name: &str,
        number: Option<i64>,
        index: i64,
        air: &serde_json::Value,
        parent: Option<&BaseItemEntity>,
    ) -> super::SeasonEpisodeRow {
        let item = BaseItemEntity {
            name: Some(name.to_owned()),
            type_: "Episode".to_owned(),
            parent_index_number: number,
            index_number: Some(index),
            data: Some(air.to_string()),
            path: Some("/media/episode.mkv".to_owned()),
            ..Default::default()
        };
        (episode_air_order(&item), item, parent.cloned())
    }
    fn names(rows: Vec<BaseItemEntity>) -> Vec<String> {
        rows.into_iter().map(|row| row.name.unwrap()).collect()
    }
    #[test]
    fn season_special_policy_and_aired_order_preserve_the_physical_specials_season() {
        let zero = season(Some(0), "zero", true);
        let one = season(Some(1), "one", true);
        // Input is SortName order; special's title deliberately sorts last.
        let rows = vec![
            row("One", Some(1), 1, &serde_json::json!({}), Some(&one)),
            row("Two", Some(1), 2, &serde_json::json!({}), Some(&one)),
            row(
                "Z Special",
                Some(0),
                4,
                &serde_json::json!({"AirsBeforeSeasonNumber":1,"AirsBeforeEpisodeNumber":2}),
                Some(&zero),
            ),
        ];
        assert_eq!(
            names(season_episode_rows(&rows, &one, false, true)),
            ["One", "Two"]
        );
        assert_eq!(
            names(season_episode_rows(&rows, &one, true, false)),
            ["One", "Z Special", "Two"]
        );
        assert_eq!(
            names(season_episode_rows(&rows, &zero, true, false)),
            ["Z Special"]
        );
        assert_eq!(
            names(season_episode_rows(&rows, &one, false, false)),
            ["One", "Two"]
        );
    }
    #[test]
    fn unknown_virtual_season_and_presentation_fallback_follow_source_rules() {
        let unknown = season(None, "unknown", false);
        let virtual_parent = season(Some(8), "elsewhere", false);
        let physical_parent = season(Some(8), "physical", true);
        let same = season(Some(99), "UNKNOWN", true);
        let rows = vec![
            row("No parent", None, 1, &serde_json::json!({}), None),
            row(
                "Virtual parent",
                None,
                2,
                &serde_json::json!({}),
                Some(&virtual_parent),
            ),
            row(
                "Physical parent",
                None,
                3,
                &serde_json::json!({}),
                Some(&physical_parent),
            ),
            row(
                "Key fallback",
                Some(8),
                4,
                &serde_json::json!({}),
                Some(&same),
            ),
        ];
        assert_eq!(
            names(season_episode_rows(&rows, &unknown, true, false)),
            ["No parent", "Virtual parent", "Key fallback"]
        );
        assert_eq!(
            names(season_episode_rows(&rows, &unknown, false, true)),
            ["Key fallback"]
        );
        let physical_unknown = season(None, "unknown", true);
        assert_eq!(
            names(season_episode_rows(&rows, &physical_unknown, true, false)),
            ["Key fallback"]
        );
    }
    #[test]
    fn specials_zero_keeps_sort_name_and_negative_seasons_use_airs_after_precedence() {
        let zero = season(Some(0), "zero", true);
        let negative = season(Some(-1), "negative", true);
        let rows = vec![
            row(
                "Alpha",
                Some(0),
                7,
                &serde_json::json!({"AirsAfterSeasonNumber":-1,"AirsBeforeSeasonNumber":2}),
                Some(&zero),
            ),
            row(
                "Beta",
                Some(0),
                1,
                &serde_json::json!({"AirsBeforeSeasonNumber":-1}),
                Some(&zero),
            ),
        ];
        assert_eq!(
            names(season_episode_rows(&rows, &zero, true, false)),
            ["Alpha", "Beta"]
        );
        assert_eq!(
            names(season_episode_rows(&rows, &negative, true, false)),
            ["Beta", "Alpha"]
        );
        assert!(season_episode_rows(&rows, &negative, false, true).is_empty());
    }
    #[test]
    fn virtual_location_uses_path_and_actual_channel_source_instead_of_the_saved_bit() {
        let mut row = BaseItemEntity {
            type_: "Episode".into(),
            path: Some("/media/episode.mkv".into()),
            is_virtual_item: true,
            ..Default::default()
        };
        assert!(!virtual_location(&row));
        row.path = None;
        row.is_virtual_item = false;
        assert!(virtual_location(&row));
        row.channel_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(!virtual_location(&row));
        row.type_ = "LiveTvProgram".into();
        assert!(virtual_location(&row));
        row.type_ = "Channel".into();
        row.channel_id = None;
        assert!(!virtual_location(&row));
    }

    #[test]
    fn regular_episode_aired_season_numbers_are_used_and_bad_data_is_neutral() {
        let one = season(Some(1), "one", true);
        let two = season(Some(2), "two", true);
        let rows = vec![row(
            "Moved",
            Some(2),
            1,
            &serde_json::json!({"AirsAfterSeasonNumber":1,"AirsBeforeSeasonNumber":3}),
            Some(&two),
        )];
        assert_eq!(
            names(season_episode_rows(&rows, &one, true, false)),
            ["Moved"]
        );
        assert!(season_episode_rows(&rows, &one, false, true).is_empty());
        let item = BaseItemEntity {
            parent_index_number: Some(3),
            data: Some("broken".to_owned()),
            ..Default::default()
        };
        assert_eq!(episode_air_order(&item).airs_before_season, None);
        assert_eq!(episode_air_order(&item).season, Some(3));
    }
}
