//! [`HermitTrickplayManager`] — a **minimal** [`TrickplayManager`] over the
//! `TrickplayInfos` table.
//!
//! Port of `Emby.Server.Implementations.Trickplay.TrickplayManager`. This unit-8
//! manager is thin: it stores, lists and deletes the trickplay *metadata* rows
//! ([`TrickplayInfoEntity`](hermit_db::entities::playback::TrickplayInfoEntity))
//! directly over `hermit-db` (there is no separate repository trait for this
//! leaf table).
//!
//! Deferred (documented, per the unit-8 minimal-manager rule):
//! - [`Self::refresh_trickplay_data`] — the actual image *tiling* (extracting
//!   frames, laying them into tile sheets, writing the on-disk directory
//!   layout) needs the un-ported `Video` domain object, an `IMediaEncoder`, and
//!   the trickplay directory service; here it is a no-op that leaves any
//!   existing rows untouched.
//! - [`Self::get_hls_playlist`] — building the `.m3u8` text from the stored
//!   tiles is likewise a tiling/HTTP concern and returns `None` for now.
//!
//! The manifest key mirrors C#: trickplay is keyed by *media-source id*, and the
//! primary media source of an item is the item's own id, so
//! [`Self::get_trickplay_manifest`] nests the resolutions under the item-id
//! string.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::playback::TrickplayInfoEntity;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::trickplay::TrickplayManager;

use crate::db_error::db_err;

/// The concrete (minimal) trickplay manager.
#[derive(Clone)]
pub struct HermitTrickplayManager {
    db: Database,
}

impl std::fmt::Debug for HermitTrickplayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitTrickplayManager")
            .finish_non_exhaustive()
    }
}

impl HermitTrickplayManager {
    /// Creates a trickplay manager over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Fetches all stored trickplay rows for an item, keyed by tile width.
    async fn resolutions_for(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, TrickplayInfoEntity>(
            r#"SELECT * FROM "TrickplayInfos" WHERE "ItemId" = ?1 ORDER BY "Width""#,
        )
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| (r.width, r)).collect())
    }
}

#[async_trait]
impl TrickplayManager for HermitTrickplayManager {
    async fn refresh_trickplay_data(
        &self,
        _item_id: Uuid,
        _replace: bool,
    ) -> Result<(), ServiceError> {
        // Tile generation is a deferred media-encoding concern (see module docs);
        // this leaves any existing metadata rows in place.
        Ok(())
    }

    async fn get_trickplay_resolutions(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
        self.resolutions_for(item_id).await
    }

    async fn get_trickplay_items(
        &self,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, TrickplayInfoEntity>(
            r#"SELECT * FROM "TrickplayInfos"
               ORDER BY "ItemId", "Width" LIMIT ?1 OFFSET ?2"#,
        )
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn save_trickplay_info(&self, info: &TrickplayInfoEntity) -> Result<(), ServiceError> {
        // Upsert on the (ItemId, Width) natural key so a re-scan replaces the row.
        sqlx::query(
            r#"INSERT INTO "TrickplayInfos"
               ("ItemId", "Width", "Bandwidth", "Height", "Interval",
                "ThumbnailCount", "TileHeight", "TileWidth")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT ("ItemId", "Width") DO UPDATE SET
                 "Bandwidth" = excluded."Bandwidth",
                 "Height" = excluded."Height",
                 "Interval" = excluded."Interval",
                 "ThumbnailCount" = excluded."ThumbnailCount",
                 "TileHeight" = excluded."TileHeight",
                 "TileWidth" = excluded."TileWidth""#,
        )
        .bind(&info.item_id)
        .bind(info.width)
        .bind(info.bandwidth)
        .bind(info.height)
        .bind(info.interval)
        .bind(info.thumbnail_count)
        .bind(info.tile_height)
        .bind(info.tile_width)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn delete_trickplay_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "TrickplayInfos" WHERE "ItemId" = ?1"#)
            .bind(item_id.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_trickplay_manifest(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
        let resolutions = self.resolutions_for(item_id).await?;
        let mut manifest = HashMap::new();
        if !resolutions.is_empty() {
            // The primary media source of an item is the item itself (its id).
            manifest.insert(item_id.to_string(), resolutions);
        }
        Ok(manifest)
    }

    async fn get_hls_playlist(
        &self,
        _item_id: Uuid,
        _width: i32,
        _api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        // The `.m3u8` text is built from the on-disk tiles (deferred tiling).
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use hermit_db::entities::playback::TrickplayInfoEntity;
    use hermit_model::data::BaseItemKind;
    use uuid::Uuid;

    use hermit_traits::trickplay::TrickplayManager;

    use crate::test_support::{seed_item, test_db};

    use super::HermitTrickplayManager;

    fn info(item: Uuid, width: i32) -> TrickplayInfoEntity {
        TrickplayInfoEntity {
            item_id: item.to_string(),
            width,
            bandwidth: 500_000,
            height: width * 9 / 16,
            interval: 10_000,
            thumbnail_count: 240,
            tile_height: 10,
            tile_width: 10,
        }
    }

    #[tokio::test]
    async fn save_upsert_query_and_delete() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let mgr = HermitTrickplayManager::new(db.clone());

        mgr.save_trickplay_info(&info(item, 320))
            .await
            .expect("save");
        mgr.save_trickplay_info(&info(item, 640))
            .await
            .expect("save2");

        let res = mgr.get_trickplay_resolutions(item).await.expect("res");
        assert_eq!(res.len(), 2);
        assert!(res.contains_key(&320));

        // Re-saving the same width upserts rather than duplicating.
        let mut bumped = info(item, 320);
        bumped.bandwidth = 999_999;
        mgr.save_trickplay_info(&bumped).await.expect("upsert");
        let res = mgr.get_trickplay_resolutions(item).await.expect("res2");
        assert_eq!(res.len(), 2);
        assert_eq!(res[&320].bandwidth, 999_999);

        // Manifest nests resolutions under the media-source (item) id.
        let manifest = mgr.get_trickplay_manifest(item).await.expect("manifest");
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[&item.to_string()].len(), 2);

        let page = mgr.get_trickplay_items(10, 0).await.expect("items");
        assert_eq!(page.len(), 2);

        mgr.delete_trickplay_data(item).await.expect("delete");
        assert!(
            mgr.get_trickplay_resolutions(item)
                .await
                .expect("res3")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn refresh_and_hls_are_deferred_noops() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let mgr = HermitTrickplayManager::new(db);

        mgr.refresh_trickplay_data(item, true)
            .await
            .expect("refresh");
        assert!(
            mgr.get_hls_playlist(item, 320, None)
                .await
                .expect("hls")
                .is_none()
        );
        // Refresh does not fabricate rows.
        assert!(
            mgr.get_trickplay_resolutions(item)
                .await
                .expect("res")
                .is_empty()
        );
    }
}
