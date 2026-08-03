//! [`HermitApiKeyManager`] — the concrete [`ApiKeyManager`] over `hermit-db`.
//!
//! Port of the API-key surface of
//! `Jellyfin.Server.Implementations.Security.AuthenticationManager`
//! (`GetApiKeys`/`CreateApiKey`/`DeleteApiKey`). Keys live in the `ApiKeys`
//! table; each is a name plus a generated access token.
//!
//! Port rules applied:
//! - `CreateApiKey` builds a C# `ApiKey(name)`, whose constructor sets
//!   `AccessToken = Guid.NewGuid().ToString("N")` (32 lowercase hex digits, no
//!   hyphens) and `DateCreated = DateTime.UtcNow`. This port mirrors that exactly
//!   ([`Uuid::new_v4().simple()`]), and also seeds `DateLastActivity` (a
//!   `NOT NULL` column the EF default leaves at `now`).
//! - `GetApiKeys` projects each row into an [`AuthenticationInfo`] with
//!   `AppName`/`AccessToken`/`DateCreated` from the key and empty device fields,
//!   matching the C# `Select`.
//! - `DeleteApiKey` deletes by access token; deleting an absent token is a no-op
//!   (the C# `ExecuteDeleteAsync` affects zero rows).

use async_trait::async_trait;
use chrono::Utc;
use hermit_db::Database;
use hermit_db::entities::security::ApiKeyEntity;
use uuid::Uuid;

use hermit_model::security::AuthenticationInfo;
use hermit_traits::error::ServiceError;
use hermit_traits::security::ApiKeyManager;

use crate::db_error::db_err;

/// The concrete API-key manager over the `ApiKeys` table.
#[derive(Clone)]
pub struct HermitApiKeyManager {
    db: Database,
}

impl std::fmt::Debug for HermitApiKeyManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitApiKeyManager")
            .finish_non_exhaustive()
    }
}

impl HermitApiKeyManager {
    /// Creates an API-key manager over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// Projects a stored [`ApiKeyEntity`] into an [`AuthenticationInfo`].
///
/// Mirrors the C# `GetApiKeys` `Select`: the key's `Name`/`AccessToken`/
/// `DateCreated` populate `AppName`/`AccessToken`/`DateCreated`; the device
/// fields are empty strings (never `null`, matching the C# projection).
fn to_authentication_info(key: ApiKeyEntity) -> AuthenticationInfo {
    AuthenticationInfo {
        app_name: Some(key.name),
        access_token: Some(key.access_token),
        date_created: key.date_created,
        device_id: Some(String::new()),
        device_name: Some(String::new()),
        app_version: Some(String::new()),
        ..Default::default()
    }
}

#[async_trait]
impl ApiKeyManager for HermitApiKeyManager {
    async fn get_api_keys(&self) -> Result<Vec<AuthenticationInfo>, ServiceError> {
        let keys: Vec<ApiKeyEntity> = sqlx::query_as(r#"SELECT * FROM "ApiKeys" ORDER BY "Id""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(keys.into_iter().map(to_authentication_info).collect())
    }

    async fn create_api_key(&self, name: &str) -> Result<(), ServiceError> {
        // C# `ApiKey(name)`: token is a hyphen-free lowercase-hex GUID; created +
        // last-activity both stamped now.
        let access_token = Uuid::new_v4().simple().to_string();
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("AccessToken", "DateCreated", "DateLastActivity", "Name")
                VALUES (?1, ?2, ?2, ?3)"#,
        )
        .bind(&access_token)
        .bind(now)
        .bind(name)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn delete_api_key(&self, access_token: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "ApiKeys" WHERE "AccessToken" = ?1"#)
            .bind(access_token)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    #[tokio::test]
    async fn create_then_list_reflects_new_key() {
        let db = test_db().await;
        let mgr = HermitApiKeyManager::new(db);

        mgr.create_api_key("my-cli").await.expect("create");
        let keys = mgr.get_api_keys().await.expect("list");
        assert_eq!(keys.len(), 1);
        let key = &keys[0];
        assert_eq!(key.app_name.as_deref(), Some("my-cli"));
        // Token is a 32-char hyphen-free hex GUID.
        let token = key.access_token.clone().expect("token");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        // Device fields are empty, not absent, per the C# projection.
        assert_eq!(key.device_id.as_deref(), Some(""));
        assert_eq!(key.device_name.as_deref(), Some(""));
        assert_eq!(key.app_version.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn delete_removes_only_the_matching_key() {
        let db = test_db().await;
        let mgr = HermitApiKeyManager::new(db);
        mgr.create_api_key("a").await.expect("create a");
        mgr.create_api_key("b").await.expect("create b");

        let keys = mgr.get_api_keys().await.expect("list");
        assert_eq!(keys.len(), 2);
        let token_a = keys[0].access_token.clone().expect("token a");

        mgr.delete_api_key(&token_a).await.expect("delete");
        let remaining = mgr.get_api_keys().await.expect("list after delete");
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].access_token, Some(token_a));
    }

    #[tokio::test]
    async fn delete_absent_token_is_a_noop() {
        let db = test_db().await;
        let mgr = HermitApiKeyManager::new(db);
        mgr.create_api_key("a").await.expect("create");
        // Deleting a non-existent token affects zero rows and does not error.
        mgr.delete_api_key("does-not-exist").await.expect("delete");
        assert_eq!(mgr.get_api_keys().await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn tokens_are_unique_across_keys() {
        let db = test_db().await;
        let mgr = HermitApiKeyManager::new(db);
        mgr.create_api_key("a").await.expect("a");
        mgr.create_api_key("b").await.expect("b");
        let keys = mgr.get_api_keys().await.expect("list");
        assert_ne!(keys[0].access_token, keys[1].access_token);
    }
}
