//! Error type for the `ferrofin-keyframes` crate.

use thiserror::Error;

/// Errors returned by the keyframe extractors.
#[derive(Debug, Error)]
pub enum KeyframesError {
    /// The ffprobe process could not be spawned or its output could not be read.
    #[error("ffprobe process error: {0}")]
    Process(#[from] std::io::Error),
    /// The Matroska file could not be opened or read.
    #[error("reading the Matroska file failed: {0}")]
    MatroskaIo(std::io::Error),
    /// The file is not laid out the way the Matroska extractor reads it: no
    /// segment, no `SeekHead` naming `Info`, `Tracks` and `Cues`, no video
    /// track, or an element that cannot be read.
    #[error("{0}")]
    InvalidMatroska(String),
}
