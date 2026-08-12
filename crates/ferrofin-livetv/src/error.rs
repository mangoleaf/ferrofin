//! Error type for the `ferrofin-livetv` crate.
//!
//! The Live TV traits in `ferrofin-traits` return
//! [`ServiceError`](ferrofin_traits::error::ServiceError), so the public methods
//! ultimately hand one back. Internally, the genuine *infrastructure* failures —
//! fetching an M3U/XMLTV source over HTTP, reading it from disk, or serializing a
//! DVR DTO to its stored JSON — are carried as this typed [`LiveTvError`] instead
//! of being flattened into `ServiceError::Backend(String)`. Converting via
//! [`From`] boxes the typed error into
//! [`ServiceError::BackendSource`](ferrofin_traits::error::ServiceError::BackendSource),
//! so the underlying [`reqwest::Error`] / [`std::io::Error`] /
//! [`serde_json::Error`] stays reachable through
//! [`Error::source`](std::error::Error::source) for logging and tests.
//!
//! Pure *semantic* failures (a missing tuner `Url` → `400`, an absent item →
//! `404`) carry no source chain and are still constructed directly as the
//! matching `ServiceError` variant at the call site — wrapping them here would
//! only obscure the HTTP status they must map to.

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// A backend/infrastructure failure raised inside `ferrofin-livetv` while fetching
/// or serializing a Live TV source.
///
/// Every variant maps to `ServiceError::BackendSource` (HTTP `500`) and keeps its
/// underlying cause as the [`source`](std::error::Error::source).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LiveTvError {
    /// An HTTP failure while fetching an M3U/XMLTV source over `http(s)://` —
    /// transport, a non-success status, or reading the response body.
    /// `context` names the operation and URL.
    #[error("{context}: {source}")]
    Http {
        /// The operation and URL, e.g. `"fetch http://tuner/playlist.m3u"`.
        context: String,
        /// The underlying `reqwest` failure.
        #[source]
        source: reqwest::Error,
    },

    /// An I/O failure while reading a `file://`/local-path Live TV source.
    /// `context` names the operation and path.
    #[error("{context}: {source}")]
    Io {
        /// The operation and path, e.g. `"read /etc/guide.xml"`.
        context: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// `serde_json` failed to serialize a DVR configuration/DTO to its stored
    /// JSON. `context` names what was being serialized.
    #[error("{context}: {source}")]
    Serialize {
        /// What was being serialized, e.g. `"serialize tuner host"`.
        context: String,
        /// The underlying serialization failure.
        #[source]
        source: serde_json::Error,
    },
}

impl LiveTvError {
    /// Builds a [`LiveTvError::Http`] tagging `source` with a `context` label.
    pub fn http(context: impl Into<String>, source: reqwest::Error) -> Self {
        Self::Http {
            context: context.into(),
            source,
        }
    }

    /// Builds a [`LiveTvError::Io`] tagging `source` with a `context` label.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Builds a [`LiveTvError::Serialize`] tagging `source` with a `context`
    /// label.
    pub fn serialize(context: impl Into<String>, source: serde_json::Error) -> Self {
        Self::Serialize {
            context: context.into(),
            source,
        }
    }
}

impl From<LiveTvError> for ServiceError {
    fn from(err: LiveTvError) -> Self {
        // Every variant is an infrastructure failure (HTTP 500); box it so the
        // http/io/serde cause survives as a `source()` chain.
        Self::backend_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::LiveTvError;
    use ferrofin_traits::error::ServiceError;
    use std::error::Error as _;

    #[test]
    fn io_converts_to_backend_source_and_keeps_the_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let err = LiveTvError::io("read /etc/guide.xml", cause);
        // Display carries the context + cause…
        assert_eq!(err.to_string(), "read /etc/guide.xml: missing file");

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // …and (via `transparent`) the io::Error stays reachable as the cause.
        assert_eq!(svc.to_string(), "read /etc/guide.xml: missing file");
        assert_eq!(svc.source().unwrap().to_string(), "missing file");
    }

    #[test]
    fn serialize_maps_to_backend_source_and_preserves_the_cause() {
        // Provoke a real `serde_json::Error`: a map with a non-string key can't
        // be serialized to JSON.
        use std::collections::HashMap;
        let mut bad: HashMap<Vec<u8>, u8> = HashMap::new();
        bad.insert(vec![1, 2], 3);
        let source = serde_json::to_string(&bad).expect_err("non-string key");

        let err = LiveTvError::serialize("serialize tuner host", source);
        assert!(err.to_string().starts_with("serialize tuner host: "));

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // The serde cause stays reachable through the `transparent` chain.
        assert!(svc.source().is_some());
    }

    #[tokio::test]
    async fn http_variant_maps_to_backend_source() {
        // A connection to an unroutable address yields a real `reqwest::Error`.
        let source = reqwest::Client::new()
            .get("http://127.0.0.1:0/")
            .send()
            .await
            .expect_err("connect must fail");
        let err = LiveTvError::http("fetch http://127.0.0.1:0/", source);
        assert!(err.to_string().starts_with("fetch http://127.0.0.1:0/: "));

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        assert!(svc.source().is_some());
    }
}
