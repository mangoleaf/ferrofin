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
//! applied by the persistence layer where portable, exactly as for `/Items`),
//! including 12.1's `audioLanguages` / `subtitleLanguages` stream filters and
//! the media-stream rules `GetItems` applies to them.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::items::{apply_media_stream_filter_rules, resolve_user};
use crate::handlers::query_parse::{
    parse_csv_enums_lenient, parse_csv_strings, parse_csv_uuids, parse_pipe_strings,
};
use crate::state::AppState;

/// The paging/search/sort/filter subset of `GET /Trailers` this port honors.
///
/// Mirrors the shared slice of `GET /Items`; the trailer browse pins the item
/// type to `Trailer`, so `includeItemTypes` is not a parameter here.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrailersQuery {
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
    /// Whether to search descendants recursively.
    #[serde(default)]
    recursive: Option<bool>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    /// Comma-delimited [`LocationType`](ferrofin_model::entities::LocationType)
    /// set to include; `IsVirtualItem` follows, as on `GET /Items`.
    #[serde(default)]
    location_types: Option<String>,
    /// Comma-delimited `LocationType` set to exclude.
    #[serde(default)]
    exclude_location_types: Option<String>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
    /// Comma-delimited audio-stream languages (12.1 `audioLanguages`).
    #[serde(default)]
    audio_languages: Option<String>,
    /// Comma-delimited subtitle-stream languages (12.1 `subtitleLanguages`).
    #[serde(default)]
    subtitle_languages: Option<String>,
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

/// Maps the trailer query onto an [`InternalItemsQuery`] pinned to the
/// `Trailer` kind, applying `GetItems`' controller rules in upstream order.
///
/// The user row moves into the query and is borrowed back out for the DTO
/// projection — `resolve_user` already cloned it off the auth context, and a
/// second full copy of every string on it buys nothing.
fn trailers_query_options(
    query: &TrailersQuery,
    user: UserEntity,
) -> Result<InternalItemsQuery, ApiError> {
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
        audio_languages: parse_csv_strings(query.audio_languages.as_deref()),
        subtitle_languages: parse_csv_strings(query.subtitle_languages.as_deref()),
        ..InternalItemsQuery::default()
    };
    // `GetItems` applies its media-stream rules before `ApplyFilters`
    // (v12 ItemsController.cs:462-489).
    apply_media_stream_filter_rules(&mut internal);
    // …and orders a search by score, then `SortName`.
    crate::handlers::items::order_search_by_sort_name(&mut internal);
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }
    // `TrailersController.GetTrailers` hands its arguments straight to
    // `ItemsController.GetItems`, so the same two parameters drive
    // `IsVirtualItem` here.
    crate::handlers::query_parse::apply_location_types(
        &mut internal.is_virtual_item,
        query.location_types.as_deref(),
        query.exclude_location_types.as_deref(),
    );
    let filters = parse_csv_enums_lenient(query.filters.as_deref());
    internal
        .apply_filters(&filters)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(internal)
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
    let internal = trailers_query_options(&query, user)?;

    let mut result = state.library.query_items(&internal).await?;
    // `GetItems` answers explicit ids through `Folder.GetItems`.
    crate::handlers::items::sort_items_by_request(&internal, &mut result.items);
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
crate::query::query_parameters! {
    TrailersQuery {
        "sortBy" => ',',
        "sortOrder" => ',',
        "filters" => ',',
        "genres" => '|',
        "genreIds" => ',',
        "ids" => ',',
        "excludeItemIds" => ',',
        "locationTypes" => ',',
        "excludeLocationTypes" => ',',
        "audioLanguages" => ',',
        "subtitleLanguages" => ',',
    } => [("get", "/Trailers")];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `audioLanguages` / `subtitleLanguages` reach the trailer query and
    /// `GetItems`' media-stream rules run (v12 TrailersController.cs:209-210;
    /// ItemsController.cs:462-487). The repeated `audioLanguages` key pins
    /// Ferrofin's own leniency, not upstream's binding: the shared query
    /// normalizer joins repeated keys before the comma split, so every piece
    /// binds, where upstream binds `["eng,ger", "fre"]` (see
    /// `parse_csv_strings`).
    #[test]
    fn trailers_bind_the_stream_language_filters() {
        let bind = |q: &str| {
            let Query(query) = Query::<TrailersQuery>::try_from_uri(
                &format!("/Trailers?{q}").parse().expect("uri"),
            )
            .expect(q);
            trailers_query_options(&query, crate::test_support::sample_user()).expect("maps")
        };
        let internal = bind("audioLanguages=eng,ger&audioLanguages=fre&subtitleLanguages=spa");
        assert_eq!(internal.include_item_types, [BaseItemKind::Trailer]);
        assert_eq!(internal.audio_languages, ["eng", "ger", "fre"]);
        assert_eq!(internal.subtitle_languages, ["spa"]);
        assert!(internal.include_owned_items);

        let internal = bind("recursive=true");
        assert!(
            internal.audio_languages.is_empty(),
            "{:?}",
            internal.audio_languages
        );
        assert!(
            internal.subtitle_languages.is_empty(),
            "{:?}",
            internal.subtitle_languages
        );
        assert!(!internal.include_owned_items);
    }

    /// `GetTrailers` hands its arguments to `ItemsController.GetItems`, so a
    /// search is ordered as there: `SortName` leads the caller's keys (see
    /// `items::order_search_by_sort_name`).
    #[test]
    fn trailers_order_a_search_by_sort_name_before_the_callers_keys() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        let order_by = |q: &str| {
            let Query(query) = Query::<TrailersQuery>::try_from_uri(
                &format!("/Trailers?{q}").parse().expect("uri"),
            )
            .expect(q);
            trailers_query_options(&query, crate::test_support::sample_user())
                .expect("maps")
                .order_by
        };
        assert_eq!(
            order_by("searchTerm=blade&sortBy=PremiereDate"),
            [
                (ItemSortBy::SortName, SortOrder::Ascending),
                (ItemSortBy::PremiereDate, SortOrder::Ascending)
            ]
        );
        assert_eq!(
            order_by("sortBy=PremiereDate"),
            [(ItemSortBy::PremiereDate, SortOrder::Ascending)]
        );
    }
}
