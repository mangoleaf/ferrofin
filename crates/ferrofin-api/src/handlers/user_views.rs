//! `UserViewsController` — a user's home-screen views.
//!
//! Ports:
//!
//! - `GET /UserViews` — resolves the target user, fetches their views via the
//!   [`UserViewManager`](ferrofin_traits::library::UserViewManager), projects each
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

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::CollectionType;
use ferrofin_model::dto::{BaseItemDto, SpecialViewOptionDto};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::{resolve_user, user_uuid};
use crate::state::AppState;

/// Maps a library's [`CollectionTypeOptions`] to the DTO [`CollectionType`] the
/// web client keys presentation off. `mixed` has no single type → `None` (a
/// generic view), matching Jellyfin.
fn map_collection_type(options: CollectionTypeOptions) -> Option<CollectionType> {
    Some(match options {
        CollectionTypeOptions::movies => CollectionType::movies,
        CollectionTypeOptions::tvshows => CollectionType::tvshows,
        CollectionTypeOptions::music => CollectionType::music,
        CollectionTypeOptions::musicvideos => CollectionType::musicvideos,
        CollectionTypeOptions::homevideos => CollectionType::homevideos,
        CollectionTypeOptions::boxsets => CollectionType::boxsets,
        CollectionTypeOptions::books => CollectionType::books,
        CollectionTypeOptions::mixed => return None,
    })
}

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
    // `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "User views returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_user_views(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserViewsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    let folders = state.user_views.get_user_views(user_id).await?;
    let options = DtoOptions::with_all_fields(false);
    let mut dtos = state
        .dto
        .get_base_item_dtos(&folders, &options, Some(&user), None, true)
        .await?;

    // The per-library collection type is not stored on the `CollectionFolder`
    // rows, so the DTO projection leaves `CollectionType` unset. jellyfin-web
    // keys a library's presentation off this field — a `tvshows` library with no
    // type renders as a plain folder and its series never surface as shows.
    // Backfill it from the virtual-folder options (matched by item id), which
    // already carry the collection type (as `/Library/VirtualFolders` returns).
    if let Ok(folders_info) = state.virtual_folders.get_virtual_folders().await {
        let by_id: HashMap<Uuid, CollectionType> = folders_info
            .into_iter()
            .filter_map(|vf| {
                let id = vf
                    .item_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())?;
                let ct = vf.collection_type.and_then(map_collection_type)?;
                Some((id, ct))
            })
            .collect();
        for dto in &mut dtos {
            if dto.collection_type.is_none()
                && let Some(ct) = by_id.get(&dto.id)
            {
                dto.collection_type = Some(*ct);
            }
        }
    }

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
    tag = "ferrofin"
)]
async fn get_grouping_options(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserViewsQuery>,
) -> Result<Json<Vec<SpecialViewOptionDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    let folders = state.user_views.get_user_views(user_id).await?;
    let mut options: Vec<SpecialViewOptionDto> = folders
        .into_iter()
        .map(|folder| SpecialViewOptionDto {
            // C#'s `Id.ToString("N")` — a dashless guid. Fall back to the raw id
            // when it is not a parseable guid. Read the id first so the name can
            // move out of the owned row instead of being copied.
            id: Some(Uuid::parse_str(&folder.id).map_or(folder.id, |g| g.simple().to_string())),
            name: folder.name,
        })
        .collect();
    options.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(options))
}

/// `GET /Users/{userId}/Views` — path-scoped form of `GET /UserViews`.
///
/// Not in the 10.11 contract (upstream keeps it `[Obsolete]` + hidden from the
/// OpenAPI doc) but still served upstream and still called by older clients.
async fn get_user_views_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let query = UserViewsQuery {
        user_id: Some(user_id),
    };
    get_user_views(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/GroupingOptions` — path-scoped form of
/// `GET /UserViews/GroupingOptions`.
async fn get_grouping_options_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
) -> Result<Json<Vec<SpecialViewOptionDto>>, ApiError> {
    let query = UserViewsQuery {
        user_id: Some(user_id),
    };
    get_grouping_options(state, auth, Query(query)).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/UserViews", get(get_user_views))
        .route("/UserViews/GroupingOptions", get(get_grouping_options))
        .route("/Users/{userId}/Views", get(get_user_views_for_user))
        .route(
            "/Users/{userId}/GroupingOptions",
            get(get_grouping_options_for_user),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_type_maps_library_options_to_dto_type() {
        // A tvshows library must surface as CollectionType::tvshows so the web
        // client renders it as a Shows view (the bug: it was unset → generic).
        assert_eq!(
            map_collection_type(CollectionTypeOptions::tvshows),
            Some(CollectionType::tvshows)
        );
        assert_eq!(
            map_collection_type(CollectionTypeOptions::movies),
            Some(CollectionType::movies)
        );
        // A mixed library has no single type → None (a generic view).
        assert_eq!(map_collection_type(CollectionTypeOptions::mixed), None);
    }
}
