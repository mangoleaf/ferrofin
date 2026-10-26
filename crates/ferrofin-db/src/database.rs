//! The [`Database`] connection handle — a [`sqlx::SqlitePool`] wrapper.

use std::str::FromStr;
use std::sync::Arc;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::entities::display_preferences::HomeSectionEntity;
use crate::error::Result;

/// The bundled initial-schema migration set (`./migrations`).
///
/// `sqlx::migrate!()` reads the directory at **compile time**, so building
/// this crate needs no live database or `DATABASE_URL`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The version of the last migration in this build's chain: a database
/// migrated past it was written by a newer build, and this one cannot open it
/// (`MIGRATOR.run` refuses applied versions it does not know).
#[must_use]
pub fn schema_head() -> i64 {
    MIGRATOR
        .iter()
        .last()
        .map_or(0, |migration| migration.version)
}

/// A `BaseItems` row projected down to what the inherited-images walk and
/// the Episode/Season/Audio parent blocks of the DTO service read off a
/// parent: its kind, the three ids the walk follows, the album-artist name
/// a `MusicAlbum` resolves its artist by, the path the collection-folder
/// fallback matches on, and the two loudness columns behind
/// `AlbumNormalizationGain`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ImageParentRow {
    /// The row's `Guid` primary key, hyphenated (`Id`).
    pub id: String,
    /// The stored CLR type name (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: String,
    /// `ParentId`, if any.
    pub parent_id: Option<String>,
    /// `OwnerId`, if any.
    pub owner_id: Option<String>,
    /// `SeriesId`, if any (a season's display parent).
    pub series_id: Option<String>,
    /// `SeasonId`, if any (an episode's display parent).
    pub season_id: Option<String>,
    /// The pipe-joined album artists (`AlbumArtists`), if any.
    pub album_artists: Option<String>,
    /// The on-disk path (`Path`), if any.
    pub path: Option<String>,
    /// The integrated loudness in LUFS (`LUFS`), if measured.
    #[sqlx(rename = "LUFS")]
    pub lufs: Option<f64>,
    /// The stored normalization gain (`NormalizationGain`), if any.
    pub normalization_gain: Option<f64>,
}

/// A handle to Ferrofin's SQLite database, wrapping two connection pools: a
/// multi-connection **reader** pool ([`Database::pool`]) and a
/// single-connection **writer** pool ([`Database::writer`]), plus a private
/// one-connection pool for [`Database::snapshot_to`].
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
    /// One connection, opened on first use, for [`Database::snapshot_to`], so
    /// a long `VACUUM INTO` holds neither the writer nor a reader slot and
    /// [`Database::close`] still waits for it.
    snapshotter: SqlitePool,
    /// One snapshot or one vacuum-and-checkpoint pass at a time
    /// ([`Database::try_maintenance_turn`]), shared by every clone.
    maintenance: Arc<tokio::sync::Mutex<()>>,
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
        init_sqlite();
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
        //     [`foreign_key_check`] for why it runs on EVERY boot.
        if let Some(path) = &file_path {
            use sqlx::ConnectOptions;
            let mut conn = write_options.clone().foreign_keys(false).connect().await?;
            crate::normalized_usernames::preflight(&mut conn).await?;
            adopt_jellyfin_database(&mut conn, path).await?;
            backup_before_rebuild(&mut conn, path).await?;
            MIGRATOR.run(&mut conn).await?;
            log_dangling_row_cleanup(&mut conn).await?;
            crate::normalized_usernames::repair(&mut conn).await?;
            foreign_key_check(&mut conn).await?;
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
        init_sqlite();
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        // Reader and writer are the SAME 1-connection pool: a second pool
        // would open its own, empty in-memory database.
        let writer = pool.clone();
        let snapshotter = pool.clone();
        Ok(Self {
            pool,
            writer,
            snapshotter,
            maintenance: Arc::default(),
            file_path: None,
        })
    }

    /// Builds the reader, writer and snapshot pools from fully-formed connect
    /// options.
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
        let snapshotter = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(read_options.clone());
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
            snapshotter,
            maintenance: Arc::default(),
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
        {
            let mut conn = self.writer.acquire().await?;
            crate::normalized_usernames::preflight(&mut conn).await?;
            // Same rule as the file path above: a table rebuild (`0007`,
            // `0032`) must not cascade-delete child rows, so foreign keys go
            // OFF for the run — outside any transaction — and back ON after.
            // The in-memory test database is the one that actually migrates
            // here, and a test seeding rows between two chain heads would
            // otherwise lose them exactly as a real upgrade would with FKs on.
            sqlx::query("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await?;
            let run = MIGRATOR.run(&mut *conn).await;
            sqlx::query("PRAGMA foreign_keys = ON")
                .execute(&mut *conn)
                .await?;
            run?;
            crate::normalized_usernames::repair(&mut conn).await?;
        }
        tracing::info!(
            migrations = MIGRATOR.iter().count(),
            head,
            "database migrations applied"
        );
        // The by-name half of migration 0028's backfill. It cannot live in the
        // chain because a by-name row's key is `{Type}-{Name.RemoveDiacritics()}`
        // and SQLite has no `RemoveDiacritics`; see
        // `presentation_key::backfill_by_name_presentation_keys` for why a
        // keyless by-name row is an item-loss bug and not a cosmetic one. The
        // pass is indexed on `Type` and idempotent, so a repaired database pays
        // one index range scan per boot.
        crate::presentation_key::backfill_by_name_presentation_keys(&self.writer).await?;
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
    /// pool over the `IX_BaseItems_Type_CleanName` index; the placeholder seed
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

    /// How many Live TV tuner hosts are configured.
    ///
    /// The Live TV availability gate reads this — C#
    /// `LiveTvManager.IsLiveTvEnabled` tests `TunerHosts.Length > 0`, and with
    /// no tuner Jellyfin leaves the Live TV view out of `GetUserViews`
    /// entirely.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn live_tv_tuner_count(&self) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinLiveTvTunerHosts""#)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Writes a `FerrofinMeta` value, replacing any existing one.
    ///
    /// The table is Ferrofin's own bookkeeping — one-shot import and repair
    /// markers — never anything Jellyfin reads.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, ?2)
               ON CONFLICT("Key") DO UPDATE SET "Value" = excluded."Value""#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// A display-preferences row's `HomeSection` children, ordered by `Order`,
    /// then `Id`.
    ///
    /// Stands in for EF's `.Include(e => e.HomeSections)` eager-load, which the
    /// flat `sqlx` seam has no equivalent for. A Jellyfin database holds several
    /// rows per `Order` (upstream's save never deletes the old ones), loaded in
    /// `Id` order, so the newest comes last and wins where a caller keys them by
    /// `Order`, as upstream's `GET /DisplayPreferences` does.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn home_sections(
        &self,
        display_preferences_id: i64,
    ) -> Result<Vec<HomeSectionEntity>> {
        let rows = sqlx::query_as::<_, HomeSectionEntity>(
            r#"SELECT "Id", "DisplayPreferencesId", "Order", "Type"
               FROM "HomeSection"
               WHERE "DisplayPreferencesId" = ?1
               ORDER BY "Order", "Id""#,
        )
        .bind(display_preferences_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Replaces a display-preferences row's `HomeSection` children with
    /// `sections`, each `(order, type discriminant)`.
    ///
    /// A wholesale replace, mirroring the C# controller's
    /// `HomeSections.Clear()` + re-add that `UpdateDisplayPreferences` flushes.
    /// One transaction, so a concurrent read never sees the empty window
    /// between the delete and the inserts.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn replace_home_sections(
        &self,
        display_preferences_id: i64,
        sections: &[(i32, i32)],
    ) -> Result<()> {
        let mut tx = self.writer.begin().await?;
        sqlx::query(r#"DELETE FROM "HomeSection" WHERE "DisplayPreferencesId" = ?1"#)
            .bind(display_preferences_id)
            .execute(&mut *tx)
            .await?;
        for (order, type_) in sections {
            sqlx::query(
                r#"INSERT INTO "HomeSection" ("DisplayPreferencesId", "Order", "Type")
                   VALUES (?1, ?2, ?3)"#,
            )
            .bind(display_preferences_id)
            .bind(*order)
            .bind(*type_)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Writes a Live TV listing provider, replacing any row with the same id.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn upsert_live_tv_listing_provider(
        &self,
        id: &str,
        kind: &str,
        path: &str,
        data: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvListingProviders" ("Id","Type","Path","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Type"=excluded."Type","Path"=excluded."Path","Data"=excluded."Data""#,
        )
        .bind(id)
        .bind(kind)
        .bind(path)
        .bind(data)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Every Live TV tuner host as `(id, url, type, data)`, id-ordered.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn live_tv_tuner_hosts(&self) -> Result<Vec<(String, String, String, String)>> {
        let rows = sqlx::query_as(
            r#"SELECT "Id", "Url", "Type", "Data" FROM "FerrofinLiveTvTunerHosts" ORDER BY "Id""#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Every Live TV listing provider as `(id, type, path)`, id-ordered.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn live_tv_listing_providers(&self) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query_as(
            r#"SELECT "Id", "Type", "Path" FROM "FerrofinLiveTvListingProviders" ORDER BY "Id""#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Removes every Live TV tuner host.
    ///
    /// Deliberately unconditional rather than `#[cfg(test)]`: its caller is a
    /// test in ANOTHER crate, and `cfg(test)` only applies while this one is
    /// being tested.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete_all_live_tv_tuner_hosts(&self) -> Result<()> {
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvTunerHosts""#)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Writes a Live TV tuner host, replacing any row with the same id.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn upsert_live_tv_tuner_host(
        &self,
        id: &str,
        url: &str,
        kind: &str,
        data: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id","Url","Type","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Url"=excluded."Url","Type"=excluded."Type","Data"=excluded."Data""#,
        )
        .bind(id)
        .bind(url)
        .bind(kind)
        .bind(data)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Every `(item id, provider key, provider value)` recorded for `ids`, with
    /// each id in its stored (uppercase, hyphenated) GUID form.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn provider_ids_for_items(
        &self,
        ids: &[String],
    ) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" IN ({placeholders})"#,
            );
            let mut query = sqlx::query_as::<_, (String, String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// The `Data` blob of each of `ids` that has one, keyed by the id in its
    /// stored (uppercase, hyphenated) GUID form.
    ///
    /// A season's or episode's TMDB link depends on its SERIES' `DisplayOrder`,
    /// which lives in that blob rather than a column, so a page of episodes
    /// reads the owning series' blobs in one query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn item_data_blobs(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Id", "Data" FROM "BaseItems"
                   WHERE "Id" IN ({placeholders}) AND "Data" IS NOT NULL"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// The `Studios` column of the rows among `ids` that have one, keyed by the
    /// id in its stored (uppercase, hyphenated) GUID form.
    ///
    /// One query for a page of episodes/seasons, whose `SeriesStudio` is
    /// `series.Studios.FirstOrDefault()` (v10.11.8
    /// Emby.Server.Implementations/Dto/DtoService.cs:1228-1234 and :1256-1262)
    /// — a value that lives on the SERIES row, not on the item being projected.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn item_studios(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Id", "Studios" FROM "BaseItems"
                   WHERE "Id" IN ({placeholders}) AND "Studios" IS NOT NULL AND "Studios" <> ''"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// The `OriginalLanguage` of each of `ids` that has one, as `(Id,
    /// OriginalLanguage)` pairs in the stored GUID form.
    ///
    /// One query for a page of episodes, whose default audio stream follows
    /// `GetInheritedOriginalLanguage()` — the SERIES' original language when
    /// the episode has none of its own (v12.2
    /// MediaBrowser.Controller/Entities/TV/Episode.cs:157-160).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn item_original_languages(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Id", "OriginalLanguage" FROM "BaseItems"
                   WHERE "Id" IN ({placeholders}) AND "OriginalLanguage" IS NOT NULL"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// How many alternate-version rows point at each of `primary_ids` through
    /// `PrimaryVersionId`, keyed by the primary's id in its stored (uppercase,
    /// hyphenated) GUID form. Primaries with no alternate are absent.
    ///
    /// The number behind a list page's `MediaSourceCount` (`linked + local +
    /// 1`, `Video.GetMediaSourceCount`) when the page did not ask for the
    /// sources themselves — a count over the partial `PrimaryVersionId` index
    /// instead of the full rows the `MediaSources` field needs.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn alternate_version_counts(
        &self,
        primary_ids: &[String],
    ) -> Result<Vec<(String, i64)>> {
        let mut out = Vec::new();
        for chunk in primary_ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            // The seeded placeholder row is excluded exactly as the full-row
            // read (`get_items_by_primary_version_batch`) excludes it.
            let sql = format!(
                r#"SELECT "PrimaryVersionId", COUNT(*) FROM "BaseItems"
                   WHERE "PrimaryVersionId" IN ({placeholders}) AND "Id" <> ?{}
                   GROUP BY "PrimaryVersionId""#,
                chunk.len() + 1
            );
            let mut query = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            query = query.bind(crate::PLACEHOLDER_ITEM_ID);
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// The [`ImageParentRow`] projection of each of `ids` that exists, keyed
    /// by the id in its stored (uppercase, hyphenated) GUID form.
    ///
    /// One batched read per hop of the DTO service's inherited-images walk
    /// (`DtoService.AddInheritedImages`), instead of a full 72-column row per
    /// parent per item.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn image_parent_rows(&self, ids: &[String]) -> Result<Vec<ImageParentRow>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Id", "Type", "ParentId", "OwnerId", "SeriesId", "SeasonId",
                          "AlbumArtists", "Path", "LUFS", "NormalizationGain"
                   FROM "BaseItems" WHERE "Id" IN ({placeholders})"#,
            );
            let mut query = sqlx::query_as::<_, ImageParentRow>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// Every row of the stored `type_name` (a CLR type name such as
    /// `MediaBrowser.Controller.Entities.CollectionFolder`) as
    /// `(Id, Path, Data)` — the candidate set of
    /// `LibraryManager.GetCollectionFolders(item)` (the user root's children,
    /// matched on `Path` or on the `PhysicalLocationsList` inside `Data`,
    /// which the caller parses).
    ///
    /// An equality on `Type` so the `Type`-led indexes serve it: a `LIKE`
    /// suffix match scans the whole table, and this read sits on every list
    /// page whose image walk reaches a library root.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn rows_of_type(
        &self,
        type_name: &str,
    ) -> Result<Vec<(String, Option<String>, Option<String>)>> {
        Ok(
            sqlx::query_as(r#"SELECT "Id", "Path", "Data" FROM "BaseItems" WHERE "Type" = ?1"#)
                .bind(type_name)
                .fetch_all(self.pool())
                .await?,
        )
    }

    /// Names the `PhotoAlbum` rows among `ids`, keyed by the id in its stored
    /// (uppercase, hyphenated) GUID form.
    ///
    /// A photo's `Album`/`AlbumId` come from its parent album, and a page of
    /// photos resolves them all in one query rather than one per photo. Rows
    /// that are not a `PhotoAlbum` — a loose photo's collection folder, say —
    /// are simply absent from the result.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn photo_album_names(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Id", "Name" FROM "BaseItems"
                   WHERE "Id" IN ({placeholders})
                     AND "Type" LIKE '%.PhotoAlbum' AND "Name" IS NOT NULL"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
    }

    /// `Playlist.OwnerUserId` of each of the playlists among `ids` (stored
    /// GUID form) that has an owner: `(PlaylistId, OwnerUserId)` from
    /// `FerrofinPlaylists`. A playlist with no owner (an older build's
    /// user-less API-key create, or a `Data` naming none) and an id that is
    /// no playlist are absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn playlist_owners(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(crate::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "PlaylistId", "OwnerUserId" FROM "FerrofinPlaylists"
                   WHERE "PlaylistId" IN ({placeholders}) AND "OwnerUserId" IS NOT NULL"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(self.pool()).await?);
        }
        Ok(out)
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

    /// The adoption record, if this database was adopted from a Jellyfin
    /// generation by a Ferrofin that wrote one (see [`AdoptionState`]).
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the query fails.
    pub async fn adoption_state(&self) -> Result<Option<AdoptionState>> {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'FerrofinAdoption'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let row: Option<(String, i64)> = sqlx::query_as(
            r#"SELECT "Generation", "MembershipImportDone" FROM "FerrofinAdoption" LIMIT 1"#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(generation, done)| AdoptionState {
            generation,
            membership_import_done: done != 0,
        }))
    }

    /// Whether Jellyfin's `__EFMigrationsHistory` records `migration_id`, i.e.
    /// Jellyfin already ran that routine here before Ferrofin adopted the
    /// database. Always `false` on a Ferrofin-native database, which has no
    /// such table (adoption keeps it untouched).
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the query fails.
    pub async fn jellyfin_migration_applied(&self, migration_id: &str) -> Result<bool> {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '__EFMigrationsHistory'",
        )
        .fetch_optional(&self.pool)
        .await?;
        if exists.is_none() {
            return Ok(false);
        }
        let applied: Option<i64> = sqlx::query_scalar(
            r#"SELECT 1 FROM "__EFMigrationsHistory" WHERE "MigrationId" = ?1 LIMIT 1"#,
        )
        .bind(migration_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(applied.is_some())
    }

    /// Plants an adoption record as the gate would — for tests of the boot
    /// repairs that key on it (an in-memory database never goes through the
    /// gate).
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the write fails.
    #[doc(hidden)]
    pub async fn record_adoption(&self, generation: &str) -> Result<()> {
        sqlx::query(ADOPTION_TABLE_DDL)
            .execute(&self.writer)
            .await?;
        sqlx::query(
            r#"INSERT INTO "FerrofinAdoption" ("Generation", "AdoptedAt", "MembershipImportDone")
               VALUES (?1, CURRENT_TIMESTAMP, 0)"#,
        )
        .bind(generation)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// The writer-pool half of a membership import: marks the adoption
    /// record's import as done. Callers run it inside the same transaction
    /// as the imported rows via [`Self::writer`]; this convenience is for the
    /// non-transactional case.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the write fails.
    pub async fn mark_membership_import_done(&self) -> Result<()> {
        sqlx::query(r#"UPDATE "FerrofinAdoption" SET "MembershipImportDone" = 1"#)
            .execute(&self.writer)
            .await?;
        Ok(())
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

    /// Checks that a reader connection can be had and answers a query: the
    /// `/health` and `/health/ready` probe (upstream
    /// `DbContextFactoryHealthCheck`, which asks `CanConnectAsync`). It never
    /// touches the writer, so a long write cannot fail it. It waits at most
    /// the reader pool's acquire timeout, which is not set here (sqlx's
    /// default, 30 s).
    ///
    /// # Errors
    ///
    /// Returns the error when no connection can be acquired (the pool is
    /// closed, or saturated past its acquire timeout) or the query fails.
    pub async fn can_connect(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Closes every pool, waiting for checked-out connections to be returned.
    ///
    /// The composition root calls this when a server lifetime ends so that an
    /// in-process restart reopens the file with NO connection from the previous
    /// host still holding it (a lingering connection closing later would
    /// checkpoint its stale WAL over a freshly restored database). Idempotent.
    pub async fn close(&self) {
        self.pool.close().await;
        self.writer.close().await;
        self.snapshotter.close().await;
    }

    /// Whether [`Database::close`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.pool.is_closed()
    }

    /// Writes a consistent snapshot of the database to `dest` (`VACUUM INTO`):
    /// one transaction's view, complete with everything in the WAL, unaffected
    /// by writers that keep going meanwhile — unlike a file copy of a live
    /// WAL-mode database, which a checkpoint can tear mid-read. `dest` must not
    /// exist.
    ///
    /// It runs on the snapshot pool's one connection. `VACUUM INTO` needs only
    /// a read transaction, and holding a writer or reader connection for the
    /// whole copy would queue every write behind it (the writer pool has one
    /// connection) or, on a one-connection reader pool, every read. The cost
    /// is a WAL that cannot be checkpointed back to empty until the snapshot's
    /// read transaction ends, so the copy waits for, and then holds, the
    /// maintenance turn ([`Database::try_maintenance_turn`]).
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the statement fails
    /// (including when `dest` already exists).
    pub async fn snapshot_to(&self, dest: &std::path::Path) -> Result<()> {
        let _turn = if let Ok(turn) = Arc::clone(&self.maintenance).try_lock_owned() {
            turn
        } else {
            // A vacuum of a large database can take minutes: say why the
            // backup is not copying yet.
            tracing::info!("database snapshot waiting for database maintenance to finish");
            Arc::clone(&self.maintenance).lock_owned().await
        };
        sqlx::query(sqlx::AssertSqlSafe(vacuum_into(dest)))
            .execute(&self.snapshotter)
            .await?;
        Ok(())
    }

    /// The database's maintenance turn when it is free, `None` while a
    /// snapshot or another maintenance pass holds it. A vacuum and a truncating checkpoint
    /// (`OptimizeDatabaseTask`) take it for their whole pass, and
    /// [`Database::snapshot_to`] waits for it. A snapshot holds a read
    /// transaction for the whole copy. A truncating checkpoint would wait
    /// for that read through the busy timeout while holding the one writer,
    /// and every write would queue behind it. A vacuum would grow the WAL
    /// by the database's size, which no checkpoint can reclaim until the
    /// snapshot ends.
    #[must_use]
    pub fn try_maintenance_turn(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Arc::clone(&self.maintenance).try_lock_owned().ok()
    }

    /// Snapshots a file-backed database once, before a boot repair rewrites
    /// what users made (as a table-rebuild migration snapshots the file to
    /// `<db>.pre-00NN`): to `<db>` with its extension set to `extension`
    /// (`"db.pre-rehome"` → `jellyfin.db.pre-rehome`), through
    /// [`Database::snapshot_to`]. Returns the path written — `None` for an
    /// in-memory database or when that snapshot already exists (an earlier
    /// boot's, the file as it was before the first attempt).
    ///
    /// The snapshot is written under a temporary name and only renamed into
    /// place once it is complete and on disk ([`publish_snapshot`]), so an
    /// existing one is never a copy a full disk or a kill cut short. Something
    /// other than a file on the snapshot's name (a directory) is no snapshot:
    /// the rename onto it fails, and so does the repair, rather than run with
    /// none.
    ///
    /// # Errors
    /// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the snapshot fails,
    /// or [`DbError::Backup`](crate::DbError::Backup) if it cannot be flushed
    /// or renamed into place.
    pub async fn snapshot_once(&self, extension: &str) -> Result<Option<std::path::PathBuf>> {
        let Some(path) = &self.file_path else {
            return Ok(None);
        };
        let backup = path.with_extension(extension);
        if backup.is_file() {
            log_kept_snapshot(&backup);
            return Ok(None);
        }
        let partial = start_snapshot(&backup)?;
        let written = match self.snapshot_to(&partial).await {
            Ok(()) => publish_snapshot(&partial, &backup),
            Err(error) => Err(error),
        };
        abandon_on_error(&partial, written)?;
        Ok(Some(backup))
    }
}

/// The exact `__EFMigrationsHistory` migration-id set a Jellyfin 10.11.8
/// server leaves behind (68 rows). Captured from a real `jellyfin/jellyfin:10.11.8`.
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

/// The exact `__EFMigrationsHistory` migration-id set a Jellyfin 12.0.0 server
/// leaves behind (102 rows: the 68 above plus 12.0's 34). Captured from the
/// fixture in `tests/data/jellyfin-12.0-schema.sql`'s source database after
/// one `jellyfin/jellyfin:12.0` boot.
const JELLYFIN_12_0_MIGRATIONS: [&str; 102] = [
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
    "20260113102337_AddLinkedChildrenTable",
    "20260113120000_MigrateLinkedChildren",
    "20260113203012_ChangeOwnerIdToGuid",
    "20260113230000_CleanupOrphanedExtras",
    "20260113233000_AddForeignKeyToOwnerId",
    "20260113233500_DropExtraIdsColumn",
    "20260115120000_FixIncorrectOwnerIdRelationships",
    "20260116114245_AddLatestItemsDateCreatedIndexes",
    "20260118182305_AddIndicesToImageInfo",
    "20260130232147_AddBaseItemNameIndex",
    "20260206200000_FixLibrarySubtitleDownloadLanguages",
    "20260206224832_IndexOptimizations",
    "20260215201634_ChangePrimaryVersionIdToGuid",
    "20260302090000_MigrateRatingLevels",
    "20260308123920_AddTypeCleanNameIndex",
    "20260504075755_AddPartialIndexForItemCounts",
    "20260508120000_MergeDuplicateMusicArtists",
    "20260508130000_MergeDuplicatePeople",
    "20260522092303_AddNormalizedUsername",
    "20260522092304_UpdateNormalizedUsername",
    "20260524120336_AddUniqueNormalizedUsernameIndex",
    "20260525010000_CleanupOrphanedExternalData",
    "20260531160000_DisableLegacyAuthorization",
    "20260610120000_RefreshCleanNamesAndValues",
    "20260722120000_RefreshForcedSortNames",
    "20260723111547_AllowDuplicatePlaylistChildren",
    "20260724185102_AddPrimaryVersionIdIndex",
    "20260728170000_AddPeopleNameLowerIndex",
    "20260728182152_AddPeopleItemMapCoveringIndex",
    "20260729120000_RestorePlaylistChildrenFromMetadata",
    "20260812050902_AddMediaStreamFilterIndex",
    "20260815063607_RemoveOrphanedUserPermissionsAndPreferences",
    "20260821120000_RecomputeSeriesPresentationKey",
    "20260825200000_ConsolidateLocalizedUserViews",
    "20260831100000_EnableLocalSimilarityProviders",
];

/// The three ids Jellyfin 10.11.10 and 10.11.11 add on top of
/// [`JELLYFIN_10_11_8_MIGRATIONS`] (10.11.9 adds none): the `NormalizedUsername`
/// column, its backfill and its unique index — exactly what Ferrofin's `0030`
/// creates, so that generation baselines `0030` as well.
const JELLYFIN_10_11_X_ADDITIVE_MIGRATIONS: [&str; 3] = [
    "20260522092303_AddNormalizedUsername",
    "20260522092304_UpdateNormalizedUsername",
    "20260524120336_AddUniqueNormalizedUsernameIndex",
];

/// The three code-migration ids Jellyfin 12.1 adds on top of
/// [`JELLYFIN_12_0_MIGRATIONS`] (no EF schema migration: the 12.1 schema is
/// 12.0's byte for byte). Captured from `jellyfin/jellyfin:12.1` booted on
/// the 10.11.8 fixture (104 ids) and on the 12.0 fixture (105 ids).
const JELLYFIN_12_1_ADDITIVE_MIGRATIONS: [&str; 3] = [
    "20260908120000_RepairAlternateVersionLinks",
    "20260910120000_MigrateRatingLevels",
    "20260911120000_StripEmbeddedLinkedChildren",
];

/// The 10.11-era code routines 12.0 retired, which a 12.x database may or may not
/// carry. Jellyfin records a routine's id only while the routine exists in its code
/// base, and never deletes a history row: a database upgraded from 10.11 keeps both
/// ids, while a fresh 12.0, 12.1 or 12.2 install never records them (captured from
/// the portable 12.0/12.1/12.2 tarballs booted on empty directories: 100, 102 and
/// 105 ids, each its generation's set without these two). 12.0 removed
/// `RemoveDuplicatePlaylistChildren` and re-dated `MigrateRatingLevels`.
const JELLYFIN_12_0_OPTIONAL_MIGRATIONS: [&str; 2] = [
    "20250420190000_RemoveDuplicatePlaylistChildren",
    "20250420220000_MigrateRatingLevels",
];

/// The ids a 12.1 database may or may not carry: the retired routines of
/// [`JELLYFIN_12_0_OPTIONAL_MIGRATIONS`], and 12.0's `MigrateRatingLevels`,
/// which 12.1 re-dated — a database upgraded 12.0 → 12.1 carries it, one that
/// went 10.11 → 12.1 or was installed fresh on 12.1 never sees it.
const JELLYFIN_12_1_OPTIONAL_MIGRATIONS: [&str; 3] = [
    "20250420190000_RemoveDuplicatePlaylistChildren",
    "20250420220000_MigrateRatingLevels",
    "20260302090000_MigrateRatingLevels",
];

/// The ids Jellyfin 12.2 records on top of [`JELLYFIN_12_0_MIGRATIONS`]: 12.1's two
/// surviving routines plus 12.2's four (`HarmonizeConflictingUserData`,
/// `FixNullEncoderPreset`, the re-dated `MigrateRatingLevels`, and the
/// `MakeOwnerIdIndexesPartial` schema migration that `0040` ports). Derived from the
/// v12.2 C# and confirmed by a fresh `jellyfin_12.2` install (2026-10-12), which
/// records these six on top of 12.0's set, less 12.0's re-dated `MigrateRatingLevels`
/// (optional here); the 107/108/109 counts on the
/// 10.11, 12.0 and 12.1 upgrade routes are confirmed by the adoption fixtures.
const JELLYFIN_12_2_ADDITIVE_MIGRATIONS: [&str; 6] = [
    "20260908120000_RepairAlternateVersionLinks",
    "20260911120000_StripEmbeddedLinkedChildren",
    "20260912120000_HarmonizeConflictingUserData",
    "20260915104305_FixNullEncoderPreset",
    "20260915120000_MigrateRatingLevels",
    "20261002210711_MakeOwnerIdIndexesPartial",
];

/// The ids a 12.2 database may or may not carry: the retired routines of
/// [`JELLYFIN_12_0_OPTIONAL_MIGRATIONS`], and the older `MigrateRatingLevels` ids —
/// 12.2 re-dated the routine again, so an install that passed through 12.0 keeps
/// `20260302…` and one that passed through 12.1 keeps `20260910…`; one upgraded
/// straight from 10.11, or installed fresh on 12.2, has neither.
const JELLYFIN_12_2_OPTIONAL_MIGRATIONS: [&str; 4] = [
    "20250420190000_RemoveDuplicatePlaylistChildren",
    "20250420220000_MigrateRatingLevels",
    "20260302090000_MigrateRatingLevels",
    "20260910120000_MigrateRatingLevels",
];

/// A Jellyfin schema generation Ferrofin adopts in place.
///
/// Adoption is gated on the **exact** `__EFMigrationsHistory` id set a real
/// server of that release leaves behind, once its `optional_ids` (re-dated or
/// retired routines a given install may or may not have recorded) are set
/// aside — anything newer, older or partial is
/// refused rather than half-adopted. `baselined_versions` are the Ferrofin
/// migrations whose *shape* the database already has, recorded as applied
/// without executing; every other migration runs.
#[derive(Debug, Clone, Copy)]
pub struct JellyfinGeneration {
    /// The release name, as shown in logs and errors (`"10.11.8"`, `"10.11.11"`,
    /// `"12.0.0"`, `"12.1.0"`, `"12.2.0"`).
    pub name: &'static str,
    /// The EF migration-id set of that release (with [`Self::additional_ids`]).
    pub migration_ids: &'static [&'static str],
    /// Ids a point release added on top of `migration_ids` (10.11.10/11 over
    /// 10.11.8, 12.1 and 12.2 over 12.0); empty for a release captured whole.
    pub additional_ids: &'static [&'static str],
    /// Ids that may or may not be present. Jellyfin records a code routine's id
    /// only while that routine is in its code base and never deletes a history
    /// row, so an install upgraded through an older release keeps ids a fresh
    /// install of this one never gets: routines this release (or an earlier one
    /// in its line) retired, and the old ids of routines it re-dated.
    pub optional_ids: &'static [&'static str],
    /// Ferrofin migrations baselined (recorded, never run) for that generation.
    pub baselined_versions: &'static [i64],
}

/// The generations Ferrofin adopts. `0001`–`0007` are the 10.11.8 shape every
/// generation already has; `0030` (the `NormalizedUsername` column and index)
/// is baselined wherever the database already owns it, `0032` (the 12.0
/// shape) for 12.0, 12.1 and 12.2, and `0040` (12.2's partial `OwnerId`
/// indexes) for 12.2 only. `0031`, `0033`, `0034`, `0039`, `0041`, `0042` and
/// `0043` are never baselined: they are data-only or Ferrofin-side convergence and must run
/// on every path, as must the Ferrofin-own `0035`–`0038`. A 12.x generation
/// matches both an upgraded install and a fresh one (see
/// [`JellyfinGeneration::optional_ids`]); `0041` gives a fresh install the one
/// index it lacks.
pub const JELLYFIN_GENERATIONS: [JellyfinGeneration; 5] = [
    JellyfinGeneration {
        name: "10.11.8",
        migration_ids: &JELLYFIN_10_11_8_MIGRATIONS,
        additional_ids: &[],
        optional_ids: &[],
        baselined_versions: &[1, 2, 3, 4, 5, 6, 7],
    },
    JellyfinGeneration {
        name: "10.11.11",
        migration_ids: &JELLYFIN_10_11_8_MIGRATIONS,
        additional_ids: &JELLYFIN_10_11_X_ADDITIVE_MIGRATIONS,
        optional_ids: &[],
        baselined_versions: &[1, 2, 3, 4, 5, 6, 7, 30],
    },
    JellyfinGeneration {
        name: "12.0.0",
        migration_ids: &JELLYFIN_12_0_MIGRATIONS,
        additional_ids: &[],
        optional_ids: &JELLYFIN_12_0_OPTIONAL_MIGRATIONS,
        baselined_versions: &[1, 2, 3, 4, 5, 6, 7, 30, 32],
    },
    JellyfinGeneration {
        name: "12.1.0",
        migration_ids: &JELLYFIN_12_0_MIGRATIONS,
        additional_ids: &JELLYFIN_12_1_ADDITIVE_MIGRATIONS,
        optional_ids: &JELLYFIN_12_1_OPTIONAL_MIGRATIONS,
        baselined_versions: &[1, 2, 3, 4, 5, 6, 7, 30, 32],
    },
    JellyfinGeneration {
        name: "12.2.0",
        migration_ids: &JELLYFIN_12_0_MIGRATIONS,
        additional_ids: &JELLYFIN_12_2_ADDITIVE_MIGRATIONS,
        optional_ids: &JELLYFIN_12_2_OPTIONAL_MIGRATIONS,
        baselined_versions: &[1, 2, 3, 4, 5, 6, 7, 30, 32, 40],
    },
];

impl JellyfinGeneration {
    /// The sorted id set a database of this release must carry — everything
    /// but the optional ids.
    fn ids(&self) -> Vec<&'static str> {
        let mut ids: Vec<&str> = self
            .migration_ids
            .iter()
            .chain(self.additional_ids)
            .filter(|id| !self.optional_ids.contains(id))
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Whether `applied` (sorted) is this release: exactly [`Self::ids`] once
    /// the optional ids are set aside.
    fn matches(&self, applied: &[String]) -> bool {
        applied
            .iter()
            .map(String::as_str)
            .filter(|a| !self.optional_ids.contains(a))
            .eq(self.ids())
    }
}

/// The generation whose id set equals `applied` (sorted), if any.
fn match_generation(applied: &[String]) -> Option<&'static JellyfinGeneration> {
    JELLYFIN_GENERATIONS.iter().find(|g| g.matches(applied))
}

/// The refusal for an EF history that matches no generation, reported against
/// the closest one so the message says what is actually off (one id short of
/// 12.0 reads as "12.0 minus X", not as "34 unknown ids on top of 10.11.8").
fn unsupported_generation(applied: &[String]) -> crate::DbError {
    let (closest, missing, extra) = JELLYFIN_GENERATIONS
        .iter()
        .map(|g| {
            let ids = g.ids();
            let missing: Vec<&str> = ids
                .iter()
                .filter(|e| !applied.iter().any(|a| a == *e))
                .copied()
                .collect();
            let extra: Vec<&str> = applied
                .iter()
                .filter(|a| !ids.contains(&a.as_str()) && !g.optional_ids.contains(&a.as_str()))
                .map(String::as_str)
                .collect();
            (g, missing, extra)
        })
        .min_by_key(|(_, m, e)| m.len() + e.len())
        .expect("at least one generation");
    let accepted: Vec<&str> = JELLYFIN_GENERATIONS.iter().map(|g| g.name).collect();
    crate::DbError::UnsupportedJellyfinDatabase {
        reason: format!(
            "its migration history matches no supported Jellyfin release \
             ({} applied; closest is {} with {} expected — missing: {missing:?}; \
             unknown: {extra:?}). Ferrofin adopts exactly these generations: {accepted:?} — \
             bring the database to one of them first (or restore a backup)",
            applied.len(),
            closest.name,
            closest.ids().len(),
        ),
    }
}

/// Which Jellyfin generation a database was adopted from, and whether the
/// one-shot membership import has run — the code-created `FerrofinAdoption`
/// row written in the same transaction as the baseline. `None` for a
/// Ferrofin-native database or one adopted before this bookkeeping existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionState {
    /// The generation name (`"10.11.8"`, `"10.11.11"`, `"12.0.0"`, `"12.1.0"`,
    /// `"12.2.0"`).
    pub generation: String,
    /// Whether the playlist/collection membership import from `Data` JSON has run.
    pub membership_import_done: bool,
}

/// Adopts an existing Jellyfin SQLite database in place, if `conn` is one —
/// the drop-in path.
///
/// Detection: an `__EFMigrationsHistory` table with no `_sqlx_migrations`
/// alongside it. The applied EF migration-id set must match one
/// [`JellyfinGeneration`] **exactly** — anything newer, older, or partial is
/// refused loudly rather than half-adopted. On adoption the file is copied
/// aside once (`<db>.pre-ferrofin`); then, in **one transaction**, that
/// generation's `baselined_versions` are recorded in `_sqlx_migrations`
/// without executing (the database already has their shape) and a
/// `FerrofinAdoption` row is written — the durable "this database was newly
/// adopted from generation X" fact the one-shot boot repairs key on. The
/// caller's normal `MIGRATOR.run` then applies everything else.
/// `__EFMigrationsHistory`/`__EFMigrationsLock` are never touched.
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

    // Version gate: exactly one accepted generation, or refuse (never half-adopt).
    let applied: Vec<String> =
        sqlx::query_scalar(r#"SELECT "MigrationId" FROM "__EFMigrationsHistory" ORDER BY 1"#)
            .fetch_all(&mut *conn)
            .await?;
    let Some(generation) = match_generation(&applied) else {
        return Err(unsupported_generation(&applied));
    };

    // One-time safety copy. An earlier boot's is kept: it is complete, and it
    // is the file as that boot found it.
    let backup = path.with_extension("db.pre-ferrofin");
    if backup.is_file() {
        log_kept_snapshot(&backup);
    } else {
        copy_database_file(conn, path, &backup).await?;
    }

    // Baseline + adoption record, atomically: a crash between the two would
    // otherwise leave a database that later boots treat as "already adopted"
    // (the history table exists) with no record of where it came from.
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result: Result<()> = async {
        conn.ensure_migrations_table(&MIGRATOR.table_name).await?;
        for migration in MIGRATOR.iter() {
            if !generation.baselined_versions.contains(&migration.version) {
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
        sqlx::query(ADOPTION_TABLE_DDL).execute(&mut *conn).await?;
        sqlx::query(
            r#"INSERT INTO "FerrofinAdoption" ("Generation", "AdoptedAt", "MembershipImportDone")
               VALUES (?1, CURRENT_TIMESTAMP, 0)"#,
        )
        .bind(generation.name)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            return Err(e);
        }
    }
    tracing::info!(
        database = %path.display(),
        backup = %backup.display(),
        generation = generation.name,
        baselined = ?generation.baselined_versions,
        "adopted an existing Jellyfin database in place"
    );
    Ok(())
}

/// The code-created adoption record — like `_sqlx_migrations`, it lives
/// outside the migration chain (the `Ferrofin` prefix keeps it out of the
/// schema-conformance snapshot). One row, written with the baseline.
const ADOPTION_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS "FerrofinAdoption" (
    "Generation" TEXT NOT NULL,
    "AdoptedAt" TEXT NOT NULL,
    "MembershipImportDone" INTEGER NOT NULL DEFAULT 0
)"#;

/// The migrations that rebuild Jellyfin-owned tables (the 12-step SQLite
/// table-rebuild dance): `0007` (10.11.8 shape) and `0032` (12.0 shape), plus
/// `0044`, which deletes the rows of deleted users outright. Each gets a file
/// snapshot before it first applies — see [`backup_before_rebuild`].
const REBUILD_MIGRATIONS: &[i64] = &[7, 32, 44];

/// Snapshots the database file before a table-rebuilding migration first
/// applies.
///
/// A rebuild drops and recreates `BaseItems`/`Users`/… — the riskiest thing a
/// migration does (one such rebuild once cascade-deleted every user's data,
/// `cbc983f`) — so an existing file-backed database is copied aside once
/// (`<db>.pre-00NN`, for the first [`REBUILD_MIGRATIONS`] entry not yet
/// recorded) before it runs. Fresh databases (no `_sqlx_migrations` yet) and
/// databases already past every rebuild are left alone.
///
/// The first attempt wins: an existing snapshot of that name is kept as an
/// earlier boot left it (complete, see [`publish_snapshot`]), because a retry
/// after a boot that committed part of the chain must not replace the file as
/// it was before it. A boot that keeps failing therefore writes one copy, not
/// one per attempt.
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
    // Name the snapshot after the FIRST rebuild that will run: that is the
    // file as it was before any rebuild touched it, which is the one to
    // restore from.
    let mut pending: Option<i64> = None;
    for &version in REBUILD_MIGRATIONS {
        let applied: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM \"_sqlx_migrations\" WHERE version = ?1")
                .bind(version)
                .fetch_optional(&mut *conn)
                .await?;
        if applied.is_none() {
            pending = Some(version);
            break;
        }
    }
    let Some(version) = pending else {
        return Ok(());
    };
    let backup = path.with_extension(format!("db.pre-{version:04}"));
    if backup.is_file() {
        log_kept_snapshot(&backup);
        return Ok(());
    }
    copy_database_file(conn, path, &backup).await?;
    tracing::info!(
        backup = %backup.display(),
        migration = version,
        "database snapshot taken before a table-rebuilding or row-deleting migration"
    );
    Ok(())
}

/// Copies the database file to `backup`, on the migration connection before
/// the pools open.
///
/// The write-ahead log is folded into the main file first, so a byte-for-byte
/// copy is complete. That checkpoint cannot finish while another process holds
/// a read transaction on the file (Jellyfin still running on it, a database
/// browser, the previous pod of a rolling update): `wal_checkpoint` then
/// reports itself busy instead of failing, and a file copy would miss the
/// commits still in the log. The copy is then a `VACUUM INTO`, which reads one
/// consistent view, log included, and waits for no other process; a warning
/// says why. Refusing the boot instead could leave a rolling update waiting
/// forever on a pod that never becomes ready (owner decision, 2026-10-25).
async fn copy_database_file(
    conn: &mut sqlx::SqliteConnection,
    path: &std::path::Path,
    backup: &std::path::Path,
) -> Result<()> {
    let partial = start_snapshot(backup)?;
    let written = async {
        // (busy, frames in the log, frames checkpointed); both counts are -1
        // when the database is not in WAL mode.
        let (busy, log, checkpointed): (i64, i64, i64) =
            sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&mut *conn)
                .await?;
        if busy == 0 && log == checkpointed {
            std::fs::copy(path, &partial).map_err(|source| crate::DbError::Backup {
                path: backup.display().to_string(),
                source,
            })?;
        } else {
            tracing::warn!(
                database = %path.display(),
                backup = %backup.display(),
                log_frames = log,
                checkpointed_frames = checkpointed,
                "another process has the database open, so its write-ahead log could not be folded in: copying it with VACUUM INTO instead"
            );
            sqlx::query(sqlx::AssertSqlSafe(vacuum_into(&partial)))
                .execute(&mut *conn)
                .await?;
        }
        publish_snapshot(&partial, backup)
    }
    .await;
    abandon_on_error(&partial, written)
}

/// The temporary name a snapshot is written under until it is complete
/// (`backup` with `.partial` appended), after removing whatever an earlier,
/// interrupted attempt left there.
fn start_snapshot(backup: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut partial = backup.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    // Removed, or nothing there: either way the name is free.
    match std::fs::remove_file(&partial) {
        Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
            Err(crate::DbError::Backup {
                path: partial.display().to_string(),
                source,
            })
        }
        _ => Ok(partial),
    }
}

/// Gives a complete snapshot its name: flushes `partial` to disk (SQLite does
/// not sync a `VACUUM INTO` file, and a plain copy sits in the page cache),
/// renames it to `backup` and flushes the directory, so the name survives a
/// crash. Until then only `partial` exists, so a snapshot that a full disk or a
/// kill cut short is never taken for a complete one: the next attempt removes
/// it and starts over.
fn publish_snapshot(partial: &std::path::Path, backup: &std::path::Path) -> Result<()> {
    let failed = |source| crate::DbError::Backup {
        path: backup.display().to_string(),
        source,
    };
    // A handle opened for writing: Windows will not flush a read-only one.
    std::fs::OpenOptions::new()
        .write(true)
        .open(partial)
        .and_then(|file| file.sync_all())
        .map_err(failed)?;
    std::fs::rename(partial, backup).map_err(failed)?;
    sync_parent_dir(backup).map_err(failed)
}

/// Passes `written` through, removing the half-written `partial` first when
/// it failed: a copy cut short by a full disk must not keep holding the space
/// the migrations after it need.
fn abandon_on_error(partial: &std::path::Path, written: Result<()>) -> Result<()> {
    if written.is_err()
        && let Err(error) = std::fs::remove_file(partial)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            path = %partial.display(),
            error = %error,
            "could not remove a half-written database snapshot"
        );
    }
    written
}

/// Flushes the directory holding `path`, so a rename into it survives a crash.
#[cfg(unix)]
fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::File::open(dir)?.sync_all()
}

/// Only Unix can open a directory to flush it; elsewhere the rename is left to
/// the filesystem.
#[cfg(not(unix))]
fn sync_parent_dir(_: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Says that a snapshot an earlier boot took was kept, so the log never passes
/// it off as a copy of the database as it is now. It is trusted by its name,
/// even if it is another database's that was moved into this folder (owner
/// decision, 2026-10-25).
fn log_kept_snapshot(backup: &std::path::Path) {
    tracing::info!(
        backup = %backup.display(),
        "kept the database snapshot an earlier boot took; it is the file as that boot found it, not a copy taken now"
    );
}

/// The `VACUUM INTO` statement that writes a copy of the database to `dest`,
/// its quotes doubled for the SQL string literal.
fn vacuum_into(dest: &std::path::Path) -> String {
    let dest = dest.to_string_lossy().replace('\'', "''");
    format!("VACUUM INTO '{dest}'")
}

/// What a database file records about the migrations applied to it, as read
/// by [`inspect_migration_history`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationHistory {
    /// The file does not exist.
    Missing,
    /// The file has neither Ferrofin's `_sqlx_migrations` nor Jellyfin's
    /// `__EFMigrationsHistory` table — an empty (0-byte) file included.
    NoHistory,
    /// A history table exists but records no migration.
    Empty,
    /// At least one applied migration is recorded.
    Recorded,
}

/// Reads the migration history of the database file at `path` without
/// creating, adopting or migrating it.
///
/// The existence check comes first because opening a missing SQLite database
/// creates an empty file. A file that exists is opened without creating or
/// migrating it and without changing its journal mode, and only queried — but
/// SQLite may still checkpoint a WAL database's `-wal` file into it on close.
/// Either history table counts: `_sqlx_migrations` for a database Ferrofin
/// has opened, `__EFMigrationsHistory` for a Jellyfin database not yet adopted.
///
/// # Errors
///
/// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if the existence check
/// fails or the file cannot be opened or read as a SQLite database.
pub async fn inspect_migration_history(path: &std::path::Path) -> Result<MigrationHistory> {
    use sqlx::ConnectOptions as _;
    if !path.try_exists().map_err(sqlx::Error::Io)? {
        return Ok(MigrationHistory::Missing);
    }
    // On a set-up server this is the first SQLite open of the process.
    init_sqlite();
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .connect()
        .await?;
    let history = read_migration_history(&mut conn).await;
    sqlx::Connection::close(conn).await?;
    history
}

/// [`inspect_migration_history`] over an open connection.
async fn read_migration_history(conn: &mut sqlx::SqliteConnection) -> Result<MigrationHistory> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name IN ('_sqlx_migrations', '__EFMigrationsHistory')",
    )
    .fetch_all(&mut *conn)
    .await?;
    if tables.is_empty() {
        return Ok(MigrationHistory::NoHistory);
    }
    for table in &tables {
        let count_query = if table == "_sqlx_migrations" {
            r#"SELECT COUNT(*) FROM "_sqlx_migrations""#
        } else {
            r#"SELECT COUNT(*) FROM "__EFMigrationsHistory""#
        };
        let applied: i64 = sqlx::query_scalar(count_query)
            .fetch_one(&mut *conn)
            .await?;
        if applied > 0 {
            return Ok(MigrationHistory::Recorded);
        }
    }
    Ok(MigrationHistory::Empty)
}

/// How many migrations `_sqlx_migrations` records as applied (`0` when the
/// table does not exist yet — a fresh or Jellyfin-native database).
#[cfg(test)]
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
/// mistake there leaves dangling child rows instead of erroring. It runs on
/// **every** file-backed boot, not only on one that applied a migration: a
/// boot that failed the check must be followed by one that fails it again,
/// and a marker written after the migration committed would have its own
/// crash window. The scan is **O(rows × foreign keys)** — measured at 0.14 s
/// on a real 42k-item, 250 MB library — noise against a cold start.
async fn foreign_key_check(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    // Rows of `foreign_key_check` are (table, rowid, parent, fkid); count them per table.
    let rows: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT "table", "parent" FROM pragma_foreign_key_check"#)
            .fetch_all(&mut *conn)
            .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut per_table = std::collections::BTreeMap::<String, usize>::new();
    for (table, parent) in &rows {
        *per_table.entry(format!("{table} → {parent}")).or_default() += 1;
    }
    let tables = per_table
        .iter()
        .map(|(table, n)| format!("{table}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(crate::DbError::MigrationIntegrity {
        violations: rows.len(),
        tables,
    })
}

/// Logs what migration `0044` deleted: the rows of users that no longer exist
/// and `AncestorIds` rows naming missing items, counted per table in the
/// temporary table the migration leaves on this connection (present only on
/// the boot that applied it). Drops that table.
async fn log_dangling_row_cleanup(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM temp.sqlite_master WHERE name = 'FerrofinDanglingRowCleanup'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    if present.is_none() {
        return Ok(());
    }
    let counts: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT "Table", "Rows" FROM temp."FerrofinDanglingRowCleanup" WHERE "Rows" > 0"#,
    )
    .fetch_all(&mut *conn)
    .await?;
    for (table, rows) in counts {
        tracing::info!(
            table,
            rows,
            "removed rows of a deleted user or item left behind by an earlier server"
        );
    }
    sqlx::query(r#"DROP TABLE temp."FerrofinDanglingRowCleanup""#)
        .execute(&mut *conn)
        .await?;
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
    usable_cores()
}

/// The number of CPUs this process may actually use — thread affinity capped by
/// the cgroup CFS quota, never below 1, defaulting to 4 when neither is legible.
///
/// This is Ferrofin's `Environment.ProcessorCount`: .NET reports the *container's*
/// core budget, not the host's, so anything ported from a C# expression over
/// `ProcessorCount` (the pool size here, `ServerConfiguration.CacheSize`) must
/// read the quota too — `std::thread::available_parallelism()` alone answers with
/// the host core count inside a `cpus: 4` container and silently diverges.
#[must_use]
pub fn usable_cores() -> u32 {
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
/// SQLite has initialized, which is why this is a process-wide one-shot on the
/// connect path rather than per-pool setup. Every path in this crate that opens
/// SQLite calls it first, and the server calls it before anything else at boot;
/// any new raw open (a `SqliteConnectOptions` connect) must call it first too.
/// A failure is not fatal: SQLite was already initialized and keeps its default
/// allocator bookkeeping, which is logged as a warning because it brings the
/// capacity cliff back.
///
/// Returns whether `MEMSTATUS` was turned off (the answer of the first call).
pub fn init_sqlite() -> bool {
    static MEMSTATUS_OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MEMSTATUS_OFF.get_or_init(|| {
        // SAFETY: `sqlite3_config` is only unsafe because it must not race with
        // an initialized library or a live connection. The `OnceLock` serializes
        // callers of this function; ordering against other SQLite use rests on
        // every opener calling it first, and `serve_once` calling it before any
        // other boot work.
        let rc = unsafe {
            libsqlite3_sys::sqlite3_config(libsqlite3_sys::SQLITE_CONFIG_MEMSTATUS, 0_i32)
        };
        if rc != libsqlite3_sys::SQLITE_OK {
            tracing::warn!(
                rc,
                "sqlite3_config(MEMSTATUS, 0) declined — SQLite was already \
                 initialized; keeping default allocator bookkeeping"
            );
        }
        // Strictly after the `sqlite3_config` above: registering an
        // auto-extension initializes SQLite, and `sqlite3_config` is refused
        // once that has happened.
        crate::sqlite_random::register_random_function();
        crate::sqlite_casing::register();
        rc == libsqlite3_sys::SQLITE_OK
    })
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
    /// A boot repair's snapshot is written once, beside the file, and never
    /// over an earlier one; an in-memory database has none.
    #[tokio::test]
    async fn a_repair_snapshot_is_taken_once_beside_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let db = Database::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrate");
        let written = db.snapshot_once("db.pre-rehome").await.expect("snapshot");
        assert_eq!(written, Some(dir.path().join("jellyfin.db.pre-rehome")));
        assert!(dir.path().join("jellyfin.db.pre-rehome").is_file());
        assert_eq!(
            db.snapshot_once("db.pre-rehome").await.expect("again"),
            None
        );
        let memory = Database::connect_in_memory().await.expect("memory");
        assert_eq!(
            memory.snapshot_once("db.pre-rehome").await.expect("memory"),
            None
        );
    }

    /// An existing repair snapshot is kept as an earlier boot left it, and
    /// something other than a file on its name fails the repair instead of
    /// letting it run with no snapshot.
    #[tokio::test]
    async fn a_repair_snapshot_keeps_an_earlier_one_and_refuses_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let db = Database::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrate");
        let earlier = dir.path().join("jellyfin.db.pre-rehome");
        std::fs::write(&earlier, b"an earlier boot's snapshot").expect("earlier");
        assert_eq!(db.snapshot_once("db.pre-rehome").await.expect("kept"), None);
        assert!(
            std::fs::read(&earlier).expect("read") == b"an earlier boot's snapshot",
            "an earlier boot's snapshot is left as it was"
        );

        std::fs::create_dir(dir.path().join("jellyfin.db.pre-harmonize")).expect("squat");
        let err = db
            .snapshot_once("db.pre-harmonize")
            .await
            .expect_err("a directory is no snapshot");
        assert!(
            matches!(&err, crate::DbError::Backup { path, .. } if path.ends_with("jellyfin.db.pre-harmonize")),
            "unexpected error: {err}"
        );
        assert!(
            !dir.path()
                .join("jellyfin.db.pre-harmonize.partial")
                .exists()
        );
    }

    /// A kill during a repair snapshot leaves a truncated file, but only under
    /// its temporary name: the final name is given to complete snapshots only,
    /// so the next attempt is not fooled, removes the leftover and starts over.
    #[tokio::test]
    async fn an_interrupted_repair_snapshot_is_redone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let db = Database::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrate");
        let partial = dir.path().join("jellyfin.db.pre-rehome.partial");
        std::fs::write(&partial, b"SQLite format 3\0cut short").expect("leftover");

        let written = db.snapshot_once("db.pre-rehome").await.expect("snapshot");
        assert_eq!(written, Some(dir.path().join("jellyfin.db.pre-rehome")));
        assert!(!partial.exists(), "the interrupted attempt is removed");
        assert_eq!(
            newest_migration(&dir.path().join("jellyfin.db.pre-rehome")).await,
            MIGRATOR.iter().last().map(|m| m.version),
            "the snapshot is a complete copy of the database"
        );
    }

    /// The newest migration the database file at `path` records, read-only:
    /// a file that opens and answers this is a complete database.
    async fn newest_migration(path: &std::path::Path) -> Option<i64> {
        use sqlx::ConnectOptions as _;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .connect()
            .await
            .expect("open the snapshot");
        let ok: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&mut conn)
            .await
            .expect("integrity_check");
        assert_eq!(ok, "ok", "{} is not intact", path.display());
        let newest = sqlx::query_scalar(r#"SELECT MAX(version) FROM "_sqlx_migrations""#)
            .fetch_one(&mut conn)
            .await
            .expect("newest migration");
        sqlx::Connection::close(conn).await.expect("close");
        newest
    }

    /// A copy taken while another process holds a read transaction misses no
    /// commit. The checkpoint cannot fold the write-ahead log into the file
    /// then (it reports itself busy rather than failing), so a file copy would
    /// lack the commit made after that reader began; the copy is a
    /// `VACUUM INTO` instead.
    #[tokio::test]
    async fn a_copy_taken_while_another_process_reads_has_every_commit() {
        use sqlx::ConnectOptions as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        // No busy timeout: the checkpoint gives up at once instead of after 30 s.
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::ZERO);
        let mut conn = options.clone().connect().await.expect("connect");
        sqlx::raw_sql("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
            .execute(&mut conn)
            .await
            .expect("seed");
        let mut reader = options.connect().await.expect("reader");
        sqlx::raw_sql("BEGIN; SELECT COUNT(*) FROM t;")
            .execute(&mut reader)
            .await
            .expect("read transaction");
        sqlx::query("INSERT INTO t VALUES (2)")
            .execute(&mut conn)
            .await
            .expect("a commit the reader's view predates");
        let (_, log, checkpointed): (i64, i64, i64) =
            sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)")
                .fetch_one(&mut conn)
                .await
                .expect("checkpoint");
        assert!(log > checkpointed, "the reader keeps a commit in the WAL");

        let backup = dir.path().join("jellyfin.db.pre-0007");
        copy_database_file(&mut conn, &path, &backup)
            .await
            .expect("copy");
        sqlx::raw_sql("COMMIT;")
            .execute(&mut reader)
            .await
            .expect("end the read");

        let mut copy = SqliteConnectOptions::new()
            .filename(&backup)
            .read_only(true)
            .connect()
            .await
            .expect("open the copy");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&mut copy)
            .await
            .expect("count");
        assert_eq!(rows, 2, "the copy must hold the commit still in the WAL");
        assert!(!dir.path().join("jellyfin.db.pre-0007.partial").exists());
    }

    /// Runs `sql` against a database file created at `path`.
    async fn create_with(path: &std::path::Path, sql: &'static str) {
        use sqlx::ConnectOptions as _;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("create");
        sqlx::raw_sql(sql).execute(&mut conn).await.expect("sql");
        sqlx::Connection::close(conn).await.expect("close");
    }

    #[tokio::test]
    async fn a_missing_database_is_reported_and_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        assert_eq!(
            super::inspect_migration_history(&path)
                .await
                .expect("inspect"),
            super::MigrationHistory::Missing
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("list").count(),
            0,
            "inspecting must not create the database"
        );
    }

    #[tokio::test]
    async fn an_empty_file_has_no_migration_history_and_stays_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        std::fs::write(&path, []).expect("empty file");
        assert_eq!(
            super::inspect_migration_history(&path)
                .await
                .expect("inspect"),
            super::MigrationHistory::NoHistory
        );
        assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
    }

    #[rstest::rstest]
    #[case::unrelated_tables("CREATE TABLE Users (Id TEXT);", super::MigrationHistory::NoHistory)]
    #[case::empty_ef_history(
        r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT);"#,
        super::MigrationHistory::Empty
    )]
    #[case::empty_sqlx_history(
        "CREATE TABLE _sqlx_migrations (version BIGINT);",
        super::MigrationHistory::Empty
    )]
    #[case::recorded_ef_history(
        r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT);
           INSERT INTO "__EFMigrationsHistory" VALUES ('20250000000000_Initial', '9.0.0');"#,
        super::MigrationHistory::Recorded
    )]
    #[case::empty_sqlx_beside_recorded_ef(
        r#"CREATE TABLE _sqlx_migrations (version BIGINT);
           CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT);
           INSERT INTO "__EFMigrationsHistory" VALUES ('20250000000000_Initial', '9.0.0');"#,
        super::MigrationHistory::Recorded
    )]
    #[tokio::test]
    async fn migration_history_is_read_from_either_table(
        #[case] sql: &'static str,
        #[case] expected: super::MigrationHistory,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        create_with(&path, sql).await;
        assert_eq!(
            super::inspect_migration_history(&path)
                .await
                .expect("inspect"),
            expected
        );
    }

    #[tokio::test]
    async fn a_migrated_database_has_recorded_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let db = Database::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrate");
        db.close().await;
        assert_eq!(
            super::inspect_migration_history(&path)
                .await
                .expect("inspect"),
            super::MigrationHistory::Recorded
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_database_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        std::fs::write(&path, b"this is not a sqlite database file at all").expect("write");
        assert!(super::inspect_migration_history(&path).await.is_err());
    }

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
        conn.ensure_migrations_table(&MIGRATOR.table_name)
            .await
            .expect("migrations table");
        for m in MIGRATOR.iter().filter(|m| m.version <= 6) {
            sqlx::raw_sql(m.sql.clone())
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
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                r#"SELECT COUNT(*) FROM "{table}""#
            )))
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
            let cols: Vec<String> =
                sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info(\"{t}\")")))
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
                let vals: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT \"{c}\" FROM \"{t}\" WHERE \"{c}\" IS NOT NULL"
                )))
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

    /// The pre-0007 snapshot is the only way back from a bad rebuild, so a
    /// snapshot that cannot be written must stop the open BEFORE the rebuild
    /// runs — never "migrate anyway and hope". A directory squatting on the
    /// backup's name makes the copy fail deterministically on every platform.
    #[tokio::test]
    async fn a_failed_pre_rebuild_snapshot_refuses_to_open_and_leaves_the_schema_alone() {
        use sqlx::ConnectOptions as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;
        std::fs::create_dir(path.with_extension("db.pre-0007")).expect("squat on the backup");

        let err = Database::connect_sized(&url, Some(1))
            .await
            .expect_err("an unwritable snapshot must refuse the open");
        assert!(
            matches!(&err, crate::DbError::Backup { path, .. } if path.ends_with("ferrofin.db.pre-0007")),
            "unexpected error: {err}"
        );

        // Nothing past 0006 may have run: the refusal comes before `MIGRATOR.run`.
        let mut conn = SqliteConnectOptions::from_str(&url)
            .expect("options")
            .connect()
            .await
            .expect("raw connect");
        let at: i64 = sqlx::query_scalar(r#"SELECT MAX(version) FROM "_sqlx_migrations""#)
            .fetch_one(&mut conn)
            .await
            .expect("version");
        assert_eq!(at, 6, "the schema must be untouched after a refused open");
        sqlx::Connection::close(conn).await.expect("close");
        assert!(
            !path.with_extension("db.pre-0007.partial").exists(),
            "the half-written copy does not keep holding the disk"
        );
    }

    /// A retried upgrade keeps the snapshot its first attempt took before the
    /// table rebuilds: that attempt may have committed part of the chain, so
    /// the file now is not the file as it was. Only a complete snapshot ever
    /// has the final name, so keeping it is safe.
    #[tokio::test]
    async fn a_retried_upgrade_keeps_the_snapshot_its_first_attempt_took() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;
        let first = path.with_extension("db.pre-0007");
        std::fs::write(&first, b"the first attempt's copy").expect("an earlier snapshot");

        Database::connect_sized(&url, Some(1))
            .await
            .expect("upgrade");
        // `assert!`, not `assert_eq!`: a replaced snapshot is a whole database.
        assert!(
            std::fs::read(&first).expect("read") == b"the first attempt's copy",
            "a retry must not replace the first attempt's snapshot"
        );
    }

    /// The snapshot before a table rebuild is taken through its temporary
    /// name: what an interrupted attempt left there is removed, and the
    /// snapshot is the complete database as it was before the rebuild.
    #[tokio::test]
    async fn the_pre_rebuild_snapshot_is_complete_and_replaces_an_interrupted_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;
        let partial = path.with_extension("db.pre-0007.partial");
        std::fs::write(&partial, b"SQLite format 3\0cut short").expect("leftover");

        Database::connect_sized(&url, Some(1))
            .await
            .expect("upgrade");
        assert!(!partial.exists(), "the interrupted attempt is removed");
        assert_eq!(
            newest_migration(&path.with_extension("db.pre-0007")).await,
            Some(6),
            "the snapshot is the database before 0007"
        );
    }

    /// The seatbelt on the `foreign_keys = OFF` migration connection: a boot
    /// that applies a migration and ends with dangling child rows must refuse
    /// to open rather than serve a database whose integrity is already gone.
    /// The dangling row is planted with enforcement off — the same state a
    /// buggy rebuild would leave behind — and the rebuild carries it through.
    #[tokio::test]
    async fn dangling_child_rows_after_a_migration_refuse_the_open() {
        use sqlx::ConnectOptions as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ferrofin.db");
        let url = seed_pre_0007_database(&path).await;

        let mut conn = SqliteConnectOptions::from_str(&url)
            .expect("options")
            .foreign_keys(false)
            .connect()
            .await
            .expect("connect with enforcement off");
        // A provider id of a missing item: no migration removes it (0044
        // deletes only the rows of missing users and `AncestorIds` of missing
        // items), so it reaches the check.
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId","ProviderId","ProviderValue")
               VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff','Tmdb','1')"#,
        )
        .execute(&mut conn)
        .await
        .expect("plant a provider id whose item does not exist");
        sqlx::Connection::close(conn).await.expect("close");

        let err = Database::connect_sized(&url, Some(1))
            .await
            .expect_err("a foreign-key violation after migrating must refuse the open");
        assert!(
            matches!(
                &err,
                crate::DbError::MigrationIntegrity { violations: 1, tables }
                    if tables == "BaseItemProviders → BaseItems: 1"
            ),
            "unexpected error: {err}"
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

    /// `jellyfin_migration_applied` reads Jellyfin's own history: false with no
    /// `__EFMigrationsHistory` (a Ferrofin-native database) or for an id the
    /// table does not hold, true for one it does.
    #[rstest::rstest]
    #[case::no_history_table(None, false)]
    #[case::another_id(Some("20260910120000_MigrateRatingLevels"), false)]
    #[case::recorded(Some("20260915120000_MigrateRatingLevels"), true)]
    #[tokio::test]
    async fn jellyfin_migration_applied_reads_the_ef_history(
        #[case] recorded: Option<&str>,
        #[case] expected: bool,
    ) {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        if let Some(id) = recorded {
            sqlx::query(
                r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT)"#,
            )
            .execute(db.writer())
            .await
            .expect("history table");
            sqlx::query(r#"INSERT INTO "__EFMigrationsHistory" VALUES (?1, '10.0.0')"#)
                .bind(id)
                .execute(db.writer())
                .await
                .expect("history row");
        }
        assert_eq!(
            db.jellyfin_migration_applied("20260915120000_MigrateRatingLevels")
                .await
                .expect("history read"),
            expected
        );
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

    /// The committed 10.11.8 schema fixture.
    const SCHEMA_10_11_8: &str = include_str!("../tests/data/jellyfin-10.11.8-schema.sql");
    /// The committed 12.0 schema fixture.
    const SCHEMA_12_0: &str = include_str!("../tests/data/jellyfin-12.0-schema.sql");
    /// The committed 12.2 schema fixture (12.0's plus the partial `OwnerId` pair).
    const SCHEMA_12_2: &str = include_str!("../tests/data/jellyfin-12.2-schema.sql");
    /// 12.0's `MigrateRatingLevels` id, which 12.1 and 12.2 re-dated.
    const RATING_LEVELS_12_0: &str = "20260302090000_MigrateRatingLevels";

    /// Creates a file-backed database shaped exactly like a real Jellyfin one:
    /// a committed schema fixture plus the EF migration rows.
    async fn seed_jellyfin_fixture(path: &std::path::Path, migrations: &[&str]) {
        seed_jellyfin_fixture_with(path, SCHEMA_10_11_8, migrations).await;
    }

    async fn seed_jellyfin_fixture_with(path: &std::path::Path, schema: &str, migrations: &[&str]) {
        use sqlx::ConnectOptions;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("open fixture db");
        sqlx::raw_sql(sqlx::AssertSqlSafe(schema))
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

    async fn recorded_versions(db: &Database) -> Vec<i64> {
        sqlx::query_scalar(r#"SELECT version FROM "_sqlx_migrations" ORDER BY version"#)
            .fetch_all(db.pool())
            .await
            .expect("sqlx history")
    }

    async fn table_exists(db: &Database, name: &str) -> bool {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("sqlite_master")
            > 0
    }

    #[test]
    fn the_convergence_migration_is_never_baselined() {
        for generation in &JELLYFIN_GENERATIONS {
            assert!(
                !generation.baselined_versions.contains(&33),
                "{}: 0033 must run on every path",
                generation.name
            );
            assert!(generation.baselined_versions.contains(&7));
        }
        let by_name = |name: &str| {
            JELLYFIN_GENERATIONS
                .iter()
                .find(|g| g.name == name)
                .expect("generation")
        };
        assert!(by_name("12.0.0").baselined_versions.contains(&32));
        assert!(by_name("12.1.0").baselined_versions.contains(&32));
        assert!(by_name("12.2.0").baselined_versions.contains(&32));
        assert!(!by_name("10.11.8").baselined_versions.contains(&32));
        assert!(!by_name("10.11.11").baselined_versions.contains(&32));
        // `0030` is baselined exactly where Jellyfin already owns the column.
        assert!(!by_name("10.11.8").baselined_versions.contains(&30));
        assert!(by_name("10.11.11").baselined_versions.contains(&30));
        assert!(by_name("12.0.0").baselined_versions.contains(&30));
        assert!(by_name("12.2.0").baselined_versions.contains(&30));
        // Retention (`0035`), intro skipper (`0036`–`0038`), the duplicate-index
        // drop (`0039`), the fresh-install convergence (`0041`), the orphan
        // clean-up (`0042`) and the by-name `IsFolder` repair (`0043`) run on
        // every path.
        for generation in &JELLYFIN_GENERATIONS {
            for version in [35, 36, 37, 38, 39, 41, 42, 43] {
                assert!(
                    !generation.baselined_versions.contains(&version),
                    "{}: {version:04} must run on every path",
                    generation.name
                );
            }
            // `0040` (12.2's partial `OwnerId` pair) is the 12.2 shape only.
            assert_eq!(
                generation.baselined_versions.contains(&40),
                generation.name == "12.2.0",
                "{}: 0040 is baselined for 12.2.0 alone",
                generation.name
            );
        }
    }

    /// A retried adoption keeps the `.pre-ferrofin` copy its first attempt
    /// took, and one cut short (only ever under its temporary name) is redone.
    #[tokio::test]
    async fn a_retried_adoption_keeps_its_first_copy_and_redoes_an_interrupted_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &JELLYFIN_10_11_8_MIGRATIONS).await;
        let copy = path.with_extension("db.pre-ferrofin");
        std::fs::write(&copy, b"the first attempt's copy").expect("an earlier copy");
        let url = format!("sqlite://{}", path.display());
        drop(Database::connect_sized(&url, Some(1)).await.expect("adopt"));
        assert!(
            std::fs::read(&copy).expect("read") == b"the first attempt's copy",
            "a retry must not replace the first attempt's copy"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &JELLYFIN_10_11_8_MIGRATIONS).await;
        let partial = path.with_extension("db.pre-ferrofin.partial");
        std::fs::write(&partial, b"SQLite format 3\0cut short").expect("leftover");
        let url = format!("sqlite://{}", path.display());
        drop(Database::connect_sized(&url, Some(1)).await.expect("adopt"));
        assert!(!partial.exists(), "the interrupted attempt is removed");
        let ef_rows: i64 = {
            use sqlx::ConnectOptions as _;
            let mut conn = SqliteConnectOptions::new()
                .filename(path.with_extension("db.pre-ferrofin"))
                .read_only(true)
                .connect()
                .await
                .expect("open the copy");
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "__EFMigrationsHistory""#)
                .fetch_one(&mut conn)
                .await
                .expect("the copy is the unadopted database")
        };
        assert_eq!(ef_rows, 68);
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

        // Shape migrations baselined, everything else actually applied.
        let versions = recorded_versions(&db).await;
        let head = MIGRATOR.iter().last().map_or(0, |m| m.version);
        let recorded = i64::try_from(versions.len()).expect("small count");
        assert_eq!(recorded, head, "every migration recorded");
        assert!(versions.contains(&7));
        assert!(versions.contains(&32) && versions.contains(&33));
        // 0032 EXECUTED here (the 10.11.8 shape had to be converged), so the
        // pre-rebuild snapshot exists; the adoption record names the generation.
        assert!(path.with_extension("db.pre-0032").exists());
        assert_eq!(
            db.adoption_state().await.expect("adoption state"),
            Some(AdoptionState {
                generation: "10.11.8".into(),
                membership_import_done: false
            })
        );
        assert!(!table_exists(&db, "sqlite_stat1").await);

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

    /// Applies the two 10.11.10 schema steps to a seeded 10.11.8 fixture, the
    /// way EF Core does (`AddColumn` with a default, then the unique index).
    async fn add_normalized_username(path: &std::path::Path) {
        use sqlx::ConnectOptions;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .connect()
            .await
            .expect("open fixture db");
        sqlx::raw_sql(
            r#"ALTER TABLE "Users" ADD "NormalizedUsername" TEXT NOT NULL DEFAULT '';
               UPDATE "Users" SET "NormalizedUsername" = 'MÜNCHEN' WHERE "Id" = 'original';
               CREATE UNIQUE INDEX "IX_Users_NormalizedUsername"
                   ON "Users" ("NormalizedUsername");"#,
        )
        .execute(&mut conn)
        .await
        .expect("apply 10.11.10 additions");
        sqlx::Connection::close(conn).await.expect("close");
    }

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn adoption_converges_old_and_new_username_schemas(#[case] newer: bool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let mut ids = JELLYFIN_10_11_8_MIGRATIONS.to_vec();
        if newer {
            ids.extend(JELLYFIN_10_11_X_ADDITIVE_MIGRATIONS);
        }
        seed_jellyfin_fixture(&path, &ids).await;
        {
            use sqlx::ConnectOptions;
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .connect()
                .await
                .unwrap();
            sqlx::query(r#"INSERT INTO "Users"
                ("Id","AuthenticationProviderId","DisplayCollectionsView","DisplayMissingEpisodes",
                 "EnableAutoLogin","EnableLocalPassword","EnableNextEpisodeAutoPlay",
                 "EnableUserPreferenceAccess","HidePlayedInLatest","InternalId",
                 "InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
                 "PasswordResetProviderId","PlayDefaultAudioTrack","RememberAudioSelections",
                 "RememberSubtitleSelections","RowVersion","SubtitleMode","SyncPlayAccess","Username","Password")
                VALUES ('original','auth',0,0,0,0,1,1,1,1,0,0,0,'reset',1,1,1,0,0,0,'münchen','original-hash')"#)
                .execute(&mut conn).await.unwrap();
            sqlx::Connection::close(conn).await.unwrap();
        }
        if newer {
            add_normalized_username(&path).await;
        }

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("adoption succeeds");

        // The normalized key and its unique index survive adoption.
        let column: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM pragma_table_info('Users') WHERE name = 'NormalizedUsername'",
        )
        .fetch_optional(db.pool())
        .await
        .expect("column probe");
        assert!(
            column.is_some(),
            "NormalizedUsername column must be retained"
        );
        let index: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' \
             AND name = 'IX_Users_NormalizedUsername'",
        )
        .fetch_optional(db.pool())
        .await
        .expect("index probe");
        assert!(
            index.is_some(),
            "IX_Users_NormalizedUsername must be retained"
        );

        // Jellyfin's own bookkeeping stays untouched (71 rows), and the
        // pre-adoption copy holds the original.
        let ef_rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "__EFMigrationsHistory""#)
            .fetch_one(db.pool())
            .await
            .expect("ef rows");
        assert_eq!(ef_rows, if newer { 71 } else { 68 });
        let original: (String, String, String) = sqlx::query_as(
            "SELECT Id, Username, Password FROM Users WHERE NormalizedUsername = 'MÜNCHEN'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            original,
            ("original".into(), "münchen".into(), "original-hash".into())
        );
        assert!(path.with_extension("db.pre-ferrofin").exists());

        // New accounts populate the normalized key and do not collide.
        for (id, name) in [("A", "alice"), ("B", "bob")] {
            sqlx::query(
                r#"INSERT INTO "Users" ("Id","AuthenticationProviderId","DisplayCollectionsView",
                   "DisplayMissingEpisodes","EnableAutoLogin","EnableLocalPassword",
                   "EnableNextEpisodeAutoPlay","EnableUserPreferenceAccess","HidePlayedInLatest",
                   "InternalId","InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
                   "PasswordResetProviderId","PlayDefaultAudioTrack","RememberAudioSelections",
                   "RememberSubtitleSelections","RowVersion","SubtitleMode","SyncPlayAccess",
                   "Username","NormalizedUsername")
                   VALUES (?1,'auth',0,0,0,0,1,1,1,
                           (SELECT COALESCE(MAX("InternalId"),0)+1 FROM "Users"),
                           0,0,0,'reset',1,1,1,0,0,0,?2,?3)"#,
            )
            .bind(id)
            .bind(name)
            .bind(ferrofin_util::string_extensions::upper_invariant(name))
            .execute(db.writer())
            .await
            .expect("insert user with NormalizedUsername");
        }

        drop(db);
        Database::connect_sized(&url, Some(2))
            .await
            .expect("re-open after adoption");
    }

    #[rstest::rstest]
    #[case(1)]
    #[case(2)]
    #[tokio::test]
    async fn refuses_partial_normalized_username_history(#[case] additions: usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jellyfin.db");
        let mut ids = JELLYFIN_10_11_8_MIGRATIONS.to_vec();
        ids.extend(&JELLYFIN_10_11_X_ADDITIVE_MIGRATIONS[..additions]);
        seed_jellyfin_fixture(&path, &ids).await;
        let err = Database::connect_sized(&format!("sqlite://{}", path.display()), Some(1))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::DbError::UnsupportedJellyfinDatabase { .. }
        ));
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn upgrading_version_29_preserves_users_and_checks_collisions(#[case] collision: bool) {
        use sqlx::{ConnectOptions, Connection};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ferrofin.db");
        let mut conn = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(false)
            .connect()
            .await
            .unwrap();
        let old = sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|m| m.version <= 29)
                    .cloned()
                    .collect(),
            ),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        old.run(&mut conn).await.unwrap();
        for (id, name) in [
            ("a", "münchen"),
            ("b", if collision { "MÜNCHEN" } else { "ı" }),
        ] {
            sqlx::query(r#"INSERT INTO "Users"
                ("Id","AuthenticationProviderId","DisplayCollectionsView","DisplayMissingEpisodes",
                 "EnableAutoLogin","EnableLocalPassword","EnableNextEpisodeAutoPlay",
                 "EnableUserPreferenceAccess","HidePlayedInLatest","InternalId",
                 "InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
                 "PasswordResetProviderId","PlayDefaultAudioTrack","RememberAudioSelections",
                 "RememberSubtitleSelections","RowVersion","SubtitleMode","SyncPlayAccess","Username","Password")
                VALUES (?1,'auth',0,0,0,0,1,1,1,1,0,0,0,'reset',1,1,1,0,0,0,?2,'original-hash')"#)
                .bind(id).bind(name).execute(&mut conn).await.unwrap();
        }
        sqlx::query(r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value") VALUES (0, 1, 'a', 1)"#)
            .execute(&mut conn).await.unwrap();
        let previous_checksums: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        conn.close().await.unwrap();
        let url = format!("sqlite://{}", path.display());
        let opened = Database::connect_sized(&url, Some(1)).await;
        if collision {
            assert!(matches!(
                opened,
                Err(crate::DbError::UsernameCollision { .. })
            ));
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .connect()
                .await
                .unwrap();
            let applied: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(applied, 29, "collision refused before SQL migrations");
            let column: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM pragma_table_info('Users') WHERE name = 'NormalizedUsername'",
            )
            .fetch_optional(&mut conn)
            .await
            .unwrap();
            assert!(column.is_none());
            return;
        }
        let db = opened.unwrap();
        let checksums: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT version, checksum FROM _sqlx_migrations WHERE version <= 29 ORDER BY version",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(checksums, previous_checksums);
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT Id, NormalizedUsername, Password FROM Users ORDER BY Id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            rows,
            [
                ("a".into(), "MÜNCHEN".into(), "original-hash".into()),
                ("b".into(), "ı".into(), "original-hash".into())
            ]
        );
        let permissions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM Permissions WHERE UserId = 'a' AND Value = 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(permissions, 1);
        let applied: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 30 AND success",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(applied, 1);
        db.close().await;
        let reopened = Database::connect_sized(&url, Some(1)).await.unwrap();
        reopened.run_migrations().await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_jellyfin_database_with_an_unknown_migration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let mut ids = JELLYFIN_10_11_8_MIGRATIONS.to_vec();
        ids.push("20270101000000_SomethingFromTheFuture");
        seed_jellyfin_fixture(&path, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("adoption must refuse an unknown migration");
        assert!(
            matches!(err, crate::DbError::UnsupportedJellyfinDatabase { .. }),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("SomethingFromTheFuture"));
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    /// 12.1 adds three code-migration ids and re-dates `MigrateRatingLevels`:
    /// an install upgraded 12.0 → 12.1 keeps the old id (105 ids), one that
    /// went 10.11 → 12.1 never had it (104). Both are the 12.1 generation; a
    /// 12.0 set with only some of the three is neither.
    #[rstest::rstest]
    #[case(true, 105)]
    #[case(false, 104)]
    #[tokio::test]
    async fn adopts_a_jellyfin_12_1_database_on_either_upgrade_route(
        #[case] upgraded_from_12_0: bool,
        #[case] expected_rows: usize,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let mut ids: Vec<&str> = JELLYFIN_12_0_MIGRATIONS
            .iter()
            .copied()
            .filter(|id| upgraded_from_12_0 || *id != RATING_LEVELS_12_0)
            .chain(JELLYFIN_12_1_ADDITIVE_MIGRATIONS)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids.len(), expected_rows);
        seed_jellyfin_fixture_with(&path, SCHEMA_12_0, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("12.1 adoption succeeds");
        assert_eq!(
            db.adoption_state()
                .await
                .expect("state")
                .map(|s| s.generation),
            Some("12.1.0".to_owned())
        );
        let versions = recorded_versions(&db).await;
        assert!(
            versions.contains(&32),
            "0032 baselined: the shape is 12.0's"
        );
        assert!(
            versions.contains(&30),
            "0030 baselined: NormalizedUsername exists"
        );
        assert!(
            !path.with_extension("db.pre-0032").exists(),
            "no rebuild ran"
        );
    }

    #[tokio::test]
    async fn refuses_a_12_0_set_with_only_part_of_12_1() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        // Two of the three 12.1 ids: closer to 12.1 (one missing) than to
        // 12.0 (two unknown), and refused either way.
        let mut ids: Vec<&str> = JELLYFIN_12_0_MIGRATIONS.to_vec();
        ids.extend(&JELLYFIN_12_1_ADDITIVE_MIGRATIONS[..2]);
        ids.sort_unstable();
        seed_jellyfin_fixture_with(&path, SCHEMA_12_0, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("a partial 12.1 history is refused");
        let text = err.to_string();
        assert!(text.contains("closest is 12.1.0"), "{text}");
        assert!(
            text.contains("20260911120000_StripEmbeddedLinkedChildren"),
            "{text}"
        );
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    /// 12.2 records four new ids on top of 12.1's two surviving routines and
    /// re-dates `MigrateRatingLevels` again, so each upgrade route leaves a
    /// different count: 10.11 → 12.2 carries neither older rating id (107),
    /// 12.0 → 12.2 keeps `20260302…` (108), 12.1-from-10.11 → 12.2 keeps
    /// `20260910…` (108), and 12.1-from-12.0 → 12.2 keeps both (109). All four
    /// are the 12.2 generation, and `0040` is baselined: the database already has
    /// the partial `OwnerId` pair.
    #[rstest::rstest]
    #[case::from_10_11(false, false, 107)]
    #[case::from_12_0(true, false, 108)]
    #[case::from_12_1_of_10_11(false, true, 108)]
    #[case::from_12_1_of_12_0(true, true, 109)]
    #[tokio::test]
    async fn adopts_a_jellyfin_12_2_database_on_every_upgrade_route(
        #[case] passed_through_12_0: bool,
        #[case] passed_through_12_1: bool,
        #[case] expected_rows: usize,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let mut ids: Vec<&str> = JELLYFIN_12_0_MIGRATIONS
            .iter()
            .copied()
            .filter(|id| passed_through_12_0 || *id != RATING_LEVELS_12_0)
            .chain(JELLYFIN_12_2_ADDITIVE_MIGRATIONS)
            .collect();
        if passed_through_12_1 {
            ids.push("20260910120000_MigrateRatingLevels");
        }
        ids.sort_unstable();
        assert_eq!(ids.len(), expected_rows);
        seed_jellyfin_fixture_with(&path, SCHEMA_12_2, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("12.2 adoption succeeds");
        assert_eq!(
            db.adoption_state()
                .await
                .expect("state")
                .map(|s| s.generation),
            Some("12.2.0".to_owned())
        );
        let versions = recorded_versions(&db).await;
        for baselined in [30, 32, 40] {
            assert!(versions.contains(&baselined), "{baselined:04} recorded");
        }
        assert!(
            !path.with_extension("db.pre-0032").exists(),
            "no rebuild ran"
        );
        let owner_index: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'IX_BaseItems_OwnerId'",
        )
        .fetch_one(db.pool())
        .await
        .expect("IX_BaseItems_OwnerId");
        assert!(
            owner_index.ends_with(r#"WHERE "OwnerId" IS NOT NULL"#),
            "{owner_index}"
        );
    }

    #[tokio::test]
    async fn refuses_a_12_1_set_with_only_part_of_12_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        // 12.1 (from 12.0) plus three of 12.2's four new ids: one id short of
        // 12.2, three unknown to 12.1. (Two of four would tie the two
        // generations, and a tie reports the older one.) Refused either way.
        let mut ids: Vec<&str> = JELLYFIN_12_0_MIGRATIONS.to_vec();
        ids.extend(JELLYFIN_12_1_ADDITIVE_MIGRATIONS);
        ids.extend(&JELLYFIN_12_2_ADDITIVE_MIGRATIONS[2..5]);
        ids.sort_unstable();
        seed_jellyfin_fixture_with(&path, SCHEMA_12_0, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("a partial 12.2 history is refused");
        let text = err.to_string();
        assert!(text.contains("closest is 12.2.0"), "{text}");
        assert!(
            text.contains("20261002210711_MakeOwnerIdIndexesPartial"),
            "{text}"
        );
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    /// The `__EFMigrationsHistory` of a fresh Jellyfin 12.0 install (100 ids):
    /// the portable `jellyfin_12.0-amd64` tarball booted once on empty directories
    /// (2026-10-12).
    const FRESH_12_0: [&str; 100] = [
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
        "20250420193000_MigrateLibraryDbCompatibilityCheck",
        "20250420200000_MigrateLibraryDb",
        "20250420210000_MoveExtractedFiles",
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
        "20260113102337_AddLinkedChildrenTable",
        "20260113120000_MigrateLinkedChildren",
        "20260113203012_ChangeOwnerIdToGuid",
        "20260113230000_CleanupOrphanedExtras",
        "20260113233000_AddForeignKeyToOwnerId",
        "20260113233500_DropExtraIdsColumn",
        "20260115120000_FixIncorrectOwnerIdRelationships",
        "20260116114245_AddLatestItemsDateCreatedIndexes",
        "20260118182305_AddIndicesToImageInfo",
        "20260130232147_AddBaseItemNameIndex",
        "20260206200000_FixLibrarySubtitleDownloadLanguages",
        "20260206224832_IndexOptimizations",
        "20260215201634_ChangePrimaryVersionIdToGuid",
        "20260302090000_MigrateRatingLevels",
        "20260308123920_AddTypeCleanNameIndex",
        "20260504075755_AddPartialIndexForItemCounts",
        "20260508120000_MergeDuplicateMusicArtists",
        "20260508130000_MergeDuplicatePeople",
        "20260522092303_AddNormalizedUsername",
        "20260522092304_UpdateNormalizedUsername",
        "20260524120336_AddUniqueNormalizedUsernameIndex",
        "20260525010000_CleanupOrphanedExternalData",
        "20260531160000_DisableLegacyAuthorization",
        "20260610120000_RefreshCleanNamesAndValues",
        "20260722120000_RefreshForcedSortNames",
        "20260723111547_AllowDuplicatePlaylistChildren",
        "20260724185102_AddPrimaryVersionIdIndex",
        "20260728170000_AddPeopleNameLowerIndex",
        "20260728182152_AddPeopleItemMapCoveringIndex",
        "20260729120000_RestorePlaylistChildrenFromMetadata",
        "20260812050902_AddMediaStreamFilterIndex",
        "20260815063607_RemoveOrphanedUserPermissionsAndPreferences",
        "20260821120000_RecomputeSeriesPresentationKey",
        "20260825200000_ConsolidateLocalizedUserViews",
        "20260831100000_EnableLocalSimilarityProviders",
    ];

    /// The `__EFMigrationsHistory` of a fresh Jellyfin 12.1 install (102 ids):
    /// the portable `jellyfin_12.1-amd64` tarball booted once on empty directories
    /// (2026-10-12).
    const FRESH_12_1: [&str; 102] = [
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
        "20250420193000_MigrateLibraryDbCompatibilityCheck",
        "20250420200000_MigrateLibraryDb",
        "20250420210000_MoveExtractedFiles",
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
        "20260113102337_AddLinkedChildrenTable",
        "20260113120000_MigrateLinkedChildren",
        "20260113203012_ChangeOwnerIdToGuid",
        "20260113230000_CleanupOrphanedExtras",
        "20260113233000_AddForeignKeyToOwnerId",
        "20260113233500_DropExtraIdsColumn",
        "20260115120000_FixIncorrectOwnerIdRelationships",
        "20260116114245_AddLatestItemsDateCreatedIndexes",
        "20260118182305_AddIndicesToImageInfo",
        "20260130232147_AddBaseItemNameIndex",
        "20260206200000_FixLibrarySubtitleDownloadLanguages",
        "20260206224832_IndexOptimizations",
        "20260215201634_ChangePrimaryVersionIdToGuid",
        "20260308123920_AddTypeCleanNameIndex",
        "20260504075755_AddPartialIndexForItemCounts",
        "20260508120000_MergeDuplicateMusicArtists",
        "20260508130000_MergeDuplicatePeople",
        "20260522092303_AddNormalizedUsername",
        "20260522092304_UpdateNormalizedUsername",
        "20260524120336_AddUniqueNormalizedUsernameIndex",
        "20260525010000_CleanupOrphanedExternalData",
        "20260531160000_DisableLegacyAuthorization",
        "20260610120000_RefreshCleanNamesAndValues",
        "20260722120000_RefreshForcedSortNames",
        "20260723111547_AllowDuplicatePlaylistChildren",
        "20260724185102_AddPrimaryVersionIdIndex",
        "20260728170000_AddPeopleNameLowerIndex",
        "20260728182152_AddPeopleItemMapCoveringIndex",
        "20260729120000_RestorePlaylistChildrenFromMetadata",
        "20260812050902_AddMediaStreamFilterIndex",
        "20260815063607_RemoveOrphanedUserPermissionsAndPreferences",
        "20260821120000_RecomputeSeriesPresentationKey",
        "20260825200000_ConsolidateLocalizedUserViews",
        "20260831100000_EnableLocalSimilarityProviders",
        "20260908120000_RepairAlternateVersionLinks",
        "20260910120000_MigrateRatingLevels",
        "20260911120000_StripEmbeddedLinkedChildren",
    ];

    /// The `__EFMigrationsHistory` of a fresh Jellyfin 12.2 install (105 ids):
    /// the portable `jellyfin_12.2-amd64` tarball booted once on empty directories
    /// (2026-10-12).
    const FRESH_12_2: [&str; 105] = [
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
        "20250420193000_MigrateLibraryDbCompatibilityCheck",
        "20250420200000_MigrateLibraryDb",
        "20250420210000_MoveExtractedFiles",
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
        "20260113102337_AddLinkedChildrenTable",
        "20260113120000_MigrateLinkedChildren",
        "20260113203012_ChangeOwnerIdToGuid",
        "20260113230000_CleanupOrphanedExtras",
        "20260113233000_AddForeignKeyToOwnerId",
        "20260113233500_DropExtraIdsColumn",
        "20260115120000_FixIncorrectOwnerIdRelationships",
        "20260116114245_AddLatestItemsDateCreatedIndexes",
        "20260118182305_AddIndicesToImageInfo",
        "20260130232147_AddBaseItemNameIndex",
        "20260206200000_FixLibrarySubtitleDownloadLanguages",
        "20260206224832_IndexOptimizations",
        "20260215201634_ChangePrimaryVersionIdToGuid",
        "20260308123920_AddTypeCleanNameIndex",
        "20260504075755_AddPartialIndexForItemCounts",
        "20260508120000_MergeDuplicateMusicArtists",
        "20260508130000_MergeDuplicatePeople",
        "20260522092303_AddNormalizedUsername",
        "20260522092304_UpdateNormalizedUsername",
        "20260524120336_AddUniqueNormalizedUsernameIndex",
        "20260525010000_CleanupOrphanedExternalData",
        "20260531160000_DisableLegacyAuthorization",
        "20260610120000_RefreshCleanNamesAndValues",
        "20260722120000_RefreshForcedSortNames",
        "20260723111547_AllowDuplicatePlaylistChildren",
        "20260724185102_AddPrimaryVersionIdIndex",
        "20260728170000_AddPeopleNameLowerIndex",
        "20260728182152_AddPeopleItemMapCoveringIndex",
        "20260729120000_RestorePlaylistChildrenFromMetadata",
        "20260812050902_AddMediaStreamFilterIndex",
        "20260815063607_RemoveOrphanedUserPermissionsAndPreferences",
        "20260821120000_RecomputeSeriesPresentationKey",
        "20260825200000_ConsolidateLocalizedUserViews",
        "20260831100000_EnableLocalSimilarityProviders",
        "20260908120000_RepairAlternateVersionLinks",
        "20260911120000_StripEmbeddedLinkedChildren",
        "20260912120000_HarmonizeConflictingUserData",
        "20260915104305_FixNullEncoderPreset",
        "20260915120000_MigrateRatingLevels",
        "20261002210711_MakeOwnerIdIndexesPartial",
    ];

    /// The one statement a fresh 12.x install lacks next to the schema pins.
    const CUSTOM_PREFS_USER_INDEX: &str = "CREATE INDEX \"IX_CustomItemDisplayPreferences_UserId\" \
         ON \"CustomItemDisplayPreferences\"(\n  \"UserId\"\n);";

    /// `schema` in the shape a fresh Jellyfin install has: without
    /// `IX_CustomItemDisplayPreferences_UserId`, which
    /// `20210320181425_AddIndexesAndCollations` drops.
    fn fresh_install_schema(schema: &str) -> String {
        assert!(
            schema.contains(CUSTOM_PREFS_USER_INDEX),
            "the pin carries the index"
        );
        schema.replace(CUSTOM_PREFS_USER_INDEX, "")
    }

    /// A fresh 12.x install records only the routines its code base still has, so
    /// it lacks the retired 10.11-era ids every upgraded database carries, and its
    /// schema lacks `IX_CustomItemDisplayPreferences_UserId`. It is still its own
    /// generation, and `0041` gives it the index.
    #[rstest::rstest]
    #[case::fresh_12_0("12.0.0", SCHEMA_12_0, &FRESH_12_0)]
    #[case::fresh_12_1("12.1.0", SCHEMA_12_0, &FRESH_12_1)]
    #[case::fresh_12_2("12.2.0", SCHEMA_12_2, &FRESH_12_2)]
    #[tokio::test]
    async fn adopts_a_fresh_jellyfin_12_x_install(
        #[case] generation: &str,
        #[case] schema: &str,
        #[case] ids: &[&str],
    ) {
        // The captured truth is exactly the generation's required set.
        let expected = JELLYFIN_GENERATIONS
            .iter()
            .find(|g| g.name == generation)
            .expect("generation")
            .ids();
        assert_eq!(
            ids,
            expected.as_slice(),
            "{generation}: fresh set = required set"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture_with(&path, &fresh_install_schema(schema), ids).await;

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("fresh-install adoption succeeds");
        assert_eq!(
            db.adoption_state()
                .await
                .expect("state")
                .map(|s| s.generation),
            Some(generation.to_owned())
        );
        assert!(
            recorded_versions(&db).await.contains(&41),
            "0041 ran (it is never baselined)"
        );
        let index: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'index' \
             AND name = 'IX_CustomItemDisplayPreferences_UserId'",
        )
        .fetch_one(db.pool())
        .await
        .expect("0041 created IX_CustomItemDisplayPreferences_UserId");
        assert_eq!(
            format!("{index};"),
            CUSTOM_PREFS_USER_INDEX,
            "byte-identical to the schema pin"
        );
    }

    /// Every id set a generation accepts — its required set plus any subset of its
    /// optional ids — matches that generation and no other, so a 12.0-fresh set can
    /// never adopt as 12.1, nor a 12.1 one as 12.2.
    #[test]
    fn no_id_set_matches_two_generations() {
        for owner in &JELLYFIN_GENERATIONS {
            let optional: Vec<&str> = owner.optional_ids.to_vec();
            for mask in 0..1_u32 << optional.len() {
                let mut applied: Vec<String> = owner.ids().into_iter().map(str::to_owned).collect();
                applied.extend(
                    optional
                        .iter()
                        .enumerate()
                        .filter(|(bit, _)| mask & (1 << bit) != 0)
                        .map(|(_, id)| (*id).to_owned()),
                );
                applied.sort_unstable();
                for other in &JELLYFIN_GENERATIONS {
                    assert_eq!(
                        other.matches(&applied),
                        other.name == owner.name,
                        "{} set (optional mask {mask:#b}) vs {}",
                        owner.name,
                        other.name
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn adopts_a_jellyfin_12_0_database_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture_with(&path, SCHEMA_12_0, &JELLYFIN_12_0_MIGRATIONS).await;
        // What a Jellyfin 12 database always carries and Ferrofin never keeps.
        {
            use sqlx::ConnectOptions;
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .connect()
                .await
                .expect("open");
            sqlx::query("ANALYZE")
                .execute(&mut conn)
                .await
                .expect("analyze");
            sqlx::Connection::close(conn).await.expect("close");
        }

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("adoption succeeds");

        let versions = recorded_versions(&db).await;
        let head = MIGRATOR.iter().last().map_or(0, |m| m.version);
        assert_eq!(i64::try_from(versions.len()).expect("small"), head);
        assert!(versions.contains(&32), "0032 baselined");
        assert!(versions.contains(&33), "0033 executed");
        // 0032 was baselined, never run: no rebuild happened, no snapshot.
        assert!(!path.with_extension("db.pre-0032").exists());
        assert!(path.with_extension("db.pre-ferrofin").exists());
        assert_eq!(
            db.adoption_state().await.expect("adoption state"),
            Some(AdoptionState {
                generation: "12.0.0".into(),
                membership_import_done: false
            })
        );
        // 0033's hygiene ran on this path too.
        assert!(!table_exists(&db, "sqlite_stat1").await);
        let ef_rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "__EFMigrationsHistory""#)
            .fetch_one(db.pool())
            .await
            .expect("ef rows");
        assert_eq!(ef_rows, 102);
        // The 12.0 fixture and the converged chain must agree on the shape —
        // the conformance test proves it for a fresh database, this proves the
        // adopted one still passes the same schema-level integrity check.
        let violations: Vec<sqlx::sqlite::SqliteRow> = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(db.pool())
            .await
            .expect("fk check");
        assert!(violations.is_empty());

        drop(db);
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("re-open after adoption");
        assert_eq!(recorded_versions(&db).await.len(), versions.len());
    }

    #[tokio::test]
    async fn refuses_a_12_0_database_missing_one_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let short = &JELLYFIN_12_0_MIGRATIONS[..JELLYFIN_12_0_MIGRATIONS.len() - 1];
        seed_jellyfin_fixture_with(&path, SCHEMA_12_0, short).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("adoption must refuse");
        let message = err.to_string();
        assert!(
            matches!(err, crate::DbError::UnsupportedJellyfinDatabase { .. }),
            "unexpected error: {err}"
        );
        assert!(message.contains("closest is 12.0.0"), "{message}");
        assert!(
            message.contains("EnableLocalSimilarityProviders"),
            "{message}"
        );
        assert!(!path.with_extension("db.pre-ferrofin").exists());
    }

    #[tokio::test]
    async fn refuses_a_database_between_generations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        // 10.11.8 plus five of 12.0's ids: a half-upgraded database.
        let mut ids: Vec<&str> = JELLYFIN_10_11_8_MIGRATIONS.to_vec();
        ids.extend(
            JELLYFIN_12_0_MIGRATIONS
                .iter()
                .filter(|id| !JELLYFIN_10_11_8_MIGRATIONS.contains(id))
                .take(5),
        );
        seed_jellyfin_fixture(&path, &ids).await;

        let url = format!("sqlite://{}", path.display());
        let err = Database::connect_sized(&url, Some(2))
            .await
            .expect_err("adoption must refuse");
        assert!(err.to_string().contains("closest is 10.11.8"), "{err}");
        assert!(!table_exists_at(&path, "_sqlx_migrations").await);
        assert!(!table_exists_at(&path, "FerrofinAdoption").await);
    }

    async fn table_exists_at(path: &std::path::Path, name: &str) -> bool {
        use sqlx::ConnectOptions;
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .connect()
            .await
            .expect("open");
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(name)
        .fetch_one(&mut conn)
        .await
        .expect("sqlite_master");
        sqlx::Connection::close(conn).await.expect("close");
        n > 0
    }

    /// A crash after the baseline transaction committed but before
    /// `MIGRATOR.run`: the next boot must skip the gate (history exists), run
    /// the chain, and still know the generation from the adoption record.
    #[tokio::test]
    async fn adoption_survives_a_crash_between_baseline_and_migrations() {
        use sqlx::ConnectOptions;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &JELLYFIN_10_11_8_MIGRATIONS).await;
        // Replay exactly what the adoption transaction writes, then "crash".
        let mut conn = SqliteConnectOptions::new()
            .filename(&path)
            .connect()
            .await
            .expect("open");
        {
            use sqlx::migrate::Migrate;
            conn.ensure_migrations_table(&MIGRATOR.table_name)
                .await
                .expect("history table");
        }
        for migration in MIGRATOR.iter().filter(|m| m.version <= 7) {
            sqlx::query(
                "INSERT INTO _sqlx_migrations \
                 (version, description, installed_on, success, checksum, execution_time) \
                 VALUES (?1, ?2, CURRENT_TIMESTAMP, TRUE, ?3, 0)",
            )
            .bind(migration.version)
            .bind(&*migration.description)
            .bind(&*migration.checksum)
            .execute(&mut conn)
            .await
            .expect("baseline row");
        }
        sqlx::query(ADOPTION_TABLE_DDL)
            .execute(&mut conn)
            .await
            .expect("adoption table");
        sqlx::query(
            r#"INSERT INTO "FerrofinAdoption" ("Generation", "AdoptedAt", "MembershipImportDone")
               VALUES ('10.11.8', CURRENT_TIMESTAMP, 0)"#,
        )
        .execute(&mut conn)
        .await
        .expect("adoption row");
        sqlx::Connection::close(conn).await.expect("close");

        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("boot after the crash completes the chain");
        let head = MIGRATOR.iter().last().map_or(0, |m| m.version);
        assert_eq!(
            i64::try_from(recorded_versions(&db).await.len()).expect("small"),
            head
        );
        // No second adoption happened: no safety copy was taken by the gate.
        assert!(!path.with_extension("db.pre-ferrofin").exists());
        assert_eq!(
            db.adoption_state()
                .await
                .expect("state")
                .map(|s| s.generation),
            Some("10.11.8".into())
        );
    }

    /// An install adopted before the adoption record existed (or a
    /// Ferrofin-native database) has no record: repairs keyed on it never run.
    #[tokio::test]
    async fn a_database_without_an_adoption_record_reports_none() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        assert_eq!(db.adoption_state().await.expect("state"), None);
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

    /// Counts foreign keys whose target table does not exist. 0009 shipped FK
    /// targets under their pre-0007 names, which dangled on every adopted
    /// database (fresh ones were rescued by 0007's rename rewriting the real
    /// tables' FK texts) — the first Live TV channel INSERT then failed with
    /// `no such table: main.LiveTvTunerHosts`. 0029 rebuilds the targets.
    async fn dangling_fk_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar(
            r#"SELECT count(*) FROM sqlite_master m, pragma_foreign_key_list(m.name) f
               WHERE m.type = 'table'
                 AND f."table" NOT IN (SELECT name FROM sqlite_master WHERE type = 'table')"#,
        )
        .fetch_one(pool)
        .await
        .expect("dangling fk audit")
    }

    #[tokio::test]
    async fn adopted_database_accepts_live_tv_writes() {
        // Found live by the parity suite on the pinned real snapshot: guide
        // refresh died on the dangling channel→tuner FK. The write path below
        // runs with foreign_keys=ON (the normal runtime pool), so it fails
        // loudly if any Live TV FK target regresses to a name that is not there.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &JELLYFIN_10_11_8_MIGRATIONS).await;
        let url = format!("sqlite://{}", path.display());
        let db = Database::connect_sized(&url, Some(2))
            .await
            .expect("adoption succeeds");

        assert_eq!(
            dangling_fk_count(db.pool()).await,
            0,
            "no FK may dangle after adoption"
        );

        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id","Url","Type","Data")
               VALUES ('t-fk','http://x','m3u','{}')"#,
        )
        .execute(db.pool())
        .await
        .expect("tuner insert");
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvChannels" ("Id","TunerHostId","Name","StreamUrl")
               VALUES ('c-fk','t-fk','Ch','http://s')"#,
        )
        .execute(db.pool())
        .await
        .expect("channel insert under FK enforcement");
        sqlx::query(
            r#"INSERT INTO "FerrofinLiveTvPrograms" ("Id","ChannelId","StartDate","Title")
               VALUES ('p-fk','c-fk','2026-01-01T00:00:00Z','Prog')"#,
        )
        .execute(db.pool())
        .await
        .expect("program insert under FK enforcement");
        // The cascade works through the CORRECT target: deleting the tuner
        // clears its channel, and the channel its programme.
        sqlx::query(r#"DELETE FROM "FerrofinLiveTvTunerHosts" WHERE "Id" = 't-fk'"#)
            .execute(db.pool())
            .await
            .expect("tuner delete");
        let left: i64 = sqlx::query_scalar(
            r#"SELECT (SELECT count(*) FROM "FerrofinLiveTvChannels" WHERE "Id"='c-fk')
                    + (SELECT count(*) FROM "FerrofinLiveTvPrograms" WHERE "Id"='p-fk')"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("cascade audit");
        assert_eq!(left, 0, "delete must cascade tuner → channel → programme");
    }

    #[tokio::test]
    async fn fresh_database_has_no_dangling_foreign_keys() {
        // connect_in_memory + run_migrations, NOT connect("sqlite::memory:"):
        // the latter skips the migration block for in-memory URLs and would
        // audit an empty schema — a vacuous pass. The table-count assertion
        // keeps this test from ever going quietly vacuous again.
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory connect");
        db.run_migrations().await.expect("migrations apply");
        let tables: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table'")
                .fetch_one(db.pool())
                .await
                .expect("table count");
        assert!(
            tables > 20,
            "audit must run against the migrated schema, not an empty db"
        );
        assert_eq!(dangling_fk_count(db.pool()).await, 0);
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
