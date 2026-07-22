//! Error type for the `hermit-db` persistence layer.

use thiserror::Error;

/// Errors returned by the `hermit-db` SQLite layer.
///
/// Wraps the underlying [`sqlx::Error`] (connection, query, and pool
/// failures) and [`sqlx::migrate::MigrateError`] (schema-migration
/// failures), plus variants for values read from the database that do not
/// map onto a known enum discriminant or a valid `Guid`.
#[derive(Debug, Error)]
pub enum DbError {
    /// A `sqlx` connection, pool, or query error.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// A stored integer did not correspond to any variant of the named enum.
    #[error("invalid discriminant {value} for enum `{enum_name}`")]
    InvalidEnumValue {
        /// The enum whose conversion failed.
        enum_name: &'static str,
        /// The out-of-range integer read from the database.
        value: i32,
    },

    /// A stored `Guid` column did not parse as a hyphenated UUID.
    #[error("invalid guid in column `{column}`: {source}")]
    InvalidGuid {
        /// The entity column whose stored `Guid` string was malformed.
        column: &'static str,
        /// The underlying UUID parse failure.
        source: uuid::Error,
    },
}

/// Convenient result alias for fallible `hermit-db` operations.
pub type Result<T> = std::result::Result<T, DbError>;
