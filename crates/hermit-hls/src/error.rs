//! Error type for the `hermit-hls` crate.

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
