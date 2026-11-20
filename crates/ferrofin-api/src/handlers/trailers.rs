//! `TrailersController` — the obsolete trailer browse.
//!
//! Ports `GET /Trailers`: Jellyfin hands every argument straight to
//! `ItemsController.GetItems`, with `includeItemTypes = [Trailer]` and no
//! `indexNumber`. This handler does the same, through `items::get_items`, so
//! the trailer browse honours every `GET /Items` parameter and rule.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::items::{ItemsQuery, get_items};
use crate::state::AppState;

/// `GET /Trailers` — the obsolete trailer browse.
///
/// Port of `TrailersController.GetTrailers` (v12.4 TrailersController.cs:126-306):
/// `GetItems` with the `Trailer` kind. The route has no `includeItemTypes` or
/// `indexNumber` of its own, so either one a client sends is replaced; it still
/// binds first, so an `indexNumber` that is no number answers 400.
#[utoipa::path(
    get,
    path = "/Trailers",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Trailers returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_trailers(
    state: State<AppState>,
    auth: RequireAuth,
    Query(mut query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.include_item_types = Some("Trailer".to_owned());
    query.index_number = None;
    get_items(state, auth, Query(query)).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Trailers", get(get_trailers))
}
