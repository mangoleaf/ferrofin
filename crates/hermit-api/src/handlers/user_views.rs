//! `UserViewsController` — a user's home-screen views.
//!
//! Ports `GET /UserViews`: resolves the target user, fetches their views via the
//! [`UserViewManager`](hermit_traits::library::UserViewManager), projects each
//! to a [`BaseItemDto`] with the [`DtoService`], and returns them as a
//! [`QueryResult`].

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use hermit_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// Query parameters for `GET /UserViews`.
///
/// `userId` is optional in the contract; when omitted it defaults to the
/// authenticated caller (Jellyfin's `RequestHelpers.GetUserId`).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserViewsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `GET /UserViews` — the target user's library views.
///
/// Port of `UserViewsController.GetUserViews`.
#[utoipa::path(
    get,
    path = "/UserViews",
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `hermit-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "User views returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_user_views(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserViewsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
    let folders = state.user_views.get_user_views(user_id).await?;
    let options = DtoOptions::with_all_fields(false);
    let dtos = state
        .dto
        .get_base_item_dtos(&folders, &options, Some(&user), None, true)
        .await?;
    Ok(Json(QueryResult::from_items(dtos)))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/UserViews", get(get_user_views))
}
