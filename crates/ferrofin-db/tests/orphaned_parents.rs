//! Migration 0042 removes orphaned parent chains — items whose `ParentId` names a
//! missing item, everything under them and every extra they own — the way Jellyfin
//! 12.1+'s `ChangeOwnerIdToGuid` does, keeping their play state on the placeholder
//! item. Ferrofin migrations run with foreign keys OFF, so nothing cascades: the
//! migration clears every dependent table by hand, and these tests hold it to that.

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use ferrofin_db::{Database, JELLYFIN_GENERATIONS, PLACEHOLDER_ITEM_ID};
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqlitePool};

const CLEANUP_SQL: &str = include_str!("../migrations/0042_orphaned_parent_cleanup.sql");
const SCHEMA_10_11_8: &str = include_str!("data/jellyfin-10.11.8-schema.sql");
const SCHEMA_12_2: &str = include_str!("data/jellyfin-12.2-schema.sql");

/// Every `(table, column)` with a foreign key to `BaseItems`.
const DEPENDENTS: &str = r#"SELECT m.name, f."from"
    FROM sqlite_master m, pragma_foreign_key_list(m.name) f
    WHERE m.type = 'table' AND f."table" = 'BaseItems' ORDER BY 1, 2"#;

// Jellyfin stores GUIDs uppercase.
const MISSING: &str = "AAAAAAAA-0000-0000-0000-00000000DEAD";
const A: &str = "AAAAAAAA-0000-0000-0000-00000000000A";
const B: &str = "AAAAAAAA-0000-0000-0000-00000000000B";
const C: &str = "AAAAAAAA-0000-0000-0000-00000000000C";
const ROOT: &str = "BBBBBBBB-0000-0000-0000-000000000001";
const KEPT: &str = "BBBBBBBB-0000-0000-0000-000000000002";
const USER: &str = "CCCCCCCC-0000-0000-0000-000000000001";

/// A file-backed database shaped like a real Jellyfin one: a committed schema
/// fixture plus its EF migration rows (`database.rs`'s private
/// `seed_jellyfin_fixture_with`, with foreign keys off so the fixture can hold the
/// dangling `ParentId`s a long-lived Jellyfin library can carry).
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

/// Inserts a `BaseItems` row with only the columns every schema requires.
async fn item<'c, E>(conn: E, id: &str, parent: Option<&str>, owner: Option<&str>)
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO "BaseItems" ("Id", "Type", "Name", "ParentId", "OwnerId", "IsFolder",
               "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, 'MediaBrowser.Controller.Entities.Movies.Movie', ?1, ?2, ?3,
                   0, 0, 0, 0, 0, 0, 0)"#,
    )
    .bind(id)
    .bind(parent)
    .bind(owner)
    .execute(conn)
    .await
    .expect("insert item");
}

/// Inserts a played `UserData` row.
async fn user_data<'c, E>(conn: E, item: &str, key: &str, play_count: i64)
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
               "PlayCount", "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?3, 0, ?4, 0, 1)"#,
    )
    .bind(item)
    .bind(USER)
    .bind(key)
    .bind(play_count)
    .execute(conn)
    .await
    .expect("insert user data");
}

async fn ids(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" ORDER BY "Id""#)
        .fetch_all(pool)
        .await
        .expect("items")
}

async fn foreign_key_violations(pool: &SqlitePool) -> usize {
    sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .expect("foreign_key_check")
        .len()
}

/// A dangling parent chain in an adopted Jellyfin database is removed, with its
/// owned extra and dependent rows, its play state moves to the placeholder item,
/// and the boot's `foreign_key_check` passes. Without 0042 that check refuses the
/// database.
#[rstest::rstest]
#[case::jellyfin_10_11_8("10.11.8", SCHEMA_10_11_8)]
#[case::jellyfin_12_2("12.2.0", SCHEMA_12_2)]
#[tokio::test]
async fn adoption_removes_orphaned_parent_chains(#[case] generation: &str, #[case] schema: &str) {
    let generation = JELLYFIN_GENERATIONS
        .iter()
        .find(|g| g.name == generation)
        .expect("generation");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("jellyfin.db");
    let mut history = generation.migration_ids.to_vec();
    history.extend(generation.additional_ids);
    seed_jellyfin_fixture(&path, schema, &history).await;
    let is_12 = generation.name.starts_with("12.");
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(false),
            )
            .await
            .expect("open fixture");
        // Jellyfin's placeholder item (20250609115616_DetachUserDataInsteadOfDelete).
        sqlx::query(
            r#"INSERT INTO "BaseItems" ("Id", "Type", "Name", "IsFolder", "IsInMixedFolder",
                   "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
               VALUES (?1, 'PLACEHOLDER',
                   'This is a placeholder item for UserData that has been detacted from its original item',
                   0, 0, 0, 0, 0, 0, 0)"#,
        )
        .bind(PLACEHOLDER_ITEM_ID)
        .execute(&pool)
        .await
        .expect("placeholder");
        sqlx::query(
            r#"INSERT INTO "Users" ("Id","AuthenticationProviderId","DisplayCollectionsView",
                "DisplayMissingEpisodes","EnableAutoLogin","EnableLocalPassword",
                "EnableNextEpisodeAutoPlay","EnableUserPreferenceAccess","HidePlayedInLatest",
                "InternalId","InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
                "PasswordResetProviderId","PlayDefaultAudioTrack","RememberAudioSelections",
                "RememberSubtitleSelections","RowVersion","SubtitleMode","SyncPlayAccess","Username")
               VALUES (?1,'auth',0,0,0,0,1,1,1,1,0,0,0,'reset',1,1,1,0,0,0,'viewer')"#,
        )
        .bind(USER)
        .execute(&pool)
        .await
        .expect("user");
        // A's parent is gone; B is A's child; C is an extra owned by B.
        item(&pool, A, Some(MISSING), None).await;
        item(&pool, B, Some(A), None).await;
        item(&pool, C, None, Some(B)).await;
        // A healthy library next to it.
        item(&pool, ROOT, None, None).await;
        item(&pool, KEPT, Some(ROOT), None).await;
        // B's own history, a key A and B share (the dedupe keeps one), a key an
        // older detached row already holds on the placeholder (the incoming one
        // replaces it), and the healthy item's history.
        user_data(&pool, B, "b-key", 3).await;
        user_data(&pool, A, "shared", 1).await;
        user_data(&pool, B, "shared", 2).await;
        user_data(&pool, PLACEHOLDER_ITEM_ID, "b-key", 99).await;
        user_data(&pool, KEPT, "kept-key", 5).await;
        for (owner, index) in [(A, 0), (KEPT, 0)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos" ("ItemId", "StreamIndex", "IsDefault",
                       "IsExternal", "IsForced", "StreamType") VALUES (?1, ?2, 0, 0, 0, 1)"#,
            )
            .bind(owner)
            .bind(index)
            .execute(&pool)
            .await
            .expect("stream");
        }
        for (child, ancestor) in [(B, A), (KEPT, ROOT)] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(child)
                .bind(ancestor)
                .execute(&pool)
                .await
                .expect("ancestor");
        }
        if is_12 {
            // A box set (the healthy root) linking to B: the link goes with B.
            sqlx::query(
                r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
                   VALUES (?1, 0, ?2, 0), (?1, 1, ?3, 0)"#,
            )
            .bind(ROOT)
            .bind(B)
            .bind(KEPT)
            .execute(&pool)
            .await
            .expect("linked children");
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

    assert_eq!(
        ids(pool).await,
        [PLACEHOLDER_ITEM_ID, ROOT, KEPT],
        "A, B and the extra C are gone; the healthy items stay"
    );
    assert_eq!(foreign_key_violations(pool).await, 0);

    let rows: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
        r#"SELECT "ItemId", "CustomDataKey", "PlayCount", "RetentionDate" FROM "UserData"
           ORDER BY "CustomDataKey""#,
    )
    .fetch_all(pool)
    .await
    .expect("user data");
    let (placeholder, kept): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|(item, ..)| item == PLACEHOLDER_ITEM_ID);
    assert_eq!(
        kept,
        [(KEPT.to_owned(), "kept-key".to_owned(), 5, None)],
        "the healthy item's history is untouched"
    );
    let detached: Vec<(&str, i64)> = placeholder
        .iter()
        .map(|(_, key, count, _)| (key.as_str(), *count))
        .collect();
    // B's row replaced the older detached copy; of the two "shared" rows the
    // first inserted (A's, MIN(rowid)) is kept, as upstream.
    assert_eq!(detached, [("b-key", 3), ("shared", 1)]);
    for (_, key, _, retention) in &placeholder {
        let retention = retention.as_deref().expect("RetentionDate is set");
        // Ferrofin's stored format (`store::datetime_to_db`): 7 fractional digits.
        assert_eq!(
            retention.len(),
            "2026-10-12 00:00:00.0000000".len(),
            "{key}"
        );
        chrono::NaiveDateTime::parse_from_str(retention, "%Y-%m-%d %H:%M:%S%.f")
            .expect("RetentionDate parses");
    }

    let streams: Vec<String> = sqlx::query_scalar(r#"SELECT "ItemId" FROM "MediaStreamInfos""#)
        .fetch_all(pool)
        .await
        .expect("streams");
    assert_eq!(streams, [KEPT], "no MediaStreamInfos for A");
    let ancestors: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT "ItemId", "ParentItemId" FROM "AncestorIds""#)
            .fetch_all(pool)
            .await
            .expect("ancestors");
    assert_eq!(ancestors, [(KEPT.to_owned(), ROOT.to_owned())]);
    if is_12 {
        let links: Vec<String> = sqlx::query_scalar(r#"SELECT "ChildId" FROM "LinkedChildren""#)
            .fetch_all(pool)
            .await
            .expect("links");
        assert_eq!(links, [KEPT]);
    }
}

/// A fresh database migrated up to (not including) 0042, on one connection with
/// foreign keys off — the boot's migration connection.
async fn migrated_before_cleanup() -> (Database, SqlitePool) {
    // `Database` registers the SQL functions earlier migrations call.
    let db = Database::connect_in_memory().await.expect("database");
    let pool = db.pool().clone();
    let full = sqlx::migrate!("./migrations");
    let before = Migrator {
        migrations: Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < 42)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate!("./migrations")
    };
    before.run(&pool).await.expect("migrations through 0041");
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&pool)
        .await
        .expect("foreign keys off");
    (db, pool)
}

/// Runs 0042 itself, as the migrator would.
async fn run_cleanup(pool: &SqlitePool) {
    let through = Migrator {
        migrations: Cow::Owned(
            sqlx::migrate!("./migrations")
                .migrations
                .iter()
                .filter(|m| m.version <= 42)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate!("./migrations")
    };
    tokio::time::timeout(Duration::from_secs(30), through.run(pool))
        .await
        .expect("0042 terminates")
        .expect("migration 0042");
}

/// The intro-skipper tables key rows by item id without a foreign key, so
/// nothing but 0042 itself clears them for an orphan; both id spellings go.
#[tokio::test]
async fn orphan_cleanup_clears_intro_skipper_rows() {
    let (_db, pool) = migrated_before_cleanup().await;
    // A season whose series is gone, and its episode.
    item(&pool, A, Some(MISSING), None).await;
    item(&pool, B, Some(A), None).await;
    item(&pool, KEPT, None, None).await;
    let episode = B.to_lowercase();
    let season = A.to_lowercase();
    let kept = KEPT.to_lowercase();
    for (item_id, kind) in [(episode.as_str(), 0), (B, 1), (kept.as_str(), 0)] {
        sqlx::query(
            r#"INSERT INTO "FerrofinIntroSkipperSegments" ("ItemId", "Type", "Start", "End")
               VALUES (?1, ?2, 0, 30)"#,
        )
        .bind(item_id)
        .bind(kind)
        .execute(&pool)
        .await
        .expect("segment");
    }
    for (season_id, episode_id) in [
        (season.as_str(), episode.as_str()),
        (A, kept.as_str()),
        (kept.as_str(), episode.as_str()),
        (kept.as_str(), kept.as_str()),
    ] {
        sqlx::query(
            r#"INSERT INTO "FerrofinIntroSkipperDisabledEpisodes" ("SeasonId", "EpisodeId")
               VALUES (?1, ?2)"#,
        )
        .bind(season_id)
        .bind(episode_id)
        .execute(&pool)
        .await
        .expect("disabled episode");
    }
    for season_id in [season.as_str(), A, kept.as_str()] {
        sqlx::query(
            r#"INSERT INTO "FerrofinIntroSkipperSeasonStates" ("SeasonId", "Type")
               VALUES (?1, 0)"#,
        )
        .bind(season_id)
        .execute(&pool)
        .await
        .expect("season state");
    }

    run_cleanup(&pool).await;

    let left = ids(&pool).await;
    assert!(!left.iter().any(|id| id == A || id == B), "{left:?}");
    assert!(left.iter().any(|id| id == KEPT));
    let segments: Vec<String> =
        sqlx::query_scalar(r#"SELECT "ItemId" FROM "FerrofinIntroSkipperSegments""#)
            .fetch_all(&pool)
            .await
            .expect("segments");
    assert_eq!(segments, std::slice::from_ref(&kept));
    let disabled: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT "SeasonId", "EpisodeId" FROM "FerrofinIntroSkipperDisabledEpisodes""#,
    )
    .fetch_all(&pool)
    .await
    .expect("disabled episodes");
    assert_eq!(disabled, [(kept.clone(), kept.clone())]);
    let states: Vec<String> =
        sqlx::query_scalar(r#"SELECT "SeasonId" FROM "FerrofinIntroSkipperSeasonStates""#)
            .fetch_all(&pool)
            .await
            .expect("season states");
    assert_eq!(states, [kept]);
    assert_eq!(foreign_key_violations(&pool).await, 0);
}

/// A `ParentId` cycle cannot loop the recursive walk: `UNION` queues each row
/// once. A cycle reached from an orphan (here through ownership) goes with it;
/// a cycle with no dangling parent has no starting row and stays, as upstream
/// leaves it — it violates no foreign key.
#[tokio::test]
async fn orphan_cleanup_terminates_on_parent_cycles() {
    let (_db, pool) = migrated_before_cleanup().await;
    let (p1, p2) = (
        "DDDDDDDD-0000-0000-0000-000000000001",
        "DDDDDDDD-0000-0000-0000-000000000002",
    );
    // A dangles; B and C point at each other, and B is owned by A.
    item(&pool, A, Some(MISSING), None).await;
    item(&pool, B, Some(C), Some(A)).await;
    item(&pool, C, Some(B), None).await;
    // A closed two-item cycle with no root.
    item(&pool, p1, Some(p2), None).await;
    item(&pool, p2, Some(p1), None).await;

    run_cleanup(&pool).await;

    assert_eq!(ids(&pool).await, [PLACEHOLDER_ITEM_ID, p1, p2]);
    assert_eq!(foreign_key_violations(&pool).await, 0);
}

/// Every foreign key to `BaseItems` has its explicit delete in 0042 (nothing
/// cascades with foreign keys off), so a table that gains one later fails here
/// instead of at a user's boot.
#[tokio::test]
async fn orphan_cleanup_covers_every_foreign_key_to_baseitems() {
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let dependents: Vec<(String, String)> = sqlx::query_as(DEPENDENTS)
        .fetch_all(db.pool())
        .await
        .expect("dependents");
    for known in [("UserData", "ItemId"), ("FerrofinPlaylists", "PlaylistId")] {
        assert!(
            dependents.contains(&(known.0.to_owned(), known.1.to_owned())),
            "the query sees the schema: {dependents:?}"
        );
    }
    // Handled by upstream's own statements: the UserData move, the LinkedChildren
    // delete, and the BaseItems delete (the orphan set is closed under ParentId
    // and OwnerId).
    let handled = ["UserData", "LinkedChildren", "BaseItems"];
    let missing: Vec<String> = dependents
        .iter()
        .filter(|(table, _)| !handled.contains(&table.as_str()))
        .filter(|(table, column)| {
            !CLEANUP_SQL.contains(&format!(r#"DELETE FROM "{table}" WHERE "{column}" IN"#))
        })
        .map(|(table, column)| format!("{table}.{column}"))
        .collect();
    assert!(
        missing.is_empty(),
        "0042 has no explicit delete for {missing:?}"
    );
    for (table, column) in &dependents {
        let handled_by = match table.as_str() {
            // The orphan set is closed under BaseItems' own keys: the recursive
            // walk follows each of them, then one delete takes the whole set.
            "BaseItems" => format!(r#"."{column}" = Orphan."Id""#),
            "UserData" | "LinkedChildren" => {
                format!(r#""{column}" IN (SELECT "Id" FROM "OrphanedBaseItemIds")"#)
            }
            _ => continue,
        };
        assert!(
            CLEANUP_SQL.contains(&handled_by),
            "{table}.{column} is not handled: no `{handled_by}`"
        );
    }
    assert!(CLEANUP_SQL.contains(
        r#"DELETE FROM "BaseItems" WHERE "Id" IN (SELECT "Id" FROM "OrphanedBaseItemIds")"#
    ));
}
