//! `TrailersController` — the obsolete trailer browse.
//!
//! Ports `GET /Trailers`: Jellyfin delegates this straight to
//! `ItemsController.GetItems` with `includeItemTypes = [Trailer]`. This handler
//! does the same — it maps the shared paging/search/sort/filter query onto an
//! [`InternalItemsQuery`] pinned to the `Trailer` kind and runs it through the
//! library manager, projecting the page to [`BaseItemDto`]s.
//!
//! The controller carries Jellyfin's full ~90-parameter item query; the
//! persistable subset shared with `GET /Items` is honored here (the remainder is
//! applied by the persistence layer where portable, exactly as for `/Items`).

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_csv_uuids, parse_pipe_strings};
use crate::state::AppState;

/// The paging/search/sort/filter subset of `GET /Trailers` this port honors.
///
/// Mirrors the shared slice of `GET /Items`; the trailer browse pins the item
/// type to `Trailer`, so `includeItemTypes` is not a parameter here.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrailersQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Whether to search descendants recursively.
    #[serde(default)]
    recursive: Option<bool>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`ItemSortBy`] columns.
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited [`SortOrder`] directions.
    #[serde(default)]
    sort_order: Option<String>,
    /// Comma-delimited [`ItemFilter`](ferrofin_model::querying::ItemFilter) flags.
    #[serde(default)]
    filters: Option<String>,
    /// Pipe-delimited genre names.
    #[serde(default)]
    genres: Option<String>,
    /// Comma-delimited genre ids.
    #[serde(default)]
    genre_ids: Option<String>,
    /// Comma-delimited explicit item ids to fetch.
    #[serde(default)]
    ids: Option<String>,
    /// Comma-delimited item ids to exclude.
    #[serde(default)]
    exclude_item_ids: Option<String>,
    /// Restrict to favourited items.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Restrict to played / unplayed items.
    #[serde(default)]
    is_played: Option<bool>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
}

/// Builds the `order_by` list from parallel `sort_by`/`sort_order` lists.
///
/// Mirrors `RequestHelpers.GetOrderBy`: each column pairs with the order at its
/// index, falling back to the last supplied order (then ascending).
fn parse_order_by(sort_by: Option<&str>, sort_order: Option<&str>) -> Vec<(ItemSortBy, SortOrder)> {
    let columns: Vec<ItemSortBy> = parse_csv_enums_lenient(sort_by);
    let orders: Vec<SortOrder> = parse_csv_enums_lenient(sort_order);
    columns
        .into_iter()
        .enumerate()
        .map(|(i, column)| {
            let order = orders
                .get(i)
                // C# pads missing orders with the FIRST requested order.
                .or_else(|| orders.first())
                .copied()
                .unwrap_or(SortOrder::Ascending);
            (column, order)
        })
        .collect()
}

/// `GET /Trailers` — the obsolete trailer browse.
///
/// Port of `TrailersController.GetTrailers` (delegates to `GetItems` pinned to
/// the `Trailer` kind).
#[utoipa::path(
    get,
    path = "/Trailers",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Trailers returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_trailers(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<TrailersQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;

    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let mut internal = InternalItemsQuery {
        user: Some(user),
        include_item_types: vec![BaseItemKind::Trailer],
        start_index: query.start_index,
        limit: query.limit,
        recursive: query.recursive.unwrap_or(false),
        search_term: query.search_term.clone(),
        order_by: parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref()),
        genres: parse_pipe_strings(query.genres.as_deref()),
        genre_ids: parse_csv_uuids(query.genre_ids.as_deref())?,
        item_ids: parse_csv_uuids(query.ids.as_deref())?,
        exclude_item_ids: parse_csv_uuids(query.exclude_item_ids.as_deref())?,
        is_favorite: query.is_favorite,
        is_played: query.is_played,
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }
    let filters = parse_csv_enums_lenient(query.filters.as_deref());
    internal
        .apply_filters(&filters)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let result = state.library.query_items(&internal).await?;
    let options = DtoOptions::with_all_fields(false);
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

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Trailers", get(get_trailers))
}
