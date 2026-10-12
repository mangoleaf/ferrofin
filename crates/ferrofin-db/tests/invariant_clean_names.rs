//! Upgrade the stored lookup keys without changing identities or references.
use ferrofin_db::Database;

#[tokio::test]
async fn upgrade_refreshes_both_join_keys_and_preserves_references() {
    let db = Database::connect_in_memory().await.unwrap();
    let all = sqlx::migrate!("./migrations");
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter().filter(|m| m.version <= 30).cloned().collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    previous.run(db.pool()).await.unwrap();
    for (id, name, clean) in [
        ("old", "ΟΣ", "ος"),
        ("correct", "ΟΣ", "οσ"),
        ("custom", "ΟΣ", "custom-key"),
    ] {
        sqlx::query(r#"INSERT INTO "BaseItems"
            ("Id", "Type", "Name", "CleanName", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
            VALUES (?1, 'MediaBrowser.Controller.Entities.Genre', ?2, ?3, 0, 0, 0, 0, 0, 0, 0)"#)
            .bind(id).bind(name).bind(clean).execute(db.writer()).await.unwrap();
    }
    sqlx::query(r#"INSERT INTO "ItemValues" ("ItemValueId", "Type", "Value", "CleanValue") VALUES ('value', 2, 'ΟΣ', 'ος')"#)
        .execute(db.writer()).await.unwrap();
    sqlx::query(r#"INSERT INTO "ItemValuesMap" ("ItemId", "ItemValueId") VALUES ('old', 'value')"#)
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = 'ος' WHERE "Id" = 'old'"#)
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = 'Custom Sort', "ForcedSortName" = 'ΟΣ' WHERE "Id" = 'custom'"#).execute(db.writer()).await.unwrap();
    sqlx::query(
        r#"UPDATE "BaseItems" SET "SortName" = 'ος', "ForcedSortName" = 'ΟΣ' WHERE "Id" = 'correct'"#,
    )
    .execute(db.writer())
    .await
    .unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(db.pool())
            .await
            .unwrap();
    for _ in 0..2 {
        db.run_migrations().await.unwrap();
        let rows: Vec<(String, String)> = sqlx::query_as(r#"SELECT "Id", "CleanName" FROM "BaseItems" WHERE "Id" IN ('old','correct','custom') ORDER BY "Id""#)
            .fetch_all(db.pool()).await.unwrap();
        assert_eq!(
            rows,
            vec![
                ("correct".into(), "οσ".into()),
                ("custom".into(), "custom-key".into()),
                ("old".into(), "οσ".into())
            ]
        );
        let sorts: Vec<(String, Option<String>)> = sqlx::query_as(r#"SELECT "SortName", "ForcedSortName" FROM "BaseItems" WHERE "Id" IN ('old','correct','custom') ORDER BY "Id""#)
            .fetch_all(db.pool()).await.unwrap();
        assert_eq!(
            sorts,
            vec![
                ("οσ".into(), Some("ΟΣ".into())),
                ("Custom Sort".into(), Some("ΟΣ".into())),
                ("οσ".into(), None)
            ]
        );
        let joined: (String, String) = sqlx::query_as(r#"SELECT b."Id", v."ItemValueId" FROM "BaseItems" b
            JOIN "ItemValuesMap" m ON m."ItemId" = b."Id"
            JOIN "ItemValues" v ON v."ItemValueId" = m."ItemValueId" AND v."CleanValue" = b."CleanName""#)
            .fetch_one(db.pool()).await.unwrap();
        assert_eq!(joined, ("old".into(), "value".into()));
        let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT version, checksum FROM _sqlx_migrations WHERE version <= 30 ORDER BY version",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(before, after);
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert!(violations.is_empty());
    }
}

/// Reproducible microbenchmark of the read-time casing cost, without HTTP or
/// media I/O. Run with `--ignored --nocapture`; use `--release` for deployment
/// estimates. It deliberately measures a scan (as the old LOWER query did).
#[tokio::test]
#[ignore = "query-cost measurement, not a timing assertion"]
async fn unicode_query_cost() {
    let db = Database::connect_in_memory().await.unwrap();
    sqlx::query("CREATE TABLE names (kind TEXT, name TEXT)")
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query("CREATE INDEX names_kind ON names(kind)")
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
        INSERT INTO names SELECT 'artist', CASE WHEN i % 10 = 0 THEN 'Élodie ' ELSE 'Artist ' END || i FROM n")
        .execute(db.writer()).await.unwrap();
    for (label, expression, parameter) in [
        ("SQLite ASCII lower", "lower(name)", "artist 12345"),
        (
            "ICU invariant upper",
            "ferrofin_upper_invariant(name)",
            "ARTIST 12345",
        ),
    ] {
        let query =
            format!("SELECT COUNT(*) FROM names WHERE kind = 'artist' AND {expression} = ?1");
        let mut samples = Vec::new();
        for _ in 0..21 {
            let start = std::time::Instant::now();
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
                .bind(parameter)
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(count, 1);
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        eprintln!(
            "{label}: median {:?} for 20,000 names (90% ASCII)",
            samples[10]
        );
    }
}
