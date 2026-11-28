//! Migration 0055 adds `FerrofinPendingChildMetadata`, the music albums whose
//! children-derived tags a library scan's walk saved them owing, on a fresh
//! database and on every adopted Jellyfin generation. The table is
//! Ferrofin-owned, so `schema_conformance` leaves it out; this file pins its
//! shape. The store over it is `ferrofin-core`'s item persistence service.

use std::path::Path;

use ferrofin_db::{Database, JELLYFIN_GENERATIONS};
use sqlx::{ConnectOptions, SqlitePool};

const TABLE: &str = "FerrofinPendingChildMetadata";
const ITEM: &str = "CCCCCCCC-0000-0000-0000-000000000001";

/// The table: one `ItemId` key without a rowid, its foreign key to the item,
/// and no other index.
async fn assert_shape(pool: &SqlitePool, label: &str) {
    let columns: Vec<(String, String, bool, Option<String>, i64)> = sqlx::query_as(
        r#"SELECT "name", "type", "notnull", "dflt_value", "pk" FROM pragma_table_info(?1)
           ORDER BY "cid""#,
    )
    .bind(TABLE)
    .fetch_all(pool)
    .await
    .expect("table info");
    assert_eq!(
        columns,
        [("ItemId".to_owned(), "TEXT".to_owned(), true, None, 1)],
        "{label}: columns"
    );

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

/// Inserts a record for `item`, as the walk does.
async fn record(pool: &SqlitePool, item: &str) -> Result<(), sqlx::Error> {
    sqlx::query(r#"INSERT OR IGNORE INTO "FerrofinPendingChildMetadata" ("ItemId") VALUES (?1)"#)
        .bind(item)
        .execute(pool)
        .await
        .map(|_| ())
}

/// With foreign keys on, as at runtime: a record needs its album, a second
/// record of it is one, and deleting the album removes it.
#[tokio::test]
async fn a_record_needs_its_album_and_goes_with_it() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let pool = db.writer();
    sqlx::query(
        r#"INSERT INTO "BaseItems" ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked",
               "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, 'MediaBrowser.Controller.Entities.Audio.MusicAlbum', 1, 0, 0, 0, 0, 0, 0)"#,
    )
    .bind(ITEM)
    .execute(pool)
    .await
    .expect("album");

    record(pool, ITEM).await.expect("record");
    record(pool, ITEM).await.expect("again");
    let orphan = record(pool, "CCCCCCCC-0000-0000-0000-00000000DEAD").await;
    assert!(
        orphan.is_err_and(|e| e.to_string().contains("FOREIGN KEY")),
        "a record needs its album"
    );
    let count = || async {
        sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*) FROM "FerrofinPendingChildMetadata""#)
            .fetch_one(pool)
            .await
            .expect("count")
    };
    assert_eq!(count().await, 1, "one record per album");

    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(ITEM)
        .execute(pool)
        .await
        .expect("delete album");
    assert_eq!(count().await, 0, "ON DELETE CASCADE removed the record");
}
