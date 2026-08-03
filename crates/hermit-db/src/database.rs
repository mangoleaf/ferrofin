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

/// A handle to Hermit's SQLite database, wrapping two connection pools: a
/// multi-connection **reader** pool ([`Database::pool`]) and a
/// single-connection **writer** pool ([`Database::writer`]).
///
/// SQLite under WAL serves many concurrent readers but exactly one writer at a
/// time. Funnelling every write through a 1-connection pool turns would-be
/// `SQLITE_BUSY`/busy-timeout collisions between writers into orderly async
/// queueing at the app layer, and keeps a stalled write from occupying one of
/// the reader slots the request path depends on (the standard
/// dedicated-writer SQLite pattern).
///
/// Cheaply cloneable — the inner [`SqlitePool`]s are reference-counted, so
/// clones share the same underlying pools.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
    writer: SqlitePool,
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
        Self::connect_sized(url, None).await
    }

    /// [`Database::connect`] with an explicit pool size.
    ///
    /// `Some(n)` pins the pool at exactly `n` connections; `None` uses the
    /// sizing formula ([`default_pool_size`]). The composition root resolves
    /// its config layers (`HERMIT_DB_POOL` env / `db_pool` in `config.toml`)
    /// into this parameter — this crate no longer reads the environment.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the URL is invalid or
    /// the pool cannot be opened.
    pub async fn connect_sized(url: &str, pool_size: Option<u32>) -> Result<Self> {
        // Readers under WAL never block on a writer, so a stuck acquisition is
        // a bug to surface fast; only the single writer keeps the long,
        // Jellyfin-matching busy timeout to ride out checkpoint stalls.
        let read_options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let write_options = read_options
            .clone()
            .busy_timeout(std::time::Duration::from_secs(30));
        Self::connect_with(read_options, write_options, pool_size).await
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
        // Reader and writer are the SAME 1-connection pool: a second pool
        // would open its own, empty in-memory database.
        let writer = pool.clone();
        Ok(Self { pool, writer })
    }

    /// Builds the reader + writer pools from fully-formed connect options.
    ///
    /// `pool_size` `None` selects [`default_pool_size`] for the reader pool;
    /// the writer pool is always exactly one connection (SQLite allows one
    /// writer at a time regardless). Each sqlx SQLite connection is backed by
    /// its own dedicated OS thread (libsqlite3 is synchronous C), so the
    /// reader pool size is the concurrent-read thread count.
    async fn connect_with(
        read_options: SqliteConnectOptions,
        write_options: SqliteConnectOptions,
        pool_size: Option<u32>,
    ) -> Result<Self> {
        let max_connections = pool_size.unwrap_or_else(default_pool_size);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(read_options)
            .await?;
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(write_options)
            .await?;
        Ok(Self { pool, writer })
    }

    /// Applies all pending migrations (currently `0001_initial`) to the
    /// database, bringing it to the head schema. Idempotent — already-applied
    /// migrations are skipped.
    ///
    /// # Errors
    /// Returns [`DbError::Migrate`](crate::DbError::Migrate) if a migration
    /// fails to apply.
    pub async fn run_migrations(&self) -> Result<()> {
        MIGRATOR.run(&self.writer).await?;
        Ok(())
    }

    /// The **reader** pool, for `SELECT`-shaped queries (`fetch_*`).
    ///
    /// Writes routed here still work (WAL + busy timeout), but belong on
    /// [`Database::writer`] so they queue on the dedicated writer connection
    /// instead of occupying a reader slot.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The single-connection **writer** pool, for `INSERT`/`UPDATE`/`DELETE`
    /// (`.execute(...)`) and write transactions (`.begin()`).
    ///
    /// One connection means concurrent writes queue asynchronously in-process
    /// rather than colliding inside SQLite and burning the busy timeout. For
    /// the in-memory test database this is the same pool as
    /// [`Database::pool`].
    #[must_use]
    pub fn writer(&self) -> &SqlitePool {
        &self.writer
    }
}

/// The `auto` reader-pool size: **usable-core count** — the min of the CPU
/// affinity mask (`available_parallelism`) and the cgroup CFS quota.
///
/// The two disagree under container CPU *limits*: Docker `--cpus` and
/// Kubernetes CPU limits set a CFS quota but leave the affinity mask spanning
/// every host core, so `available_parallelism` alone over-sizes the pool.
/// Falls back to 4 when neither signal is readable.
///
/// ## Why pool ≈ cores is right in BOTH load regimes (measured, 2026-08-03)
///
/// SQLite reads on a page-cached library are CPU-bound, so the two candidate
/// regimes were measured with `benchmark/pool-sweep.sh` (50-VU mixed lockstep
/// over all 83 read endpoints, 4-CPU container, sizes 4→64, order-reversed
/// control run to exclude cumulative-state confounds):
///
/// - **Saturation** (one hot endpoint, open-model): pool ≈ cores maximizes
///   throughput; beyond that adds scheduler overhead. (Phase-B, earlier.)
/// - **Mixed lockstep** (many endpoints, closed-model): pool = 4 gave median
///   p50 12.7 ms / 1087 rps total; pool = 64 collapsed to 141 rps with worst
///   endpoints 10-50× slower (items_series 266 ms → 14 s) — while the pool
///   sampler showed `in_use=50, idle=14`, i.e. **zero acquisition queueing**.
///   The wide pool loses to CPU oversubscription thrash: ~50 CPU-bound
///   queries time-slicing 4 cores finish ~uniformly late, where the
///   FIFO-at-cores queue finishes them in ~service time each. Processor
///   sharing only beats FIFO when job sizes vary wildly — which they did
///   before the 2026-08 aggregate-query fixes (per-name full-library scans of
///   seconds each convoyed the 4-slot queue into 19 s p50s). Fix the slow
///   queries, not the pool: with uniform-ish job sizes, pool = cores wins.
///
/// Re-derive with `benchmark/pool-sweep.sh` before changing this; do not
/// resize from single-endpoint (or polluted-host) evidence. Raw curves:
/// `benchmark/results/pool-sweep-c11f1ce.json`.
fn default_pool_size() -> u32 {
    let affinity = std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok());
    match (affinity, cgroup_cpu_quota()) {
        (Some(a), Some(q)) => a.min(q).max(1),
        (Some(n), None) | (None, Some(n)) => n.max(1),
        (None, None) => 4,
    }
}

/// The whole-core CPU budget from the cgroup CFS quota, if one is set.
///
/// Reads cgroup v2 (`/sys/fs/cgroup/cpu.max`) when present, else falls back to
/// v1. Returns `None` when unlimited or unreadable (bare metal, quota `max`,
/// or non-Linux). The quota is rounded **up** — a 1.5-core limit permits 2.
fn cgroup_cpu_quota() -> Option<u32> {
    // cgroup v2: single `cpu.max` file present ⇒ trust it (`max` ⇒ unlimited).
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        return quota_cores_from_cpu_max(&s);
    }
    // cgroup v1: quota and period live in separate files; -1 ⇒ unlimited.
    let q = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?;
    let p = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?;
    quota_cores(q.trim().parse().ok()?, p.trim().parse().ok()?)
}

/// Parse a cgroup v2 `cpu.max` line (`"<quota|max> <period>"`) into whole cores.
fn quota_cores_from_cpu_max(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let quota = it.next()?; // an integer, or the literal `max` (⇒ parse fails ⇒ None)
    let period = it.next()?.parse().ok()?;
    quota_cores(quota.parse().ok()?, period)
}

/// `ceil(quota / period)` as a core count, or `None` if either is non-positive.
fn quota_cores(quota: i64, period: i64) -> Option<u32> {
    if quota <= 0 || period <= 0 {
        return None;
    }
    u32::try_from((quota + period - 1) / period).ok() // ceil; i64 div_ceil is unstable
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

    #[test]
    fn cpu_max_parses_to_usable_cores() {
        assert_eq!(quota_cores_from_cpu_max("400000 100000"), Some(4)); // --cpus=4
        assert_eq!(quota_cores_from_cpu_max("150000 100000"), Some(2)); // 1.5 rounds up
        assert_eq!(quota_cores_from_cpu_max("100000 100000"), Some(1));
        assert_eq!(quota_cores_from_cpu_max("max 100000"), None); // unlimited
        assert_eq!(quota_cores_from_cpu_max("garbage"), None);
        assert_eq!(quota_cores_from_cpu_max(""), None);
        assert_eq!(quota_cores(-1, 100_000), None); // cgroup v1 unlimited sentinel
        // A quota never inflates the pool above it, and never below 1.
        assert!(default_pool_size() >= 1);
    }

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
    async fn concurrent_writes_serialize_through_the_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("w.db").display());
        let db = Database::connect_sized(&url, Some(4))
            .await
            .expect("file connect");
        db.run_migrations().await.expect("migrations apply");

        // 16 concurrent writes race for the single writer connection; every
        // one must land (queued, not SQLITE_BUSY-dropped).
        let mut handles = Vec::new();
        for i in 0..16 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                sqlx::query(
                    r#"INSERT INTO "ActivityLogs"
                       ("Name","Type","UserId","DateCreated","LogSeverity","RowVersion")
                       VALUES (?1,'test','00000000-0000-0000-0000-000000000001',
                               '2026-01-01 00:00:00',0,0)"#,
                )
                .bind(format!("row-{i}"))
                .execute(db.writer())
                .await
            }));
        }
        for h in handles {
            h.await.expect("task").expect("insert lands");
        }
        let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ActivityLogs""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(count, 16);
    }

    #[tokio::test]
    async fn write_lands_while_readers_hold_connections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("rw.db").display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("file connect");
        db.run_migrations().await.expect("migrations apply");

        // Saturate the 2-connection reader pool with slow-ish reads while a
        // write goes through the dedicated writer — the write must not wait on
        // (or error against) the reader pool. WAL keeps them independent.
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                tokio::spawn(async move {
                    // A long JOIN-ish scan holds a reader connection a while.
                    let _: i64 = sqlx::query_scalar(
                        r#"SELECT COUNT(*) FROM "BaseItems" a, "BaseItems" b, "BaseItems" c"#,
                    )
                    .fetch_one(db.pool())
                    .await
                    .expect("read");
                    0
                })
            })
            .collect();
        sqlx::query(
            r#"INSERT INTO "ActivityLogs"
               ("Name","Type","UserId","DateCreated","LogSeverity","RowVersion")
               VALUES ('during-read','test','00000000-0000-0000-0000-000000000001',
                       '2026-01-01 00:00:00',0,0)"#,
        )
        .execute(db.writer())
        .await
        .expect("write lands while readers are busy");
        for r in readers {
            r.await.expect("reader task");
        }
    }

    #[tokio::test]
    async fn in_memory_writer_and_reader_share_one_database() {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("migrations apply");
        // A write through the writer pool must be visible to the reader pool —
        // for in-memory they are the same single connection by construction.
        sqlx::query(
            r#"INSERT INTO "ActivityLogs"
               ("Name","Type","UserId","DateCreated","LogSeverity","RowVersion")
               VALUES ('shared','test','00000000-0000-0000-0000-000000000001',
                       '2026-01-01 00:00:00',0,0)"#,
        )
        .execute(db.writer())
        .await
        .expect("insert");
        let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ActivityLogs""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(count, 1);
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
