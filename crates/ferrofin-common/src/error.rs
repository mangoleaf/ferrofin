//! Crate error type for the cryptography path.
//!
//! C# throws `ArgumentException`, `FormatException` and `NotSupportedException`
//! from `PasswordHash.Parse` and `CryptographyProvider.Verify`. Each maps to a
//! distinct variant here so callers (and the ported tests) can discriminate the
//! failure kind the same way `Assert.Throws<T>` does.

/// Errors raised while parsing or verifying password hashes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// A required argument was null or empty (C# `ArgumentException`).
    #[error("{0}")]
    Argument(String),

    /// A hash string was malformed (C# `FormatException`).
    #[error("{0}")]
    Format(String),

    /// A hash id is not supported by the provider (C# `NotSupportedException`).
    #[error("{0}")]
    NotSupported(String),
}

/// Convenience result alias for the cryptography path.
pub type Result<T> = std::result::Result<T, CryptoError>;
