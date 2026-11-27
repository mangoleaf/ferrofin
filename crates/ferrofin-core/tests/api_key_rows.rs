//! What `create_api_key` stores in the `ApiKeys` table.
//!
//! The assertion reads the table, so it lives here rather than in the
//! manager's unit tests: `crates/ferrofin-db/tests/sql_boundary.rs` ratchets the
//! `sqlx::query` count of every file under `crates/*/src`.

use ferrofin_core::FerrofinApiKeyManager;
use ferrofin_db::Database;
use ferrofin_traits::security::ApiKeyManager;

/// `ApiKey(name)` sets only `Name`, `AccessToken` and `DateCreated`, so EF
/// stores `DateLastActivity` as `default(DateTime)`: `0001-01-01 00:00:00`, as
/// a live Jellyfin 12.4 does, never the creation time.
#[tokio::test]
async fn a_new_key_is_stored_never_used() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    FerrofinApiKeyManager::new(db.clone())
        .create_api_key(" padded ")
        .await
        .expect("create");

    let (name, created, last_activity): (String, String, String) =
        sqlx::query_as(r#"SELECT "Name", "DateCreated", "DateLastActivity" FROM "ApiKeys""#)
            .fetch_one(db.pool())
            .await
            .expect("key row");
    assert_eq!(name, " padded ", "the name is stored as sent");
    assert_eq!(last_activity, "0001-01-01 00:00:00");
    assert_ne!(created, last_activity);
}
