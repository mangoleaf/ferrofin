//! Error type for the `ferrofin-hls` crate.

use ferrofin_traits::error::ServiceError;
use thiserror::Error;

/// Errors returned by the HLS playlist generator.
///
/// Ports the C# exception surface of `DynamicHlsPlaylistGenerator`: the only
/// exception raised on the public path is the `InvalidOperationException` thrown
/// by `ComputeEqualLengthSegments` for a zero segment length or zero runtime.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HlsError {
    /// The requested segment length or total runtime was invalid (zero).
    ///
    /// Mirrors `InvalidOperationException("Invalid segment length (..) or runtime ticks (..)")`.
    #[error(
        "Invalid segment length ({desired_segment_length_ms}) or runtime ticks ({total_runtime_ticks})"
    )]
    InvalidOperation {
        /// The rejected desired segment length in milliseconds.
        desired_segment_length_ms: i32,
        /// The rejected total runtime in ticks.
        total_runtime_ticks: i64,
    },
}

impl From<HlsError> for ServiceError {
    fn from(err: HlsError) -> Self {
        // A playlist-generation failure is an internal (HTTP 500) fault; box it
        // so the typed `HlsError` stays reachable through the source chain.
        Self::backend_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::{HlsError, ServiceError};
    use std::error::Error as _;

    #[test]
    fn converts_to_backend_source_and_keeps_the_cause() {
        let err = HlsError::InvalidOperation {
            desired_segment_length_ms: 0,
            total_runtime_ticks: 0,
        };
        let svc: ServiceError = err.into();
        assert!(matches!(svc, ServiceError::BackendSource(_)));
        // `transparent` surfaces the (leaf) HlsError's message; it has no deeper
        // cause of its own, so the source chain ends here.
        assert!(svc.to_string().contains("Invalid segment length"));
        assert!(svc.source().is_none());
    }
}
