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

/// A handle to Ferrofin's SQLite database, wrapping two connection pools: a
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
    /// The on-disk database file, when file-backed (`None` for in-memory).
    /// Lets [`Database::run_migrations`] snapshot the file before a
    /// table-rebuild migration.
    file_path: Option<std::path::PathBuf>,
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
    /// its config layers (`FERROFIN_DB_POOL` env / `db_pool` in `config.toml`)
    /// into this parameter — this crate no longer reads the environment.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the URL is invalid or
    /// the pool cannot be opened.
    pub async fn connect_sized(url: &str, pool_size: Option<u32>) -> Result<Self> {
        // Readers keep the full 30s busy timeout: WAL readers normally never
        // block on the writer, but VACUUM / wal_checkpoint(TRUNCATE) take
        // EXCLUSIVE locks — a 5s reader timeout turned a long vacuum into hard
        // SQLITE_BUSY failures for every in-flight request (a real
        // mid-playback black-screen). The maintenance task now also skips the
        // vacuum during playback; the long timeout is the second seatbelt.
        configure_sqlite_for_concurrency();
        let read_options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30))
            .pragma("mmap_size", MMAP_SIZE_BYTES.to_string())
            .foreign_keys(true);
        let write_options = read_options
            .clone()
            .busy_timeout(std::time::Duration::from_secs(30));
        let file_path = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
            .filter(|p| !p.is_empty() && *p != ":memory:")
            .map(std::path::PathBuf::from);
        // Apply migrations on a dedicated throwaway connection BEFORE the
        // pools open, with foreign keys DISABLED.
        //
        // Two reasons this connection is special:
        //  1. A pooled connection that spans a table-rebuild migration
        //     (0007 rebuilds `Users`/`BaseItems`) keeps stale column metadata
        //     and mis-decodes `SELECT *` by position afterwards.
        //  2. `foreign_keys = OFF` is LOAD-BEARING. A table rebuild drops the
        //     parent table; SQLite's implicit row-delete on `DROP TABLE` fires
        //     `ON DELETE CASCADE` on every child (Permissions, Preferences,
        //     Devices, UserData/watch-history, AncestorIds, MediaStreamInfos,
        //     …), silently gutting user data. `defer_foreign_keys` only defers
        //     the integrity CHECK, NOT the cascade ACTION — it does not help.
        //     `foreign_keys` cannot be changed inside a transaction, so it must
        //     be set OFF on this connection BEFORE `MIGRATOR.run` opens its
        //     per-migration transactions (SQLite's own recommended table-
        //     rebuild procedure). A `foreign_key_check` afterwards surfaces any
        //     integrity violation the rebuild introduced — see
        //     [`foreign_key_check`] for why it runs only on a boot that
        //     actually applied something.
        if let Some(path) = &file_path {
            use sqlx::ConnectOptions;
            let mut conn = write_options.clone().foreign_keys(false).connect().await?;
            adopt_jellyfin_database(&mut conn, path).await?;
            backup_before_rebuild(&mut conn, path).await?;
            let before = applied_migration_count(&mut conn).await?;
            MIGRATOR.run(&mut conn).await?;
            if applied_migration_count(&mut conn).await? != before {
                foreign_key_check(&mut conn).await?;
            }
            sqlx::Connection::close(conn).await?;
        }
        Self::connect_with(read_options, write_options, pool_size, file_path).await
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
        // Same process-wide setup as the file-backed path: without it this
        // pool's connection would have no `ferrofin_random()` and every
        // random-ordered query would fail with "no such function".
        configure_sqlite_for_concurrency();
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        // Reader and writer are the SAME 1-connection pool: a second pool
        // would open its own, empty in-memory database.
        let writer = pool.clone();
        Ok(Self {
            pool,
            writer,
            file_path: None,
        })
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
        file_path: Option<std::path::PathBuf>,
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
        // One-time at startup: the reader pool size is the concurrent-read thread
        // count (each sqlx SQLite connection owns a dedicated OS thread), the
        // single operational knob behind read throughput.
        tracing::info!(
            reader_connections = max_connections,
            writer_connections = 1,
            "database pools opened"
        );
        Ok(Self {
            pool,
            writer,
            file_path,
        })
    }

    /// Applies all pending migrations (currently `0001_initial`) to the
    /// database, bringing it to the head schema. Idempotent — already-applied
    /// migrations are skipped.
    ///
    /// # Errors
    /// Returns [`DbError::Migrate`](crate::DbError::Migrate) if a migration
    /// fails to apply.
    pub async fn run_migrations(&self) -> Result<()> {
        // File-backed databases were already migrated on a dedicated
        // connection inside `connect_sized` (see the staleness note there);
        // this pass is then a no-op. It remains the real migration path for
        // the in-memory test database, whose single shared connection cannot
        // go stale.
        if let Some(path) = &self.file_path {
            let mut conn = self.writer.acquire().await?;
            backup_before_rebuild(&mut conn, path).await?;
        }
        // One-time on the fresh/upgrade path: which schema head we brought the DB
        // to. `MIGRATOR.run` is idempotent, so this logs the target head, not
        // necessarily a newly-applied set.
        let head = MIGRATOR.iter().last().map(|m| m.version);
        MIGRATOR.run(&self.writer).await?;
        tracing::info!(
            migrations = MIGRATOR.iter().count(),
            head,
            "database migrations applied"
        );
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

    /// Counts library items grouped by their stored (C#) `Type` name.
    ///
    /// Feeds the `ferrofin_library_items{type=…}` metric gauge. Runs on the read
    /// pool over the `FerrofinIX_BaseItems_Type_CleanName` index; the placeholder seed
    /// row (`Type = 'PLACEHOLDER'`) is included as-is (the metric wiring maps the
    /// stored name to its last `.`-segment, so callers filter as they see fit).
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the query fails.
    pub async fn item_counts_by_type(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as(r#"SELECT "Type", COUNT(*) FROM "BaseItems" GROUP BY "Type""#)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// Reads a `FerrofinMeta` value (Ferrofin's own key/value table), or [`None`]
    /// when the key is unset.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the query fails.
    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let value =
            sqlx::query_scalar(r#"SELECT "Value" FROM "FerrofinMeta" WHERE "Key" = ?1 LIMIT 1"#)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(value)
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

/// The exact `__EFMigrationsHistory` migration-id set a Jellyfin 10.11.8
/// server leaves behind (68 rows) — the only Jellyfin database
/// generation Ferrofin adopts. Captured from a real `jellyfin/jellyfin:10.11.8`.
const JELLYFIN_10_11_8_MIGRATIONS: [&str; 68] = [
    "20200514181226_AddActivityLog",
    "20200613202153_AddUsers",
    "20200728005145_AddDisplayPreferences",
    "20200905220533_FixDisplayPreferencesIndex",
    "20201004171403_AddMaxActiveSessions",
    "20201204223655_AddCustomDisplayPreferences",
    "20210320181425_AddIndexesAndCollations",
    "20210407110544_NullableCustomPrefValue",
    "20210814002109_AddDevices",
    "20221022080052_AddIndexActivityLogsDateCreated",
    "20230526173516_RemoveEasyPassword",
    "20230626233818_AddTrickplayInfos",
    "20230923170422_UserCastReceiver",
    "20240729140605_AddMediaSegments",
    "20240928082930_MarkSegmentProviderIdNonNullable",
    "20241020103111_LibraryDbMigration",
    "20241111131257_AddedCustomDataKey",
    "20241111135439_AddedCustomDataKeyKey",
    "20241112152323_FixAncestorIdConfig",
    "20241112232041_FixMediaStreams",
    "20241112234144_FixMediaStreams2",
    "20241113133548_EnforceUniqueItemValue",
    "20250202021306_FixedCollation",
    "20250204092455_MakeStartEndDateNullable",
    "20250214031148_ChannelIdGuid",
    "20250326065026_AddInheritedParentalRatingSubValue",
    "20250327101120_AddKeyframeData",
    "20250327171413_AddHdr10PlusFlag",
    "20250331182844_FixAttachmentMigration",
    "20250401142247_FixAncestors",
    "20250405075612_FixItemValuesIndices",
    "20250420000000_CreateNetworkConfiguration",
    "20250420010000_MigrateNetworkConfiguration",
    "20250420020000_MigrateMusicBrainzTimeout",
    "20250420030000_MigrateEncodingOptions",
    "20250420040000_RenameEnableGroupingIntoCollections",
    "20250420050000_DisableTranscodingThrottling",
    "20250420060000_CreateUserLoggingConfigFile",
    "20250420070000_MigrateActivityLogDb",
    "20250420080000_RemoveDuplicateExtras",
    "20250420090000_AddDefaultPluginRepository",
    "20250420100000_MigrateUserDb",
    "20250420110000_ReaddDefaultPluginRepository",
    "20250420120000_MigrateDisplayPreferencesDb",
    "20250420130000_RemoveDownloadImagesInAdvance",
    "20250420140000_MigrateAuthenticationDb",
    "20250420150000_FixPlaylistOwner",
    "20250420160000_AddDefaultCastReceivers",
    "20250420170000_UpdateDefaultPluginRepository",
    "20250420180000_FixAudioData",
    "20250420190000_RemoveDuplicatePlaylistChildren",
    "20250420193000_MigrateLibraryDbCompatibilityCheck",
    "20250420200000_MigrateLibraryDb",
    "20250420210000_MoveExtractedFiles",
    "20250420220000_MigrateRatingLevels",
    "20250420230000_MoveTrickplayFiles",
    "20250420230000_RefreshInternalDateModified",
    "20250421000000_MigrateKeyframeData",
    "20250609115616_DetachUserDataInsteadOfDelete",
    "20250618010000_MigrateLibraryUserData",
    "20250620180000_FixDates",
    "20250622170802_BaseItemImageInfoDateModifiedNullable",
    "20250714044826_ResetJournalMode",
    "20250730215000_ReseedFolderFlag",
    "20250913211637_AddProperParentChildRelationBaseItemWithCascade",
    "20250925203415_ExtendPeopleMapKey",
    "20251009200000_CleanMusicArtist",
    "20260206200000_FixLibrarySubtitleDownloadLanguages",
];

/// Ferrofin migrations at or below this version define the Jellyfin-owned
/// schema shape; an adopted Jellyfin database already HAS that shape, so they
/// are baselined as applied-without-running. Everything above is
/// Ferrofin-additive (and written to be a no-op on Jellyfin-formatted data).
const JELLYFIN_SHAPE_MIGRATION_HEAD: i64 = 7;

/// Adopts an existing Jellyfin 10.11.8 SQLite database in place, if `conn` is
/// one — the drop-in path (exercised end-to-end by `suite/roundtrip.sh`).
///
/// Detection: an `__EFMigrationsHistory` table with no `_sqlx_migrations`
/// alongside it. The applied EF migration-id set must match
/// [`JELLYFIN_10_11_8_MIGRATIONS`] **exactly** — anything newer, older, or
/// partial is refused loudly rather than half-adopted. On adoption the file
/// is copied aside once (`<db>.pre-ferrofin`), Ferrofin's schema-shape migrations
/// (`0001`–`0007`) are baselined into `_sqlx_migrations` without executing —
/// the Jellyfin database already has that exact shape — and the caller's
/// normal `MIGRATOR.run` then applies only the Ferrofin-additive tail.
/// `__EFMigrationsHistory`/`__EFMigrationsLock` are never touched, so
/// switching back to Jellyfin keeps working.
async fn adopt_jellyfin_database(
    conn: &mut sqlx::SqliteConnection,
    path: &std::path::Path,
) -> Result<()> {
    use sqlx::migrate::Migrate;

    let is_jellyfin: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '__EFMigrationsHistory'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    if is_jellyfin.is_none() {
        return Ok(()); // Ferrofin-native (or fresh) database
    }
    let already_adopted: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    if already_adopted.is_some() {
        return Ok(());
    }

    // Version gate: exactly the 10.11.8 set, or refuse (never half-adopt).
    let applied: Vec<String> =
        sqlx::query_scalar(r#"SELECT "MigrationId" FROM "__EFMigrationsHistory" ORDER BY 1"#)
            .fetch_all(&mut *conn)
            .await?;
    let mut expected: Vec<&str> = JELLYFIN_10_11_8_MIGRATIONS.to_vec();
    expected.sort_unstable();
    if applied
        .iter()
        .map(String::as_str)
        .ne(expected.iter().copied())
    {
        let missing: Vec<&str> = expected
            .iter()
            .filter(|e| !applied.iter().any(|a| a == *e))
            .copied()
            .collect();
        let extra: Vec<&str> = applied
            .iter()
            .filter(|a| !expected.contains(&a.as_str()))
            .map(String::as_str)
            .collect();
        return Err(crate::DbError::UnsupportedJellyfinDatabase {
            reason: format!(
                "its migration history does not match Jellyfin 10.11.8 \
                 ({} applied vs {} expected; missing: {missing:?}; unknown: {extra:?}). \
                 Ferrofin adopts exactly the 10.11.8 schema generation — \
                 bring the database to Jellyfin 10.11.x first (or restore a backup)",
                applied.len(),
                expected.len(),
            ),
        });
    }

    // One-time safety copy, WAL folded in first so the file is complete.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut *conn)
        .await?;
    let backup = path.with_extension("db.pre-ferrofin");
    if !backup.exists() {
        std::fs::copy(path, &backup).map_err(|source| crate::DbError::Backup {
            path: backup.display().to_string(),
            source,
        })?;
    }

    // Baseline the schema-shape migrations: recorded as applied, never run.
    conn.ensure_migrations_table().await?;
    for migration in MIGRATOR.iter() {
        if migration.version > JELLYFIN_SHAPE_MIGRATION_HEAD {
            continue;
        }
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, installed_on, success, checksum, execution_time) \
             VALUES (?1, ?2, CURRENT_TIMESTAMP, TRUE, ?3, 0)",
        )
        .bind(migration.version)
        .bind(&*migration.description)
        .bind(&*migration.checksum)
        .execute(&mut *conn)
        .await?;
    }
    tracing::info!(
        database = %path.display(),
        backup = %backup.display(),
        baselined_through = JELLYFIN_SHAPE_MIGRATION_HEAD,
        "adopted an existing Jellyfin 10.11.8 database in place"
    );
    Ok(())
}

/// Snapshots the database file before migration `0007` first applies.
///
/// `0007` rebuilds `BaseItems`/`Users` (the 12-step SQLite table-rebuild
/// dance) — the riskiest migration shipped so far — so an existing
/// file-backed database gets copied aside once (`<db>.pre-0007`) before it
/// runs. Fresh databases (no `_sqlx_migrations` yet) and databases already at
/// or past 0007 are left alone.
async fn backup_before_rebuild(
    conn: &mut sqlx::SqliteConnection,
    path: &std::path::Path,
) -> Result<()> {
    let has_history: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    if has_history.is_none() {
        return Ok(()); // fresh database — nothing worth snapshotting
    }
    let applied_0007: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM \"_sqlx_migrations\" WHERE version = 7")
            .fetch_optional(&mut *conn)
            .await?;
    if applied_0007.is_some() {
        return Ok(());
    }
    // Fold the WAL into the main file so a plain file copy is a complete,
    // consistent snapshot (startup: no other writers yet).
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut *conn)
        .await?;
    let backup = path.with_extension("db.pre-0007");
    std::fs::copy(path, &backup).map_err(|source| crate::DbError::Backup {
        path: backup.display().to_string(),
        source,
    })?;
    tracing::info!(
        backup = %backup.display(),
        "database snapshot taken before the 0007 schema-rebuild migration"
    );
    Ok(())
}

/// How many migrations `_sqlx_migrations` records as applied (`0` when the
/// table does not exist yet — a fresh or Jellyfin-native database).
///
/// Used to tell "this boot applied something" from "this boot was a no-op
/// history check", which is what gates the post-migration
/// [`foreign_key_check`].
async fn applied_migration_count(conn: &mut sqlx::SqliteConnection) -> Result<i64> {
    let has_history: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    if has_history.is_none() {
        return Ok(0);
    }
    Ok(
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "_sqlx_migrations""#)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// Runs `PRAGMA foreign_key_check` and fails the open when it reports rows.
///
/// This is the seatbelt on the `foreign_keys = OFF` migration connection: a
/// table-rebuild migration drops a parent table, and with enforcement off a
/// mistake there leaves dangling child rows instead of erroring. The scan is
/// **O(rows × foreign keys)** — ~20 ms on a 10k-item library, and it grows with
/// the library — so it runs only on a boot that actually applied a migration.
/// That is not a weakened guarantee: a boot whose `MIGRATOR.run` was a no-op
/// changed no schema and can have introduced no violation, and the boot that
/// *did* apply the migration already ran this check and refused to open if it
/// failed. Every migration is therefore still verified, exactly once, on the
/// boot that applies it.
async fn foreign_key_check(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await?
        .len();
    if violations > 0 {
        tracing::error!(
            violations,
            "foreign-key violations after migration — database integrity is compromised"
        );
        return Err(crate::DbError::MigrationIntegrity { violations });
    }
    Ok(())
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
/// regimes were measured with `suite/perf/pool-sweep.sh` (50-VU mixed lockstep
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
/// Re-derive with `suite/perf/pool-sweep.sh` before changing this; do not
/// resize from single-endpoint (or polluted-host) evidence. Raw curves:
/// `suite/perf/results/pool-sweep-c11f1ce.json`.
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

/// The `mmap_size` ceiling requested on every read connection.
///
/// This is a **maximum, not a reservation**: SQLite maps `min(mmap_size, file
/// size)` and extends the mapping as the database grows, so a generous ceiling
/// costs nothing on a small database and needs no adjustment as a library fills
/// up. Verified by growing a database from 0 to 183 MB with the pragma
/// untouched — resident memory tracked the file, not the ceiling, and reads
/// kept using the mapping. That is why this is a constant rather than a tunable
/// or something recomputed after a scan: there is no value to adapt.
///
/// SQLite clamps the request to its own compile-time `SQLITE_MAX_MMAP_SIZE`
/// (0x7FFF0000, just under 2 GiB), so asking for more is harmless and simply
/// resolves to that limit. A database larger than the clamp is still served
/// correctly — only the portion beyond it reads through the page cache.
///
/// Why it matters: without it, SQLite's page cache serves reads from the
/// process heap, and every connection contends on the shared page-cache mutex.
/// Measured on a 100-item list endpoint at 1500 req/s: 14,021 ms p50 with
/// 1,564 MB anonymous heap versus 2.9 ms p50 with 264 MB. See
/// `suite/micro/FINDINGS.md`.
const MMAP_SIZE_BYTES: i64 = 0x7FFF_0000;

/// One-time process-wide SQLite setup: drops the global allocator mutex and
/// registers Ferrofin's connection-local `RANDOM()` replacement.
///
/// SQLite's default build wraps every `sqlite3Malloc`/`sqlite3_free` in a global
/// mutex purely to keep `sqlite3_memory_used()` accurate. Nothing here reads
/// those counters, but every connection pays the mutex — and since each sqlx
/// SQLite connection is its own OS thread, the reader pool contends on ONE lock
/// for every allocation the query engine makes.
///
/// Profiling a 100-item list endpoint at 400 req/s found 82 of 85 mutex-blocked
/// stacks sitting in `sqlite3_free`/`sqlite3Malloc`. That contention is what
/// capped effective parallelism at ~1-2 on a 32-core box and produced the
/// capacity cliff where latency went from 5 ms to ~9 s between 200 and 400
/// req/s. It is also why a SMALLER reader pool measured faster: fewer threads,
/// less contention on the same global lock.
///
/// The second step registers [`sqlite_random`](crate::sqlite_random)'s
/// `ferrofin_random()` for every connection opened afterwards — the same story
/// one lock further on: SQLite's `RANDOM()` serializes every row of a random
/// sort on the global PRNG mutex.
///
/// Must run before the first connection is opened — `sqlite3_config` fails once
/// SQLite has initialized, which is why this is a process-wide `Once` on the
/// connect path rather than per-pool setup. A failure is not fatal: it means
/// SQLite was already initialized and we keep the default behaviour.
fn configure_sqlite_for_concurrency() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_config` is only unsafe because it must not race with
        // an initialized library or a live connection. The `Once` plus its
        // placement before the first pool open give that ordering.
        let rc = unsafe {
            libsqlite3_sys::sqlite3_config(libsqlite3_sys::SQLITE_CONFIG_MEMSTATUS, 0_i32)
        };
        if rc != libsqlite3_sys::SQLITE_OK {
            tracing::debug!(
                rc,
                "sqlite3_config(MEMSTATUS, 0) declined — SQLite was already \
                 initialized; keeping default allocator bookkeeping"
            );
        }
        // Strictly after the `sqlite3_config` above: registering an
        // auto-extension initializes SQLite, and `sqlite3_config` is refused
        // once that has happened.
        crate::sqlite_random::register_random_function();
    });
}

#[cfg(test)]
mod tests {

    /// The gate on the post-migration `foreign_key_check`.
    ///
    /// The check is skipped on a boot whose `MIGRATOR.run` applied nothing —
    /// that boot changed no schema, so it can have introduced no violation, and
    /// the boot that DID apply the migration already checked. The whole
    /// argument rests on this counter telling "applied something" from "history
    /// check only", so it is pinned here: `0` before any migration exists (the
    /// table itself is absent — this must not error), the full chain length
    /// after, and unchanged across a re-open. If it ever reported a constant,
    /// the check would silently stop running on the boots that need it.
    #[tokio::test]
    async fn applied_migration_count_tracks_the_chain_and_is_stable_on_reopen() {
        use sqlx::ConnectOptions as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("count.db");
        let url = format!("sqlite://{}", path.display());

        let mut fresh = SqliteConnectOptions::from_str(&url)
            .expect("options")
            .create_if_missing(true)
            .connect()
            .await
            .expect("connect");
        assert_eq!(
            super::applied_migration_count(&mut fresh)
                .await
                .expect("count on a database with no history table"),
            0,
            "a database with no _sqlx_migrations must count 0, not error"
        );
        sqlx::Connection::close(fresh).await.expect("close");

        let db = Database::connect(&url).await.expect("connect");
        drop(db);

        let mut migrated = SqliteConnectOptions::from_str(&url)
            .expect("options")
            .connect()
            .await
            .expect("reconnect");
        let after = super::applied_migration_count(&mut migrated)
            .await
            .expect("count after migrating");
        let expected = i64::try_from(MIGRATOR.iter().count()).expect("chain length fits i64");
        assert_eq!(
            after, expected,
            "every bundled migration must be recorded — this is what makes \
             'nothing was applied' distinguishable from 'the chain just ran'"
        );

        // The second open is the no-op path the skip exists for: the count must
        // come back identical, so `before == after` and the scan is skipped.
        let db = Database::connect(&url).await.expect("reconnect handle");
        drop(db);
        assert_eq!(
            super::applied_migration_count(&mut migrated)
                .await
                .expect("count after a no-op open"),
            after,
            "a no-op open must leave the count untouched"
        );
        sqlx::Connection::close(migrated).await.expect("close");
    }

    /// The `mmap_size` ceiling must survive database growth without being
    /// re-applied, because nothing re-applies it: it is set once when a pooled
    /// connection opens, and a fresh install whose library is imported later
    /// would otherwise be stuck with whatever was right at first boot.
    ///
    /// Asserts the two properties that make the fixed ceiling correct: SQLite
    /// clamps the request to its own maximum rather than rejecting it, and the
    /// effective value is unchanged after the file grows by orders of magnitude.
    #[tokio::test]
    async fn mmap_ceiling_is_clamped_and_survives_database_growth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grow.db");
        let url = format!("sqlite://{}", path.display());
        let db = Database::connect(&url).await.expect("connect");

        let effective: i64 = sqlx::query_scalar("PRAGMA mmap_size")
            .fetch_one(db.pool())
            .await
            .expect("read mmap_size");
        assert!(
            effective > 0,
            "mmap must be enabled — with it off, list endpoints collapse under load"
        );
        assert!(
            effective <= MMAP_SIZE_BYTES,
            "SQLite clamps to its own maximum; asking for more must not error"
        );

        sqlx::query("CREATE TABLE grow (id INTEGER PRIMARY KEY, blob TEXT)")
            .execute(db.writer())
            .await
            .expect("create");
        let payload = "x".repeat(2048);
        for _ in 0..2000 {
            sqlx::query("INSERT INTO grow (blob) VALUES (?1)")
                .bind(&payload)
                .execute(db.writer())
                .await
                .expect("insert");
        }

        // The pragma was never re-applied, and the reads below must still work
        // against the now much larger file.
        let after: i64 = sqlx::query_scalar("PRAGMA mmap_size")
            .fetch_one(db.pool())
            .await
            .expect("read mmap_size after growth");
        assert_eq!(
            after, effective,
            "the ceiling must not need re-applying as the database grows"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grow")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(rows, 2000);
    }

    use super::*;
    use sqlx::Row as _;

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
        "FerrofinLinkedChildren",
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

    /// Seeds a file database at the pre-0007 schema (migrations 0001–0006
    /// applied and recorded) so a test can drive the real 0007+ upgrade path
    /// over pre-existing rows. Returns the `sqlite://` url.
    #[allow(clippy::too_many_lines)] // linear seed of many representative rows
    async fn seed_pre_0007_database(path: &std::path::Path) -> String {
        use sqlx::ConnectOptions;
        // Foreign keys ON here so the seeded child rows are genuinely
        // constrained — exactly the production shape that made the 0007
        // rebuild cascade-delete them.
        use sqlx::migrate::Migrate;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .connect()
            .await
            .expect("open seed db");
        conn.ensure_migrations_table()
            .await
            .expect("migrations table");
        for m in MIGRATOR.iter().filter(|m| m.version <= 6) {
            sqlx::raw_sql(&m.sql)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
            sqlx::query(
                "INSERT INTO _sqlx_migrations \
                 (version, description, installed_on, success, checksum, execution_time) \
                 VALUES (?1, ?2, CURRENT_TIMESTAMP, TRUE, ?3, 0)",
            )
            .bind(m.version)
            .bind(&*m.description)
            .bind(&*m.checksum)
            .execute(&mut conn)
            .await
            .expect("record migration");
        }
        // A user with an IsAdministrator permission (Kind=2) + a preference +
        // a played UserData row — the FK children that must survive the
        // Users/BaseItems rebuilds in 0007.
        let uid = "eef5ffe4-b970-4ed2-8ce8-f1881e032051";
        sqlx::query(
            r#"INSERT INTO "Users" ("Id","AuthenticationProviderId","DisplayCollectionsView",
               "DisplayMissingEpisodes","EnableAutoLogin","EnableLocalPassword",
               "EnableNextEpisodeAutoPlay","EnableUserPreferenceAccess","HidePlayedInLatest",
               "InternalId","InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
               "NormalizedUsername","PasswordResetProviderId","PlayDefaultAudioTrack",
               "RememberAudioSelections","RememberSubtitleSelections","RowVersion","SubtitleMode",
               "SyncPlayAccess","Username")
               VALUES (?1,'auth',0,0,0,0,1,1,1,1,0,0,0,'ADMIN','reset',1,1,1,0,0,0,'admin')"#,
        )
        .bind(uid)
        .execute(&mut conn)
        .await
        .expect("seed user");
        sqlx::query(
            r#"INSERT INTO "Permissions" ("Id","Kind","RowVersion","Value","UserId")
               VALUES (1,2,0,1,?1)"#,
        )
        .bind(uid)
        .execute(&mut conn)
        .await
        .expect("seed permission");
        sqlx::query(
            r#"INSERT INTO "Preferences" ("Id","Kind","RowVersion","Value","UserId")
               VALUES (1,0,0,'x',?1)"#,
        )
        .bind(uid)
        .execute(&mut conn)
        .await
        .expect("seed preference");
        // A BaseItem + a UserData child (watch history) referencing it.
        let item = "00000000-0000-0000-0000-0000000000aa";
        sqlx::query(
            r#"INSERT INTO "BaseItems" ("Id","IsFolder","IsInMixedFolder","IsLocked","IsMovie",
               "IsRepeat","IsSeries","IsVirtualItem","Type")
               VALUES (?1,0,0,0,1,0,0,0,'MediaBrowser.Controller.Entities.Movies.Movie')"#,
        )
        .bind(item)
        .execute(&mut conn)
        .await
        .expect("seed item");
        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite",
               "PlayCount","PlaybackPositionTicks","Played")
               VALUES (?1,?2,?1,0,1,0,1)"#,
        )
        .bind(item)
        .bind(uid)
        .execute(&mut conn)
        .await
        .expect("seed userdata");
        // A second item as the primary version, linked from `item` via a
        // LOWERCASE PrimaryVersionId — the alternate-version link that the
        // migration must uppercase (read case-sensitively).
        let primary = "00000000-0000-0000-0000-0000000000bb";
        sqlx::query(
            r#"INSERT INTO "BaseItems" ("Id","IsFolder","IsInMixedFolder","IsLocked","IsMovie",
               "IsRepeat","IsSeries","IsVirtualItem","Type")
               VALUES (?1,0,0,0,1,0,0,0,'MediaBrowser.Controller.Entities.Movies.Movie')"#,
        )
        .bind(primary)
        .execute(&mut conn)
        .await
        .expect("seed primary item");
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(primary)
            .bind(item)
            .execute(&mut conn)
            .await
            .expect("link alternate version");
        // A Live TV tuner + channel + programme with LOWERCASE GUIDs — the
        // join key (`Channels.Id = Programs.ChannelId`) and by-id lookups the
        // migration must uppercase or Live TV goes blank after upgrade.
        let tuner = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let channel = "11111111-2222-3333-4444-555555555555";
        sqlx::query(
            r#"INSERT INTO "LiveTvTunerHosts" ("Id","Url","Type","Data")
               VALUES (?1,'http://x','m3u','{}')"#,
        )
        .bind(tuner)
        .execute(&mut conn)
        .await
        .expect("seed tuner");
        sqlx::query(
            r#"INSERT INTO "LiveTvChannels" ("Id","TunerHostId","Name","StreamUrl")
               VALUES (?1,?2,'Ch','http://s')"#,
        )
        .bind(channel)
        .bind(tuner)
        .execute(&mut conn)
        .await
        .expect("seed channel");
        sqlx::query(
            r#"INSERT INTO "LiveTvPrograms" ("Id","ChannelId","StartDate","Title")
               VALUES ('66666666-7777-8888-9999-aaaaaaaaaaaa',?1,'2026-01-01 00:00:00','Show')"#,
        )
        .bind(channel)
        .execute(&mut conn)
        .await
        .expect("seed programme");
        sqlx::Connection::close(conn).await.expect("close");
        format!("sqlite://{}", path.display())
    }

    /// Regression for the 0007 cascade-delete data-loss bug: rebuilding a
    /// parent table (`Users`/`BaseItems`) via DROP fired `ON DELETE CASCADE`
    /// on every FK child, wiping Permissions/Preferences/Devices/UserData on
    /// any populated database. The migration connection must run with foreign
    /// keys OFF so the rebuild preserves child rows.
    #[tokio::test]
    async fn upgrading_a_populated_database_preserves_fk_child_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;

        // The real upgrade path: connect_sized runs 0007..=head with the fix.
        let db = Database::connect_sized(&url, Some(1))
            .await
            .expect("upgrade a populated database");

        let head = MIGRATOR.iter().last().map_or(0, |m| m.version);
        let at: i64 = sqlx::query_scalar(r#"SELECT MAX(version) FROM "_sqlx_migrations""#)
            .fetch_one(db.pool())
            .await
            .expect("version");
        assert_eq!(at, head, "migrated to head");

        for (table, expected) in [
            ("Permissions", 1),
            ("Preferences", 1),
            ("Users", 1),
            ("BaseItems", 3), // seeded movie + its primary version + placeholder
            ("UserData", 1),
        ] {
            let n: i64 = sqlx::query_scalar(&format!(r#"SELECT COUNT(*) FROM "{table}""#))
                .fetch_one(db.pool())
                .await
                .unwrap_or_else(|e| panic!("count {table}: {e}"));
            assert_eq!(n, expected, "`{table}` rows must survive the 0007 rebuild");
        }
        // The admin's IsAdministrator permission specifically — this is what
        // the device-access check needs, and losing it broke login.
        let admin: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "Permissions" WHERE "Kind" = 2 AND "Value" = 1"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("admin perm");
        assert_eq!(admin, 1, "the IsAdministrator permission must survive");
    }

    /// Invariant guard for the GUID-casing bug class: after upgrading a
    /// populated database, NO column may still hold a lowercase hyphenated
    /// GUID. The seed plants lowercase GUIDs in the columns the migration is
    /// responsible for (user/item ids, `PrimaryVersionId`, Live TV
    /// channel/programme ids); this test scans EVERY TEXT column of EVERY
    /// table so a future column added without a matching UPPER also trips it.
    ///
    /// N-format keys (`PresentationUniqueKey`, `CustomDataKey`,
    /// `ActivityLogs.ItemId`) are lowercase BY DESIGN but carry no hyphens, so
    /// the hyphenated-GUID pattern never matches them.
    #[tokio::test]
    async fn upgrading_a_populated_database_leaves_no_lowercase_guid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;
        let db = Database::connect_sized(&url, Some(1))
            .await
            .expect("upgrade");

        // A lowercase hyphenated GUID: 8-4-4-4-12 hex with a lowercase a–f.
        let is_lower_guid = |v: &str| {
            v.len() == 36
                && v.as_bytes()[8] == b'-'
                && v.as_bytes()[13] == b'-'
                && v.as_bytes()[18] == b'-'
                && v.as_bytes()[23] == b'-'
                && v.chars().any(|c| c.is_ascii_lowercase())
                && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        };

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' \
             AND name NOT LIKE '\\_%' ESCAPE '\\'",
        )
        .fetch_all(db.pool())
        .await
        .expect("tables");

        // Columns that are lowercase-hyphenated BY DESIGN, not join keys:
        //  - `UserData.CustomDataKey` — real Jellyfin stores the key lowercase
        //    while `ItemId` is uppercase (verified against a real 10.11.8 DB).
        //  - `FerrofinPlaybackSessions.PlaySessionId` — an opaque self-referential
        //    session id (written and matched as the same raw string, never
        //    cross-joined to a Jellyfin-owned uppercase id).
        let allowed_lowercase: &[(&str, &str)] = &[
            ("UserData", "CustomDataKey"),
            ("FerrofinPlaybackSessions", "PlaySessionId"),
        ];

        let mut offenders = Vec::new();
        for t in &tables {
            let cols: Vec<String> = sqlx::query(&format!("PRAGMA table_info(\"{t}\")"))
                .fetch_all(db.pool())
                .await
                .expect("cols")
                .into_iter()
                .filter(|r| r.get::<String, _>("type").eq_ignore_ascii_case("TEXT"))
                .map(|r| r.get::<String, _>("name"))
                .collect();
            for c in cols {
                if allowed_lowercase.contains(&(t.as_str(), c.as_str())) {
                    continue;
                }
                let vals: Vec<String> = sqlx::query_scalar(&format!(
                    "SELECT \"{c}\" FROM \"{t}\" WHERE \"{c}\" IS NOT NULL"
                ))
                .fetch_all(db.pool())
                .await
                .unwrap_or_default();
                if vals.iter().any(|v| is_lower_guid(v)) {
                    offenders.push(format!("{t}.{c}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "columns still holding lowercase hyphenated GUIDs after upgrade \
             (add them to 0007's UPPER block): {offenders:?}"
        );

        // And the specific links resolve: the alternate version is uppercase.
        let pv: Option<String> = sqlx::query_scalar(
            r#"SELECT "PrimaryVersionId" FROM "BaseItems" WHERE "PrimaryVersionId" IS NOT NULL LIMIT 1"#,
        )
        .fetch_optional(db.pool())
        .await
        .expect("pv");
        assert_eq!(
            pv.as_deref(),
            Some("00000000-0000-0000-0000-0000000000BB"),
            "PrimaryVersionId must be uppercased"
        );
        // The Live TV channel↔programme join key matches (both uppercase).
        let joined: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId""#,
        )
        .fetch_one(db.pool())
        .await
        .expect("join");
        assert_eq!(
            joined, 1,
            "Live TV programme must still join to its channel"
        );
    }

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
    async fn item_counts_group_by_type() {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("migrations apply");

        // Two movies + one episode on top of the seeded PLACEHOLDER row.
        for (id, ty) in [
            ("00000000-0000-0000-0000-0000000000a1", "Movie"),
            ("00000000-0000-0000-0000-0000000000a2", "Movie"),
            ("00000000-0000-0000-0000-0000000000b1", "Episode"),
        ] {
            sqlx::query(
                r#"INSERT INTO "BaseItems"
                   ("Id","IsFolder","IsInMixedFolder","IsLocked","IsMovie",
                    "IsRepeat","IsSeries","IsVirtualItem","Name","Type")
                   VALUES (?1,0,0,0,0,0,0,0,?2,?2)"#,
            )
            .bind(id)
            .bind(ty)
            .execute(db.writer())
            .await
            .expect("insert base item");
        }

        let counts: std::collections::HashMap<String, i64> = db
            .item_counts_by_type()
            .await
            .expect("counts")
            .into_iter()
            .collect();
        assert_eq!(counts.get("Movie"), Some(&2));
        assert_eq!(counts.get("Episode"), Some(&1));
        assert_eq!(counts.get("PLACEHOLDER"), Some(&1));
    }

    /// Creates a file-backed database shaped exactly like a real Jellyfin
    /// 10.11.8 one: the committed schema fixture plus the EF migration rows.
    async fn seed_jellyfin_fixture(path: &std::path::Path, migrations: &[&str]) {
        use sqlx::ConnectOptions;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("open fixture db");
        sqlx::raw_sql(include_str!("../tests/data/jellyfin-10.11.8-schema.sql"))
            .execute(&mut conn)
            .await
            .expect("apply fixture schema");
        for id in migrations {
            sqlx::query(
                r#"INSERT INTO "__EFMigrationsHistory" ("MigrationId", "ProductVersion")
                   VALUES (?1, '10.11.8.0')"#,
            )
            .bind(id)
            .execute(&mut conn)
            .await
            .expect("insert EF migration row");
        }
        sqlx::Connection::close(conn).await.expect("close");
    }

    #[tokio::test]
    async fn adopts_a_jellyfin_10_11_8_database_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &JELLYFIN_10_11_8_MIGRATIONS).await;

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("adoption succeeds");

        // Shape migrations baselined, additive tail actually applied.
        let versions: Vec<i64> =
            sqlx::query_scalar(r#"SELECT version FROM "_sqlx_migrations" ORDER BY version"#)
                .fetch_all(db.pool())
                .await
                .expect("sqlx history");
        let head = MIGRATOR.iter().last().map_or(0, |m| m.version);
        let recorded = i64::try_from(versions.len()).expect("small count");
        assert_eq!(recorded, head, "every migration recorded");
        assert!(versions.contains(&JELLYFIN_SHAPE_MIGRATION_HEAD));

        // Jellyfin's bookkeeping untouched; safety copy exists.
        let ef_rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "__EFMigrationsHistory""#)
            .fetch_one(db.pool())
            .await
            .expect("ef rows");
        assert_eq!(ef_rows, 68);
        assert!(path.with_extension("db.pre-ferrofin").exists());

        // Second open is a clean no-op (already adopted).
        drop(db);
        Database::connect_sized(&url, Some(2))
            .await
            .expect("re-open after adoption");
    }

    #[tokio::test]
    async fn refuses_a_wrong_generation_jellyfin_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        // One migration short of the 10.11.8 set — e.g. an older 10.11.x or a
        // partially upgraded database.
        let short = &JELLYFIN_10_11_8_MIGRATIONS[..JELLYFIN_10_11_8_MIGRATIONS.len() - 1];
        seed_jellyfin_fixture(&path, short).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("adoption must refuse");
        assert!(
            matches!(err, crate::DbError::UnsupportedJellyfinDatabase { .. }),
            "unexpected error: {err}"
        );
        // Refusal leaves the database untouched: no sqlx history, no backup.
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    #[tokio::test]
    async fn connect_file_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin-test.db");
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
