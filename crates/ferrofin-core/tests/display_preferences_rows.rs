//! What `get_item_display_preferences` leaves behind in the table.
//!
//! The assertion is a **row count**, so it lives here rather than in the
//! manager's unit tests: `crates/ferrofin-db/tests/sql_boundary.rs` ratchets the
//! `sqlx::query` count of every file under `crates/*/src`, and the counting
//! query would have to be raw SQL.

use ferrofin_core::FerrofinDisplayPreferencesManager;
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_traits::configuration::DisplayPreferencesManager;
use uuid::Uuid;

/// An in-memory database with the schema applied.
async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    db
}

/// Inserts the minimal `Users` row the preferences foreign key needs.
async fn seed_user(db: &Database, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'prefs')"#,
    )
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("insert user");
}

/// Rows in `ItemDisplayPreferences` for a user.
async fn row_count(db: &Database, user: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM "ItemDisplayPreferences" WHERE "UserId" = ?1"#,
    )
    .bind(guid_to_db(user))
    .fetch_one(db.pool())
    .await
    .expect("count")
}

// The default row is stored under the EMPTY item id, so a lookup for a real item
// id can never match it. Upstream therefore inserts *another* default row on
// every call — and jellyfin-web calls `/DisplayPreferences/{id}` (GET and POST)
// on every page load, so the table grows without bound and every one of those
// requests pays a write. Ferrofin reuses the existing default row instead.
#[tokio::test]
async fn repeated_item_prefs_lookups_reuse_one_default_row() {
    let db = test_db().await;
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let mgr = FerrofinDisplayPreferencesManager::new(db.clone());

    let first = mgr
        .get_item_display_preferences(user, Uuid::new_v4(), "web")
        .await
        .expect("first");
    assert_eq!(row_count(&db, user).await, 1);

    // Same client, different item ids, repeatedly: still one row, and the same
    // row comes back every time.
    for _ in 0..5 {
        let again = mgr
            .get_item_display_preferences(user, Uuid::new_v4(), "web")
            .await
            .expect("again");
        assert_eq!(again.id, first.id);
        assert_eq!(again.item_id, first.item_id);
    }
    assert_eq!(row_count(&db, user).await, 1);

    // A different client still gets its own default row — the row is keyed by
    // client, which is how the caller addresses it.
    mgr.get_item_display_preferences(user, Uuid::new_v4(), "emby")
        .await
        .expect("other client");
    assert_eq!(row_count(&db, user).await, 2);
}
