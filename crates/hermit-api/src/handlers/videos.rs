//! `VideosController` — direct video stream serving.
//!
//! Ports `GET`/`HEAD /Videos/{itemId}/stream`: resolves the item's static
//! (direct-play) [`MediaSourceInfo`](hermit_model::dto::MediaSourceInfo), takes
//! its on-disk path, and serves the file. Serving delegates to
//! [`tower_http::services::ServeFile`], which honours HTTP `Range` requests
//! (`206 Partial Content`), `HEAD`, and returns `404` for a missing file —
//! exactly the basic direct-stream behaviour a client needs before transcoding
//! is ported.
//!
//! Transcoding, stream copy, and the wide set of encoding query parameters are
//! out of scope for First-Light; only the static file path is served.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use axum::routing::get;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Resolves the direct-play file path for an item's first static media source.
///
/// Returns `404` when the item has no static media source or that source has no
/// on-disk path (nothing to direct-stream).
async fn stream_path(state: &AppState, item_id: Uuid) -> Result<String, ApiError> {
    let sources = state
        .media_sources
        .get_static_media_sources(item_id, true, None)
        .await?;
    sources
        .into_iter()
        .find_map(|s| s.path)
        .ok_or_else(|| ApiError::NotFound(format!("no direct stream for item {item_id}")))
}

/// `GET`/`HEAD /Videos/{itemId}/stream` — serve the item's video file.
///
/// Port of `VideosController.GetVideoStream` (direct-stream path only). Range
/// requests yield `206 Partial Content`; `HEAD` returns headers only.
async fn get_video_stream(
    State(state): State<AppState>,
    Path(item_id): Path<Uuid>,
    request: Request,
) -> Result<Response, ApiError> {
    let path = stream_path(&state, item_id).await?;
    // `ServeFile` performs the Range/HEAD/206/404 handling; map its infallible
    // response into an axum body.
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok(response.map(Body::new))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/Videos/{itemId}/stream",
        get(get_video_stream).head(get_video_stream),
    )
}
