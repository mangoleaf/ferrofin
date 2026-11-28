//! gl-1969: a database Ferrofin created itself holds several of Jellyfin's
//! tables in another shape (the columns in another order, an integer key
//! without `AUTOINCREMENT`). Its export rebuilds them in the shape of
//! Jellyfin's schema, rows, ids and sequence included. An adopted database
//! keeps Jellyfin's own tables.

use std::collections::BTreeSet;
use std::path::Path;

use ferrofin_db::Database;
use ferrofin_db::export::{DOCKER, EXPORTED_DATABASE, SourceDirs, export_data_dir};
use sqlx::ConnectOptions as _;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;

/// The Jellyfin schema an exported database must have: the 12.2 pin (12.3
/// and 12.4 add no schema migration).
const JELLYFIN_SCHEMA: &str = include_str!("data/jellyfin-12.2-schema.sql");

/// The build this test exports as, whose start the source records: the export
/// refuses a data directory another build started last (gl-1910).
const BUILD: &str = "v0.0.0-test";

/// The item every fresh database holds, which the rows below hang on (the
/// statements spell it out).
const PLACEHOLDER: &str = "00000000-0000-0000-0000-000000000001";

/// A user's id.
const USER: &str = "5D1E2C3B-4A59-4687-9C8B-7A6F5E4D3C2B";

/// Every table (its columns in order, and whether its key is
/// `AUTOINCREMENT`) and index (its columns and whether it is unique) of
/// `conn`'s database, one line each, SQLite's own objects aside: what the
/// round trip gate's schema check compares (the columns in order), with
/// `AUTOINCREMENT` added.
async fn schema(conn: &mut SqliteConnection) -> BTreeSet<String> {
    let objects: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let mut lines = BTreeSet::new();
    for (kind, name, table, sql) in objects {
        let detail: Vec<String> = if kind == "table" {
            sqlx::query_as::<_, (String, String, i64, Option<String>, i64)>(
                r#"SELECT name, upper(type), "notnull", dflt_value, pk FROM pragma_table_info(?1)"#,
            )
            .bind(&name)
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .into_iter()
            .map(|column| format!("{column:?}"))
            .chain(
                sql.contains("AUTOINCREMENT")
                    .then(|| "AUTOINCREMENT".to_owned()),
            )
            .collect()
        } else {
            sqlx::query_scalar(
                r#"SELECT name FROM pragma_index_info(?1)
                   UNION ALL SELECT 'unique=' || "unique" FROM pragma_index_list(?2) WHERE name = ?1"#,
            )
            .bind(&name)
            .bind(&table)
            .fetch_all(&mut *conn)
            .await
            .unwrap()
        };
        lines.insert(format!("{kind} {name} on {table}: {detail:?}"));
    }
    lines
}

/// A data directory whose database Ferrofin created and migrated, with a
/// user, an access schedule, a genre on the placeholder item and the user's
/// data on it: rows in tables whose shape the export changes, one of them
/// (`ItemValues`) referenced by another (`ItemValuesMap`).
async fn native_source(scratch: &Path) -> std::path::PathBuf {
    let source = scratch.join("source");
    let database = source.join(EXPORTED_DATABASE);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let db = Database::connect_sized(&format!("sqlite://{}", database.display()), Some(2))
        .await
        .unwrap();
    db.run_migrations().await.unwrap();
    for statement in [
        r#"INSERT INTO "Users" ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest",
            "InternalId", "InvalidLoginAttemptCount", "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections",
            "RememberSubtitleSelections", "RowVersion", "SubtitleMode", "SyncPlayAccess",
            "Username", "NormalizedUsername")
           VALUES (?1, 'auth', 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 'reset', 1, 1, 1, 0, 0, 0, 'ann', 'ANN')"#,
        r#"INSERT INTO "AccessSchedules" ("Id", "UserId", "DayOfWeek", "StartHour", "EndHour")
           VALUES (7, ?1, 3, 8.5, 20.0)"#,
        r#"INSERT INTO "ItemValues" ("ItemValueId", "Type", "Value", "CleanValue")
           VALUES ('B1C2D3E4-F5A6-4B7C-8D9E-0F1A2B3C4D5E', 2, 'Rock', 'rock')"#,
        r#"INSERT INTO "ItemValuesMap" ("ItemId", "ItemValueId")
           VALUES ('00000000-0000-0000-0000-000000000001', 'B1C2D3E4-F5A6-4B7C-8D9E-0F1A2B3C4D5E')"#,
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES ('00000000-0000-0000-0000-000000000001', ?1, 'tmdb-603', 1, 2, 300, 1)"#,
    ] {
        let query = sqlx::query(statement);
        // Only the statements naming the user bind it.
        let query = if statement.contains("?1") {
            query.bind(USER)
        } else {
            query
        };
        query.execute(db.writer()).await.unwrap();
    }
    db.record_started(BUILD).await.unwrap();
    db.pool().close().await;
    db.writer().close().await;
    source
}

#[tokio::test]
async fn a_native_database_s_tables_are_exported_in_jellyfin_s_shape() {
    let scratch = tempfile::tempdir().unwrap();
    let source = native_source(scratch.path()).await;
    let out = scratch.path().join("out");

    let summary = export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
        BUILD,
    )
    .await
    .unwrap();

    let mut exported = SqliteConnectOptions::new()
        .filename(out.join(EXPORTED_DATABASE))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let mut pinned = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(JELLYFIN_SCHEMA)
        .execute(&mut pinned)
        .await
        .unwrap();
    let (exported_schema, pinned_schema) = (schema(&mut exported).await, schema(&mut pinned).await);
    assert_eq!(
        exported_schema
            .difference(&pinned_schema)
            .collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "only in the export"
    );
    assert_eq!(
        pinned_schema
            .difference(&exported_schema)
            .collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "only in Jellyfin's schema"
    );

    // The rows, by column name, and no row lost its parent: the drop of
    // `ItemValues` did not cascade into `ItemValuesMap`.
    let schedule: (i64, String, i64, f64, f64) = sqlx::query_as(
        r#"SELECT "Id", "UserId", "DayOfWeek", "StartHour", "EndHour" FROM "AccessSchedules""#,
    )
    .fetch_one(&mut exported)
    .await
    .unwrap();
    assert_eq!(schedule, (7, USER.to_owned(), 3, 8.5, 20.0));
    let genre: (String, String, String) = sqlx::query_as(
        r#"SELECT m."ItemId", v."Value", v."CleanValue" FROM "ItemValuesMap" m
           JOIN "ItemValues" v USING ("ItemValueId")"#,
    )
    .fetch_one(&mut exported)
    .await
    .unwrap();
    assert_eq!(
        genre,
        (PLACEHOLDER.to_owned(), "Rock".to_owned(), "rock".to_owned())
    );
    let user_data: (String, i64, i64, i64) = sqlx::query_as(
        r#"SELECT "CustomDataKey", "PlayCount", "PlaybackPositionTicks", "IsFavorite" FROM "UserData""#,
    )
    .fetch_one(&mut exported)
    .await
    .unwrap();
    assert_eq!(user_data, ("tmdb-603".to_owned(), 2, 300, 1));
    let violations: Vec<String> =
        sqlx::query_scalar(r#"SELECT "table" FROM pragma_foreign_key_check"#)
            .fetch_all(&mut exported)
            .await
            .unwrap();
    assert_eq!(violations, Vec::<String>::new());
    // The highest id copied is where Jellyfin's next one starts.
    let sequence: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'AccessSchedules'")
            .fetch_optional(&mut exported)
            .await
            .unwrap();
    assert_eq!(sequence, Some(7));
    sqlx::Connection::close(exported).await.unwrap();
    sqlx::Connection::close(pinned).await.unwrap();
    // Exactly the tables `0001` made in another shape.
    assert_eq!(
        summary.edits.rebuilt,
        [
            "AccessSchedules",
            "ActivityLogs",
            "ApiKeys",
            "Chapters",
            "CustomItemDisplayPreferences",
            "DeviceOptions",
            "Devices",
            "HomeSection",
            "ImageInfos",
            "ItemDisplayPreferences",
            "ItemValues",
            "ItemValuesMap",
            "KeyframeData",
            "UserData",
        ]
    );
}
