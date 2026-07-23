//! `CollectionController` — create collections (box sets) and edit membership.
//!
//! Ports every route of `Jellyfin.Api.Controllers.CollectionController`:
//!
//! - `POST /Collections` — create a collection from a name + seed item ids,
//!   returning its [`CollectionCreationResult`].
//! - `POST /Collections/{collectionId}/Items` — add items to a collection.
//! - `DELETE /Collections/{collectionId}/Items` — remove items from a
//!   collection.
//!
//! All three sit behind Jellyfin's `CollectionManagement` policy; here they take
//! the [`RequireAuth`] extractor (a missing/invalid token is `401`). Item id
//! lists arrive as the comma-delimited `ids` query parameter (Jellyfin's
//! `CommaDelimitedCollectionModelBinder`) and are split via
//! [`parse_csv_uuids`](crate::handlers::query_parse::parse_csv_uuids). The work
//! is delegated to the [`CollectionManager`](hermit_traits::collections::CollectionManager)
//! seam on [`AppState`].

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use hermit_model::collections::CollectionCreationResult;
use hermit_traits::collections::CollectionCreationOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::parse_csv_uuids;
use crate::state::AppState;

/// The query parameters for `POST /Collections`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCollectionQuery {
    /// The name of the new collection.
    #[serde(default)]
    name: Option<String>,
    /// Comma-delimited item ids to seed the collection with.
    #[serde(default)]
    ids: Option<String>,
    /// Optional parent folder to create the collection within.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Whether to lock the new collection's metadata against refresh.
    #[serde(default)]
    is_locked: bool,
}

/// The query parameters for the add/remove membership routes.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionItemsQuery {
    /// Comma-delimited item ids to add or remove.
    #[serde(default)]
    ids: Option<String>,
}

/// `POST /Collections` — creates a new collection.
///
/// Port of `CollectionController.CreateCollection`; the authenticated caller's id
/// seeds `UserIds`, matching `User.GetUserId()`.
#[utoipa::path(
    post,
    path = "/Collections",
    responses((status = 200, description = "Collection created (CollectionCreationResult)")),
    tag = "hermit"
)]
async fn create_collection(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<CreateCollectionQuery>,
) -> Result<Json<CollectionCreationResult>, ApiError> {
    let item_id_list = parse_csv_uuids(query.ids.as_deref())?;
    let user_id = auth.user_id();
    let user_ids = if user_id.is_nil() {
        Vec::new()
    } else {
        vec![user_id]
    };
    let options = CollectionCreationOptions {
        name: query.name.unwrap_or_default(),
        parent_id: query.parent_id,
        is_locked: query.is_locked,
        provider_ids: std::collections::HashMap::new(),
        item_id_list,
        user_ids,
    };
    let item = state.collections.create_collection(&options).await?;
    let id = Uuid::parse_str(&item.id)
        .map_err(|_| ApiError::Service(hermit_traits::error::ServiceError::backend("bad id")))?;
    Ok(Json(CollectionCreationResult { id }))
}

/// `POST /Collections/{collectionId}/Items` — adds items to a collection.
///
/// Port of `CollectionController.AddToCollection`.
#[utoipa::path(
    post,
    path = "/Collections/{collectionId}/Items",
    params(("collectionId" = String, Path, description = "The collection id")),
    responses((status = 204, description = "Items added to collection")),
    tag = "hermit"
)]
async fn add_to_collection(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(collection_id): Path<Uuid>,
    Query(query): Query<CollectionItemsQuery>,
) -> Result<StatusCode, ApiError> {
    let ids = parse_csv_uuids(query.ids.as_deref())?;
    state
        .collections
        .add_to_collection(collection_id, &ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Collections/{collectionId}/Items` — removes items from a collection.
///
/// Port of `CollectionController.RemoveFromCollection`.
#[utoipa::path(
    delete,
    path = "/Collections/{collectionId}/Items",
    params(("collectionId" = String, Path, description = "The collection id")),
    responses((status = 204, description = "Items removed from collection")),
    tag = "hermit"
)]
async fn remove_from_collection(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(collection_id): Path<Uuid>,
    Query(query): Query<CollectionItemsQuery>,
) -> Result<StatusCode, ApiError> {
    let ids = parse_csv_uuids(query.ids.as_deref())?;
    state
        .collections
        .remove_from_collection(collection_id, &ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers the collection routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Collections", post(create_collection)).route(
        "/Collections/{collectionId}/Items",
        post(add_to_collection).delete(remove_from_collection),
    )
}
