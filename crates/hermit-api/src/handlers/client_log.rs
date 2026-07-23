//! `ClientLogController` — accept client-uploaded diagnostic documents.
//!
//! Ports the portable route of `ClientLogController`:
//! - `POST /ClientLog/Document` — save a plain-text document a client uploads,
//!   returning its generated filename.
//!
//! The route is `[Authorize]` (any authenticated user); [`RequireAuth`] carries
//! the caller's client/app metadata used to build the filename.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The maximum accepted client-log document size, in bytes.
///
/// Mirrors the C# `ClientLogController.MaxDocumentSize` (`1_000_000`), applied
/// both as the request-size limit and the `413` guard.
const MAX_DOCUMENT_SIZE: usize = 1_000_000;

/// Client log document response DTO.
///
/// Port of `Jellyfin.Api.Models.ClientLogDtos.ClientLogDocumentResponseDto`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ClientLogDocumentResponseDto {
    /// Gets the resulting filename.
    pub file_name: String,
}

/// `POST /ClientLog/Document` — save an uploaded diagnostic document.
///
/// Port of `ClientLogController.LogFile`: rejects with `403` when
/// `AllowClientLogUpload` is disabled, `413` when the body exceeds
/// [`MAX_DOCUMENT_SIZE`], otherwise writes the document (its filename built from
/// the caller's client name/version) and returns the generated filename. The
/// client version is the literal `"apikey"` for API-key callers, mirroring the
/// C# `GetRequestInformation`.
#[utoipa::path(
    post,
    path = "/ClientLog/Document",
    request_body = String,
    responses(
        (status = 200, description = "Document saved", body = ClientLogDocumentResponseDto),
        (status = 403, description = "Event logging disabled"),
        (status = 413, description = "Upload size too large")
    ),
    tag = "hermit"
)]
async fn log_file(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !state.config.configuration().await?.allow_client_log_upload {
        return Err(ApiError::Forbidden("client log upload disabled".to_owned()));
    }

    // The contract distinguishes an over-limit upload as `413 Payload Too Large`
    // (C# returns it manually); `ApiError` has no such variant, so build the
    // response directly.
    if body.len() > MAX_DOCUMENT_SIZE {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Payload must be less than {MAX_DOCUMENT_SIZE} bytes"),
        )
            .into_response());
    }

    let client_name = auth.client.as_deref().unwrap_or("unknown-client");
    let client_version = if auth.is_api_key {
        "apikey"
    } else {
        auth.version.as_deref().unwrap_or("unknown-version")
    };

    let file_name = state
        .client_event_logger
        .write_document(client_name, client_version, &body)
        .await?;
    Ok(Json(ClientLogDocumentResponseDto { file_name }).into_response())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/ClientLog/Document", post(log_file))
}
