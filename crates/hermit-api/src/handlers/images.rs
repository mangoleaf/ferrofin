//! `ImageController` — basic item-image serving.
//!
//! Ports `GET`/`HEAD /Items/{itemId}/Images/{imageType}` at the First-Light
//! level: resolve the item, and serve its image file or `404`.
//!
//! Port scope: image *location* in Jellyfin comes from the item's stored image
//! infos and the image processor (resize/format), neither of which is exposed by
//! the manager traits injected into this layer. So this handler implements the
//! contract's success/`404` shape — a missing item (or an item with no
//! resolvable on-disk image) is a `404` — and serves the file via
//! [`tower_http::services::ServeFile`] (Range/HEAD-aware) once a path is known.
//! Image resizing and the stored-image-info lookup arrive with the image
//! processor port.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use axum::routing::get;
use hermit_model::entities::ImageType;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Parses the `{imageType}` path segment into an [`ImageType`].
///
/// Jellyfin serializes the enum by its variant name (case-insensitive over the
/// wire in practice); an unknown value is a `400`.
fn parse_image_type(raw: &str) -> Result<ImageType, ApiError> {
    // `ImageType` is a plain C-like enum; match its names directly so this stays
    // independent of any serde representation.
    let parsed = match raw.to_ascii_lowercase().as_str() {
        "primary" => ImageType::Primary,
        "art" => ImageType::Art,
        "backdrop" => ImageType::Backdrop,
        "banner" => ImageType::Banner,
        "logo" => ImageType::Logo,
        "thumb" => ImageType::Thumb,
        "disc" => ImageType::Disc,
        "box" => ImageType::Box,
        "screenshot" => ImageType::Screenshot,
        "menu" => ImageType::Menu,
        "chapter" => ImageType::Chapter,
        "boxrear" => ImageType::BoxRear,
        "profile" => ImageType::Profile,
        other => return Err(ApiError::BadRequest(format!("unknown image type {other}"))),
    };
    Ok(parsed)
}

/// `GET`/`HEAD /Items/{itemId}/Images/{imageType}` — serve an item's image.
///
/// Port of `ImageController.GetItemImage` (basic path). Returns `404` when the
/// item does not exist or has no resolvable image file.
async fn get_item_image(
    State(state): State<AppState>,
    Path((item_id, image_type)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    // Validate the image type up front (a `400` for garbage), matching the
    // typed C# route parameter.
    let _image_type = parse_image_type(&image_type)?;

    // The item must exist; a missing item is a `404`.
    let _item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;

    // Image-file location requires the stored image infos + image processor,
    // which are not injected at this layer yet, so there is no path to serve:
    // report `404` (no image), the contract's not-found outcome.
    let Some(path) = resolve_image_path() else {
        return Err(ApiError::NotFound(format!("no image for item {item_id}")));
    };

    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok(response.map(Body::new))
}

/// Resolves the on-disk path of the requested image.
///
/// Always `None` until the image-processor / stored-image-info lookup is ported;
/// factored out so wiring it up later touches one place.
fn resolve_image_path() -> Option<String> {
    None
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/Items/{itemId}/Images/{imageType}",
        get(get_item_image).head(get_item_image),
    )
}
