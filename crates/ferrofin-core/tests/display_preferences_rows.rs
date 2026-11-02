//! What `get_item_display_preferences` leaves behind in the table.
//!
//! The assertions read the table, so they live here rather than in the
//! manager's unit tests: `crates/ferrofin-db/tests/sql_boundary.rs` ratchets the
//! `sqlx::query` count of every file under `crates/*/src`, and those reads
//! would have to be raw SQL.

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
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'prefs', 'PREFS')"#,
    )
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("insert user");
}

/// The `ItemDisplayPreferences` rows of `user`, as `(ItemId, SortBy)`.
async fn rows(db: &Database, user: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        r#"SELECT "ItemId", "SortBy" FROM "ItemDisplayPreferences"
           WHERE "UserId" = ?1 ORDER BY "Id""#,
    )
    .bind(guid_to_db(user))
    .fetch_all(db.pool())
    .await
    .expect("rows")
}

/// A manager over a fresh database with one user.
async fn manager() -> (Database, Uuid, FerrofinDisplayPreferencesManager) {
    let db = test_db().await;
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let mgr = FerrofinDisplayPreferencesManager::new(db.clone());
    (db, user, mgr)
}

// v12.2 `DisplayPreferencesManagerTests.GetItemDisplayPreferences_NewItem_StoresThePreferencesUnderThatItem`.
#[tokio::test]
async fn a_new_items_preferences_are_stored_under_that_item() {
    let (db, user, mgr) = manager().await;
    let item = Uuid::new_v4();

    let prefs = mgr
        .get_item_display_preferences(user, item, "client")
        .await
        .expect("get");

    assert_eq!(prefs.item_id, guid_to_db(item));
    assert_eq!(
        rows(&db, user).await,
        [(guid_to_db(item), "SortName".to_owned())]
    );
}

// v12.2 `DisplayPreferencesManagerTests.GetItemDisplayPreferences_SameItemTwice_ReturnsTheStoredPreferences`.
#[tokio::test]
async fn the_same_item_twice_returns_the_stored_preferences() {
    let (db, user, mgr) = manager().await;
    let item = Uuid::new_v4();

    let first = mgr
        .get_item_display_preferences(user, item, "client")
        .await
        .expect("first");
    let second = mgr
        .get_item_display_preferences(user, item, "client")
        .await
        .expect("second");

    assert_eq!(first.id, second.id);
    assert_eq!(rows(&db, user).await.len(), 1);
}

// `POST /DisplayPreferences/{A}` saves A's sort settings; B keeps its own
// (the defaults), as upstream keeps every item's row apart.
#[tokio::test]
async fn an_item_update_leaves_other_items_alone() {
    let (_db, user, mgr) = manager().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

    let mut prefs = mgr
        .get_item_display_preferences(user, a, "client")
        .await
        .expect("a");
    prefs.sort_by = "DateCreated".to_owned();
    mgr.update_item_display_preferences(&prefs)
        .await
        .expect("update a");

    let a_again = mgr
        .get_item_display_preferences(user, a, "client")
        .await
        .expect("a again");
    let b_prefs = mgr
        .get_item_display_preferences(user, b, "client")
        .await
        .expect("b");
    assert_eq!(a_again.sort_by, "DateCreated");
    assert_eq!(b_prefs.sort_by, "SortName");
}

// An empty-GUID row an older Ferrofin (or Jellyfin 10.11's GET) stored is
// left where it is: no item reads it, and the list leaves it out.
#[tokio::test]
async fn an_empty_guid_row_is_left_alone() {
    let (db, user, mgr) = manager().await;
    sqlx::query(
        r#"INSERT INTO "ItemDisplayPreferences"
           ("Client", "ItemId", "RememberIndexing", "RememberSorting", "SortBy",
            "SortOrder", "UserId", "ViewType")
           VALUES ('client', ?1, 0, 0, 'DateCreated', 0, ?2, 0)"#,
    )
    .bind(guid_to_db(Uuid::nil()))
    .bind(guid_to_db(user))
    .execute(db.writer())
    .await
    .expect("seed empty-GUID row");
    let item = Uuid::new_v4();

    let prefs = mgr
        .get_item_display_preferences(user, item, "client")
        .await
        .expect("get");
    let listed = mgr
        .list_item_display_preferences(user, "client")
        .await
        .expect("list");

    assert_eq!(prefs.sort_by, "SortName");
    assert_eq!(
        listed.iter().map(|p| p.item_id.clone()).collect::<Vec<_>>(),
        [guid_to_db(item)]
    );
    assert_eq!(
        rows(&db, user).await,
        [
            (guid_to_db(Uuid::nil()), "DateCreated".to_owned()),
            (guid_to_db(item), "SortName".to_owned()),
        ]
    );
}
