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
//! [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo) and serve its on-disk
//! file via the shared [`streaming`](crate::handlers::streaming) helpers (Range /
//! `HEAD` / `206` / `404`). Transcoding, stream copy, HLS, and the encoding query
//! parameters are out of scope (no ffmpeg runner) and stay on the `501` stub.
//!
//! `MergeVersions` / `AlternateSources` port the version-group linkage
//! (`PrimaryVersionId`) via the [`LibraryManager`](ferrofin_traits::library::LibraryManager);
//! the C# `LinkedAlternateVersions` array and linked-child reroute are not modeled
//! at that seam (see the manager docs).

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth, RequireDownload};
use crate::error::ApiError;
use crate::handlers::items::{require_visible_item, resolve_user_opt};
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// `GET /Videos/{itemId}/AdditionalParts` — the item's additional parts.
///
/// Port of `VideosController.GetAdditionalPart`. Jellyfin returns the video's
/// `GetAdditionalParts()` children (the split parts of a multi-file movie).
/// Ferrofin does not model additional-part children at this seam, so a video with
/// none yields an empty [`QueryResult`], matching the C# `else` branch (non-video
/// items also return empty). A missing item is `404`.
// Body schema omitted: `BaseItemDto` is self-referential and its derived
// `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect),
// overflowing the OpenAPI generator when inlined — see `items::get_items`.
#[utoipa::path(
    get,
    path = "/Videos/{itemId}/AdditionalParts",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Additional parts returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_additional_parts(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<AdditionalPartsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    // Honour the userId filter for parity (resolving a bad user is a 404); the
    // resolved user is otherwise unused since no parts are attached.
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    if item_id.is_nil() {
        state
            .library
            .get_user_root_folder()
            .await?
            .ok_or_else(|| ApiError::NotFound("user root folder".into()))?;
    } else {
        require_visible_item(&state, item_id, user.as_ref()).await?;
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
/// [`LibraryManager::merge_versions`](ferrofin_traits::library::LibraryManager::merge_versions).
/// Returns `204 No Content` on success. Administrators and API keys only
/// (`Policies.RequiresElevation`, enforced by [`RequireAdmin`]).
#[utoipa::path(
    post,
    path = "/Videos/MergeVersions",
    params(("ids" = String, Query, description = "Item id list, comma delimited")),
    responses(
        (status = 204, description = "Videos merged"),
        (status = 400, description = "Supply at least 2 video ids")
    ),
    tag = "ferrofin"
)]
async fn merge_versions(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    Query(query): Query<MergeVersionsQuery>,
) -> Result<StatusCode, ApiError> {
    let mut ids = Vec::new();
    for id in parse_csv_uuids(Some(&query.ids))? {
        if state
            .library
            .get_item_by_id_for_user(id, auth.user.as_ref())
            .await?
            .is_some_and(|item| is_video(&item))
        {
            ids.push(id);
        }
    }
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
/// [`LibraryManager::remove_alternate_sources`](ferrofin_traits::library::LibraryManager::remove_alternate_sources).
/// Returns `204 No Content`, or `404` when the item does not exist.
/// Administrators and API keys only (`Policies.RequiresElevation`, enforced by
/// [`RequireAdmin`]); upstream asks no `CanDelete(user)` here, and no item or
/// file is deleted — only the version links are cleared.
#[utoipa::path(
    delete,
    path = "/Videos/{itemId}/AlternateSources",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Alternate sources deleted"),
        (status = 404, description = "Video not found")
    ),
    tag = "ferrofin"
)]
async fn delete_alternate_sources(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let item = require_visible_item(&state, item_id, auth.user.as_ref()).await?;
    if !is_video(&item) {
        return Err(ApiError::NotFound(format!("video {item_id}")));
    }
    state.library.remove_alternate_sources(item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Items/{itemId}/Download` — download the item's media file.
///
/// Port of `LibraryController.GetDownload`: resolves the item and streams its
/// on-disk file as an attachment after enforcing the download permission.
/// The file is served through the shared streaming helper (Range /
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
    tag = "ferrofin"
)]
async fn get_download(
    State(state): State<AppState>,
    RequireDownload(auth, policy): RequireDownload,
    Path(item_id): Path<Uuid>,
    request: Request,
) -> Result<Response, ApiError> {
    require_visible_item(&state, item_id, auth.user.as_ref()).await?;
    let path = stream_path(&state, item_id).await?;
    // The controller's CanDownload(user) check follows the policy check.
    // Administrators pass the policy, but an explicitly disabled download
    // permission still fails this per-item check. API keys have no user policy.
    if policy.is_some_and(|p| !p.enable_content_downloading) {
        return Err(ApiError::BadRequest(
            "user cannot download this item".to_owned(),
        ));
    }
    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let mut response = serve_static_file(&path, request).await?;
    if let Ok(value) = header::HeaderValue::from_str(&attachment_disposition(filename)) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// The `Content-Disposition` value Jellyfin's `GetDownload` produces:
/// `attachment; filename=<name>; filename*=UTF-8''<rfc5987>`.
///
/// `LibraryController.GetDownload` first strips every `"` from the file name,
/// then ASP.NET's `ContentDispositionHeaderValue.SetHttpFileName` builds both
/// forms: `filename=` carries the name with every control / non-ASCII character
/// replaced by `_`, quoted (with `\` escaped) only when the result is not a
/// plain HTTP token; `filename*` carries the exact UTF-8 name percent-encoded
/// per RFC 5987 (only `attr-char` survives unencoded). Both are sent on every
/// download, so a client with a non-ASCII title still gets the real file name.
///
/// One deliberate non-port: ASP.NET sanitizes UTF-16 code units, so an
/// astral-plane character (an emoji) becomes `__` there and `_` here; the
/// `filename*` form — the one clients prefer — is byte-identical either way.
fn attachment_disposition(filename: &str) -> String {
    let filename = filename.replace('"', "");
    // ASP.NET `HttpRuleParser` token characters.
    let is_token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    let sanitized: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '_'
            }
        })
        .collect();
    let plain = if sanitized.chars().all(is_token) {
        sanitized
    } else {
        format!("\"{}\"", sanitized.replace('\\', "\\\\"))
    };
    let mut star = String::with_capacity(filename.len() * 3);
    for b in filename.bytes() {
        // RFC 5987 attr-char: ALPHA / DIGIT / "!" / "#" / "$" / "&" / "+" / "-" / "." /
        // "^" / "_" / "`" / "|" / "~".
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            star.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(star, "%{b:02X}");
        }
    }
    format!("attachment; filename={plain}; filename*=UTF-8''{star}")
}

/// Whether the stored type is Video or one of its concrete subclasses.
pub(crate) fn is_video(item: &ferrofin_db::entities::base_items::BaseItemEntity) -> bool {
    matches!(
        item.type_.rsplit('.').next().unwrap_or(&item.type_),
        "Video" | "Movie" | "Episode" | "MusicVideo" | "Trailer"
    )
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

#[cfg(test)]
mod tests {
    use super::attachment_disposition;

    #[test]
    fn disposition_matches_aspnet_set_http_file_name() {
        // Spaces and parens: quoted verbatim, percent-encoded in the RFC 5987 form.
        assert_eq!(
            attachment_disposition("Movie 0001 (2020).mkv"),
            "attachment; filename=\"Movie 0001 (2020).mkv\"; \
             filename*=UTF-8''Movie%200001%20%282020%29.mkv"
        );
        // A plain HTTP token (the dot-separated release shape) is NOT quoted.
        assert_eq!(
            attachment_disposition("Movie.2020.1080p.mkv"),
            "attachment; filename=Movie.2020.1080p.mkv; filename*=UTF-8''Movie.2020.1080p.mkv"
        );
        // Non-ASCII: `_` in the sanitized form (still a token), exact UTF-8 in the star form.
        assert_eq!(
            attachment_disposition("Amélie.mkv"),
            "attachment; filename=Am_lie.mkv; filename*=UTF-8''Am%C3%A9lie.mkv"
        );
        // Quotes are stripped up front (GetDownload); a backslash forces quoting and is escaped.
        assert_eq!(
            attachment_disposition("a\"b\\c.mkv"),
            "attachment; filename=\"ab\\\\c.mkv\"; filename*=UTF-8''ab%5Cc.mkv"
        );
    }
}
