//! The one shared error type returned by every service/manager trait.
//!
//! Port note: Jellyfin's C# service layer throws exceptions
//! (`ResourceNotFoundException`, `AuthenticationException`, `ArgumentException`,
//! …). Rust services instead return `Result<_, ServiceError>`. Trait methods
//! collapse the exception zoo into these variants; concrete impls (Wave 6,
//! `ferrofin-core`) map their internal failures onto them. Persistence failures
//! flow in from [`ferrofin_db::DbError`] via [`From`], so repository-backed
//! managers can use `?` directly.

use thiserror::Error;

/// The error returned by every `ferrofin-traits` service/manager method.
///
/// A deliberately small, transport-agnostic taxonomy: the HTTP layer maps each
/// variant to a status code (`NotFound` → 404, `Unauthorized` → 401,
/// `InvalidInput` → 400, `Conflict` → 409, `Db`/`Backend` → 500). Keep it flat —
/// richer, domain-specific context belongs in the message strings, not new
/// variants (a variant is only added for a genuinely distinct HTTP semantic).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// The requested entity does not exist (C# `ResourceNotFoundException`).
    #[error("not found: {0}")]
    NotFound(String),

    /// The caller is not authenticated or lacks permission for the operation
    /// (C# `AuthenticationException` / `SecurityException`).
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// A caller-supplied argument was missing, malformed, or contradictory
    /// (C# `ArgumentException` / `ArgumentNullException`).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The operation conflicts with existing state — e.g. renaming a library to
    /// a name that already exists (C# `ConflictResult`). Maps to HTTP `409`.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A persistence-layer failure surfaced from [`ferrofin_db`].
    #[error("database error: {0}")]
    Db(#[from] ferrofin_db::DbError),

    /// Any other backend/infrastructure failure (I/O, an external process, a
    /// transcode subprocess, …) that has no more specific variant.
    #[error("backend error: {0}")]
    Backend(String),

    /// A backend/infrastructure failure that preserves the underlying error's
    /// [`source`](std::error::Error::source) chain instead of flattening it to a
    /// message like [`Backend`](Self::Backend).
    ///
    /// Subsystem crates (`ferrofin-drawing`, `ferrofin-mediaencoding`, …) define
    /// their own typed `thiserror` error and convert it into this variant via
    /// [`From`], so the original cause — an `io::Error`, an ffprobe parse
    /// failure, an HTTP transport error — stays inspectable through
    /// `Error::source()`. Maps to the same HTTP `500` as [`Backend`](Self::Backend).
    #[error(transparent)]
    BackendSource(Box<dyn std::error::Error + Send + Sync>),
}

impl ServiceError {
    /// Constructs a [`ServiceError::NotFound`] from anything string-like.
    ///
    /// Convenience for the common `Err(ServiceError::not_found("item 42"))`
    /// shape so call sites need not name the variant or call `.to_string()`.
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    /// Constructs a [`ServiceError::Unauthorized`] from anything string-like.
    pub fn unauthorized(why: impl Into<String>) -> Self {
        Self::Unauthorized(why.into())
    }

    /// Constructs a [`ServiceError::InvalidInput`] from anything string-like.
    pub fn invalid_input(why: impl Into<String>) -> Self {
        Self::InvalidInput(why.into())
    }

    /// Constructs a [`ServiceError::Conflict`] from anything string-like.
    pub fn conflict(why: impl Into<String>) -> Self {
        Self::Conflict(why.into())
    }

    /// Constructs a [`ServiceError::Backend`] from anything string-like.
    pub fn backend(why: impl Into<String>) -> Self {
        Self::Backend(why.into())
    }

    /// Constructs a [`ServiceError::BackendSource`] that preserves `source` as
    /// the error's [`source`](std::error::Error::source) chain.
    pub fn backend_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::BackendSource(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceError;

    #[test]
    fn constructors_build_the_matching_variant() {
        assert!(matches!(
            ServiceError::not_found("item"),
            ServiceError::NotFound(_)
        ));
        assert!(matches!(
            ServiceError::unauthorized("no token"),
            ServiceError::Unauthorized(_)
        ));
        assert!(matches!(
            ServiceError::invalid_input("bad arg"),
            ServiceError::InvalidInput(_)
        ));
        assert!(matches!(
            ServiceError::backend("ffmpeg exited 1"),
            ServiceError::Backend(_)
        ));
        assert!(matches!(
            ServiceError::conflict("name taken"),
            ServiceError::Conflict(_)
        ));
    }

    #[test]
    fn display_includes_the_message() {
        assert_eq!(
            ServiceError::not_found("item 42").to_string(),
            "not found: item 42"
        );
        assert_eq!(ServiceError::backend("io").to_string(), "backend error: io");
    }

    #[test]
    fn backend_source_preserves_the_source_chain() {
        use std::error::Error as _;

        // Stands in for a subsystem crate's typed error that wraps a lower-level
        // cause via `#[source]` — the shape `From<XError> for ServiceError` feeds
        // into `BackendSource`.
        #[derive(Debug, thiserror::Error)]
        #[error("decode failed")]
        struct Subsystem(#[source] std::io::Error);

        let cause = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let err = ServiceError::backend_source(Subsystem(cause));

        // `transparent` delegates Display to the wrapped subsystem error…
        assert_eq!(err.to_string(), "decode failed");
        // …and keeps its `#[source]` cause reachable through the chain.
        assert_eq!(err.source().unwrap().to_string(), "missing file");
        assert!(matches!(err, ServiceError::BackendSource(_)));
    }
}
