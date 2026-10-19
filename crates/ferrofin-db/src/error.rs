//! Error type for the `ferrofin-db` persistence layer.

use thiserror::Error;

/// Errors returned by the `ferrofin-db` SQLite layer.
///
/// Wraps the underlying [`sqlx::Error`] (connection, query, and pool
/// failures) and [`sqlx::migrate::MigrateError`] (schema-migration
/// failures), plus variants for values read from the database that do not
/// map onto a known enum discriminant or a valid `Guid`.
#[derive(Debug, Error)]
pub enum DbError {
    /// Two existing accounts would share the same invariant username key.
    #[error(
        "username normalization collision between accounts {first:?} and {second:?}; resolve their names in the original server and retry migration; accounts were not merged"
    )]
    UsernameCollision {
        /// First account ID and displayed name.
        first: (String, String),
        /// Second account ID and displayed name.
        second: (String, String),
    },

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

    /// An existing Jellyfin database could not be adopted (wrong schema
    /// generation). The database is left untouched.
    #[error("cannot adopt this Jellyfin database: {reason}")]
    UnsupportedJellyfinDatabase {
        /// Why adoption was refused, with the supported-version statement.
        reason: String,
    },

    /// The database holds dangling foreign-key references after the migration
    /// chain ran (`foreign_key_check` reported violations). They may pre-date
    /// this boot or come from a migration. The database is not opened; the
    /// pre-migration backup should be restored.
    #[error(
        "database has {violations} foreign-key violation(s) after migrating ({tables}); database not opened"
    )]
    MigrationIntegrity {
        /// The number of `foreign_key_check` rows reported.
        violations: usize,
        /// The violations per `child → parent` table pair, e.g. `UserData → Users: 3`.
        tables: String,
    },

    /// The pre-migration database snapshot could not be written.
    #[error("failed to write database backup `{path}`: {source}")]
    Backup {
        /// The destination the snapshot was being copied to.
        path: String,
        /// The underlying filesystem failure.
        source: std::io::Error,
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

/// Convenient result alias for fallible `ferrofin-db` operations.
pub type Result<T> = std::result::Result<T, DbError>;
