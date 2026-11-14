//! Migration 0050 rewrites the `DateTime` text Ferrofin stored into the form
//! Jellyfin's EF provider writes (`yyyy-MM-dd HH:mm:ss.FFFFFFF`: no trailing
//! fraction zeros, no `.` for a whole second), and leaves Jellyfin's own text,
//! and anything that is not a date in one of Ferrofin's forms, as it is. Every
//! column gets the same expression; `ApiKeys`' two date columns stand for them.

use std::borrow::Cow;

use sqlx::migrate::Migrator;

/// Each stored value, and what 0050 leaves in its place.
const CASES: &[(&str, &str)] = &[
    // datetime_to_db's seven digits, trailing zeros trimmed.
    ("2020-01-01 00:00:00.0000000", "2020-01-01 00:00:00"),
    ("2026-11-11 00:53:27.3326680", "2026-11-11 00:53:27.332668"),
    ("2026-11-11 00:53:20.5000000", "2026-11-11 00:53:20.5"),
    // 0007's strftime: three digits, zeros kept.
    ("2024-05-01 10:00:00.120", "2024-05-01 10:00:00.12"),
    // sqlx's RFC 3339 for a raw DateTime: 0, 3, 6 or 9 digits, cut to the tick.
    (
        "2026-11-11T00:54:23.994092718+00:00",
        "2026-11-11 00:54:23.9940927",
    ),
    ("2026-11-11T00:54:23.500+00:00", "2026-11-11 00:54:23.5"),
    ("2026-11-11T00:54:20+00:00", "2026-11-11 00:54:20"),
    // Already Jellyfin's form, or not one of Ferrofin's: untouched.
    ("2026-11-11 00:53:27.4371883", "2026-11-11 00:53:27.4371883"),
    ("2020-01-01 00:00:00", "2020-01-01 00:00:00"),
    ("0001-01-01 00:00:00", "0001-01-01 00:00:00"),
    ("2026-11-11T00:54:23+02:00", "2026-11-11T00:54:23+02:00"),
];

#[tokio::test]
async fn ferrofins_datetime_text_takes_jellyfins_form() {
    // `Database` registers the SQL functions earlier migrations call.
    let db = ferrofin_db::Database::connect_in_memory()
        .await
        .expect("database");
    let pool = db.pool().clone();
    let full = sqlx::migrate!("./migrations");
    let before = Migrator {
        migrations: Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < 50)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate!("./migrations")
    };
    before.run(&pool).await.expect("migrations through 0049");

    for (id, (stored, _)) in CASES.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("Id", "AccessToken", "DateCreated", "DateLastActivity", "Name")
               VALUES (?1, ?2, ?3, ?3, 'key')"#,
        )
        .bind(i64::try_from(id).expect("small"))
        .bind(format!("token-{id}"))
        .bind(stored)
        .execute(&pool)
        .await
        .expect("seed key");
    }

    full.run(&pool).await.expect("migration 0050");
    let rows: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT "DateCreated", "DateLastActivity" FROM "ApiKeys" ORDER BY "Id""#)
            .fetch_all(&pool)
            .await
            .expect("keys");
    let expected: Vec<(String, String)> = CASES
        .iter()
        .map(|(_, after)| ((*after).to_owned(), (*after).to_owned()))
        .collect();
    assert_eq!(rows, expected);

    // A second run finds nothing to change.
    let again = sqlx::raw_sql(include_str!("../migrations/0050_datetime_text_ef_form.sql"))
        .execute(&pool)
        .await
        .expect("rerun");
    assert_eq!(again.rows_affected(), 0);
}

/// Every date column 0050 rewrites: Jellyfin's 21 `DateTime` columns and
/// Ferrofin's own date columns.
const COLUMNS: &[(&str, &[&str])] = &[
    ("ActivityLogs", &["DateCreated"]),
    ("ApiKeys", &["DateCreated", "DateLastActivity"]),
    ("BaseItemImageInfos", &["DateModified"]),
    (
        "BaseItems",
        &[
            "DateCreated",
            "DateLastMediaAdded",
            "DateLastRefreshed",
            "DateLastSaved",
            "DateModified",
            "EndDate",
            "PremiereDate",
            "StartDate",
        ],
    ),
    ("Chapters", &["ImageDateModified"]),
    (
        "Devices",
        &["DateCreated", "DateModified", "DateLastActivity"],
    ),
    ("ImageInfos", &["LastModified"]),
    ("UserData", &["LastPlayedDate", "RetentionDate"]),
    ("Users", &["LastActivityDate", "LastLoginDate"]),
    ("FerrofinExternalIdentities", &["DateCreated"]),
    ("FerrofinLiveTvChannels", &["DateCreated"]),
    (
        "FerrofinLiveTvPrograms",
        &["StartDate", "EndDate", "DateCreated"],
    ),
    (
        "FerrofinLiveTvRecordings",
        &["StartDate", "EndDate", "DateCreated"],
    ),
    ("FerrofinLiveTvTimers", &["StartDate", "EndDate"]),
    (
        "FerrofinPlaybackSessions",
        &["DecidedAt", "StartedAt", "StoppedAt"],
    ),
    ("FerrofinPluginOfferedItems", &["OfferedAt"]),
    (
        "FerrofinUserDataRetentionSnapshots",
        &["LastPlayedDate", "RetentionDate"],
    ),
];

/// The text between `start` and the next `end` in `text`, after `start`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).unwrap_or_else(|| panic!("no {start:?}")) + start.len();
    let to = text[from..]
        .find(end)
        .unwrap_or_else(|| panic!("no {end:?} after {start:?}"));
    &text[from..from + to]
}

/// The behaviour test above proves `ApiKeys.DateCreated`'s expression; every
/// other column must carry the same one, under its own name, in its table's
/// SET and in its WHERE, so a slip in any column's text fails here.
#[test]
fn every_date_column_gets_the_tested_expression() {
    const SQL: &str = include_str!("../migrations/0050_datetime_text_ef_form.sql");
    let statement = |table: &str| between(SQL, &format!("UPDATE \"{table}\"\n"), ";\n");
    let template = between(
        statement("ApiKeys"),
        "\"DateCreated\" = ",
        ",\n       \"DateLastActivity\"",
    );
    let space_form = between(template, "CASE WHEN ", " THEN rtrim(rtrim(");
    let rfc3339 = between(template, "\n            WHEN ", " THEN CASE WHEN");
    let named =
        |text: &str, column: &str| text.replace("\"DateCreated\"", &format!("\"{column}\""));

    for (table, columns) in COLUMNS {
        let statement = statement(table);
        let filter = &statement[statement.find("\n WHERE ").expect("a WHERE")..];
        let mut joins = filter.replacen("\n WHERE ", "", 1);
        for column in *columns {
            let set = format!("\"{column}\" = {}", named(template, column));
            assert!(statement.contains(&set), "{table}.{column}: SET");
            for condition in [named(space_form, column), named(rfc3339, column)] {
                assert!(joins.contains(&condition), "{table}.{column}: WHERE");
                joins = joins.replacen(&condition, "", 1);
            }
        }
        // Only OR joins the conditions, so a row needing any rewrite is selected.
        assert!(
            joins.split_whitespace().all(|word| word == "OR"),
            "{table}: WHERE joins {joins:?}"
        );
    }
    let columns: usize = COLUMNS.iter().map(|(_, columns)| columns.len()).sum();
    assert_eq!(
        SQL.matches(" = CASE WHEN ").count(),
        columns,
        "no column outside the list"
    );
    assert_eq!(
        SQL.matches("UPDATE \"").count(),
        COLUMNS.len(),
        "no table outside the list"
    );
}
