//! Migration 0054 clears the resume positions Ferrofin builds before gl-46 saved for
//! items Jellyfin never lets resume (`BaseItem.SupportsPositionTicksResume`): a song,
//! and a video extra of type Sample, ThemeVideo or Trailer. Every other field of those
//! rows, and the position of every item that resumes, stay as stored.

use std::borrow::Cow;

use ferrofin_db::Database;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

const USER: &str = "CCCCCCCC-0000-0000-0000-000000000001";
const POSITION: i64 = 2_000_000_000;

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

/// Inserts an item of `type_` with `extra_type`, and one user-data row per `keys`
/// holding a resume position.
async fn item_in_progress(
    pool: &SqlitePool,
    id: &str,
    type_: &str,
    extra_type: Option<i64>,
    keys: &[&str],
) {
    sqlx::query(
        r#"INSERT INTO "BaseItems" ("Id", "Type", "Name", "ExtraType", "IsFolder",
               "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, ?2, ?1, ?3, 0, 0, 0, 0, 0, 0, 0)"#,
    )
    .bind(id)
    .bind(type_)
    .bind(extra_type)
    .execute(pool)
    .await
    .expect("insert item");
    for key in keys {
        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
                   "LastPlayedDate", "PlayCount", "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?3, 1, '2026-01-06 12:00:00', 2, ?4, 0)"#,
        )
        .bind(id)
        .bind(USER)
        .bind(key)
        .bind(POSITION)
        .execute(pool)
        .await
        .expect("insert user data");
    }
}

#[tokio::test]
async fn unresumable_items_lose_their_stored_position() {
    // `Database` registers the SQL functions earlier migrations call.
    let db = Database::connect_in_memory().await.expect("database");
    let pool = db.pool().clone();
    through(53)
        .run(&pool)
        .await
        .expect("migrations through 0053");
    sqlx::query(
        r#"INSERT INTO "Users" ("Id","AuthenticationProviderId","DisplayCollectionsView",
            "DisplayMissingEpisodes","EnableAutoLogin","EnableLocalPassword",
            "EnableNextEpisodeAutoPlay","EnableUserPreferenceAccess","HidePlayedInLatest",
            "InternalId","InvalidLoginAttemptCount","MaxActiveSessions","MustUpdatePassword",
            "PasswordResetProviderId","PlayDefaultAudioTrack","RememberAudioSelections",
            "RememberSubtitleSelections","RowVersion","SubtitleMode","SyncPlayAccess","Username")
           VALUES (?1,'auth',0,0,0,0,1,1,1,1,0,0,0,'reset',1,1,1,0,0,0,'listener')"#,
    )
    .bind(USER)
    .execute(&pool)
    .await
    .expect("user");
    // (id, type, ExtraType, position after 0054). ExtraType: 2 Trailer, 3
    // BehindTheScenes, 7 Sample, 8 ThemeSong, 9 ThemeVideo.
    let items = [
        ("A1", "Audio.Audio", None, 0),
        ("A2", "Audio.Audio", Some(8), 0),
        ("V1", "Video", Some(2), 0),
        ("V2", "Video", Some(7), 0),
        ("V3", "Video", Some(9), 0),
        ("V4", "Trailer", Some(2), 0),
        ("V5", "Movies.Movie", Some(9), 0),
        ("K1", "Movies.Movie", None, POSITION),
        ("K2", "TV.Episode", None, POSITION),
        ("K3", "MusicVideo", None, POSITION),
        ("K4", "Trailer", None, POSITION),
        ("K5", "Video", Some(3), POSITION),
        ("K6", "AudioBook", None, POSITION),
        ("K7", "Book", None, POSITION),
    ];
    for (id, type_, extra_type, _) in items {
        let type_ = format!("MediaBrowser.Controller.Entities.{type_}");
        // A song's provider-keyed row is cleared with its own.
        let keys: &[&str] = if id == "A1" {
            &[id, "mbz-track"]
        } else {
            &[id]
        };
        item_in_progress(&pool, id, &type_, extra_type, keys).await;
    }

    through(54).run(&pool).await.expect("migration 0054");

    for (id, type_, _, expected) in items {
        let rows: Vec<(String, i64, i64, i64, i64, Option<String>)> = sqlx::query_as(
            r#"SELECT "CustomDataKey", "PlaybackPositionTicks", "Played", "PlayCount",
                      "IsFavorite", "LastPlayedDate"
               FROM "UserData" WHERE "ItemId" = ?1"#,
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .expect("rows");
        assert!(!rows.is_empty(), "{type_} {id}: rows kept");
        for (key, position, played, play_count, favorite, last_played) in rows {
            assert_eq!(position, expected, "{type_} {id} {key}: position");
            // Only the position changes.
            assert_eq!(
                (played, play_count, favorite, last_played.as_deref()),
                (0, 2, 1, Some("2026-01-06 12:00:00")),
                "{type_} {id} {key}: the other fields"
            );
        }
    }
}
