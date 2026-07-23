//! `ItemLookupController` — the portable external-id descriptor route.
//!
//! Ports the one route of Jellyfin's `ItemLookupController` whose result is
//! backed by the ported provider seam:
//!
//! - `GET /Items/{itemId}/ExternalIdInfos` — the external-id descriptors (IMDb,
//!   TMDb, MusicBrainz, …) applicable to an item.
//!
//! Every `POST /Items/RemoteSearch/*` route and `POST
//! /Items/RemoteSearch/Apply/{itemId}` stay on the shared `501` stub as
//! intentional deferrals: remote metadata *search* and *apply* require the
//! network metadata-provider fetchers (TMDb/TVDb/…), which Hermit does not port
//! — [`ProviderManager`](hermit_traits::providers::ProviderManager) deliberately
//! omits the per-provider strategy interfaces and carries no
//! `get_remote_search_results` method, so those routes cannot return real
//! results at this seam.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::providers::ExternalIdInfo;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /Items/{itemId}/ExternalIdInfos` — the item's external-id descriptors.
///
/// Port of `ItemLookupController.GetExternalIdInfos`: resolves the item (`404`
/// when absent), then returns the external-id descriptors the provider manager
/// advertises for it.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/ExternalIdInfos",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "External id info retrieved", body = Vec<ExternalIdInfo>),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_external_id_infos(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<ExternalIdInfo>>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    let infos = state.providers.get_external_id_infos(item_id).await?;
    Ok(Json(infos))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/Items/{itemId}/ExternalIdInfos",
        get(get_external_id_infos),
    )
}
