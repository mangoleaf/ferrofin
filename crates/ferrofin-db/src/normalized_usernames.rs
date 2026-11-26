//! Unicode username data backfill. Migration 0030 owns the schema; this module
//! only validates and updates values before requests can reach the database.

use crate::{DbError, Result};
use ferrofin_util::string_extensions::upper_invariant;
use sqlx::{Connection, SqliteConnection};

const MARKER: &str = "normalized_usernames_icu_v1";

fn validate_users(users: &[(String, String)]) -> Result<()> {
    let mut keys = std::collections::HashMap::new();
    for (id, name) in users {
        if let Some(first) = keys.insert(upper_invariant(name), (id.clone(), name.clone())) {
            return Err(DbError::UsernameCollision {
                first,
                second: (id.clone(), name.clone()),
            });
        }
    }
    Ok(())
}

/// Reject conflicting identities before applying SQL migrations to an existing
/// database. A fresh database has no Users table and needs no preflight.
///
/// # Errors
/// Returns a collision error naming both accounts, or a database read error.
pub async fn preflight(conn: &mut SqliteConnection) -> Result<()> {
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'Users'")
            .fetch_optional(&mut *conn)
            .await?;
    if exists.is_none() {
        return Ok(());
    }
    let users = sqlx::query_as(r#"SELECT "Id", "Username" FROM "Users" ORDER BY "Id""#)
        .fetch_all(&mut *conn)
        .await?;
    validate_users(&users)
}

/// Repairs normalized values in the schema established by migration 0030, and
/// bumps `RowVersion` where Jellyfin's `UpdateNormalizedUsername` would.
/// Data and completion marker change atomically; no schema objects are changed.
///
/// # Errors
/// Returns a collision error without rewriting values when two accounts have
/// the same final key, or a database error. Callers must stop startup on error.
pub async fn repair(conn: &mut SqliteConnection) -> Result<()> {
    let mut tx = conn.begin().await?;
    let done: Option<String> =
        sqlx::query_scalar(r#"SELECT "Value" FROM "FerrofinMeta" WHERE "Key" = ?1"#)
            .bind(MARKER)
            .fetch_optional(&mut *tx)
            .await?;
    if done.as_deref() == Some("1") {
        return Ok(());
    }
    let users: Vec<(String, String, String)> = sqlx::query_as(
        r#"SELECT "Id", "Username", "NormalizedUsername" FROM "Users" ORDER BY "Id""#,
    )
    .fetch_all(&mut *tx)
    .await?;
    validate_users(
        &users
            .iter()
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .collect::<Vec<_>>(),
    )?;

    // Jellyfin's `UpdateNormalizedUsername` sets every key and saves once, so
    // EF bumps `RowVersion` on each row whose key changes. Until the routine is
    // in `__EFMigrationsHistory` (a 10.11.8/10.11.9 or a Ferrofin-native
    // database), the key it replaces is `AddNormalizedUsername`'s '' default,
    // for which 0030 stores the `Username`.
    let routine_ran = crate::database::jellyfin_migration_applied(
        &mut tx,
        "20260522092304_UpdateNormalizedUsername",
    )
    .await?;
    let mut occupied = std::collections::HashSet::new();
    let mut changes = Vec::new();
    let mut bumped = Vec::new();
    for (id, name, stored) in users {
        let desired = upper_invariant(&name);
        let replaced = if routine_ran { stored.as_str() } else { "" };
        if desired != replaced {
            bumped.push(id.clone());
        }
        occupied.insert(stored.clone());
        occupied.insert(desired.clone());
        if desired != stored {
            changes.push((id, desired));
        }
    }
    // Stage changing rows under unused values so stale keys can be swapped
    // without dropping the unique index. Temporary values are never committed.
    let mut sequence = 0_u64;
    for (id, _) in &changes {
        let temporary = loop {
            let candidate = format!("ferrofin-normalization-{sequence}");
            sequence += 1;
            if occupied.insert(candidate.clone()) {
                break candidate;
            }
        };
        sqlx::query(r#"UPDATE "Users" SET "NormalizedUsername" = ?2 WHERE "Id" = ?1"#)
            .bind(id)
            .bind(temporary)
            .execute(&mut *tx)
            .await?;
    }
    for (id, desired) in &changes {
        sqlx::query(r#"UPDATE "Users" SET "NormalizedUsername" = ?2 WHERE "Id" = ?1"#)
            .bind(id)
            .bind(desired)
            .execute(&mut *tx)
            .await?;
    }
    for id in &bumped {
        sqlx::query(r#"UPDATE "Users" SET "RowVersion" = "RowVersion" + 1 WHERE "Id" = ?1"#)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')"#)
        .bind(MARKER)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// `has_column`: a database whose history records Jellyfin's
    /// `UpdateNormalizedUsername`, with stale keys; otherwise one `0030` just
    /// gave the column (a Ferrofin-native database: no Jellyfin history).
    async fn fixture(has_column: bool) -> SqliteConnection {
        let mut conn = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(
            r#"
            CREATE TABLE "FerrofinMeta" ("Key" TEXT PRIMARY KEY, "Value" TEXT NOT NULL);
            CREATE TABLE "Users" ("Id" TEXT PRIMARY KEY, "Username" TEXT UNIQUE,
                "Password" TEXT, "Permission" INTEGER, "RowVersion" INTEGER NOT NULL DEFAULT 0);
            INSERT INTO "Users" VALUES ('a', 'münchen', 'password-hash-a', 1, 4),
                ('b', 'ı', 'password-hash-b', 0, 0);
        "#,
        )
        .execute(&mut conn)
        .await
        .unwrap();
        if has_column {
            sqlx::raw_sql(
                r#"
                ALTER TABLE "Users" ADD COLUMN "NormalizedUsername" TEXT NOT NULL DEFAULT '';
                UPDATE "Users" SET "NormalizedUsername" = upper("Username");
                CREATE UNIQUE INDEX "IX_Users_NormalizedUsername" ON "Users" ("NormalizedUsername");
                CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT PRIMARY KEY,
                    "ProductVersion" TEXT NOT NULL);
                INSERT INTO "__EFMigrationsHistory"
                    VALUES ('20260522092304_UpdateNormalizedUsername', '10.11.11');
            "#,
            )
            .execute(&mut conn)
            .await
            .unwrap();
        } else {
            sqlx::raw_sql(include_str!("../migrations/0030_normalized_usernames.sql"))
                .execute(&mut conn)
                .await
                .unwrap();
        }
        conn
    }

    async fn row_versions(conn: &mut SqliteConnection) -> Vec<i64> {
        sqlx::query_scalar(r#"SELECT "RowVersion" FROM "Users" ORDER BY "Id""#)
            .fetch_all(conn)
            .await
            .unwrap()
    }

    /// The `RowVersion`s after the repair: every account where Jellyfin's
    /// routine has not run (each key it sets replaces ''), else only those
    /// whose key changed (`b`'s ASCII `upper()` key is already right).
    fn bumped(has_column: bool) -> Vec<i64> {
        if has_column { vec![5, 0] } else { vec![5, 1] }
    }

    #[tokio::test]
    async fn backfill_swaps_stale_keys_without_dropping_the_unique_index() {
        let mut conn = fixture(true).await;
        sqlx::query("UPDATE Users SET NormalizedUsername = 'temporary' WHERE Id = 'a'")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("UPDATE Users SET NormalizedUsername = 'MÜNCHEN' WHERE Id = 'b'")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("UPDATE Users SET NormalizedUsername = 'ı' WHERE Id = 'a'")
            .execute(&mut conn)
            .await
            .unwrap();
        let before: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        repair(&mut conn).await.unwrap();
        let after: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(before, after);
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT NormalizedUsername FROM Users ORDER BY Id")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(keys, ["MÜNCHEN", "ı"]);
        assert_eq!(row_versions(&mut conn).await, [5, 1], "both keys changed");
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn backfill_preserves_accounts_and_enforces_unicode_uniqueness(#[case] has_column: bool) {
        let mut conn = fixture(has_column).await;
        repair(&mut conn).await.unwrap();
        let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
            r#"SELECT "Id", "Username", "NormalizedUsername", "Password", "Permission" FROM "Users" ORDER BY "Id""#
        ).fetch_all(&mut conn).await.unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "a".into(),
                    "münchen".into(),
                    "MÜNCHEN".into(),
                    "password-hash-a".into(),
                    1
                ),
                (
                    "b".into(),
                    "ı".into(),
                    "ı".into(),
                    "password-hash-b".into(),
                    0
                ),
            ]
        );
        let duplicate = sqlx::query(r#"INSERT INTO "Users" ("Id", "Username", "NormalizedUsername") VALUES ('c', 'MÜNCHEN', 'MÜNCHEN')"#)
            .execute(&mut conn).await.unwrap_err();
        assert!(duplicate.as_database_error().unwrap().is_unique_violation());
        repair(&mut conn).await.unwrap();
        let markers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM FerrofinMeta")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(markers, 1);
        assert_eq!(
            row_versions(&mut conn).await,
            bumped(has_column),
            "bumped once"
        );
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn failed_backfill_rolls_back_values_and_completion(#[case] has_column: bool) {
        let mut conn = fixture(has_column).await;
        sqlx::query("CREATE TRIGGER fail_backfill BEFORE UPDATE ON Users BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
            .execute(&mut conn).await.unwrap();
        let before: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(repair(&mut conn).await.is_err());
        let after: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(before, after);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM FerrofinMeta")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(row_versions(&mut conn).await, [4, 0]);
        sqlx::query("DROP TRIGGER fail_backfill")
            .execute(&mut conn)
            .await
            .unwrap();
        // The retry still knows which keys the routine replaces.
        repair(&mut conn).await.unwrap();
        assert_eq!(row_versions(&mut conn).await, bumped(has_column));
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn collisions_leave_username_schema_values_and_marker_untouched(
        #[case] has_column: bool,
    ) {
        let mut conn = fixture(has_column).await;
        sqlx::query("UPDATE Users SET Username = 'MÜNCHEN' WHERE Id = 'b'")
            .execute(&mut conn)
            .await
            .unwrap();
        let before: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let error = repair(&mut conn).await.unwrap_err();
        assert!(matches!(error, DbError::UsernameCollision { .. }));
        let message = error.to_string();
        assert!(message.contains("münchen") && message.contains("MÜNCHEN"));
        let after: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(before, after);
        let markers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM FerrofinMeta")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(markers, 0);
        if has_column {
            let key: String =
                sqlx::query_scalar("SELECT NormalizedUsername FROM Users WHERE Id = 'a'")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(key, "MüNCHEN");
        }
        // After the operator resolves the conflict, migration retries normally.
        sqlx::query("UPDATE Users SET Username = 'different' WHERE Id = 'b'")
            .execute(&mut conn)
            .await
            .unwrap();
        repair(&mut conn).await.unwrap();
    }
}
