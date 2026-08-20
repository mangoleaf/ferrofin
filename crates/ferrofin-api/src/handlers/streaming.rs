//! Shared direct-play (static) file-serving helpers for the streaming
//! controllers (`Videos`, `Audio`, `UniversalAudio`).
//!
//! Every one of those controllers, on its non-transcoding path, resolves an
//! item's static [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo), takes
//! its on-disk path, and serves the file with HTTP `Range` support. The
//! transcoding / HLS / stream-copy machinery those controllers also expose is
//! deferred (no ffmpeg runner), so only this direct-stream slice is ported; it is
//! factored here so `videos`/`audio` don't each duplicate it.

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Resolves the direct-play file path for an item's first static media source.
///
/// Ports the direct-stream lookup shared by the streaming controllers: the item's
/// static (non-probed) source's on-disk `Path`. Returns `404` when the item has no
/// static media source, or that source has no on-disk path (nothing to
/// direct-stream). The transcoding parameters the controllers accept are ignored
/// here — only the static path is served.
pub(crate) async fn stream_path(state: &AppState, item_id: Uuid) -> Result<String, ApiError> {
    let sources = state
        .media_sources
        .get_static_media_sources(item_id, true, None)
        .await?;
    sources
        .into_iter()
        .find_map(|s| s.path)
        .ok_or_else(|| ApiError::NotFound(format!("no direct stream for item {item_id}")))
}

/// Serves the file at `path` for `request`, honouring `Range`/`HEAD`.
///
/// Delegates to [`tower_http::services::ServeFile`], which performs the
/// `Range`/`HEAD`/`206 Partial Content`/`404` handling; its infallible response is
/// mapped into an axum body. A resolution/IO failure surfaces as `404`.
pub(crate) async fn serve_static_file(path: &str, request: Request) -> Result<Response, ApiError> {
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    // A path the database resolved but the filesystem rejects otherwise
    // surfaces as a bare 404 indistinguishable from a missing route — name the
    // fs-level cause (stale NFS handle, permissions, moved file) in the log.
    // Only on the 404 branch: this helper serves every HLS segment and every
    // Range request of a direct play, and an unconditional pre-`stat` doubled
    // the metadata syscalls on that path (two round trips per request on
    // network storage) to produce a diagnostic that never fired. A 206/416 is
    // about the Range header, not the file, so it is not interesting here.
    if response.status() == axum::http::StatusCode::NOT_FOUND {
        let reason = tokio::fs::metadata(path)
            .await
            .err()
            .map_or_else(|| "unreadable".to_owned(), |e| e.to_string());
        tracing::warn!(path, error = reason, "direct-stream file is not accessible");
    }
    Ok(response.map(Body::new))
}
