//! [`HermitDisplayPreferencesManager`] — the concrete
//! [`DisplayPreferencesManager`] over `hermit-db`.
//!
//! Port of `Jellyfin.Server.Implementations.Users.DisplayPreferencesManager`.
//! The C# manager runs raw EF Core queries against four tables; those become
//! `sqlx` queries here over the matching `hermit-db` entities:
//! `DisplayPreferences`, `ItemDisplayPreferences`, `CustomItemDisplayPreferences`
//! (the `HomeSection` children of a display-preferences row are loaded by the
//! DTO layer, not here).
//!
//! Port departures:
//! - EF's `.Include(HomeSections)` eager-load is dropped: the trait returns the
//!   flat [`DisplayPreferencesEntity`] row (home sections are a separate concern).
//! - The C# getters *create and persist* a default row when none exists
//!   (`Add` + `SaveChanges`). That is reproduced: a missing row is inserted with
//!   the Jellyfin default column values and re-read, so the returned entity
//!   always carries a real surrogate `Id`.
//! - `UpdateDisplayPreferences`/`UpdateItemDisplayPreferences` attach a modified
//!   entity in C#. Here they `UPDATE` the row addressed by
//!   `(UserId, ItemId, Client)` (an `upsert` is unnecessary — the getters
//!   guarantee the row exists before a client edits it).
//! - `Guid` identity arguments are [`Uuid`]; they are bound as their hyphenated
//!   string form to match the `TEXT` columns, consistent with the rest of the
//!   crate.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use hermit_traits::configuration::DisplayPreferencesManager;
use hermit_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;

/// The concrete display-preferences manager.
#[derive(Clone)]
pub struct HermitDisplayPreferencesManager {
    db: Database,
}

impl std::fmt::Debug for HermitDisplayPreferencesManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitDisplayPreferencesManager")
            .finish_non_exhaustive()
    }
}

impl HermitDisplayPreferencesManager {
    /// Creates a display-preferences manager over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl DisplayPreferencesManager for HermitDisplayPreferencesManager {
    async fn get_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError> {
        let existing = sqlx::query_as::<_, DisplayPreferencesEntity>(
            r#"SELECT * FROM "DisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(item_id.to_string())
        .bind(client)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        if let Some(row) = existing {
            return Ok(row);
        }

        // No row: insert the Jellyfin defaults (mirrors the C#
        // `new DisplayPreferences(userId, itemId, client)` constructor defaults)
        // and re-read so the caller sees the assigned surrogate id.
        sqlx::query(
            r#"INSERT INTO "DisplayPreferences"
               ("ChromecastVersion", "Client", "DashboardTheme",
                "EnableNextVideoInfoOverlay", "IndexBy", "ItemId",
                "ScrollDirection", "ShowBackdrop", "ShowSidebar",
                "SkipBackwardLength", "SkipForwardLength", "TvHome", "UserId")
               VALUES (?1, ?2, NULL, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)"#,
        )
        .bind(DEFAULT_CHROMECAST_VERSION)
        .bind(client)
        .bind(i64::from(DEFAULT_ENABLE_NEXT_VIDEO_INFO_OVERLAY))
        .bind(item_id.to_string())
        .bind(DEFAULT_SCROLL_DIRECTION)
        .bind(i64::from(DEFAULT_SHOW_BACKDROP))
        .bind(i64::from(DEFAULT_SHOW_SIDEBAR))
        .bind(DEFAULT_SKIP_BACKWARD_LENGTH)
        .bind(DEFAULT_SKIP_FORWARD_LENGTH)
        .bind(user_id.to_string())
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;

        sqlx::query_as::<_, DisplayPreferencesEntity>(
            r#"SELECT * FROM "DisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(item_id.to_string())
        .bind(client)
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)
    }

    async fn get_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError> {
        let existing = sqlx::query_as::<_, ItemDisplayPreferencesEntity>(
            r#"SELECT * FROM "ItemDisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(item_id.to_string())
        .bind(client)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        if let Some(row) = existing {
            return Ok(row);
        }

        // C# inserts `new ItemDisplayPreferences(userId, Guid.Empty, client)`:
        // note the item id is the *empty* GUID for the freshly created row, not
        // the queried `itemId`. That quirk is preserved so a subsequent
        // `ListItemDisplayPreferences` (which filters out empty item ids) behaves
        // identically to Jellyfin.
        sqlx::query(
            r#"INSERT INTO "ItemDisplayPreferences"
               ("Client", "IndexBy", "ItemId", "RememberIndexing",
                "RememberSorting", "SortBy", "SortOrder", "UserId", "ViewType")
               VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
        )
        .bind(client)
        .bind(Uuid::nil().to_string())
        .bind(i64::from(DEFAULT_REMEMBER_INDEXING))
        .bind(i64::from(DEFAULT_REMEMBER_SORTING))
        .bind(DEFAULT_SORT_BY)
        .bind(DEFAULT_SORT_ORDER)
        .bind(user_id.to_string())
        .bind(DEFAULT_VIEW_TYPE)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;

        sqlx::query_as::<_, ItemDisplayPreferencesEntity>(
            r#"SELECT * FROM "ItemDisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(Uuid::nil().to_string())
        .bind(client)
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)
    }

    async fn list_item_display_preferences(
        &self,
        user_id: Uuid,
        client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError> {
        // Excludes the empty-item-id rows created by `get_item_display_preferences`
        // (C# `!prefs.ItemId.Equals(default)`).
        sqlx::query_as::<_, ItemDisplayPreferencesEntity>(
            r#"SELECT * FROM "ItemDisplayPreferences"
               WHERE "UserId" = ?1 AND "Client" = ?2 AND "ItemId" <> ?3"#,
        )
        .bind(user_id.to_string())
        .bind(client)
        .bind(Uuid::nil().to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)
    }

    async fn list_custom_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<HashMap<String, Option<String>>, ServiceError> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            r#"SELECT "Key", "Value" FROM "CustomItemDisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(item_id.to_string())
        .bind(client)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(rows.into_iter().collect())
    }

    async fn set_custom_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
        custom_preferences: &HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError> {
        // C#: delete-all-then-insert inside one SaveChanges. Reproduced in a
        // transaction so the replacement is atomic.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"DELETE FROM "CustomItemDisplayPreferences"
               WHERE "UserId" = ?1 AND "ItemId" = ?2 AND "Client" = ?3"#,
        )
        .bind(user_id.to_string())
        .bind(item_id.to_string())
        .bind(client)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        for (key, value) in custom_preferences {
            sqlx::query(
                r#"INSERT INTO "CustomItemDisplayPreferences"
                   ("Client", "ItemId", "Key", "UserId", "Value")
                   VALUES (?1, ?2, ?3, ?4, ?5)"#,
            )
            .bind(client)
            .bind(item_id.to_string())
            .bind(key)
            .bind(user_id.to_string())
            .bind(value.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)
    }

    async fn update_display_preferences(
        &self,
        display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "DisplayPreferences" SET
                 "ChromecastVersion" = ?1, "DashboardTheme" = ?2,
                 "EnableNextVideoInfoOverlay" = ?3, "IndexBy" = ?4,
                 "ScrollDirection" = ?5, "ShowBackdrop" = ?6, "ShowSidebar" = ?7,
                 "SkipBackwardLength" = ?8, "SkipForwardLength" = ?9, "TvHome" = ?10
               WHERE "UserId" = ?11 AND "ItemId" = ?12 AND "Client" = ?13"#,
        )
        .bind(display_preferences.chromecast_version)
        .bind(display_preferences.dashboard_theme.as_deref())
        .bind(i64::from(
            display_preferences.enable_next_video_info_overlay,
        ))
        .bind(display_preferences.index_by)
        .bind(display_preferences.scroll_direction)
        .bind(i64::from(display_preferences.show_backdrop))
        .bind(i64::from(display_preferences.show_sidebar))
        .bind(display_preferences.skip_backward_length)
        .bind(display_preferences.skip_forward_length)
        .bind(display_preferences.tv_home.as_deref())
        .bind(&display_preferences.user_id)
        .bind(&display_preferences.item_id)
        .bind(&display_preferences.client)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn update_item_display_preferences(
        &self,
        item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "ItemDisplayPreferences" SET
                 "IndexBy" = ?1, "RememberIndexing" = ?2, "RememberSorting" = ?3,
                 "SortBy" = ?4, "SortOrder" = ?5, "ViewType" = ?6
               WHERE "UserId" = ?7 AND "ItemId" = ?8 AND "Client" = ?9"#,
        )
        .bind(item_display_preferences.index_by)
        .bind(i64::from(item_display_preferences.remember_indexing))
        .bind(i64::from(item_display_preferences.remember_sorting))
        .bind(&item_display_preferences.sort_by)
        .bind(item_display_preferences.sort_order)
        .bind(item_display_preferences.view_type)
        .bind(&item_display_preferences.user_id)
        .bind(&item_display_preferences.item_id)
        .bind(&item_display_preferences.client)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

// Default column values for a freshly created preferences row, matching the C#
// `DisplayPreferences`/`ItemDisplayPreferences` entity constructors.
const DEFAULT_CHROMECAST_VERSION: i32 = 1;
const DEFAULT_ENABLE_NEXT_VIDEO_INFO_OVERLAY: bool = true;
const DEFAULT_SCROLL_DIRECTION: i32 = 0;
const DEFAULT_SHOW_BACKDROP: bool = true;
const DEFAULT_SHOW_SIDEBAR: bool = false;
const DEFAULT_SKIP_BACKWARD_LENGTH: i32 = 10_000;
const DEFAULT_SKIP_FORWARD_LENGTH: i32 = 30_000;
const DEFAULT_REMEMBER_INDEXING: bool = false;
const DEFAULT_REMEMBER_SORTING: bool = false;
const DEFAULT_SORT_BY: &str = "SortName";
const DEFAULT_SORT_ORDER: i32 = 0;
const DEFAULT_VIEW_TYPE: i32 = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_user, test_db};

    #[tokio::test]
    async fn get_creates_and_persists_default_row() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let item = Uuid::new_v4();
        let mgr = HermitDisplayPreferencesManager::new(db);

        let row = mgr
            .get_display_preferences(user, item, "web")
            .await
            .expect("get");
        assert!(row.id > 0, "surrogate id assigned");
        assert_eq!(row.chromecast_version, 1);
        assert!(row.enable_next_video_info_overlay);
        assert_eq!(row.skip_forward_length, 30_000);

        // Second call returns the same persisted row, not a new one.
        let again = mgr
            .get_display_preferences(user, item, "web")
            .await
            .expect("get again");
        assert_eq!(row.id, again.id);
    }

    #[tokio::test]
    async fn update_display_preferences_round_trips() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let item = Uuid::new_v4();
        let mgr = HermitDisplayPreferencesManager::new(db);

        let mut row = mgr
            .get_display_preferences(user, item, "web")
            .await
            .expect("get");
        row.dashboard_theme = Some("dark".to_owned());
        row.show_sidebar = true;
        row.skip_forward_length = 15_000;
        mgr.update_display_preferences(&row).await.expect("update");

        let reread = mgr
            .get_display_preferences(user, item, "web")
            .await
            .expect("reread");
        assert_eq!(reread.dashboard_theme.as_deref(), Some("dark"));
        assert!(reread.show_sidebar);
        assert_eq!(reread.skip_forward_length, 15_000);
    }

    #[tokio::test]
    async fn item_prefs_created_with_empty_item_id_excluded_from_list() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let mgr = HermitDisplayPreferencesManager::new(db);

        // Creating item prefs stores an empty item id (Jellyfin quirk).
        let created = mgr
            .get_item_display_preferences(user, Uuid::new_v4(), "web")
            .await
            .expect("get item prefs");
        assert_eq!(created.item_id, Uuid::nil().to_string());

        // The list filters out the empty-item-id placeholder row.
        let listed = mgr
            .list_item_display_preferences(user, "web")
            .await
            .expect("list");
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn custom_prefs_replace_atomically() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let item = Uuid::new_v4();
        let mgr = HermitDisplayPreferencesManager::new(db);

        let mut prefs = HashMap::new();
        prefs.insert("a".to_owned(), Some("1".to_owned()));
        prefs.insert("b".to_owned(), None);
        mgr.set_custom_item_display_preferences(user, item, "web", &prefs)
            .await
            .expect("set");

        let got = mgr
            .list_custom_item_display_preferences(user, item, "web")
            .await
            .expect("list");
        assert_eq!(got.get("a"), Some(&Some("1".to_owned())));
        assert_eq!(got.get("b"), Some(&None));

        // Replacing wipes the previous set.
        let mut next = HashMap::new();
        next.insert("c".to_owned(), Some("3".to_owned()));
        mgr.set_custom_item_display_preferences(user, item, "web", &next)
            .await
            .expect("replace");
        let got = mgr
            .list_custom_item_display_preferences(user, item, "web")
            .await
            .expect("list2");
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("c"), Some(&Some("3".to_owned())));
    }
}
