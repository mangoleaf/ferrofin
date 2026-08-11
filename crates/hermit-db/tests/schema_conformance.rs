//! Schema-equality gate: a fresh Hermit database's Jellyfin-owned schema must
//! be IDENTICAL to a real Jellyfin 10.11.8 database's.
//!
//! The fixture (`tests/data/jellyfin-10.11.8-schema.sql`) is the sqlite_master
//! dump of a database created by a real `jellyfin/jellyfin:10.11.8` server —
//! the drop-in contract (`brain/plans/PLAN_DB_DROPIN.md` Workstream E). This
//! test is the tripwire for future drift: any schema change that breaks
//! byte-parity with 10.11.8 fails here, and the future Jellyfin-12 sync will
//! be gated on an updated fixture the same way.
//!
//! Comparison rules:
//! - tables: names, columns (declared type, notnull, default, pk position),
//!   and foreign keys (target/from/to/on-delete) must match exactly;
//! - Hermit-own objects (`Hermit*` tables, `_sqlx_migrations`) are additive
//!   and excluded; Jellyfin's EF bookkeeping exists only on real databases;
//! - indexes: Jellyfin's named index set must exist verbatim; Hermit may add
//!   only `HermitIX_`-prefixed indexes (EF-invisible, collision-proof).

use std::collections::{BTreeMap, BTreeSet};

use hermit_db::Database;
use sqlx::{Row, SqlitePool};

/// One table's comparable shape.
#[derive(Debug, PartialEq, Eq)]
struct TableShape {
    /// column name → (declared type uppercased, notnull, default, pk position)
    columns: BTreeMap<String, (String, i64, Option<String>, i64)>,
    /// (target table, from column, to column, on-delete action)
    foreign_keys: BTreeSet<(String, String, Option<String>, String)>,
}

/// (index name, ordered column list, unique flag)
type IndexShape = (String, Vec<String>, i64);

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
        if table.starts_with("__") || table == "_sqlx_migrations" || table.starts_with("Hermit") {
            continue;
        }
        let mut columns = BTreeMap::new();
        for row in sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
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
        for row in sqlx::query(&format!("PRAGMA foreign_key_list(\"{table}\")"))
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
        let index_rows = sqlx::query(&format!("PRAGMA index_list(\"{table}\")"))
            .fetch_all(pool)
            .await
            .expect("index_list");
        for row in index_rows {
            if row.get::<String, _>("origin") != "c" {
                continue; // auto PK/unique indexes follow from the column defs
            }
            let name: String = row.get("name");
            let unique: i64 = row.get("unique");
            let cols: Vec<String> = sqlx::query(&format!("PRAGMA index_info(\"{name}\")"))
                .fetch_all(pool)
                .await
                .expect("index_info")
                .into_iter()
                .map(|r| r.get::<Option<String>, _>("name").unwrap_or_default())
                .collect();
            indexes.insert((name, cols, unique));
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
async fn fresh_hermit_schema_equals_real_jellyfin_10_11_8() {
    // The real 10.11.8 schema, from the committed fixture dump.
    let jellyfin = Database::connect_in_memory().await.expect("jf connect");
    sqlx::raw_sql(include_str!("data/jellyfin-10.11.8-schema.sql"))
        .execute(jellyfin.pool())
        .await
        .expect("apply fixture schema");

    // A fresh Hermit database through the full migration chain.
    let hermit = Database::connect_in_memory().await.expect("hermit connect");
    hermit.run_migrations().await.expect("migrate");

    let (jf_tables, jf_indexes) = snapshot(jellyfin.pool()).await;
    let (hm_tables, hm_indexes) = snapshot(hermit.pool()).await;

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
            "column shape of `{name}` diverges from 10.11.8"
        );
        assert_eq!(
            jf_shape.foreign_keys, hm_shape.foreign_keys,
            "foreign keys of `{name}` diverge from 10.11.8"
        );
    }

    let missing: Vec<_> = jf_indexes.difference(&hm_indexes).collect();
    assert!(
        missing.is_empty(),
        "10.11.8 indexes missing from Hermit: {missing:?}"
    );
    let extra: Vec<_> = hm_indexes
        .difference(&jf_indexes)
        .filter(|(name, _, _)| !name.starts_with("HermitIX_"))
        .collect();
    assert!(
        extra.is_empty(),
        "non-HermitIX_ index surplus on Jellyfin-owned tables: {extra:?}"
    );
}
