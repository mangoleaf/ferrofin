//! `RemoteImageController` — remote (provider) image discovery and download.
//!
//! Ports the `RemoteImageController` surface over the [`ProviderManager`] trait
//! (whose remote-image methods already exist):
//!
//! - `GET  /Items/{itemId}/RemoteImages` — the remote image candidates for an
//!   item, paged and filtered by type/provider, plus the set of providers.
//! - `GET  /Items/{itemId}/RemoteImages/Providers` — the remote image providers
//!   applicable to an item.
//! - `POST /Items/{itemId}/RemoteImages/Download` — download one remote image
//!   URL onto an item and record the image update.
//!
//! [`ProviderManager`]: ferrofin_traits::providers::ProviderManager

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::entities::ImageType;
use ferrofin_model::providers::{
    ImageProviderInfo, RemoteImageInfo, RemoteImageQuery, RemoteImageResult,
};
use ferrofin_traits::providers::ItemUpdateType;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /Items/{itemId}/RemoteImages`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteImagesQuery {
    /// Restrict candidates to this image type.
    #[serde(default)]
    #[serde(rename = "type")]
    image_type: Option<ImageType>,
    /// The first record to return (records before it are dropped).
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Restrict candidates to a single provider.
    #[serde(default)]
    provider_name: Option<String>,
    /// Whether to include candidates in all languages.
    #[serde(default)]
    include_all_languages: Option<bool>,
}

/// `GET /Items/{itemId}/RemoteImages` — the remote image candidates for an item.
///
/// Port of `RemoteImageController.GetRemoteImages`: queries the provider manager
/// for candidates, filters the provider list by the requested type, and applies
/// `startIndex`/`limit` paging (the total count reflects the pre-paging size, as
/// in C#). A missing item is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/RemoteImages",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Remote images returned (RemoteImageResult)"),
        (status = 404, description = "Item not found"),
    ),
    tag = "ferrofin"
)]
async fn get_remote_images(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<RemoteImagesQuery>,
) -> Result<Json<RemoteImageResult>, ApiError> {
    require_item(&state, item_id).await?;

    let image_query = RemoteImageQuery {
        provider_name: query.provider_name.clone().unwrap_or_default(),
        image_type: query.image_type,
        include_disabled_providers: true,
        include_all_languages: query.include_all_languages.unwrap_or(false),
    };
    let mut images: Vec<RemoteImageInfo> = state
        .providers
        .get_available_remote_images(item_id, &image_query)
        .await?;

    let mut providers = state
        .providers
        .get_remote_image_provider_info(item_id)
        .await?;
    if let Some(image_type) = query.image_type {
        providers.retain(|p| p.supported_images.contains(&image_type));
    }
    let provider_names = distinct_provider_names(&providers);

    let total = i32::try_from(images.len()).unwrap_or(i32::MAX);
    if let Some(start) = query.start_index {
        let start = usize::try_from(start).unwrap_or(0).min(images.len());
        images.drain(..start);
    }
    if let Some(limit) = query.limit {
        images.truncate(usize::try_from(limit).unwrap_or(0));
    }

    Ok(Json(RemoteImageResult {
        images,
        total_record_count: total,
        providers: provider_names,
    }))
}

/// `GET /Items/{itemId}/RemoteImages/Providers` — the item's remote image
/// providers.
///
/// Port of `RemoteImageController.GetRemoteImageProviders`. A missing item is a
/// `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/RemoteImages/Providers",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Remote image providers returned"),
        (status = 404, description = "Item not found"),
    ),
    tag = "ferrofin"
)]
async fn get_remote_image_providers(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<ImageProviderInfo>>, ApiError> {
    require_item(&state, item_id).await?;
    let providers = state
        .providers
        .get_remote_image_provider_info(item_id)
        .await?;
    Ok(Json(providers))
}

/// Query parameters for `POST /Items/{itemId}/RemoteImages/Download`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadQuery {
    /// The image type to store the download as (required by the contract).
    #[serde(default)]
    #[serde(rename = "type")]
    image_type: Option<ImageType>,
    /// The source image URL to download.
    #[serde(default)]
    image_url: Option<String>,
}

/// `POST /Items/{itemId}/RemoteImages/Download` — download a remote image onto
/// an item.
///
/// Port of `RemoteImageController.DownloadRemoteImage`: stores the URL via the
/// provider manager and records an [`ItemUpdateType::ImageUpdate`]. A missing
/// item is a `404`; a missing `type` is a `400`. On success `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/RemoteImages/Download",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Remote image downloaded"),
        (status = 400, description = "Missing image type"),
        (status = 404, description = "Item not found"),
    ),
    tag = "ferrofin"
)]
async fn download_remote_image(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<DownloadQuery>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    let image_type = query
        .image_type
        .ok_or_else(|| ApiError::BadRequest("type is required".to_owned()))?;
    let url = query
        .image_url
        .ok_or_else(|| ApiError::BadRequest("imageUrl is required".to_owned()))?;

    state
        .providers
        .save_image_from_url(item_id, &url, image_type, None)
        .await?;
    state
        .providers
        .save_metadata(item_id, ItemUpdateType::ImageUpdate)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolves an item, returning a `404` when it does not exist.
async fn require_item(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    Ok(())
}

/// The distinct provider display names, case-insensitively, in first-seen order —
/// mirroring C#'s `Distinct(StringComparer.OrdinalIgnoreCase)`.
fn distinct_provider_names(providers: &[ImageProviderInfo]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for provider in providers {
        if let Some(name) = &provider.name
            && !seen.iter().any(|n| n.eq_ignore_ascii_case(name))
        {
            seen.push(name.clone());
        }
    }
    seen
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items/{itemId}/RemoteImages", get(get_remote_images))
        .route(
            "/Items/{itemId}/RemoteImages/Providers",
            get(get_remote_image_providers),
        )
        .route(
            "/Items/{itemId}/RemoteImages/Download",
            post(download_remote_image),
        )
}
