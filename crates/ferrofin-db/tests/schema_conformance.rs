//! Schema-equality gates.
//!
//! 1. A fresh Ferrofin database's Jellyfin-owned schema must be IDENTICAL to a
//!    real Jellyfin 12.2 database's.
//! 2. A database adopted from every supported Jellyfin generation must end on
//!    exactly the schema a fresh Ferrofin database has — Ferrofin's own
//!    `FerrofinIX_` indexes included.
//!
//! The 12.2 fixture (`tests/data/jellyfin-12.2-schema.sql`) is the
//! `jellyfin-12.0-schema.sql` dump — the owner's real library after one
//! `jellyfin/jellyfin:12.0` boot (the realistic adoption case; a fresh 12.0,
//! 12.1 or 12.2 install differs only by lacking
//! `IX_CustomItemDisplayPreferences_UserId`, which `0041` adds on adoption) —
//! with the two indexes 12.2's only schema migration
//! (`20261002210711_MakeOwnerIdIndexesPartial`) makes partial replaced by the
//! text a fresh Jellyfin 12.2 install stores. That is the drop-in contract.
//! These tests are the tripwire for future drift: any schema change that breaks
//! byte-parity with 12.2 fails here, and the next Jellyfin generation will be
//! gated on an updated fixture the same way.
//!
//! Comparison rules:
//! - tables: names, columns (declared type, notnull, default, pk position),
//!   and foreign keys (target/from/to/on-delete) must match exactly;
//! - Ferrofin-own objects (`Ferrofin*` tables, `_sqlx_migrations`) are additive
//!   and excluded; Jellyfin's EF bookkeeping exists only on real databases;
//! - indexes: Jellyfin's named index set must exist verbatim — columns,
//!   uniqueness and the partial `WHERE` clause; Ferrofin may add only
//!   `FerrofinIX_`-prefixed indexes (EF-invisible, collision-proof).

use std::collections::{BTreeMap, BTreeSet};

use ferrofin_db::Database;
use ferrofin_db::database::JELLYFIN_GENERATIONS;
use sqlx::{Row, SqlitePool};

/// One table's comparable shape.
#[derive(Debug, PartialEq, Eq)]
struct TableShape {
    /// column name → (declared type uppercased, notnull, default, pk position)
    columns: BTreeMap<String, (String, i64, Option<String>, i64)>,
    /// (target table, from column, to column, on-delete action)
    foreign_keys: BTreeSet<(String, String, Option<String>, String)>,
}

/// (index name, ordered column list, unique flag, normalised partial `WHERE`)
type IndexShape = (String, Vec<String>, i64, Option<String>);

/// The partial-index predicate of `CREATE INDEX` text: whatever follows the
/// last `WHERE` (any case), lowercased with every whitespace character removed
/// so dump formatting never matters; `None` for a full index.
fn partial_where(sql: &str) -> Option<String> {
    let at = sql.to_ascii_uppercase().rfind("WHERE")?;
    Some(
        sql[at + "WHERE".len()..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect(),
    )
}

async fn snapshot(pool: &SqlitePool) -> (BTreeMap<String, TableShape>, BTreeSet<IndexShape>) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("list tables");

    let mut shapes = BTreeMap::new();
    let mut indexes = BTreeSet::new();
    for table in tables {
        if table.starts_with("__") || table == "_sqlx_migrations" || table.starts_with("Ferrofin") {
            continue;
        }
        let mut columns = BTreeMap::new();
        for row in sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA table_info(\"{table}\")"
        )))
        .fetch_all(pool)
        .await
        .expect("table_info")
        {
            columns.insert(
                row.get::<String, _>("name"),
                (
                    row.get::<String, _>("type").to_uppercase(),
                    row.get::<i64, _>("notnull"),
                    row.get::<Option<String>, _>("dflt_value"),
                    row.get::<i64, _>("pk"),
                ),
            );
        }
        let mut foreign_keys = BTreeSet::new();
        for row in sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA foreign_key_list(\"{table}\")"
        )))
        .fetch_all(pool)
        .await
        .expect("foreign_key_list")
        {
            foreign_keys.insert((
                row.get::<String, _>("table"),
                row.get::<String, _>("from"),
                row.get::<Option<String>, _>("to"),
                row.get::<String, _>("on_delete"),
            ));
        }
        // Materialize index_list fully before issuing index_info queries — the
        // original Python differ interleaved them on one cursor and silently
        // truncated the walk (see JELLYFIN_DB_SCHEMA_DIFF.md's correction).
        let index_rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA index_list(\"{table}\")"
        )))
        .fetch_all(pool)
        .await
        .expect("index_list");
        for row in index_rows {
            if row.get::<String, _>("origin") != "c" {
                continue; // auto PK/unique indexes follow from the column defs
            }
            let name: String = row.get("name");
            let unique: i64 = row.get("unique");
            let cols: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
                "PRAGMA index_info(\"{name}\")"
            )))
            .fetch_all(pool)
            .await
            .expect("index_info")
            .into_iter()
            .map(|r| r.get::<Option<String>, _>("name").unwrap_or_default())
            .collect();
            let sql: Option<String> = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(&name)
            .fetch_one(pool)
            .await
            .expect("index sql");
            let partial = sql.as_deref().and_then(partial_where);
            indexes.insert((name, cols, unique, partial));
        }
        shapes.insert(
            table,
            TableShape {
                columns,
                foreign_keys,
            },
        );
    }
    (shapes, indexes)
}

#[tokio::test]
async fn fresh_ferrofin_schema_equals_real_jellyfin_12_2() {
    // The real 12.2 schema, from the committed fixture dump.
    let jellyfin = Database::connect_in_memory().await.expect("jf connect");
    sqlx::raw_sql(include_str!("data/jellyfin-12.2-schema.sql"))
        .execute(jellyfin.pool())
        .await
        .expect("apply fixture schema");

    // No code backfill: the SQL migration chain alone must own the full schema.
    let sql_only = Database::connect_in_memory().await.expect("SQL connect");
    sqlx::migrate!("./migrations")
        .run(sql_only.pool())
        .await
        .expect("SQL chain");

    // A fresh Ferrofin database through the full migration chain.
    let ferrofin = Database::connect_in_memory()
        .await
        .expect("ferrofin connect");
    ferrofin.run_migrations().await.expect("migrate");

    let (jf_tables, jf_indexes) = snapshot(jellyfin.pool()).await;
    let (hm_tables, hm_indexes) = snapshot(ferrofin.pool()).await;
    assert_eq!(
        snapshot(sql_only.pool()).await,
        snapshot(ferrofin.pool()).await,
        "Rust backfill must not alter the schema produced by SQL migrations"
    );

    let jf_names: BTreeSet<_> = jf_tables.keys().collect();
    let hm_names: BTreeSet<_> = hm_tables.keys().collect();
    assert_eq!(
        jf_names, hm_names,
        "Jellyfin-owned table sets differ (missing/extra tables)"
    );

    for (name, jf_shape) in &jf_tables {
        let hm_shape = &hm_tables[name];
        assert_eq!(
            jf_shape.columns, hm_shape.columns,
            "column shape of `{name}` diverges from 12.2"
        );
        assert_eq!(
            jf_shape.foreign_keys, hm_shape.foreign_keys,
            "foreign keys of `{name}` diverge from 12.2"
        );
    }

    let missing: Vec<_> = jf_indexes.difference(&hm_indexes).collect();
    assert!(
        missing.is_empty(),
        "12.2 indexes missing from Ferrofin: {missing:?}"
    );
    let extra: Vec<_> = hm_indexes
        .difference(&jf_indexes)
        .filter(|(name, _, _, _)| !name.starts_with("FerrofinIX_"))
        .collect();
    assert!(
        extra.is_empty(),
        "non-FerrofinIX_ index surplus on Jellyfin-owned tables: {extra:?}"
    );
}

/// The schema dump a real database of `generation` carries.
fn generation_schema(generation: &str) -> &'static str {
    match generation {
        g if g.starts_with("10.11.") => include_str!("data/jellyfin-10.11.8-schema.sql"),
        "12.0.0" | "12.1.0" => include_str!("data/jellyfin-12.0-schema.sql"),
        "12.2.0" => include_str!("data/jellyfin-12.2-schema.sql"),
        other => panic!("no schema fixture for generation {other}"),
    }
}

/// Creates a file-backed database shaped like a real Jellyfin one: a schema
/// dump plus its EF history rows (a copy of `database.rs`'s private
/// `seed_jellyfin_fixture_with`), then `ANALYZE`, which every real Jellyfin
/// database has run.
async fn seed_jellyfin_fixture(path: &std::path::Path, schema: &str, migrations: &[&str]) {
    use sqlx::ConnectOptions;
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
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
    sqlx::query("ANALYZE")
        .execute(&mut conn)
        .await
        .expect("analyze");
    let stat1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("sqlite_master");
    assert_eq!(
        stat1, 1,
        "ANALYZE leaves sqlite_stat1, as on a real database"
    );
    sqlx::Connection::close(conn).await.expect("close");
}

async fn has_sqlite_stat1(pool: &SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
    )
    .fetch_one(pool)
    .await
    .expect("sqlite_master")
        > 0
}

/// The one statement a fresh Jellyfin 12.x install lacks next to the schema
/// pins: `20210320181425_AddIndexesAndCollations` drops it, so only databases
/// that predate that drop (like the ones the pins were dumped from) carry it.
const CUSTOM_PREFS_USER_INDEX: &str = "CREATE INDEX \"IX_CustomItemDisplayPreferences_UserId\" \
     ON \"CustomItemDisplayPreferences\"(\n  \"UserId\"\n);";

/// `schema` in the shape a fresh Jellyfin install has. Booting the portable
/// 12.0, 12.1 and 12.2 tarballs on empty directories (2026-10-12) showed this
/// index to be the only shape difference from the pins (`sqlite_sequence` and
/// `sqlite_stat1` aside; EF's DDL whitespace differs, which `snapshot` ignores).
fn fresh_install_schema(schema: &str) -> String {
    assert!(
        schema.contains(CUSTOM_PREFS_USER_INDEX),
        "the pin carries the index"
    );
    schema.replace(CUSTOM_PREFS_USER_INDEX, "")
}

/// Adoption must land every generation on the one schema a fresh database has
/// — Ferrofin's own `FerrofinIX_` indexes included. A 12.0/12.1 database
/// baselines `0032`, so the duplicate `FerrofinIX_BaseItems_*` set `0009`/`0014`
/// created survives there unless `0039` drops it; a 12.2 database baselines
/// `0040` and must still match the partial `OwnerId` pair a fresh chain builds.
///
/// Each generation is checked as an upgraded install (every optional id present,
/// the schema pin as dumped) and, for 12.x, as a fresh Jellyfin install: no
/// optional id (the set a fresh install records, pinned against the captured
/// truth by `database.rs`'s `adopts_a_fresh_jellyfin_12_x_install`) and no
/// `IX_CustomItemDisplayPreferences_UserId`, which `0041` restores.
#[tokio::test]
async fn adopted_and_fresh_databases_converge_on_one_schema() {
    let fresh = Database::connect_in_memory().await.expect("fresh connect");
    fresh.run_migrations().await.expect("fresh migrate");
    let fresh_snapshot = snapshot(fresh.pool()).await;
    assert!(!has_sqlite_stat1(fresh.pool()).await);

    // Every generation is checked before failing, so one run names them all.
    let mut divergences = Vec::new();
    let fresh_install_rows = JELLYFIN_GENERATIONS
        .iter()
        .filter(|g| g.name.starts_with("12."))
        .map(|g| (g, true));
    let rows = JELLYFIN_GENERATIONS
        .iter()
        .map(|g| (g, false))
        .chain(fresh_install_rows);
    for (generation, fresh_install) in rows {
        let ids: BTreeSet<&str> = generation
            .migration_ids
            .iter()
            .chain(generation.additional_ids)
            .chain(generation.optional_ids)
            .filter(|id| !fresh_install || !generation.optional_ids.contains(id))
            .copied()
            .collect();
        let ids: Vec<&str> = ids.into_iter().collect();
        let route = if fresh_install {
            "fresh install"
        } else {
            "upgraded"
        };
        let name = format!("{} ({route}, {} ids)", generation.name, ids.len());
        let schema = if fresh_install {
            fresh_install_schema(generation_schema(generation.name))
        } else {
            generation_schema(generation.name).to_owned()
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        seed_jellyfin_fixture(&path, &schema, &ids).await;

        let adopted = Database::connect_sized(&format!("sqlite://{}", path.display()), Some(2))
            .await
            .unwrap_or_else(|e| panic!("{name}: adoption failed: {e}"));
        adopted
            .run_migrations()
            .await
            .unwrap_or_else(|e| panic!("{name}: migrate failed: {e}"));
        assert_eq!(
            adopted
                .adoption_state()
                .await
                .expect("adoption state")
                .map(|s| s.generation),
            Some(generation.name.to_owned()),
            "{name}: the database was adopted as its own generation"
        );

        let (tables, indexes) = snapshot(adopted.pool()).await;
        if tables != fresh_snapshot.0 {
            let differing: Vec<_> = tables
                .keys()
                .chain(fresh_snapshot.0.keys())
                .filter(|t| tables.get(*t) != fresh_snapshot.0.get(*t))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            divergences.push(format!("{name}: tables differ: {differing:?}"));
        }
        let only_adopted: Vec<_> = indexes.difference(&fresh_snapshot.1).collect();
        let only_fresh: Vec<_> = fresh_snapshot.1.difference(&indexes).collect();
        if !only_adopted.is_empty() || !only_fresh.is_empty() {
            divergences.push(format!(
                "{name}: indexes differ — only on the adopted database: {only_adopted:?}; \
                 only on a fresh one: {only_fresh:?}"
            ));
        }
        if has_sqlite_stat1(adopted.pool()).await {
            divergences.push(format!("{name}: sqlite_stat1 survived adoption"));
        }
        adopted.pool().close().await;
        adopted.writer().close().await;
    }
    assert!(
        divergences.is_empty(),
        "adopted schemas diverge from a fresh one:\n{}",
        divergences.join("\n")
    );
}
