//! Error type for the `ferrofin-db` persistence layer.

use thiserror::Error;

/// Errors returned by the `ferrofin-db` SQLite layer.
///
/// Wraps the underlying [`sqlx::Error`] (connection, query, and pool
/// failures) and [`sqlx::migrate::MigrateError`] (schema-migration
/// failures), plus a variant for values read from the database that do not
/// map onto a known enum discriminant.
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
    /// this boot or come from a migration. The database is not opened. When
    /// the boot that applied the migrations snapshotted the file first
    /// (`<db>.pre-NNNN`, which a failed boot keeps), that copy is the one to
    /// restore; there is none when nothing was pending or the copy could not
    /// be written.
    #[error(
        "database has {violations} foreign-key violation(s) after migrating ({tables}); database not opened"
    )]
    MigrationIntegrity {
        /// The number of `foreign_key_check` rows reported.
        violations: usize,
        /// The violations per `child → parent` table pair, e.g. `UserData → Users: 3`.
        tables: String,
    },

    /// A database snapshot (the adoption copy, the one kept before a
    /// table-rebuilding migration, or a boot repair's) could not be written,
    /// flushed or renamed into place.
    #[error("failed to write database backup `{path}`: {source}")]
    Backup {
        /// The destination the snapshot was being copied to.
        path: String,
        /// The underlying filesystem failure.
        source: std::io::Error,
    },

    /// An export of a data directory (`ferrofin-server export-jellyfin`) could
    /// not read a source file or write its copy.
    #[error("export failed at `{path}`: {source}")]
    Export {
        /// The file or directory being read or written.
        path: String,
        /// The underlying filesystem failure.
        source: std::io::Error,
    },

    /// The source data directory was not the same after an export as before
    /// it, so the export cannot vouch for having left it untouched, nor for its
    /// output being a copy of one state of it.
    #[error(
        "the source data directory changed during the export, so its output cannot be trusted; changed: {}",
        .changed.join(", ")
    )]
    ExportSourceChanged {
        /// Each path that was added, removed or changed: relative to the
        /// source, or absolute for a file SQLite keeps beside a database
        /// that a link puts outside it.
        changed: Vec<String>,
    },
}

/// Convenient result alias for fallible `ferrofin-db` operations.
pub type Result<T> = std::result::Result<T, DbError>;
