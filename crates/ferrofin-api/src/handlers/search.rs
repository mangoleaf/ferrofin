//! `SearchController` — search-hint typeahead.
//!
//! Ports `GET /Search/Hints`: a ranked list of [`SearchHint`]s for an
//! autocomplete term, built through the
//! [`SearchManager`](ferrofin_traits::library::SearchManager) seam and wrapped in a
//! [`SearchHintResult`]. The per-hint image tags Jellyfin resolves through its
//! `IImageProcessor` are left unset here (the image processor is a later wave);
//! the search manager already fills the textual hint fields.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::search::{SearchHintResult, SearchQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::parse_csv_enums_lenient;
use crate::state::AppState;

/// The query parameters honoured by `GET /Search/Hints`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct SearchHintsQuery {
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// The target user; scopes the search to a user's library when present.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The search term to filter on (required).
    search_term: String,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited [`BaseItemKind`] set to exclude.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited [`MediaType`] set to include.
    #[serde(default)]
    media_types: Option<String>,
    /// Localizes the search to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Optional live-tv "is movie" filter.
    #[serde(default)]
    is_movie: Option<bool>,
    /// Optional live-tv "is series" filter.
    #[serde(default)]
    is_series: Option<bool>,
    /// Optional live-tv "is news" filter.
    #[serde(default)]
    is_news: Option<bool>,
    /// Optional live-tv "is kids" filter.
    #[serde(default)]
    is_kids: Option<bool>,
    /// Optional live-tv "is sports" filter.
    #[serde(default)]
    is_sports: Option<bool>,
    /// Whether to include people hints (defaults `true`).
    #[serde(default = "default_true")]
    include_people: bool,
    /// Whether to include media hints (defaults `true`).
    #[serde(default = "default_true")]
    include_media: bool,
    /// Whether to include genre hints (defaults `true`).
    #[serde(default = "default_true")]
    include_genres: bool,
    /// Whether to include studio hints (defaults `true`).
    #[serde(default = "default_true")]
    include_studios: bool,
    /// Whether to include artist hints (defaults `true`).
    #[serde(default = "default_true")]
    include_artists: bool,
}

/// The `true` default for the `include*` toggles (serde needs a fn).
fn default_true() -> bool {
    true
}

/// `GET /Search/Hints` — ranked search-hint typeahead.
///
/// Port of `SearchController.GetSearchHints`.
#[utoipa::path(
    get,
    path = "/Search/Hints",
    responses((status = 200, description = "Search hints returned", body = SearchHintResult)),
    tag = "ferrofin"
)]
async fn get_search_hints(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<SearchHintsQuery>,
) -> Result<Json<SearchHintResult>, ApiError> {
    // A user id resolves for visibility scoping; an absent one is the nil id (the
    // C# `RequestHelpers.GetUserId` fallback), searching all libraries.
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let user_id = user
        .as_ref()
        .and_then(|u| Uuid::parse_str(&u.id).ok())
        .unwrap_or_else(Uuid::nil);

    let include_item_types: Vec<BaseItemKind> =
        parse_csv_enums_lenient(query.include_item_types.as_deref());
    let exclude_item_types: Vec<BaseItemKind> =
        parse_csv_enums_lenient(query.exclude_item_types.as_deref());
    let media_types: Vec<MediaType> = parse_csv_enums_lenient(query.media_types.as_deref());

    let search_query = SearchQuery {
        user_id,
        search_term: query.search_term.clone(),
        start_index: query.start_index,
        limit: query.limit,
        include_people: query.include_people,
        include_media: query.include_media,
        include_genres: query.include_genres,
        include_studios: query.include_studios,
        include_artists: query.include_artists,
        media_types,
        include_item_types,
        exclude_item_types,
        parent_id: query.parent_id,
        is_movie: query.is_movie,
        is_series: query.is_series,
        is_news: query.is_news,
        is_kids: query.is_kids,
        is_sports: query.is_sports,
    };

    let result = state.search.get_search_hints(&search_query).await?;
    Ok(Json(SearchHintResult::new(
        result.items,
        result.total_record_count,
    )))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Search/Hints", get(get_search_hints))
}
