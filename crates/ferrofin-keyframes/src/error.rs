//! Error type for the `ferrofin-keyframes` crate.

use thiserror::Error;

/// Errors returned by the keyframe extractors.
#[derive(Debug, Error)]
pub enum KeyframesError {
    /// The ffprobe process could not be spawned or its output could not be read.
    #[error("ffprobe process error: {0}")]
    Process(#[from] std::io::Error),
}
