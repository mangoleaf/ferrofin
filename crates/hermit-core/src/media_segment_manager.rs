//! [`HermitMediaSegmentManager`] — a **minimal** [`MediaSegmentManager`] over the
//! `MediaSegments` table.
//!
//! Port of `Emby.Server.Implementations.MediaSegments.MediaSegmentManager`. This
//! unit-8 manager is thin: it stores, queries and deletes the typed time-span
//! rows (`MediaSegments`) directly over `hermit-db` (there is no separate
//! repository trait for this leaf table), resolving an item's kind through the
//! injected [`LibraryManager`](hermit_traits::library::LibraryManager) to answer
//! [`MediaSegmentManager::is_type_supported`]. Rows cross the boundary as the
//! [`MediaSegmentDto`] wire type, reusing the `hermit-db`
//! [`MediaSegmentEntity`](hermit_db::entities::playback::MediaSegmentEntity) →
//! DTO conversion.
//!
//! Deferred (documented): the plugin **provider** fan-out — `GetSegments` with
//! `filter_by_provider`, `GetSupportedProviders`, and per-library provider
//! enablement — needs the un-ported `IMediaSegmentProvider` plugin registry and
//! `LibraryOptions`. Here `filter_by_provider` is accepted but not narrowed
//! (all stored segments are returned) and [`Self::get_supported_providers`]
//! returns an empty list. Segment *generation* is out of scope for this seam.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::playback::MediaSegmentEntity;
use hermit_db::store::guid_to_db;
use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::is_video;

/// The concrete (minimal) media-segment manager.
#[derive(Clone)]
pub struct HermitMediaSegmentManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
}

impl std::fmt::Debug for HermitMediaSegmentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitMediaSegmentManager")
            .finish_non_exhaustive()
    }
}

impl HermitMediaSegmentManager {
    /// Creates a media-segment manager over the database and library seam.
    #[must_use]
    pub fn new(db: Database, library_manager: Arc<dyn LibraryManager>) -> Self {
        Self {
            db,
            library_manager,
        }
    }

    /// The stored `INTEGER` discriminant for a [`MediaSegmentType`], matching the
    /// C# declaration order mirrored by the `hermit-db` → DTO conversion.
    fn type_discriminant(kind: MediaSegmentType) -> i32 {
        match kind {
            MediaSegmentType::Unknown => 0,
            MediaSegmentType::Commercial => 1,
            MediaSegmentType::Preview => 2,
            MediaSegmentType::Recap => 3,
            MediaSegmentType::Outro => 4,
            MediaSegmentType::Intro => 5,
        }
    }

    /// Maps a stored row onto the wire DTO, surfacing a malformed row as a
    /// backend error rather than silently dropping it.
    fn to_dto(entity: MediaSegmentEntity) -> Result<MediaSegmentDto, ServiceError> {
        MediaSegmentDto::try_from(entity).map_err(|e| ServiceError::backend(e.to_string()))
    }
}

#[async_trait]
impl MediaSegmentManager for HermitMediaSegmentManager {
    async fn is_type_supported(&self, item_id: Uuid) -> Result<bool, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(false);
        };
        // Segments attach to playable `Video` items (mirrors the C# type gate).
        Ok(kind_from_type_name(&item.type_).is_some_and(is_video))
    }

    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        segment_provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        let id = if segment.id.is_nil() {
            Uuid::new_v4()
        } else {
            segment.id
        };
        sqlx::query(
            r#"INSERT INTO "MediaSegments"
               ("Id", "EndTicks", "ItemId", "SegmentProviderId", "StartTicks", "Type")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        )
        .bind(guid_to_db(id))
        .bind(segment.end_ticks)
        .bind(guid_to_db(segment.item_id))
        .bind(segment_provider_id)
        .bind(segment.start_ticks)
        .bind(Self::type_discriminant(segment.type_))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;

        Ok(MediaSegmentDto {
            id,
            ..segment.clone()
        })
    }

    async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "MediaSegments" WHERE "Id" = ?1"#)
            .bind(guid_to_db(segment_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn delete_segments(&self, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "MediaSegments" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn delete_provider_segments(
        &self,
        item_id: Uuid,
        provider_id: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let mut sql = String::from(
            r#"DELETE FROM "MediaSegments" WHERE "ItemId" = ?1 AND "SegmentProviderId" = ?2"#,
        );
        if type_filter.is_some() {
            sql.push_str(r#" AND "Type" = ?3"#);
        }
        let mut query = sqlx::query(&sql)
            .bind(guid_to_db(item_id))
            .bind(provider_id.to_owned());
        if let Some(kind) = type_filter {
            query = query.bind(Self::type_discriminant(kind));
        }
        query.execute(self.db.writer()).await.map_err(db_err)?;
        Ok(())
    }

    async fn delete_all_provider_segments(
        &self,
        provider_id: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let mut sql = String::from(r#"DELETE FROM "MediaSegments" WHERE "SegmentProviderId" = ?1"#);
        if type_filter.is_some() {
            sql.push_str(r#" AND "Type" = ?2"#);
        }
        let mut query = sqlx::query(&sql).bind(provider_id.to_owned());
        if let Some(kind) = type_filter {
            query = query.bind(Self::type_discriminant(kind));
        }
        query.execute(self.db.writer()).await.map_err(db_err)?;
        Ok(())
    }

    async fn get_segments(
        &self,
        item_id: Uuid,
        type_filter: Option<&[MediaSegmentType]>,
        _filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        // `filter_by_provider` (per-library provider enablement) is a documented
        // deferral; all stored segments for the item are returned and then
        // narrowed only by the optional type filter.
        let rows = sqlx::query_as::<_, MediaSegmentEntity>(
            r#"SELECT * FROM "MediaSegments" WHERE "ItemId" = ?1 ORDER BY "StartTicks""#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let dto = Self::to_dto(row)?;
            if let Some(types) = type_filter
                && !types.is_empty()
                && !types.contains(&dto.type_)
            {
                continue;
            }
            out.push(dto);
        }
        Ok(out)
    }

    async fn has_segments(&self, item_id: Uuid) -> Result<bool, ServiceError> {
        let count: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "MediaSegments" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(count > 0)
    }

    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
        // Provider registry is an un-ported plugin subsystem (documented
        // deferral); no providers are advertised.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use hermit_model::data::BaseItemKind;
    use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use uuid::Uuid;

    use hermit_traits::media_segments::MediaSegmentManager;

    use crate::test_support::{library_manager_over, seed_item, test_db};

    use super::HermitMediaSegmentManager;

    fn segment(item: Uuid, kind: MediaSegmentType, start: i64, end: i64) -> MediaSegmentDto {
        MediaSegmentDto {
            id: Uuid::nil(),
            item_id: item,
            type_: kind,
            start_ticks: start,
            end_ticks: end,
        }
    }

    #[tokio::test]
    async fn create_query_and_delete_round_trip() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let mgr = HermitMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));

        assert!(!mgr.has_segments(item).await.expect("empty"));

        let created = mgr
            .create_segment(&segment(item, MediaSegmentType::Intro, 0, 100), "prov")
            .await
            .expect("create");
        assert!(!created.id.is_nil());

        mgr.create_segment(&segment(item, MediaSegmentType::Outro, 900, 1000), "prov")
            .await
            .expect("create outro");

        assert!(mgr.has_segments(item).await.expect("has"));

        let all = mgr.get_segments(item, None, false).await.expect("all");
        assert_eq!(all.len(), 2);
        // Ordered by start ticks.
        assert_eq!(all[0].type_, MediaSegmentType::Intro);

        // Type filter narrows to the requested kind.
        let outros = mgr
            .get_segments(item, Some(&[MediaSegmentType::Outro]), false)
            .await
            .expect("outros");
        assert_eq!(outros.len(), 1);
        assert_eq!(outros[0].type_, MediaSegmentType::Outro);

        mgr.delete_segment(created.id).await.expect("delete one");
        assert_eq!(
            mgr.get_segments(item, None, false)
                .await
                .expect("after")
                .len(),
            1
        );

        mgr.delete_segments(item).await.expect("delete all");
        assert!(!mgr.has_segments(item).await.expect("empty again"));
    }

    #[tokio::test]
    async fn type_support_and_no_providers() {
        let db = test_db().await;
        let episode = Uuid::new_v4();
        let series = Uuid::new_v4();
        seed_item(&db, episode, BaseItemKind::Episode).await;
        seed_item(&db, series, BaseItemKind::Series).await;
        let mgr = HermitMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));

        assert!(mgr.is_type_supported(episode).await.expect("episode"));
        assert!(!mgr.is_type_supported(series).await.expect("series"));
        assert!(
            mgr.get_supported_providers(episode)
                .await
                .expect("providers")
                .is_empty()
        );
    }
}
