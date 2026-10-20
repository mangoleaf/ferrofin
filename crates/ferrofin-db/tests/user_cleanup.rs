//! Migration 0044 removes what a long-lived Jellyfin database keeps of deleted users
//! (Permissions, Preferences, UserData, Devices, display preferences, …) and
//! `AncestorIds` rows whose item is gone. Jellyfin 12.1+ boots on such a database;
//! without 0044 the boot's `foreign_key_check` refuses it. Migrations run with
//! foreign keys OFF, so nothing cascades: these tests hold 0044 to every foreign key
//! to `Users` and to the children of the rows it deletes. 0045 re-points an `OwnerId`
//! that names no item at the placeholder item, which the boot repair then deletes.

use std::path::Path;

use ferrofin_db::{Database, JELLYFIN_GENERATIONS};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};

const CLEANUP_SQL: &str = include_str!("../migrations/0044_dangling_user_and_ancestor_cleanup.sql");
const SCHEMA_10_11_8: &str = include_str!("data/jellyfin-10.11.8-schema.sql");
const SCHEMA_12_2: &str = include_str!("data/jellyfin-12.2-schema.sql");

// Jellyfin stores GUIDs uppercase.
const KEPT_USER: &str = "CCCCCCCC-0000-0000-0000-000000000001";
const GONE_USER: &str = "CCCCCCCC-0000-0000-0000-00000000DEAD";
const ITEM: &str = "BBBBBBBB-0000-0000-0000-000000000001";
const GONE_ITEM: &str = "BBBBBBBB-0000-0000-0000-00000000DEAD";
const EXTRA: &str = "BBBBBBBB-0000-0000-0000-0000000000E1";
const ORPHAN: &str = "BBBBBBBB-0000-0000-0000-0000000000E2";
const CASED: &str = "BBBBBBBB-0000-0000-0000-0000000000E3";
const EMPTY_OWNER: &str = "BBBBBBBB-0000-0000-0000-0000000000E4";
const ZERO_OWNER: &str = "BBBBBBBB-0000-0000-0000-0000000000E5";
const ZERO_GUID: &str = "00000000-0000-0000-0000-000000000000";
/// The item Jellyfin re-points detached `UserData` and dangling owners at.
const PLACEHOLDER: &str = "00000000-0000-0000-0000-000000000001";

/// The tables holding per-user rows that 0044 clears, with their user column.
const PER_USER: [&str; 9] = [
    "Permissions",
    "Preferences",
    "UserData",
    "Devices",
    "AccessSchedules",
    "ImageInfos",
    "DisplayPreferences",
    "ItemDisplayPreferences",
    "CustomItemDisplayPreferences",
];

/// A file-backed database shaped like a real Jellyfin one: a committed schema
/// fixture plus its EF migration rows (as in `orphaned_parents.rs`).
async fn seed_jellyfin_fixture(path: &Path, schema: &str, migrations: &[&str]) {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(false)
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

/// Inserts a row into `table` with `given` columns, filling every other
/// `NOT NULL` column without a default with a placeholder of its type, so one
/// call works on every schema generation.
async fn insert(pool: &SqlitePool, table: &str, given: &[(&str, &str)]) -> i64 {
    let columns: Vec<(String, String, bool, Option<String>, i64)> = sqlx::query_as(
        r#"SELECT "name", "type", "notnull", "dflt_value", "pk" FROM pragma_table_info(?1)"#,
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("table info");
    assert!(!columns.is_empty(), "no table {table}");
    let mut names = Vec::new();
    let mut values = Vec::new();
    for (name, kind, not_null, default, pk) in columns {
        if let Some((_, value)) = given.iter().find(|(column, _)| *column == name) {
            names.push(format!(r#""{name}""#));
            values.push(format!("'{value}'"));
        } else if not_null && default.is_none() && !(pk == 1 && kind == "INTEGER") {
            names.push(format!(r#""{name}""#));
            values.push(if kind == "TEXT" { "'x'" } else { "0" }.to_owned());
        }
    }
    let sql = format!(
        r#"INSERT INTO "{table}" ({}) VALUES ({})"#,
        names.join(", "),
        values.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("insert into {table}: {e}"))
        .last_insert_rowid()
}

/// One row in every per-user table for `user`, plus a home section under its
/// display preferences.
async fn per_user_rows(pool: &SqlitePool, user: &str) {
    for table in PER_USER {
        let given: &[(&str, &str)] = match table {
            "UserData" => &[("UserId", user), ("ItemId", ITEM), ("CustomDataKey", user)],
            "ItemDisplayPreferences" | "CustomItemDisplayPreferences" => {
                &[("UserId", user), ("ItemId", ITEM)]
            }
            _ => &[("UserId", user)],
        };
        let id = insert(pool, table, given).await;
        if table == "DisplayPreferences" {
            insert(
                pool,
                "HomeSection",
                &[("DisplayPreferencesId", &id.to_string())],
            )
            .await;
        }
    }
}

async fn count(pool: &SqlitePool, table: &str, filter: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        r#"SELECT COUNT(*) FROM "{table}" WHERE {filter}"#
    )))
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

async fn foreign_key_violations(pool: &SqlitePool) -> usize {
    sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .expect("foreign_key_check")
        .len()
}

/// A Jellyfin database holding a deleted user's rows in every per-user table and
/// `AncestorIds` rows of a deleted item adopts: 0044 removes exactly those rows,
/// the remaining user's rows stay, and the boot's `foreign_key_check` passes.
#[rstest::rstest]
#[case::jellyfin_10_11_8("10.11.8", SCHEMA_10_11_8)]
#[case::jellyfin_12_2("12.2.0", SCHEMA_12_2)]
#[tokio::test]
async fn adoption_removes_rows_of_deleted_users_and_items(
    #[case] generation: &str,
    #[case] schema: &str,
) {
    let generation = JELLYFIN_GENERATIONS
        .iter()
        .find(|g| g.name == generation)
        .expect("generation");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("jellyfin.db");
    let mut history = generation.migration_ids.to_vec();
    history.extend(generation.additional_ids);
    seed_jellyfin_fixture(&path, schema, &history).await;
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(false),
            )
            .await
            .expect("open fixture");
        insert(&pool, "Users", &[("Id", KEPT_USER), ("Username", "viewer")]).await;
        insert(&pool, "BaseItems", &[("Id", ITEM), ("Type", "Movie")]).await;
        per_user_rows(&pool, KEPT_USER).await;
        per_user_rows(&pool, GONE_USER).await;
        for (child, ancestor) in [(ITEM, ITEM), (GONE_ITEM, ITEM), (ITEM, GONE_ITEM)] {
            insert(
                &pool,
                "AncestorIds",
                &[("ItemId", child), ("ParentItemId", ancestor)],
            )
            .await;
        }
        assert!(
            foreign_key_violations(&pool).await > 0,
            "the fixture really dangles"
        );
        pool.close().await;
    }

    let url = format!("sqlite://{}", path.display());
    let db = Database::connect_sized(&url, Some(2))
        .await
        .expect("adoption succeeds: the boot's foreign_key_check passed");
    let pool = db.pool();

    assert_eq!(foreign_key_violations(pool).await, 0);
    for table in PER_USER {
        let user = |id: &str| format!(r#"upper("UserId") = '{id}'"#);
        assert_eq!(count(pool, table, &user(GONE_USER)).await, 0, "{table}");
        assert!(count(pool, table, &user(KEPT_USER)).await > 0, "{table}");
    }
    assert_eq!(
        count(pool, "HomeSection", "1").await,
        1,
        "only the kept user's home section stays"
    );
    assert_eq!(
        count(pool, "AncestorIds", "1").await,
        1,
        "only the ancestry between existing items stays"
    );
}

/// An extra whose `OwnerId` names no item, in a Jellyfin database that never ran 0032's
/// re-pointing (a 12.x file already has the 12.x shape), adopts: 0045 re-points it at the
/// placeholder item, as 0032 does on the 10.11 chain, so the boot's `foreign_key_check`
/// passes and the `cleanup_orphaned_extras` boot repair (ferrofin-core, its own test)
/// finds it and deletes it. An extra whose owner exists is left alone, also when the
/// owner is spelled in another case. The 10.11.8 case is a control: 0032 already does
/// all of this there, so it holds the two chains to one end state.
#[rstest::rstest]
#[case::jellyfin_10_11_8("10.11.8", SCHEMA_10_11_8)]
#[case::jellyfin_12_2("12.2.0", SCHEMA_12_2)]
#[tokio::test]
async fn adoption_re_points_a_dangling_owner_at_the_placeholder(
    #[case] generation: &str,
    #[case] schema: &str,
) {
    let generation = JELLYFIN_GENERATIONS
        .iter()
        .find(|g| g.name == generation)
        .expect("generation");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("jellyfin.db");
    let mut history = generation.migration_ids.to_vec();
    history.extend(generation.additional_ids);
    seed_jellyfin_fixture(&path, schema, &history).await;
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(false),
            )
            .await
            .expect("open fixture");
        insert(
            &pool,
            "BaseItems",
            &[("Id", PLACEHOLDER), ("Type", "PLACEHOLDER")],
        )
        .await;
        insert(&pool, "BaseItems", &[("Id", ITEM), ("Type", "Movie")]).await;
        insert(
            &pool,
            "BaseItems",
            &[("Id", EXTRA), ("Type", "Video"), ("OwnerId", ITEM)],
        )
        .await;
        insert(
            &pool,
            "BaseItems",
            &[("Id", ORPHAN), ("Type", "Video"), ("OwnerId", GONE_ITEM)],
        )
        .await;
        // An owner that exists, spelled in lower case: SQLite's key compares bytes.
        let lower_owner = ITEM.to_lowercase();
        insert(
            &pool,
            "BaseItems",
            &[("Id", CASED), ("Type", "Video"), ("OwnerId", &lower_owner)],
        )
        .await;
        // Owners that are no Guid at all: 0032 maps them to NULL, an unowned item.
        for (id, owner) in [(EMPTY_OWNER, ""), (ZERO_OWNER, ZERO_GUID)] {
            insert(
                &pool,
                "BaseItems",
                &[("Id", id), ("Type", "Video"), ("OwnerId", owner)],
            )
            .await;
        }
        assert_eq!(
            foreign_key_violations(&pool).await,
            4 * usize::from(generation.name != "10.11.8"),
            "a 12.x fixture really dangles: the dangling, lower-case, empty and all-zero \
             owners (the 10.11.8 table has no key to violate)"
        );
        pool.close().await;
    }

    let url = format!("sqlite://{}", path.display());
    let db = Database::connect_sized(&url, Some(2))
        .await
        .expect("adoption succeeds: the boot's foreign_key_check passed");
    let pool = db.pool();

    assert_eq!(foreign_key_violations(pool).await, 0);
    let owner = |id: &str| format!(r#"upper("Id") = '{id}'"#);
    assert_eq!(
        count(
            pool,
            "BaseItems",
            &format!(r#"{} AND "OwnerId" = '{PLACEHOLDER}'"#, owner(ORPHAN))
        )
        .await,
        1,
        "the dangling owner is re-pointed at the placeholder"
    );
    assert_eq!(
        count(
            pool,
            "BaseItems",
            &format!(r#"{} AND "OwnerId" = '{ITEM}'"#, owner(EXTRA))
        )
        .await,
        1,
        "an existing owner stays"
    );
    assert_eq!(
        count(
            pool,
            "BaseItems",
            &format!(r#"{} AND "OwnerId" = '{ITEM}'"#, owner(CASED))
        )
        .await,
        1,
        "an owner spelled in another case is the same item: it keeps its extra, \
         which takes the spelling of the owner's Id"
    );
    for id in [EMPTY_OWNER, ZERO_OWNER] {
        assert_eq!(
            count(
                pool,
                "BaseItems",
                &format!(r#"{} AND "OwnerId" IS NULL"#, owner(id))
            )
            .await,
            1,
            "an owner that is no Guid is dropped, not handed to the repair: {id}"
        );
    }
    assert_eq!(
        count(pool, "BaseItems", "1").await,
        7,
        "the migration deletes nothing: the repair does, later"
    );
}

/// Every foreign key to `Users` has its delete in 0044, so a table that gains
/// one later fails here instead of at a user's boot.
#[tokio::test]
async fn cleanup_covers_every_foreign_key_to_users() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let dependents: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT m.name, f."from"
           FROM sqlite_master m, pragma_foreign_key_list(m.name) f
           WHERE m.type = 'table' AND f."table" = 'Users' ORDER BY 1, 2"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("dependents");
    assert!(
        dependents.contains(&("UserData".to_owned(), "UserId".to_owned())),
        "the query sees the schema: {dependents:?}"
    );
    let missing: Vec<String> = dependents
        .iter()
        .filter(|(table, column)| {
            !CLEANUP_SQL.contains(&format!(
                r#"DELETE FROM "{table}" WHERE "{column}" NOT IN (SELECT "Id" FROM "Users")"#
            )) && !CLEANUP_SQL.contains(&format!(
                r#"DELETE FROM "{table}" WHERE "{column}" IS NULL OR "{column}" NOT IN (SELECT "Id" FROM "Users")"#
            ))
        })
        .map(|(table, column)| format!("{table}.{column}"))
        .collect();
    assert!(missing.is_empty(), "0044 has no delete for {missing:?}");
}
