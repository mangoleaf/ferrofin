//! Error types for `hermit-util`.

use thiserror::Error;

/// Errors raised by the stream/file comparison helpers in
/// [`crate::stream_extensions`].
#[derive(Debug, Error)]
pub enum StreamError {
    /// The stream does not support seeking, but the operation requires it
    /// (mirrors the C# `ArgumentException("Stream must support seeking.")`).
    #[error("stream must support seeking")]
    NotSeekable,

    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
