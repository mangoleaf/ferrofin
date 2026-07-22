//! Error type for `hermit-networking`.

/// Errors returned by fallible `hermit-networking` operations.
///
/// The ported Jellyfin networking surface is almost entirely `bool`/`Option`
/// based (C# `TryParse` idioms), so this enum is small; it exists to satisfy
/// the workspace convention that every crate defines its own error type.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NetworkingError {
    /// A value could not be parsed into a network address or subnet.
    #[error("invalid network value: {0}")]
    InvalidValue(String),
}
