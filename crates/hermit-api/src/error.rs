//! The HTTP error type — maps [`ServiceError`] onto status codes.
//!
//! Port note: Jellyfin's controllers throw exceptions that ASP.NET's exception
//! middleware turns into status codes. Here every handler returns
//! `Result<_, ApiError>`; [`ApiError`]'s [`IntoResponse`] does the mapping. The
//! service layer speaks [`ServiceError`] (from `hermit-traits`), which folds in
//! via [`From`] so handlers can use `?` directly on trait-method results.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hermit_traits::error::ServiceError;
use serde::Serialize;
use thiserror::Error;

/// The error returned by every `hermit-api` handler.
///
/// Each variant maps to exactly one HTTP status via [`IntoResponse`]. Most
/// failures arrive as a [`ServiceError`] from a manager trait and are carried in
/// [`ApiError::Service`]; the remaining variants let handlers (and the shared
/// `not_implemented` stub) return a specific status without inventing a
/// [`ServiceError`] for it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// A failure surfaced from a `hermit-traits` service/manager method. Its
    /// [`ServiceError`] variant selects the status (see [`IntoResponse`]).
    #[error(transparent)]
    Service(#[from] ServiceError),

    /// The route exists in the contract but has no ported handler yet → `501`.
    /// Returned by the shared `not_implemented` handler.
    #[error("not implemented")]
    NotImplemented,

    /// The request lacked valid credentials → `401`.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The requested entity does not exist → `404`.
    #[error("not found: {0}")]
    NotFound(String),

    /// A caller-supplied argument was missing or malformed → `400`.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The caller is authenticated but not permitted to perform the operation
    /// → `403`. Ported from the controllers' `StatusCode(403, …)` returns (e.g.
    /// updating another user without elevation, disabling the last admin).
    #[error("forbidden: {0}")]
    Forbidden(String),
}

impl ApiError {
    /// The HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Service(ServiceError::NotFound(_)) | Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Service(ServiceError::Unauthorized(_)) | Self::Unauthorized(_) => {
                StatusCode::UNAUTHORIZED
            }
            Self::Service(ServiceError::InvalidInput(_)) | Self::BadRequest(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            // `Db`/`Backend` (and any future non-exhaustive variant) are internal
            // failures the client cannot act on.
            Self::Service(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The JSON body accompanying an [`ApiError`] response: a single `error`
/// message. Kept minimal and stable so clients can rely on its shape.
#[derive(Debug, Serialize)]
struct ErrorBody {
    /// A human-readable description of the failure.
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // 5xx messages can leak internals, so log the detail server-side but
        // return a generic body; 4xx/501 messages are safe to surface.
        if status.is_server_error() {
            tracing::error!(error = %self, "handler failed");
        }
        let error = if status.is_server_error() {
            "internal server error".to_owned()
        } else {
            self.to_string()
        };
        (status, Json(ErrorBody { error })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, StatusCode};
    use hermit_traits::error::ServiceError;

    #[test]
    fn service_variants_map_to_expected_status() {
        assert_eq!(
            ApiError::from(ServiceError::not_found("x")).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(ServiceError::unauthorized("x")).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::from(ServiceError::invalid_input("x")).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::from(ServiceError::backend("x")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn direct_variants_map_to_expected_status() {
        assert_eq!(
            ApiError::NotImplemented.status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            ApiError::Unauthorized("no token".into()).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::NotFound("item".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::BadRequest("bad".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::Forbidden("nope".into()).status(),
            StatusCode::FORBIDDEN
        );
    }
}
