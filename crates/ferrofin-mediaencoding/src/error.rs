//! Error type for the `ferrofin-mediaencoding` crate.
//!
//! The media-encoding traits in `ferrofin-traits` (`MediaEncoder`,
//! `SubtitleEncoder`, `AttachmentExtractor`, `TrickplayFrameExtractor`, …) return
//! [`ServiceError`](ferrofin_traits::error::ServiceError), so the public methods
//! ultimately hand one back. Internally, the genuine *infrastructure* failures —
//! a filesystem read/create, an ffprobe JSON parse, or an ffmpeg/ffprobe
//! subprocess invocation — are carried as this typed [`MediaEncodingError`]
//! instead of being flattened into `ServiceError::Backend(String)`. Converting
//! via [`From`] boxes the typed error into
//! [`ServiceError::BackendSource`](ferrofin_traits::error::ServiceError::BackendSource),
//! so an underlying [`std::io::Error`] / [`serde_json::Error`] stays reachable
//! through [`Error::source`](std::error::Error::source) for logging and tests.
//!
//! Pure *semantic* failures (empty id → `400`, missing media source → `404`, an
//! unsupported attachment → `404`) carry no source chain and are still
//! constructed directly as the matching `ServiceError` variant at the call site —
//! wrapping them here would only obscure the HTTP status they must map to.
//!
//! The [`Process`](MediaEncodingError::Process) variant is the exception to
//! source-preservation: the `Transcoder` / ffmpeg seam methods return
//! `Result<_, String>`, so the lower-level cause is already flattened to a
//! message string at the process boundary — there is no `Error` object left to
//! chain. It carries that message verbatim; the remaining variants keep their
//! typed [`source`](std::error::Error::source).

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// A backend/infrastructure failure raised inside `ferrofin-mediaencoding` while
/// touching the filesystem, parsing ffprobe output, or driving an ffmpeg process.
///
/// Every variant maps to `ServiceError::BackendSource` (HTTP `500`). The typed
/// variants keep their underlying cause as the
/// [`source`](std::error::Error::source); [`Process`](Self::Process) carries the
/// already-stringified process failure message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MediaEncodingError {
    /// An I/O failure while reading or creating a frame/attachment directory or
    /// file. `context` names the operation and path.
    #[error("{context}: {source}")]
    Io {
        /// The operation and path, e.g. `"cannot create frame directory /cache"`.
        context: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// ffprobe's captured JSON could not be deserialized into the internal
    /// media-info structure.
    #[error("failed to parse ffprobe output: {source}")]
    ProbeParse {
        /// The underlying JSON deserialization failure.
        #[source]
        source: serde_json::Error,
    },

    /// An ffmpeg/ffprobe subprocess (or the filesystem seam wrapping it) failed.
    ///
    /// The `Transcoder` / IO seam methods return `Result<_, String>`, so the
    /// lower-level cause is already a message string at this boundary — there is
    /// no `Error` object to keep as a `source()`; the message is carried as-is.
    #[error("{0}")]
    Process(String),
}

impl MediaEncodingError {
    /// Builds a [`MediaEncodingError::Io`] tagging `source` with a `context` label.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Builds a [`MediaEncodingError::ProbeParse`] from a JSON deserialization error.
    #[must_use]
    pub fn probe_parse(source: serde_json::Error) -> Self {
        Self::ProbeParse { source }
    }

    /// Builds a [`MediaEncodingError::Process`] from a seam/process failure message.
    pub fn process(message: impl Into<String>) -> Self {
        Self::Process(message.into())
    }
}

impl From<MediaEncodingError> for ServiceError {
    fn from(err: MediaEncodingError) -> Self {
        // Every variant is an infrastructure failure (HTTP 500); box it so the
        // io/JSON cause survives as a `source()` chain where one exists.
        Self::backend_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::MediaEncodingError;
    use ferrofin_traits::error::ServiceError;
    use std::error::Error as _;

    #[test]
    fn io_converts_to_backend_source_and_keeps_the_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = MediaEncodingError::io("cannot create frame directory /cache", cause);
        // Display carries the context + cause…
        assert_eq!(
            err.to_string(),
            "cannot create frame directory /cache: denied"
        );

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // …and (via `transparent`) the io::Error stays reachable as the cause.
        assert_eq!(
            svc.to_string(),
            "cannot create frame directory /cache: denied"
        );
        assert_eq!(svc.source().unwrap().to_string(), "denied");
    }

    #[test]
    fn probe_parse_maps_to_backend_source_and_keeps_the_json_cause() {
        let json_err = serde_json::from_str::<i32>("not json").unwrap_err();
        let expected = json_err.to_string();
        let err = MediaEncodingError::probe_parse(json_err);
        assert_eq!(
            err.to_string(),
            format!("failed to parse ffprobe output: {expected}")
        );

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        assert_eq!(svc.source().unwrap().to_string(), expected);
    }

    #[test]
    fn process_maps_to_backend_source_with_no_deeper_source() {
        let err = MediaEncodingError::process("ffmpeg spawn failed: No such file");
        assert_eq!(err.to_string(), "ffmpeg spawn failed: No such file");

        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // Display is preserved through the transparent wrapper…
        assert_eq!(svc.to_string(), "ffmpeg spawn failed: No such file");
        // …but the stringified process failure has no further `source()`.
        assert!(svc.source().is_none());
    }
}
