//! The [`Database`] connection handle — a [`sqlx::SqlitePool`] wrapper.

use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::error::Result;

/// The bundled initial-schema migration set (`./migrations`).
///
/// `sqlx::migrate!()` reads the directory at **compile time**, so building
/// this crate needs no live database or `DATABASE_URL`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A handle to Hermit's SQLite database, wrapping a connection pool.
///
/// Cheaply cloneable — the inner [`SqlitePool`] is reference-counted, so
/// clones share the same underlying pool.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Opens a pool against the SQLite database at `url`, creating the file if
    /// it does not exist.
    ///
    /// The connection is configured for server workloads: **WAL** journal
    /// mode, `NORMAL` synchronous, busy-timeout, and enforced foreign keys.
    /// Call [`Database::run_migrations`] afterwards to apply the schema.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the URL is invalid or
    /// the pool cannot be opened.
    pub async fn connect(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30))
            .foreign_keys(true);
        Self::connect_with(options).await
    }

    /// Opens a pool against a fresh, private in-memory SQLite database.
    ///
    /// Uses a single-connection pool so the in-memory database persists for
    /// the lifetime of the handle (a multi-connection pool would give each
    /// connection its own empty database). Intended for tests.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the pool cannot be
    /// opened.
    pub async fn connect_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Builds the pool from fully-formed connect options.
    async fn connect_with(options: SqliteConnectOptions) -> Result<Self> {
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        Ok(Self { pool })
    }

    /// Applies all pending migrations (currently `0001_initial`) to the
    /// database, bringing it to the head schema. Idempotent — already-applied
    /// migrations are skipped.
    ///
    /// # Errors
    /// Returns [`DbError::Migrate`](crate::DbError::Migrate) if a migration
    /// fails to apply.
    pub async fn run_migrations(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// A reference to the underlying connection pool, for issuing queries.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 31 head tables the migration must create.
    const EXPECTED_TABLES: &[&str] = &[
        "AccessSchedules",
        "ActivityLogs",
        "AncestorIds",
        "ApiKeys",
        "AttachmentStreamInfos",
        "BaseItemImageInfos",
        "BaseItemMetadataFields",
        "BaseItemProviders",
        "BaseItemTrailerTypes",
        "BaseItems",
        "Chapters",
        "CustomItemDisplayPreferences",
        "DeviceOptions",
        "Devices",
        "DisplayPreferences",
        "HomeSection",
        "ImageInfos",
        "ItemDisplayPreferences",
        "ItemValues",
        "ItemValuesMap",
        "KeyframeData",
        "LinkedChildren",
        "MediaSegments",
        "MediaStreamInfos",
        "PeopleBaseItemMap",
        "Peoples",
        "Permissions",
        "Preferences",
        "TrickplayInfos",
        "UserData",
        "Users",
    ];

    #[tokio::test]
    async fn connect_and_migrate_creates_expected_tables() {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("migrations apply");

        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(db.pool())
                .await
                .expect("query sqlite_master");

        for table in EXPECTED_TABLES {
            assert!(
                names.iter().any(|n| n == table),
                "missing expected table `{table}`"
            );
        }
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("first run");
        db.run_migrations().await.expect("second run is a no-op");
    }

    #[tokio::test]
    async fn seeds_placeholder_base_item() {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("migrations apply");

        let kind: String = sqlx::query_scalar(
            "SELECT \"Type\" FROM \"BaseItems\" \
             WHERE \"Id\" = '00000000-0000-0000-0000-000000000001'",
        )
        .fetch_one(db.pool())
        .await
        .expect("placeholder row exists");
        assert_eq!(kind, "PLACEHOLDER");
    }

    #[tokio::test]
    async fn connect_file_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hermit-test.db");
        let url = format!("sqlite://{}", path.display());

        let db = Database::connect(&url).await.expect("file connect");
        db.run_migrations().await.expect("migrations apply");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM \"Users\"")
            .fetch_one(db.pool())
            .await
            .expect("query Users");
        assert_eq!(count, 0);
    }
}
