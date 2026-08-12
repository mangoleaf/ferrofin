//! `AudioController` + `UniversalAudioController` — direct audio stream serving.
//!
//! Ports the direct-play (static) slice of the two audio streaming controllers:
//! - `GET`/`HEAD /Audio/{itemId}/stream`
//! - `GET`/`HEAD /Audio/{itemId}/stream.{container}`
//! - `GET`/`HEAD /Audio/{itemId}/universal`
//!
//! Each resolves the item's static
//! [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo) and serves its on-disk
//! file via the shared [`streaming`](crate::handlers::streaming) helpers (Range /
//! `HEAD` / `206` / `404`).
//!
//! `UniversalAudioController.GetUniversalAudioStream` first resolves playback info
//! and, when the chosen source supports direct stream (`isStatic`), serves the
//! original file progressively; only when transcoding is required does it fall
//! back to HLS. Ferrofin's static sources always report `supports_direct_stream`, so
//! the direct-serve branch is taken here. The remote-redirect (`302`) and HLS
//! branches, plus the full transcoding parameter set, are deferred (no ffmpeg
//! runner) and are not exercised by this port.

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use axum::routing::get;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::streaming::{serve_static_file, stream_path};
use crate::state::AppState;

/// `GET`/`HEAD /Audio/{itemId}/stream` — serve the item's audio file.
///
/// Port of `AudioController.GetAudioStream` (direct-stream path only). Range
/// requests yield `206 Partial Content`; `HEAD` returns headers only.
async fn get_audio_stream(
    State(state): State<AppState>,
    Path(item_id): Path<Uuid>,
    request: Request,
) -> Result<Response, ApiError> {
    let path = stream_path(&state, item_id).await?;
    serve_static_file(&path, request).await
}

/// `GET`/`HEAD /Audio/{itemId}/stream.{container}` — serve the item's audio file.
///
/// Port of `AudioController.GetAudioStreamByContainer` (which delegates to
/// `GetAudioStream`). After axum path normalization the `stream.{container}`
/// segment is captured as a single `{container}` parameter; the captured value is
/// the requested container hint and is ignored for the direct-stream slice.
async fn get_audio_stream_by_container(
    State(state): State<AppState>,
    Path((item_id, _container)): Path<(Uuid, String)>,
    Query(hls_query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    match stream_path(&state, item_id).await {
        Ok(path) => serve_static_file(&path, request).await,
        Err(ApiError::NotFound(_)) => {
            let raw = request.uri().query().map(ToOwned::to_owned);
            let req = crate::handlers::hls::request_from_query(item_id, hls_query, raw);
            crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request)
                .await
        }
        Err(other) => Err(other),
    }
}

/// `GET`/`HEAD /Audio/{itemId}/universal` — serve the item's audio file.
///
/// Port of `UniversalAudioController.GetUniversalAudioStream`, direct-play branch:
/// the resolved source supports direct stream, so the original file is served
/// progressively (Range/`HEAD`). Requires authentication (`[Authorize]`).
async fn get_universal_audio_stream(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(hls_query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    // Direct-play when a static source exists; otherwise transcode (the
    // UniversalAudioController fallback), now wired to the real runtime.
    match stream_path(&state, item_id).await {
        Ok(path) => serve_static_file(&path, request).await,
        Err(ApiError::NotFound(_)) => {
            let raw = request.uri().query().map(ToOwned::to_owned);
            let req = crate::handlers::hls::request_from_query(item_id, hls_query, raw);
            crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request)
                .await
        }
        Err(other) => Err(other),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Audio/{itemId}/stream",
            get(get_audio_stream).head(get_audio_stream),
        )
        .route(
            "/Audio/{itemId}/{container}",
            get(get_audio_stream_by_container).head(get_audio_stream_by_container),
        )
        .route(
            "/Audio/{itemId}/universal",
            get(get_universal_audio_stream).head(get_universal_audio_stream),
        )
}
