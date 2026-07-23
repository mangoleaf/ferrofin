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
//!
//! Real (Batch 11): [`Self::get_hls_playlist`] builds the `.m3u8` text purely
//! from the stored [`TrickplayInfoEntity`] row (a port of C# `GetHlsPlaylist`),
//! and [`Self::get_trickplay_tile_path`] resolves a tile's on-disk path from the
//! [`PathManager`] trickplay directory layout — both are metadata/path concerns
//! that do not require the deferred tiling.
//!
//! The manifest key mirrors C#: trickplay is keyed by *media-source id*, and the
//! primary media source of an item is the item's own id, so
//! [`Self::get_trickplay_manifest`] nests the resolutions under the item-id
//! string.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::playback::TrickplayInfoEntity;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::system::PathManager;
use hermit_traits::trickplay::TrickplayManager;

use crate::db_error::db_err;

/// The concrete (minimal) trickplay manager.
#[derive(Clone)]
pub struct HermitTrickplayManager {
    db: Database,
    path_manager: Arc<dyn PathManager>,
}

impl std::fmt::Debug for HermitTrickplayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitTrickplayManager")
            .finish_non_exhaustive()
    }
}

impl HermitTrickplayManager {
    /// Creates a trickplay manager over the given database and path manager.
    #[must_use]
    pub fn new(db: Database, path_manager: Arc<dyn PathManager>) -> Self {
        Self { db, path_manager }
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
        item_id: Uuid,
        width: i32,
        api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        // Port of C# `TrickplayManager.GetHlsPlaylist`: the playlist is derived
        // wholly from the stored `TrickplayInfo` row, so no on-disk tiles are
        // needed to emit it.
        let resolutions = self.resolutions_for(item_id).await?;
        let Some(info) = resolutions.get(&width) else {
            return Ok(None);
        };
        if info.thumbnail_count <= 0 {
            return Ok(None);
        }

        let resolution = format!("{}x{}", info.width, info.height);
        let layout = format!("{}x{}", info.tile_width, info.tile_height);
        let thumbnails_per_tile = info.tile_width * info.tile_height;
        // A malformed row with a zero tile grid would divide by zero; treat it as
        // "no playlist" rather than panicking.
        if thumbnails_per_tile <= 0 {
            return Ok(None);
        }
        let thumbnail_duration = f64::from(info.interval) / 1000.0;
        let tile_count = (info.thumbnail_count + thumbnails_per_tile - 1) / thumbnails_per_tile;
        let api_key = api_key.unwrap_or("");
        let item_id_dashless = item_id.simple().to_string();

        let mut out = String::with_capacity(128);
        out.push_str("#EXTM3U\n");
        let _ = writeln!(out, "#EXT-X-TARGETDURATION:{tile_count}");
        out.push_str("#EXT-X-VERSION:7\n");
        out.push_str("#EXT-X-MEDIA-SEQUENCE:1\n");
        out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        out.push_str("#EXT-X-IMAGES-ONLY\n");

        for i in 0..tile_count {
            // Every tile but the last carries a full grid of thumbnails.
            let per_tile = if i == tile_count - 1 {
                info.thumbnail_count - (i * thumbnails_per_tile)
            } else {
                thumbnails_per_tile
            };
            let inf_duration = thumbnail_duration * f64::from(per_tile);

            let _ = writeln!(out, "#EXTINF:{},", format_decimal(inf_duration));
            let _ = writeln!(
                out,
                "#EXT-X-TILES:RESOLUTION={resolution},LAYOUT={layout},DURATION={}",
                format_decimal(thumbnail_duration)
            );
            let _ = writeln!(
                out,
                "{i}.jpg?MediaSourceId={item_id_dashless}&ApiKey={api_key}"
            );
        }

        out.push_str("#EXT-X-ENDLIST\n");
        Ok(Some(out))
    }

    async fn get_trickplay_tile_path(
        &self,
        item_id: Uuid,
        width: i32,
        index: i32,
    ) -> Result<Option<String>, ServiceError> {
        // Port of C# `GetTrickplayTilePathAsync`: locate the resolution, then
        // build `{trickplay-dir}/{width} - {tw}x{th}/{index}.jpg`. The C#
        // `saveWithMedia` flag (a per-library option) is not modeled at this
        // seam, so the internal (non-save-with-media) directory is used; the
        // media path it ignores is passed empty.
        let resolutions = self.resolutions_for(item_id).await?;
        let Some(info) = resolutions.get(&width) else {
            return Ok(None);
        };
        let base = self.path_manager.trickplay_directory(item_id, "", false);
        let subdir = format!("{} - {}x{}", width, info.tile_width, info.tile_height);
        let path = std::path::Path::new(&base)
            .join(subdir)
            .join(format!("{index}.jpg"));
        Ok(Some(path.to_string_lossy().into_owned()))
    }
}

/// Formats a floating-point duration like C#'s `{0:0.###}` — up to three
/// fractional digits, trailing zeros (and a bare decimal point) trimmed.
fn format_decimal(value: f64) -> String {
    let mut s = format!("{value:.3}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use hermit_db::entities::playback::TrickplayInfoEntity;
    use hermit_model::data::BaseItemKind;
    use uuid::Uuid;

    use std::sync::Arc;

    use hermit_db::Database;
    use hermit_traits::trickplay::TrickplayManager;

    use crate::app_paths::test_paths;
    use crate::path_manager::HermitPathManager;
    use crate::test_support::{seed_item, test_db};

    use super::HermitTrickplayManager;

    /// Builds a trickplay manager plus a temp-dir path manager, returning the
    /// temp dir so its lifetime outlives the test.
    fn manager_with_paths(db: Database) -> (tempfile::TempDir, HermitTrickplayManager) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let pm = Arc::new(HermitPathManager::new(paths));
        (tmp, HermitTrickplayManager::new(db, pm))
    }

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
        let (_tmp, mgr) = manager_with_paths(db.clone());

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
    async fn refresh_is_deferred_and_hls_needs_a_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let (_tmp, mgr) = manager_with_paths(db);

        mgr.refresh_trickplay_data(item, true)
            .await
            .expect("refresh");
        // No stored row → no playlist and no tile path.
        assert!(
            mgr.get_hls_playlist(item, 320, None)
                .await
                .expect("hls")
                .is_none()
        );
        assert!(
            mgr.get_trickplay_tile_path(item, 320, 0)
                .await
                .expect("tile")
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

    #[tokio::test]
    async fn hls_playlist_and_tile_path_from_stored_row() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let (_tmp, mgr) = manager_with_paths(db.clone());

        // A 2x2 tile grid with 6 thumbnails → 2 tiles (4 + 2).
        let stored = TrickplayInfoEntity {
            item_id: item.to_string(),
            width: 320,
            bandwidth: 500_000,
            height: 180,
            interval: 10_000,
            thumbnail_count: 6,
            tile_height: 2,
            tile_width: 2,
        };
        mgr.save_trickplay_info(&stored).await.expect("save");

        let playlist = mgr
            .get_hls_playlist(item, 320, Some("KEY"))
            .await
            .expect("hls")
            .expect("some playlist");
        assert!(playlist.starts_with("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-IMAGES-ONLY"));
        assert!(playlist.contains("RESOLUTION=320x180"));
        assert!(playlist.contains("LAYOUT=2x2"));
        // Two tiles, addressed 0.jpg / 1.jpg with the dashless id + api key.
        assert!(playlist.contains(&format!("0.jpg?MediaSourceId={}&ApiKey=KEY", item.simple())));
        assert!(playlist.contains("1.jpg?MediaSourceId="));
        assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));

        // Tile path is `{trickplay}/{id}/{width} - {tw}x{th}/{index}.jpg`.
        let tile = mgr
            .get_trickplay_tile_path(item, 320, 1)
            .await
            .expect("tile")
            .expect("some path");
        assert!(
            tile.ends_with("320 - 2x2/1.jpg"),
            "unexpected tile path {tile}"
        );

        // Unknown resolution → no playlist / no tile path.
        assert!(
            mgr.get_hls_playlist(item, 999, None)
                .await
                .expect("hls")
                .is_none()
        );
        assert!(
            mgr.get_trickplay_tile_path(item, 999, 0)
                .await
                .expect("tile")
                .is_none()
        );
    }
}
