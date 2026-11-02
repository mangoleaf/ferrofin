//! Migration 0048 adds `FerrofinExternalIdentities`, the links from an outside
//! identity (an LDAP uid, later an OIDC issuer and subject) to a user, with its
//! two indexes, on a fresh database and on every adopted Jellyfin generation.
//! The table is Ferrofin-owned, so `schema_conformance` leaves it out; this file
//! pins its shape. The store over it is `ferrofin-core`'s
//! `external_identity_repository`.

use std::path::Path;

use ferrofin_db::{Database, JELLYFIN_GENERATIONS};
use sqlx::{ConnectOptions, SqlitePool};

const TABLE: &str = "FerrofinExternalIdentities";
const USER: &str = "CCCCCCCC-0000-0000-0000-000000000001";
const LDAP: &str = "Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin";

/// Each column: name, declared type, `NOT NULL`, default and primary-key position.
type Column = (String, String, bool, Option<String>, i64);

fn expected_columns() -> Vec<Column> {
    [
        ("Id", "INTEGER", false, None, 1),
        ("UserId", "TEXT", true, None, 0),
        ("ProviderId", "TEXT", true, None, 0),
        ("Issuer", "TEXT", true, Some("''"), 0),
        ("Subject", "TEXT", true, None, 0),
        ("AvatarHash", "TEXT", false, None, 0),
        ("DateCreated", "TEXT", true, None, 0),
    ]
    .into_iter()
    .map(|(name, kind, not_null, default, pk)| {
        (
            name.to_owned(),
            kind.to_owned(),
            not_null,
            default.map(str::to_owned),
            pk,
        )
    })
    .collect()
}

/// The table, its foreign key and its two indexes, exactly.
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
            "Users".to_owned(),
            "UserId".to_owned(),
            "Id".to_owned(),
            "CASCADE".to_owned()
        )],
        "{label}: foreign keys"
    );

    let indexes: Vec<(String, bool, String, bool)> = sqlx::query_as(
        r#"SELECT "name", "unique", "origin", "partial" FROM pragma_index_list(?1)
           ORDER BY "name""#,
    )
    .bind(TABLE)
    .fetch_all(pool)
    .await
    .expect("index list");
    let mut shapes = Vec::new();
    for (name, unique, origin, partial) in indexes {
        let columns: Vec<String> =
            sqlx::query_scalar(r#"SELECT "name" FROM pragma_index_info(?1) ORDER BY "seqno""#)
                .bind(&name)
                .fetch_all(pool)
                .await
                .expect("index info");
        shapes.push((name, unique, origin, partial, columns));
    }
    let index = |name: &str, unique: bool, columns: &[&str]| {
        (
            name.to_owned(),
            unique,
            "c".to_owned(),
            false,
            columns.iter().map(|&c| c.to_owned()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        shapes,
        [
            index(
                "FerrofinIX_ExternalIdentities_Provider_Issuer_Subject",
                true,
                &["ProviderId", "Issuer", "Subject"]
            ),
            index("FerrofinIX_ExternalIdentities_UserId", false, &["UserId"]),
        ],
        "{label}: indexes"
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

/// Inserts a row into `table` with `given` columns, filling every other
/// `NOT NULL` column without a default with a placeholder of its type (as in
/// `user_cleanup.rs`).
async fn insert(pool: &SqlitePool, table: &str, given: &[(&str, &str)]) -> Result<(), sqlx::Error> {
    let columns: Vec<(String, String, bool, Option<String>, i64)> = sqlx::query_as(
        r#"SELECT "name", "type", "notnull", "dflt_value", "pk" FROM pragma_table_info(?1)"#,
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("table info");
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
        .map(|_| ())
}

/// With foreign keys on, as at runtime: one identity has one row, a link needs
/// its user, the issuer defaults to empty (LDAP's), and deleting the user
/// removes their links.
#[tokio::test]
async fn an_identity_is_unique_and_its_links_go_with_their_user() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let pool = db.pool();
    insert(pool, "Users", &[("Id", USER), ("Username", "alice")])
        .await
        .expect("user");
    let link = |user: &'static str, subject: &'static str| {
        [("UserId", user), ("ProviderId", LDAP), ("Subject", subject)]
    };

    insert(pool, TABLE, &link(USER, "alice-uid"))
        .await
        .expect("link");
    let issuer: String = sqlx::query_scalar(r#"SELECT "Issuer" FROM "FerrofinExternalIdentities""#)
        .fetch_one(pool)
        .await
        .expect("issuer");
    assert_eq!(issuer, "");

    let duplicate = insert(pool, TABLE, &link(USER, "alice-uid")).await;
    assert!(
        duplicate.is_err_and(|e| e.to_string().contains("UNIQUE")),
        "one identity, one row"
    );
    let orphan = insert(
        pool,
        TABLE,
        &link("CCCCCCCC-0000-0000-0000-00000000DEAD", "nobody"),
    )
    .await;
    assert!(
        orphan.is_err_and(|e| e.to_string().contains("FOREIGN KEY")),
        "a link needs its user"
    );
    insert(pool, TABLE, &link(USER, "alice-second-uid"))
        .await
        .expect("a second subject");

    sqlx::query(r#"DELETE FROM "Users" WHERE "Id" = ?1"#)
        .bind(USER)
        .execute(db.writer())
        .await
        .expect("delete user");
    let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinExternalIdentities""#)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(left, 0, "ON DELETE CASCADE removed the links");
}
