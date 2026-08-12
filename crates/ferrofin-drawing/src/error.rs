//! Error type for the `ferrofin-drawing` crate.
//!
//! The image-processing traits in `ferrofin-traits` return
//! [`ServiceError`](ferrofin_traits::error::ServiceError), so the public methods
//! ultimately hand one back. Internally, the genuine *infrastructure* failures —
//! reading, creating, or encoding an image file — are carried as this typed
//! [`DrawingError`] instead of being flattened into
//! `ServiceError::Backend(String)`. Converting via [`From`] boxes the typed
//! error into [`ServiceError::BackendSource`](ferrofin_traits::error::ServiceError::BackendSource),
//! so the underlying [`std::io::Error`] / [`image::ImageError`] stays reachable
//! through [`Error::source`](std::error::Error::source) for logging and tests.
//!
//! Pure *semantic* failures (empty path → `400`, missing file → `404`, an
//! undecodable image → `400`) carry no source chain and are still constructed
//! directly as the matching `ServiceError` variant at the call site — wrapping
//! them here would only obscure the HTTP status they must map to.

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// A backend/infrastructure failure raised inside `ferrofin-drawing` while
/// touching an image file.
///
/// Every variant maps to `ServiceError::BackendSource` (HTTP `500`) and keeps
/// its underlying cause as the [`source`](std::error::Error::source).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DrawingError {
    /// An I/O failure while opening, probing, `stat`-ing, creating, or writing
    /// an image file. `context` names the operation and path.
    #[error("{context}: {source}")]
    Io {
        /// The operation and path, e.g. `"open /media/a.jpg"`.
        context: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The `image` crate failed to encode an image to its output file.
    #[error("{context}: {source}")]
    Encode {
        /// The operation and path, e.g. `"encode /cache/a.png"`.
        context: String,
        /// The underlying codec failure.
        #[source]
        source: image::ImageError,
    },
}

impl DrawingError {
    /// Builds an [`DrawingError::Io`] tagging `source` with a `context` label.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Builds an [`DrawingError::Encode`] tagging `source` with a `context` label.
    pub fn encode(context: impl Into<String>, source: image::ImageError) -> Self {
        Self::Encode {
            context: context.into(),
            source,
        }
    }
}

impl From<DrawingError> for ServiceError {
    fn from(err: DrawingError) -> Self {
        // Both variants are infrastructure failures (HTTP 500); box them so the
        // io/codec cause survives as a `source()` chain.
        Self::backend_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::DrawingError;
    use ferrofin_traits::error::ServiceError;
    use std::error::Error as _;

    #[test]
    fn io_converts_to_backend_source_and_keeps_the_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = DrawingError::io("open /media/a.jpg", cause);
        // Display carries the context + cause…
        assert_eq!(err.to_string(), "open /media/a.jpg: denied");

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // …and (via `transparent`) the io::Error stays reachable as the cause.
        assert_eq!(svc.to_string(), "open /media/a.jpg: denied");
        assert_eq!(svc.source().unwrap().to_string(), "denied");
    }

    #[test]
    fn encode_maps_to_backend_source() {
        let img_err = image::ImageError::Parameter(image::error::ParameterError::from_kind(
            image::error::ParameterErrorKind::Generic("bad".to_owned()),
        ));
        let svc: ServiceError = DrawingError::encode("encode /cache/a.png", img_err).into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
    }
}
