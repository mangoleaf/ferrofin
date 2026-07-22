//! Plain exception types ported from `MediaBrowser.Common`.
//!
//! Each was a bare `System.Exception` subclass carrying an optional message.
//! In Rust they become `thiserror` error types with an optional message, so
//! they can flow through `Result` and `?` at their call sites.

/// Resource-not-found error (`MediaBrowser.Common.Extensions.ResourceNotFoundException`).
#[derive(Debug, Clone, Default, PartialEq, Eq, thiserror::Error)]
#[error("{}", .message.as_deref().unwrap_or("resource not found"))]
pub struct ResourceNotFoundException {
    /// The error message, if one was supplied.
    pub message: Option<String>,
}

impl ResourceNotFoundException {
    /// Creates an error with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

/// Rate-limit-exceeded error (`MediaBrowser.Common.Extensions.RateLimitExceededException`).
#[derive(Debug, Clone, Default, PartialEq, Eq, thiserror::Error)]
#[error("{}", .message.as_deref().unwrap_or("rate limit exceeded"))]
pub struct RateLimitExceededException {
    /// The error message, if one was supplied.
    pub message: Option<String>,
}

impl RateLimitExceededException {
    /// Creates an error with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

/// Method-not-allowed error (`MediaBrowser.Common.Extensions.MethodNotAllowedException`).
#[derive(Debug, Clone, Default, PartialEq, Eq, thiserror::Error)]
#[error("{}", .message.as_deref().unwrap_or("method not allowed"))]
pub struct MethodNotAllowedException {
    /// The error message, if one was supplied.
    pub message: Option<String>,
}

impl MethodNotAllowedException {
    /// Creates an error with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

/// Error during interaction with ffmpeg (`MediaBrowser.Common.FfmpegException`).
#[derive(Debug, Default, thiserror::Error)]
#[error("{}", .message.as_deref().unwrap_or("ffmpeg error"))]
pub struct FfmpegException {
    /// The error message, if one was supplied.
    pub message: Option<String>,
    /// The underlying cause, if any (C# `innerException`).
    #[source]
    pub inner: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl FfmpegException {
    /// Creates an error with the given message and no inner cause.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            inner: None,
        }
    }

    /// Creates an error with a message and an inner cause.
    #[must_use]
    pub fn with_source(
        message: impl Into<String>,
        inner: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: Some(message.into()),
            inner: Some(Box::new(inner)),
        }
    }
}
