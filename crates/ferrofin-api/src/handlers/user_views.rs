//! `UserViewsController` — a user's home-screen views.
//!
//! Ports:
//!
//! - `GET /UserViews` — resolves the target user, fetches their views via the
//!   [`UserViewManager`](ferrofin_traits::library::UserViewManager), projects each
//!   to a [`BaseItemDto`] with the `DtoService`, and returns them as a
//!   [`QueryResult`].
//! - `GET /UserViews/GroupingOptions` — the user's grouping-eligible library
//!   folders as [`SpecialViewOptionDto`] `{ Name, Id }` pairs, name-sorted.
//!
//! Port note — grouping eligibility: C#'s `UserView.IsEligibleForGrouping` keeps
//! only collection folders whose `CollectionType` is `movies`/`tvshows`/unset.
//! The persisted `CollectionFolder` rows do not carry the type; the virtual-folder
//! options do (what `/Library/VirtualFolders` returns), so both endpoints read the
//! per-folder type from there, keyed by the folder's item id.

use std::collections::HashMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::CollectionType;
use ferrofin_model::dto::{BaseItemDto, SpecialViewOptionDto};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, UserViewQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::{DotnetGuid, Query};
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

/// The collection type of every configured library, keyed by its folder item id.
///
/// A `mixed` library has no single type and maps to `None` — which is still an
/// entry, because C# treats an unset `CollectionType` as grouping-eligible.
async fn collection_types_by_id(
    state: &AppState,
) -> Result<HashMap<Uuid, Option<CollectionType>>, ApiError> {
    Ok(state
        .virtual_folders
        .library_folders()
        .await?
        .iter()
        .filter_map(|vf| {
            let id = vf
                .item_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())?;
            Some((id, vf.collection_type.and_then(map_collection_type)))
        })
        .collect())
}

/// C#'s `UserView.IsEligibleForGrouping`: only `movies`, `tvshows` and an unset
/// collection type may be grouped into a home-screen view.
fn is_eligible_for_grouping(collection_type: Option<CollectionType>) -> bool {
    matches!(
        collection_type,
        None | Some(CollectionType::movies | CollectionType::tvshows)
    )
}

/// Query parameters for `GET /UserViews/GroupingOptions`.
///
/// `userId` is optional in the contract; when omitted it defaults to the
/// authenticated caller (Jellyfin's `RequestHelpers.GetUserId`).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupingOptionsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// Query parameters for `GET /UserViews`: `UserViewsController.GetUserViews`'
/// four. `userId` defaults to the authenticated caller.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetUserViewsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Whether to list the channels and the Live TV view; `true` when absent
    /// (`UserViewQuery`'s default).
    #[serde(default)]
    include_external_content: Option<bool>,
    /// Comma-delimited library types to list as their shadow views; tokens
    /// that are no `CollectionType` are dropped
    /// (`CommaDelimitedCollectionModelBinder`).
    #[serde(default)]
    preset_views: Option<String>,
    /// Include views hidden from the home screen.
    #[serde(default)]
    include_hidden: bool,
}

/// `GET /UserViews` — the target user's library views.
///
/// Port of `UserViewsController.GetUserViews`: `includeExternalContent`, when
/// given, and a non-empty `presetViews` go into the `UserViewQuery`.
#[utoipa::path(
    get,
    path = "/UserViews",
    params(
        ("userId" = Option<String>, Query, description = "The user id"),
        ("includeExternalContent" = Option<bool>, Query, description = "Include channels and Live TV (default true)"),
        ("presetViews" = Option<String>, Query, description = "Comma-delimited library types listed as their shadow views"),
        ("includeHidden" = Option<bool>, Query, description = "Include views hidden from the home screen"),
    ),
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "User views returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_user_views(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<GetUserViewsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    let view_query = UserViewQuery {
        include_external_content: query.include_external_content.unwrap_or(true),
        include_hidden: query.include_hidden,
        preset_views: crate::handlers::query_parse::parse_csv_enums_lenient(
            query.preset_views.as_deref(),
        ),
    };
    let folders = state
        .user_views
        .get_user_views_with_query(user_id, &view_query)
        .await?;
    // C# `GetUserViews` builds `new DtoOptions()` — ALL item fields (minus the two
    // default-excluded ones); its `AddClientFields` + explicit appends are subsumed.
    // `with_all_fields(false)` served views with NO fields — strict-SDK clients
    // (Android TV et al.) assume Jellyfin's always-present view fields exist.
    let options = DtoOptions::with_all_fields(true);
    // `CollectionType` needs no backfill here: the DTO service sets it for every
    // `IHasCollectionType` row off the row's own stored type, on every endpoint,
    // as `DtoService.AttachBasicFields` does (DtoService.cs:1061-1064). This
    // handler used to patch it in after the fact, which left
    // `/Library/MediaFolders`, `/Items` and `/Items/{id}/Ancestors` serving a
    // null type for the same folders.
    let dtos = state
        .dto
        .get_base_item_dtos(&folders, &options, Some(&user), None, true)
        .await?;

    Ok(Json(QueryResult::from_items(dtos)))
}

/// `GET /UserViews/GroupingOptions` — the user's grouping-eligible views.
///
/// Port of `UserViewsController.GetGroupingOptions`: resolves the user (a missing
/// user is `404`), takes their root media folders, keeps the ones
/// `UserView.IsEligibleForGrouping` accepts (movies, tvshows, untyped), and
/// returns each as a [`SpecialViewOptionDto`] `{ Name, Id }`, id rendered as a
/// dashless guid and the list ordered by name. A folder that is not a configured
/// library (no virtual-folder entry) is not a collection folder and is dropped,
/// as C#'s `folder is ICollectionFolder` test does.
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
    Query(query): Query<GroupingOptionsQuery>,
) -> Result<Json<Vec<SpecialViewOptionDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    // Grouping options come from the root libraries the user can open,
    // independently of which home views they hid (`MyMediaExcludes`).
    let folders = state.user_views.get_user_root_children(&user).await?;
    let by_id = collection_types_by_id(&state).await?;
    let mut options: Vec<SpecialViewOptionDto> = folders
        .into_iter()
        .filter_map(|folder| {
            let id = Uuid::parse_str(&folder.id).ok()?;
            let collection_type = by_id.get(&id).copied()?;
            is_eligible_for_grouping(collection_type).then(|| SpecialViewOptionDto {
                // C#'s `Id.ToString("N")` — a dashless guid.
                id: Some(id.simple().to_string()),
                name: folder.name,
            })
        })
        .collect();
    // `OrderBy(i => i.Name)`: `string.CompareTo`, in the request's culture.
    let comparer =
        ferrofin_util::current_culture::comparer(&ferrofin_util::current_culture::capture()).ok();
    options.sort_by(|a, b| {
        let (a, b) = (
            a.name.as_deref().unwrap_or_default(),
            b.name.as_deref().unwrap_or_default(),
        );
        comparer
            .as_ref()
            .map_or_else(|| a.cmp(b), |comparer| comparer.compare(a, b))
    });
    Ok(Json(options))
}

/// `GET /Users/{userId}/Views` — path-scoped form of `GET /UserViews`.
///
/// Not in the 10.11 contract (upstream keeps it `[Obsolete]` + hidden from the
/// OpenAPI doc) but still served upstream and still called by older clients.
async fn get_user_views_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    axum::extract::Path(DotnetGuid(user_id)): axum::extract::Path<DotnetGuid>,
    Query(mut query): Query<GetUserViewsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_user_views(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/GroupingOptions` — path-scoped form of
/// `GET /UserViews/GroupingOptions`.
async fn get_grouping_options_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    axum::extract::Path(DotnetGuid(user_id)): axum::extract::Path<DotnetGuid>,
) -> Result<Json<Vec<SpecialViewOptionDto>>, ApiError> {
    let query = GroupingOptionsQuery {
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

crate::query::query_parameters! {
    GetUserViewsQuery { "presetViews" => ',' } => [("get", "/UserViews")];
    GroupingOptionsQuery {} => [("get", "/UserViews/GroupingOptions")];
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

    #[test]
    fn user_views_can_request_hidden_views() {
        let hidden: GetUserViewsQuery =
            serde_urlencoded::from_str("includeHidden=true").expect("query parses");
        assert!(hidden.include_hidden);

        let defaulted: GetUserViewsQuery = serde_urlencoded::from_str("").expect("empty query");
        assert!(!defaulted.include_hidden);
        assert_eq!(defaulted.include_external_content, None);
        assert_eq!(defaulted.preset_views, None);
    }
}
