//! `UserViewsController` — a user's home-screen views.
//!
//! Ports:
//!
//! - `GET /UserViews` — resolves the target user, fetches their views via the
//!   [`UserViewManager`](hermit_traits::library::UserViewManager), projects each
//!   to a [`BaseItemDto`] with the [`DtoService`], and returns them as a
//!   [`QueryResult`].
//! - `GET /UserViews/GroupingOptions` — the user's grouping-eligible library
//!   folders as [`SpecialViewOptionDto`] `{ Name, Id }` pairs, name-sorted.
//!
//! Port note — grouping eligibility: C#'s `UserView.IsEligibleForGrouping` keeps
//! only collection folders whose `CollectionType` is `movies`/`tvshows`/unset.
//! That per-folder collection-type metadata is not carried on the persisted
//! [`BaseItemEntity`] rows at this seam (the same grouping metadata the
//! `UserViewManager` port already documents as deferred), so the portable
//! equivalent offers every top-level view folder the user sees — the superset the
//! C# filter narrows. The projection, id format (`guid.simple`), name-ordering,
//! and `404`-on-missing-user outcomes are already the final ones.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::dto::{BaseItemDto, SpecialViewOptionDto};
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

/// `GET /UserViews/GroupingOptions` — the user's grouping-eligible views.
///
/// Port of `UserViewsController.GetGroupingOptions`: resolves the user (a missing
/// user is `404`), takes their top-level view folders, and returns each as a
/// [`SpecialViewOptionDto`] `{ Name, Id }`, id rendered as a dashless guid and
/// the list ordered by name (see the module docs on the eligibility superset).
#[utoipa::path(
    get,
    path = "/UserViews/GroupingOptions",
    params(("userId" = Option<String>, Query, description = "The user id")),
    responses(
        (status = 200, description = "Grouping options returned", body = [SpecialViewOptionDto]),
        (status = 404, description = "User not found"),
    ),
    tag = "hermit"
)]
async fn get_grouping_options(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserViewsQuery>,
) -> Result<Json<Vec<SpecialViewOptionDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
    let folders = state.user_views.get_user_views(user_id).await?;
    let mut options: Vec<SpecialViewOptionDto> = folders
        .into_iter()
        .map(|folder| SpecialViewOptionDto {
            name: folder.name.clone(),
            // C#'s `Id.ToString("N")` — a dashless guid. Fall back to the raw id
            // when it is not a parseable guid.
            id: Some(
                Uuid::parse_str(&folder.id)
                    .map_or_else(|_| folder.id.clone(), |g| g.simple().to_string()),
            ),
        })
        .collect();
    options.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(options))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/UserViews", get(get_user_views))
        .route("/UserViews/GroupingOptions", get(get_grouping_options))
}
