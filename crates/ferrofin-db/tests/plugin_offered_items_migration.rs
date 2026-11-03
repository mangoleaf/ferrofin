//! Migration 0049 adds `FerrofinPluginOfferedItems`, the library items the
//! "Plugin media analysis" pass has offered to each analyzer plugin, on a fresh
//! database and on every adopted Jellyfin generation. The table is
//! Ferrofin-owned, so `schema_conformance` leaves it out; this file pins its
//! shape. The store over it is `ferrofin-core`'s
//! `plugin_offered_item_repository`.

use std::path::Path;

use ferrofin_db::{Database, JELLYFIN_GENERATIONS};
use sqlx::{ConnectOptions, SqlitePool};

const TABLE: &str = "FerrofinPluginOfferedItems";
const ITEM: &str = "CCCCCCCC-0000-0000-0000-000000000001";
const PLUGIN: &str = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEFFFF";

/// Each column: name, declared type, `NOT NULL`, default and primary-key position.
type Column = (String, String, bool, Option<String>, i64);

fn expected_columns() -> Vec<Column> {
    [
        ("PluginId", "TEXT", true, 2),
        ("ItemId", "TEXT", true, 1),
        ("OfferedAt", "TEXT", true, 0),
    ]
    .into_iter()
    .map(|(name, kind, not_null, pk)| (name.to_owned(), kind.to_owned(), not_null, None, pk))
    .collect()
}

/// The table, keyed on (ItemId, PluginId) without a rowid, with its foreign
/// key to the item and no other index.
async fn assert_shape(pool: &SqlitePool, label: &str) {
    let columns: Vec<Column> = sqlx::query_as(
        r#"SELECT "name", "type", "notnull", "dflt_value", "pk" FROM pragma_table_info(?1)
           ORDER BY "cid""#,
    )
    .bind(TABLE)
    .fetch_all(pool)
    .await
    .expect("table info");
    assert_eq!(columns, expected_columns(), "{label}: columns");

    let without_rowid: bool =
        sqlx::query_scalar(r#"SELECT "wr" FROM pragma_table_list WHERE "name" = ?1"#)
            .bind(TABLE)
            .fetch_one(pool)
            .await
            .expect("table list");
    assert!(without_rowid, "{label}: WITHOUT ROWID");

    let foreign_keys: Vec<(String, String, String, String)> = sqlx::query_as(
        r#"SELECT "table", "from", "to", "on_delete" FROM pragma_foreign_key_list(?1)"#,
    )
    .bind(TABLE)
    .fetch_all(pool)
    .await
    .expect("foreign keys");
    assert_eq!(
        foreign_keys,
        [(
            "BaseItems".to_owned(),
            "ItemId".to_owned(),
            "Id".to_owned(),
            "CASCADE".to_owned()
        )],
        "{label}: foreign keys"
    );

    let indexes: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT "name", "origin" FROM pragma_index_list(?1) ORDER BY "name""#)
            .bind(TABLE)
            .fetch_all(pool)
            .await
            .expect("index list");
    assert_eq!(
        indexes,
        [(format!("sqlite_autoindex_{TABLE}_1"), "pk".to_owned())],
        "{label}: only the primary key"
    );
}

#[tokio::test]
async fn a_fresh_database_has_the_table() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    assert_shape(db.pool(), "fresh").await;
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

/// A file-backed database shaped like a real Jellyfin one: a schema dump plus
/// its EF history rows (as in `schema_conformance.rs`).
async fn seed_jellyfin_fixture(path: &Path, schema: &str, migrations: &[&str]) {
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
    sqlx::Connection::close(conn).await.expect("close");
}

#[tokio::test]
async fn every_adopted_generation_gains_the_table() {
    for generation in JELLYFIN_GENERATIONS {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jellyfin.db");
        let mut history = generation.migration_ids.to_vec();
        history.extend(generation.additional_ids);
        seed_jellyfin_fixture(&path, generation_schema(generation.name), &history).await;

        let db = Database::connect_sized(&format!("sqlite://{}", path.display()), Some(2))
            .await
            .unwrap_or_else(|e| panic!("{}: adoption failed: {e}", generation.name));
        db.run_migrations()
            .await
            .unwrap_or_else(|e| panic!("{}: migrate failed: {e}", generation.name));
        assert_shape(db.pool(), generation.name).await;
        db.pool().close().await;
        db.writer().close().await;
    }
}

/// Inserts an offer of `item` to `plugin`.
async fn offer(pool: &SqlitePool, item: &str, plugin: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO "FerrofinPluginOfferedItems" ("PluginId", "ItemId", "OfferedAt")
           VALUES (?1, ?2, '2026-11-02 00:00:00.0000000')"#,
    )
    .bind(plugin)
    .bind(item)
    .execute(pool)
    .await
    .map(|_| ())
}

/// With foreign keys on, as at runtime: an offer needs its item, an item is
/// offered to a plugin once, and deleting the item removes its offers.
#[tokio::test]
async fn an_offer_needs_its_item_and_goes_with_it() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let pool = db.pool();
    sqlx::query(
        r#"INSERT INTO "BaseItems" ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked",
               "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, 'MediaBrowser.Controller.Entities.Movies.Movie', 0, 0, 0, 1, 0, 0, 0)"#,
    )
    .bind(ITEM)
    .execute(pool)
    .await
    .expect("item");

    offer(pool, ITEM, PLUGIN).await.expect("offer");
    let again = offer(pool, ITEM, PLUGIN).await;
    assert!(
        again.is_err_and(|e| e.to_string().contains("UNIQUE")),
        "once per item and plugin"
    );
    offer(pool, ITEM, "AAAAAAAA-0000-0000-0000-000000000002")
        .await
        .expect("another plugin");
    let orphan = offer(pool, "CCCCCCCC-0000-0000-0000-00000000DEAD", PLUGIN).await;
    assert!(
        orphan.is_err_and(|e| e.to_string().contains("FOREIGN KEY")),
        "an offer needs its item"
    );

    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(ITEM)
        .execute(db.writer())
        .await
        .expect("delete item");
    let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinPluginOfferedItems""#)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(left, 0, "ON DELETE CASCADE removed the offers");
}
