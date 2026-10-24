//! The panic boundary — C# `ExceptionMiddleware`
//! (`Jellyfin.Api/Middleware/ExceptionMiddleware.cs`) for the one failure
//! Ferrofin does not answer as an [`ApiError`](crate::ApiError): a panic.
//!
//! Without it a panicking handler unwinds out of the connection task, so the
//! client gets a reset instead of an answer, and over HTTP/2 every other
//! stream on that connection dies with it. Upstream catches every exception
//! and answers it; a panic is an exception of no mapped type, so it gets what
//! upstream answers one with outside the Development environment:
//! `GetStatusCode`'s `500`, `Content-Type: text/plain` and the body
//! [`ERROR_BODY`].
//!
//! It is logged once, at `error`, as upstream logs it: the method and the path
//! (`Request.Path`, so never the query, which can carry an `api_key`), with
//! the panic's message. The panic hook has already logged the panic where it
//! happened (LOGGING.md §7); this is the request's line, logged by the
//! outermost layer that still has the request (§2). A boundary hands the
//! layers above it an ordinary `500`, so a second boundary further out never
//! logs the same panic again.
//!
//! The boundary covers the work up to the response head. A panic while the
//! body streams, after the head has gone, is not caught and still drops the
//! connection, with the panic hook's line as its only record: upstream cannot
//! answer then either, and logs a warning and rethrows
//! (`Response.HasStarted`).

use std::panic::AssertUnwindSafe;

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt as _;

/// The body upstream's `ExceptionMiddleware` writes outside the Development
/// environment, whatever the exception.
pub const ERROR_BODY: &str = "Error processing request.";

/// Runs the rest of the stack and answers a panic in it as upstream answers an
/// unhandled exception (see the [module docs](self)).
///
/// Everything the panicking work owned, the request included, is dropped with
/// it, and the boundary reads only its own copies of the method and the URI,
/// so nothing half-updated is observed here ([`AssertUnwindSafe`]); shared
/// state a handler held a lock on is that lock's concern.
pub async fn panic_boundary_layer(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    match AssertUnwindSafe(next.run(request)).catch_unwind().await {
        Ok(response) => response,
        Err(panic) => {
            tracing::error!(
                http.request.method = %method,
                url.path = %uri.path(),
                panic = %ferrofin_util::panic::message(panic.as_ref()),
                "Error processing request"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                ERROR_BODY,
            )
                .into_response()
        }
    }
}
