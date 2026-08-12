//! Error type for the `ferrofin-providers` crate.
//!
//! The provider traits in `ferrofin-traits` return
//! [`ServiceError`](ferrofin_traits::error::ServiceError), so the public methods
//! ultimately hand one back. Internally, the genuine *infrastructure* failures —
//! an HTTP request to a remote provider (TMDB, OpenSubtitles, …) or a local
//! filesystem write while saving a fetched image — are carried as this typed
//! [`ProvidersError`] instead of being flattened into `ServiceError::Backend(String)`.
//! Converting via [`From`] boxes the typed error into
//! [`ServiceError::BackendSource`](ferrofin_traits::error::ServiceError::BackendSource),
//! so the underlying [`reqwest::Error`] / [`std::io::Error`] stays reachable
//! through [`Error::source`](std::error::Error::source) for logging and tests.
//!
//! Pure *semantic* failures (unconfigured/invalid credentials → `400`, rejected
//! login → `401`, a non-success HTTP status with no transport cause) carry no
//! source chain and are still constructed directly as the matching
//! `ServiceError` variant at the call site — wrapping them here would only
//! obscure the HTTP status they must map to.

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// A backend/infrastructure failure raised inside `ferrofin-providers` while
/// talking to a remote provider or writing a fetched asset locally.
///
/// Every variant maps to `ServiceError::BackendSource` (HTTP `500`) and keeps
/// its underlying cause as the [`source`](std::error::Error::source).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProvidersError {
    /// An HTTP transport or response-decode failure while calling a remote
    /// provider. `context` names the operation, e.g. `"OpenSubtitles request"`.
    #[error("{context}: {source}")]
    Http {
        /// The operation being performed, e.g. `"OpenSubtitles request"`.
        context: String,
        /// The underlying transport / decode failure.
        #[source]
        source: reqwest::Error,
    },

    /// An I/O failure while creating a directory or writing a fetched image to
    /// the local metadata tree. `context` names the operation and target.
    #[error("{context}: {source}")]
    Io {
        /// The operation and path, e.g. `"create image dir /meta/<id>"`.
        context: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

impl ProvidersError {
    /// Builds a [`ProvidersError::Http`] tagging `source` with a `context` label.
    pub fn http(context: impl Into<String>, source: reqwest::Error) -> Self {
        Self::Http {
            context: context.into(),
            source,
        }
    }

    /// Builds a [`ProvidersError::Io`] tagging `source` with a `context` label.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl From<ProvidersError> for ServiceError {
    fn from(err: ProvidersError) -> Self {
        // Both variants are infrastructure failures (HTTP 500); box them so the
        // transport/io cause survives as a `source()` chain.
        Self::backend_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::ProvidersError;
    use ferrofin_traits::error::ServiceError;
    use std::error::Error as _;

    #[test]
    fn io_converts_to_backend_source_and_keeps_the_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = ProvidersError::io("write image /meta/a.jpg", cause);
        // Display carries the context + cause…
        assert_eq!(err.to_string(), "write image /meta/a.jpg: denied");

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // …and (via `transparent`) the io::Error stays reachable as the cause.
        assert_eq!(svc.to_string(), "write image /meta/a.jpg: denied");
        assert_eq!(svc.source().unwrap().to_string(), "denied");
    }

    #[test]
    fn http_converts_to_backend_source() {
        // A reqwest::Error can only be produced by exercising reqwest; a failed
        // request to an unroutable URL yields a transport error we can wrap.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req_err = rt.block_on(async {
            reqwest::Client::new()
                .get("http://127.0.0.1:1/never")
                .send()
                .await
                .expect_err("connection to a closed port must fail")
        });

        let err = ProvidersError::http("OpenSubtitles request", req_err);
        assert!(err.to_string().starts_with("OpenSubtitles request: "));

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // The reqwest cause stays reachable through the source chain.
        assert!(svc.source().is_some());
    }
}
