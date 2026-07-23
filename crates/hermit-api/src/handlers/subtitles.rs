//! `SubtitleController` — subtitle management + fallback fonts.
//!
//! Ports the portable slice of Jellyfin's `SubtitleController`:
//! - `DELETE /Videos/{itemId}/Subtitles/{index}` — delete a stored external
//!   subtitle stream (+ its sidecar file).
//! - `POST /Videos/{itemId}/Subtitles` — upload an external subtitle file.
//! - `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}` — search providers.
//! - `POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}` — download one.
//! - `GET /Providers/Subtitles/Subtitles/{subtitleId}` — fetch a remote subtitle.
//!
//! The delete route is DB-backed and real. Upload / remote search / download /
//! get drive the un-ported `ISubtitleProvider` registry (deferred); the routes
//! exist (not `501`) and surface the manager's empty/"not enabled" behaviour so
//! clients see stable semantics.
//!
//! On-the-fly subtitle *conversion* (`Videos/{itemId}/{mediaSourceId}/Subtitles/
//! {index}/Stream.{format}` and the `.m3u8` playlist) needs the un-ported
//! `SubtitleEncoder` and stays on the `501` stub. The `FallbackFont` routes need
//! the encoding-options config (`FallbackFontPath`), which is not surfaced at the
//! `ServerConfigurationManager` seam yet, so they also stay on the `501` stub.

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hermit_model::providers::RemoteSubtitleInfo;
use hermit_traits::subtitles::{SubtitleResponse, SubtitleSearchRequest};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::queue_high_priority_refresh;
use crate::state::AppState;

/// Ensures an item exists, returning `404` otherwise.
async fn require_item(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    Ok(())
}

/// `DELETE /Videos/{itemId}/Subtitles/{index}` — delete a stored subtitle.
///
/// Port of `SubtitleController.DeleteSubtitle`: `404` when the item is missing,
/// else `204`. The manager drops the external subtitle stream at `index` and its
/// sidecar file (deleting a non-existent index is idempotent). Elevation policy
/// is deferred to the auth layer.
#[utoipa::path(
    delete,
    path = "/Videos/{itemId}/Subtitles/{index}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("index" = i32, Path, description = "The index of the subtitle file")
    ),
    responses(
        (status = 204, description = "Subtitle deleted"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn delete_subtitle(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, index)): Path<(Uuid, i32)>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    state.subtitles.delete_subtitles(item_id, index).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The `POST /Videos/{itemId}/Subtitles` request body — a base64-encoded subtitle
/// plus its metadata (port of the C# `UploadSubtitleDto`).
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct UploadSubtitleDto {
    /// The subtitle language (three-letter ISO code).
    language: String,
    /// The subtitle format (e.g. `srt`, `ass`).
    format: String,
    /// Whether the subtitle is forced.
    is_forced: bool,
    /// Whether the subtitle is for the hearing impaired (SDH).
    is_hearing_impaired: bool,
    /// The base64-encoded subtitle file bytes.
    data: String,
}

/// `POST /Videos/{itemId}/Subtitles` — upload an external subtitle file.
///
/// Port of `SubtitleController.UploadSubtitle`: `404` when the item is missing,
/// `400` when the `Data` is not valid base64. The decoded bytes are handed to the
/// [`SubtitleManager`](hermit_traits::subtitles::SubtitleManager); with no
/// subtitle-provider host wired the manager rejects the write (`400`), otherwise
/// a metadata refresh is queued and `204` returned. Elevation policy is deferred.
#[utoipa::path(
    post,
    path = "/Videos/{itemId}/Subtitles",
    params(("itemId" = String, Path, description = "The item the subtitle belongs to")),
    request_body = UploadSubtitleDto,
    responses(
        (status = 204, description = "Subtitle uploaded"),
        (status = 400, description = "Invalid subtitle data"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn upload_subtitle(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Json(body): Json<UploadSubtitleDto>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    let content = decode_base64(&body.data)
        .ok_or_else(|| ApiError::BadRequest("subtitle data is not valid base64".to_owned()))?;
    let response = SubtitleResponse {
        language: body.language,
        format: body.format,
        is_forced: body.is_forced,
        is_hearing_impaired: body.is_hearing_impaired,
        content,
    };
    state.subtitles.upload_subtitle(item_id, &response).await?;
    queue_high_priority_refresh(&state, item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSearchQuery {
    /// Optional. Only show subtitles which are a perfect match.
    #[serde(default)]
    is_perfect_match: Option<bool>,
}

/// `GET /Items/{itemId}/RemoteSearch/Subtitles/{language}` — search providers.
///
/// Port of `SubtitleController.SearchRemoteSubtitles`: `404` for a missing item,
/// else the (possibly empty) provider results for the language.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("language" = String, Path, description = "The language of the subtitles"),
        ("isPerfectMatch" = Option<bool>, Query, description = "Only show subtitles which are a perfect match")
    ),
    responses(
        (status = 200, description = "Subtitles retrieved", body = [RemoteSubtitleInfo]),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn search_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, language)): Path<(Uuid, String)>,
    Query(query): Query<RemoteSearchQuery>,
) -> Result<Json<Vec<RemoteSubtitleInfo>>, ApiError> {
    require_item(&state, item_id).await?;
    let request = SubtitleSearchRequest {
        item_id,
        language,
        is_perfect_match: query.is_perfect_match,
        is_automated: false,
    };
    let results = state.subtitles.search_subtitles(&request).await?;
    Ok(Json(results))
}

/// `POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}` — download one.
///
/// Port of `SubtitleController.DownloadRemoteSubtitles`: `404` for a missing
/// item, else `204` after attempting the download (the C# swallows download
/// errors and still returns `204`; a metadata refresh is queued). After router
/// normalization the trailing id segment is captured as `{language}`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("language" = String, Path, description = "The subtitle id")
    ),
    responses(
        (status = 204, description = "Subtitle downloaded"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn download_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, subtitle_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    // The C# logs-and-continues on a provider failure, still returning 204, then
    // queues a refresh. A download that succeeds queues the refresh too.
    if state
        .subtitles
        .download_subtitles(item_id, &subtitle_id)
        .await
        .is_ok()
    {
        queue_high_priority_refresh(&state, item_id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Providers/Subtitles/Subtitles/{subtitleId}` — fetch a remote subtitle.
///
/// Port of `SubtitleController.GetRemoteSubtitles`: streams the raw subtitle
/// bytes with a MIME type derived from its format. With no provider host wired
/// the fetch is rejected (`400`); a provider would yield the file.
#[utoipa::path(
    get,
    path = "/Providers/Subtitles/Subtitles/{subtitleId}",
    params(("subtitleId" = String, Path, description = "The subtitle id")),
    responses((status = 200, description = "File returned")),
    tag = "hermit"
)]
async fn get_remote_subtitles(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(subtitle_id): Path<String>,
) -> Result<Response, ApiError> {
    let result = state.subtitles.get_remote_subtitles(&subtitle_id).await?;
    let mime = subtitle_mime(&result.format);
    Ok(([(header::CONTENT_TYPE, mime)], result.content).into_response())
}

/// The MIME type for a subtitle of the given format (a small, common-format map;
/// unknown formats fall back to `text/plain`).
fn subtitle_mime(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "vtt" => "text/vtt",
        "srt" | "subrip" => "application/x-subrip",
        "ass" | "ssa" => "text/x-ssa",
        _ => "text/plain",
    }
}

/// Decodes a standard (RFC 4648) base64 string into bytes, returning `None` on
/// any invalid character or length.
///
/// A tiny self-contained decoder (the workspace has no base64 dependency and the
/// C# controller base64-decodes the uploaded `Data` field). Whitespace is
/// ignored; `=` padding is accepted.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in input.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            // Mask to the low 8 bits — the shift leaves exactly one byte.
            out.push(u8::try_from((acc >> bits) & 0xFF).expect("masked to a byte"));
        }
    }
    Some(out)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Videos/{itemId}/Subtitles", post(upload_subtitle))
        .route(
            "/Videos/{itemId}/Subtitles/{index}",
            delete(delete_subtitle),
        )
        .route(
            "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
            get(search_remote_subtitles).post(download_remote_subtitles),
        )
        .route(
            "/Providers/Subtitles/Subtitles/{subtitleId}",
            get(get_remote_subtitles),
        )
}

#[cfg(test)]
mod tests {
    use super::decode_base64;

    #[test]
    fn base64_round_trips_known_values() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("").unwrap(), b"");
        // Whitespace is ignored.
        assert_eq!(decode_base64("aGVs\nbG8=").unwrap(), b"hello");
    }

    #[test]
    fn base64_rejects_invalid_chars() {
        assert!(decode_base64("!!!!").is_none());
    }
}
