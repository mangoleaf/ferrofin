//! The HTTP error type — maps [`ServiceError`] onto status codes.
//!
//! Port note: Jellyfin's controllers throw exceptions that ASP.NET's exception
//! middleware turns into status codes. Here every handler returns
//! `Result<_, ApiError>`; [`ApiError`]'s [`IntoResponse`] does the mapping. The
//! service layer speaks [`ServiceError`] (from `ferrofin-traits`), which folds in
//! via [`From`] so handlers can use `?` directly on trait-method results.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ferrofin_traits::error::ServiceError;
use serde::Serialize;
use thiserror::Error;

/// The error returned by every `ferrofin-api` handler.
///
/// Each variant maps to exactly one HTTP status via [`IntoResponse`]. Most
/// failures arrive as a [`ServiceError`] from a manager trait and are carried in
/// [`ApiError::Service`]; the remaining variants let handlers (and the shared
/// `not_implemented` stub) return a specific status without inventing a
/// [`ServiceError`] for it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// A failure surfaced from a `ferrofin-traits` service/manager method. Its
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

    /// The operation conflicts with existing state → `409`. Ported from the
    /// controllers' `Conflict(…)` returns (e.g. renaming a library to a name
    /// that already exists).
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller is authenticated but not permitted to perform the operation
    /// → `403`. Ported from the controllers' `StatusCode(403, …)` returns (e.g.
    /// updating another user without elevation, disabling the last admin).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The operation cannot run right now because a conflicting one already is
    /// → `503`. The contract documents this for `POST /Backup/Create`, which is
    /// serialized: two creates in the same second would write the same archive
    /// path, and each holds the whole database in memory.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// The port of an ASP.NET **parameterless** action result — `NotFound()`,
    /// `BadRequest()` — whose body ASP.NET fills in from
    /// `ApiBehaviorOptions.ClientErrorMapping`: an RFC 9110 `ProblemDetails`
    /// document, and no message.
    ///
    /// A separate variant, not a global change to how every error renders,
    /// because Jellyfin's error bodies are **not** uniform and a blanket
    /// ProblemDetails would trade one divergence for another. Measured on a
    /// live 10.11.8 (the campaign's own container) in one session:
    ///
    /// | request | body |
    /// |---|---|
    /// | `GET /MusicGenres/R-B` (`NotFound()`) | `{"type":…,"title":"Not Found","status":404,"traceId":…}` |
    /// | `GET /Users/{unknown}` (`NotFound("User not found")`) | `"User not found"` — a bare JSON string |
    /// | `GET /Items/not-a-guid` (model binding) | a `ValidationProblemDetails` with an `errors` map |
    /// | `GET /Users/Me` with no token (auth challenge) | **empty**, no content type |
    /// | `POST` to a GET-only route | **empty** `405` |
    ///
    /// So the faithful port is per-call-site, exactly as the C# is: a handler
    /// whose controller writes `NotFound()` with no argument returns THIS, and
    /// a handler whose controller passes a message keeps
    /// [`ApiError::NotFound`]. Adopting it for a route is a parity claim about
    /// that route and has to be checked against the C# for it.
    ///
    /// `traceId` is deliberately not emitted: it is ASP.NET's per-request W3C
    /// trace-context id, which cannot match across two instances and is on the
    /// parity harness's VOLATILE list for that reason.
    #[error("{0}")]
    Problem(ProblemStatus),
}

/// The statuses [`ApiError::Problem`] can carry, each with the `type` and
/// `title` ASP.NET's `ClientErrorMapping` writes for it.
///
/// A closed enum rather than a bare [`StatusCode`] so a caller cannot ask for a
/// ProblemDetails body on a status whose upstream wording has not been checked
/// against a live server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProblemStatus {
    /// `404` — `NotFound()`. Verified live against Jellyfin 10.11.8.
    NotFound,
}

impl ProblemStatus {
    /// The HTTP status.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
        }
    }

    /// The `type` URI ASP.NET writes — an RFC 9110 section link.
    #[must_use]
    pub fn type_uri(self) -> &'static str {
        match self {
            Self::NotFound => "https://tools.ietf.org/html/rfc9110#section-15.5.5",
        }
    }

    /// The `title`, which is the status's reason phrase.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::NotFound => "Not Found",
        }
    }
}

impl std::fmt::Display for ProblemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}

/// An RFC 9110 `ProblemDetails` body, in ASP.NET's field order.
#[derive(Debug, Serialize)]
struct ProblemBody {
    /// The RFC 9110 section link for the status.
    #[serde(rename = "type")]
    type_uri: &'static str,
    /// The status's reason phrase.
    title: &'static str,
    /// The numeric status, repeated in the body as ASP.NET does.
    status: u16,
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
            Self::Service(ServiceError::Conflict(_)) | Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            Self::Problem(p) => p.status(),
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
        if let Self::Problem(problem) = self {
            return (
                status,
                Json(ProblemBody {
                    type_uri: problem.type_uri(),
                    title: problem.title(),
                    status: status.as_u16(),
                }),
            )
                .into_response();
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
    use super::{ApiError, ProblemStatus, StatusCode};
    use ferrofin_traits::error::ServiceError;

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
        assert_eq!(
            ApiError::Problem(ProblemStatus::NotFound).status(),
            StatusCode::NOT_FOUND
        );
    }

    /// The exact document a live Jellyfin 10.11.8 returned for
    /// `GET /MusicGenres/R-B`, minus `traceId` (per-request, VOLATILE).
    #[tokio::test]
    async fn problem_renders_aspnet_problem_details() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let response = ApiError::Problem(ProblemStatus::NotFound).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            body,
            serde_json::json!({
                "type": "https://tools.ietf.org/html/rfc9110#section-15.5.5",
                "title": "Not Found",
                "status": 404,
            })
        );
    }

    /// …and a message-carrying `NotFound` is untouched: upstream's
    /// `NotFound("User not found")` writes the bare message, not a
    /// ProblemDetails, so the two variants must stay distinguishable.
    #[tokio::test]
    async fn a_message_carrying_not_found_keeps_its_own_body() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let response = ApiError::NotFound("thing".into()).into_response();
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert!(body.get("error").is_some(), "got {body}");
        assert!(body.get("type").is_none());
    }
}
