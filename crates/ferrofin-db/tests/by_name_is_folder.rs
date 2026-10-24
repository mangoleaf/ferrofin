//! Migration 0043 stores `IsFolder = 0` on the by-name rows earlier Ferrofin
//! builds wrote as folders: every `Genre`/`MusicGenre`/`Studio`/`Person`/`Year`
//! row, and a `MusicArtist` with no parent. A physically parented artist folder
//! and every other folder keep `IsFolder = 1`, as Jellyfin stores them.

use std::borrow::Cow;

use ferrofin_db::Database;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

const LIBRARY: &str = "AAAAAAAA-0000-0000-0000-000000000001";

/// A migrator holding the migrations up to and including `last`.
fn through(last: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            sqlx::migrate!("./migrations")
                .migrations
                .iter()
                .filter(|m| m.version <= last)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate!("./migrations")
    }
}

/// Inserts a folder row of `type_` under `parent`, the shape the old scan wrote.
async fn folder(pool: &SqlitePool, id: &str, type_: &str, parent: Option<&str>) {
    sqlx::query(
        r#"INSERT INTO "BaseItems" ("Id", "Type", "Name", "ParentId", "IsFolder",
               "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, ?2, ?1, ?3, 1, 0, 0, 0, 0, 0, 0)"#,
    )
    .bind(id)
    .bind(type_)
    .bind(parent)
    .execute(pool)
    .await
    .expect("insert item");
}

#[tokio::test]
async fn by_name_rows_stop_being_folders() {
    // `Database` registers the SQL functions earlier migrations call.
    let db = Database::connect_in_memory().await.expect("database");
    let pool = db.pool().clone();
    through(42)
        .run(&pool)
        .await
        .expect("migrations through 0042");
    // (type, id, parent, IsFolder after 0043)
    let rows = [
        ("CollectionFolder", LIBRARY, None, 1),
        ("Audio.MusicArtist", "B1", None, 0),
        ("Audio.MusicArtist", "B2", Some(LIBRARY), 1),
        ("Genre", "B3", None, 0),
        ("Audio.MusicGenre", "B4", None, 0),
        ("Studio", "B5", None, 0),
        ("Person", "B6", None, 0),
        ("Year", "B7", None, 0),
        ("TV.Series", "B8", Some(LIBRARY), 1),
    ];
    for (type_, id, parent, _) in rows {
        let type_ = format!("MediaBrowser.Controller.Entities.{type_}");
        folder(&pool, id, &type_, parent).await;
    }

    through(43).run(&pool).await.expect("migration 0043");

    for (type_, id, _, expected) in rows {
        let is_folder: i64 =
            sqlx::query_scalar(r#"SELECT "IsFolder" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("row");
        assert_eq!(is_folder, expected, "{type_} {id}");
    }
}
