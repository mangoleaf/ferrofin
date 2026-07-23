//! `VideosController` — direct video stream serving + version management.
//!
//! Ports the portable slice of `VideosController` plus the media download route:
//! - `GET`/`HEAD /Videos/{itemId}/stream` — direct stream the item's file
//! - `GET`/`HEAD /Videos/{itemId}/stream.{container}` — the extension form
//! - `GET /Videos/{itemId}/AdditionalParts` — the item's additional parts
//! - `POST /Videos/MergeVersions` — merge videos into one version group
//! - `DELETE /Videos/{itemId}/AlternateSources` — split a version group apart
//! - `GET /Items/{itemId}/Download` — download the item's media file
//!
//! The stream verbs resolve the item's static
//! [`MediaSourceInfo`](hermit_model::dto::MediaSourceInfo) and serve its on-disk
//! file via the shared [`streaming`](crate::handlers::streaming) helpers (Range /
//! `HEAD` / `206` / `404`). Transcoding, stream copy, HLS, and the encoding query
//! parameters are out of scope (no ffmpeg runner) and stay on the `501` stub.
//!
//! `MergeVersions` / `AlternateSources` port the version-group linkage
//! (`PrimaryVersionId`) via the [`LibraryManager`](hermit_traits::library::LibraryManager);
//! the C# `LinkedAlternateVersions` array and linked-child reroute are not modeled
//! at that seam (see the manager docs).

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::parse_csv_uuids;
use crate::handlers::streaming::{serve_static_file, stream_path};
use crate::state::AppState;

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
    serve_static_file(&path, request).await
}

/// `GET`/`HEAD /Videos/{itemId}/stream.{container}` — serve the item's video file.
///
/// Port of `VideosController.GetVideoStreamByContainer`, which forwards to
/// `GetVideoStream` with `container` from the URL. After axum path normalization
/// the `stream.{container}` segment is captured as a single `{container}`
/// parameter (the `stream.` literal prefix is dropped); the captured value is the
/// requested container hint and is ignored for the direct-stream slice.
async fn get_video_stream_by_container(
    State(state): State<AppState>,
    Path((item_id, _container)): Path<(Uuid, String)>,
    Query(hls_query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    // Direct-play the static file when the item has one; otherwise fall back to
    // the progressive-transcode branch (VideosController.GetVideoStream), now
    // wired to the real transcode runtime via the HlsStreamManager seam.
    match stream_path(&state, item_id).await {
        Ok(path) => serve_static_file(&path, request).await,
        Err(ApiError::NotFound(_)) => {
            let raw = request.uri().query().map(ToOwned::to_owned);
            let req = crate::handlers::hls::request_from_query(item_id, hls_query, raw);
            crate::handlers::hls::transcode_stream_fallback(&state, item_id, false, req, request)
                .await
        }
        Err(other) => Err(other),
    }
}

/// Query parameters for `GET /Videos/{itemId}/AdditionalParts`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdditionalPartsQuery {
    /// Optional. Filter by user id, and attach user data.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `GET /Videos/{itemId}/AdditionalParts` — the item's additional parts.
///
/// Port of `VideosController.GetAdditionalPart`. Jellyfin returns the video's
/// `GetAdditionalParts()` children (the split parts of a multi-file movie).
/// Hermit does not model additional-part children at this seam, so a video with
/// none yields an empty [`QueryResult`], matching the C# `else` branch (non-video
/// items also return empty). A missing item is `404`.
// Body schema omitted: `BaseItemDto` is self-referential and its derived
// `utoipa::ToSchema` recurses without bound (a `hermit-model` DTO defect),
// overflowing the OpenAPI generator when inlined — see `items::get_items`.
#[utoipa::path(
    get,
    path = "/Videos/{itemId}/AdditionalParts",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Additional parts returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_additional_parts(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<AdditionalPartsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    // Honour the userId filter for parity (resolving a bad user is a 404); the
    // resolved user is otherwise unused since no parts are attached.
    let _user = resolve_user_opt(&state, &auth, query.user_id).await?;
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    Ok(Json(QueryResult::new(Some(0), Some(0), Vec::new())))
}

/// Query parameters for `POST /Videos/MergeVersions`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeVersionsQuery {
    /// Comma-delimited item ids to merge into one version group.
    ids: String,
}

/// `POST /Videos/MergeVersions` — merge videos into a single version group.
///
/// Port of `VideosController.MergeVersions`. Requires at least two ids (else
/// `400`); delegates the primary-selection and `PrimaryVersionId` linkage to
/// [`LibraryManager::merge_versions`](hermit_traits::library::LibraryManager::merge_versions).
/// Returns `204 No Content` on success. Elevation policy enforcement is deferred
/// to the auth layer.
#[utoipa::path(
    post,
    path = "/Videos/MergeVersions",
    params(("ids" = String, Query, description = "Item id list, comma delimited")),
    responses(
        (status = 204, description = "Videos merged"),
        (status = 400, description = "Supply at least 2 video ids")
    ),
    tag = "hermit"
)]
async fn merge_versions(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<MergeVersionsQuery>,
) -> Result<StatusCode, ApiError> {
    let ids = parse_csv_uuids(Some(&query.ids))?;
    if ids.len() < 2 {
        return Err(ApiError::BadRequest(
            "please supply at least two videos to merge".to_owned(),
        ));
    }
    state.library.merge_versions(&ids).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Videos/{itemId}/AlternateSources` — split a version group apart.
///
/// Port of `VideosController.DeleteAlternateSources`: clears the group's
/// `PrimaryVersionId` links via
/// [`LibraryManager::remove_alternate_sources`](hermit_traits::library::LibraryManager::remove_alternate_sources).
/// Returns `204 No Content`, or `404` when the item does not exist. Elevation
/// policy enforcement is deferred to the auth layer.
#[utoipa::path(
    delete,
    path = "/Videos/{itemId}/AlternateSources",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Alternate sources deleted"),
        (status = 404, description = "Video not found")
    ),
    tag = "hermit"
)]
async fn delete_alternate_sources(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state.library.remove_alternate_sources(item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Items/{itemId}/Download` — download the item's media file.
///
/// Port of `LibraryController.GetDownload`: resolves the item and streams its
/// on-disk file as an attachment. The `CanDownload` policy check is deferred to
/// the auth layer; the file is served through the shared streaming helper (Range /
/// `HEAD` / `404`), with a `Content-Disposition: attachment` header carrying the
/// file name (matching the C# `FileResult` download semantics).
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Download",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Media downloaded"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_download(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    request: Request,
) -> Result<Response, ApiError> {
    let path = stream_path(&state, item_id).await?;
    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| "download".to_owned(), |n| n.replace('"', ""));
    let mut response = serve_static_file(&path, request).await?;
    if let Ok(value) =
        header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Videos/{itemId}/stream",
            get(get_video_stream).head(get_video_stream),
        )
        .route(
            "/Videos/{itemId}/{container}",
            get(get_video_stream_by_container).head(get_video_stream_by_container),
        )
        .route(
            "/Videos/{itemId}/AdditionalParts",
            get(get_additional_parts),
        )
        .route("/Videos/MergeVersions", post(merge_versions))
        .route(
            "/Videos/{itemId}/AlternateSources",
            delete(delete_alternate_sources),
        )
        .route("/Items/{itemId}/Download", get(get_download))
}
