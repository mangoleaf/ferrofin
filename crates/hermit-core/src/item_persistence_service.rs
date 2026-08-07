//! [`HermitItemPersistenceService`] — the concrete [`ItemPersistenceService`].
//!
//! Port of `ItemPersistenceService`. Writes `BaseItems` rows and deletes items.
//! In C# this service maps a domain `BaseItem` onto the entity via
//! `BaseItemMapper` and then saves; here the trait already receives mapped
//! [`BaseItemEntity`] rows (per the persistence-trait port rules), so
//! [`save_items`](HermitItemPersistenceService::save_items) is a full-column
//! upsert. Child-collection writes (images, streams, people, item-values) have
//! their own repositories/services; the image write is provided here to satisfy
//! the trait, delegating the row layout to `BaseItemImageInfos`.
//!
//! The `IServerApplicationHost` constructor dependency only supplies path
//! normalization in C# and is not needed to persist already-mapped rows, so it
//! is not taken here.

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::ItemImageInfo;
use hermit_traits::persistence::ItemPersistenceService;

use hermit_model::data::BaseItemKind;

use crate::db_error::db_err;
use crate::item_repository::image_type_to_disc;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::PLACEHOLDER_ID;

/// Maps an `ItemValues.Type` discriminant to the stored `BaseItems.Type` name of
/// its browsable by-name item, or [`None`] for value types with no browse tab
/// (tags, artists — handled elsewhere).
///
/// ponytail: Genre (2) → `Genre` and Studios (3) → `Studio` only. Music genres
/// share the Genre value type here, so a music-only library's MusicGenre tab
/// would want its own mapping; add when a music library needs it.
fn by_name_type_name(value_type: i32) -> Option<&'static str> {
    match value_type {
        2 => stored_type_name(BaseItemKind::Genre),
        3 => stored_type_name(BaseItemKind::Studio),
        _ => None,
    }
}

/// The concrete item-persistence service.
#[derive(Clone)]
pub struct HermitItemPersistenceService {
    db: Database,
}

impl std::fmt::Debug for HermitItemPersistenceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitItemPersistenceService")
            .finish_non_exhaustive()
    }
}

impl HermitItemPersistenceService {
    /// Creates the service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Upserts a single item row (`INSERT … ON CONFLICT("Id") DO UPDATE`).
    ///
    /// Every non-key column is bound; on conflict each is overwritten, so a save
    /// fully replaces the stored row (matching the C# save semantics for the
    /// scalar `BaseItems` columns).
    async fn upsert_item(&self, item: &BaseItemEntity) -> Result<(), ServiceError> {
        // C# `SaveItem` always stamps `CleanName = GetCleanValue(item.Name)` at
        // write time (no caller pre-computes it); deriving here keeps every
        // saved item matchable by the search filter, which queries `CleanName`.
        let clean_name = item
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(crate::text_util::get_clean_value);
        sqlx::query(UPSERT_SQL)
            .bind(&item.id)
            .bind(&item.album)
            .bind(&item.album_artists)
            .bind(&item.artists)
            .bind(item.audio)
            .bind(&item.channel_id)
            .bind(clean_name)
            .bind(item.community_rating)
            .bind(item.critic_rating)
            .bind(&item.custom_rating)
            .bind(&item.data)
            .bind(item.date_created)
            .bind(item.date_last_media_added)
            .bind(item.date_last_refreshed)
            .bind(item.date_last_saved)
            .bind(item.date_modified)
            .bind(item.end_date)
            .bind(&item.episode_title)
            .bind(&item.external_id)
            .bind(&item.external_series_id)
            .bind(&item.external_service_id)
            .bind(item.extra_type)
            .bind(&item.forced_sort_name)
            .bind(&item.genres)
            .bind(item.height)
            .bind(item.index_number)
            .bind(item.inherited_parental_rating_sub_value)
            .bind(item.inherited_parental_rating_value)
            .bind(item.is_folder)
            .bind(item.is_in_mixed_folder)
            .bind(item.is_locked)
            .bind(item.is_movie)
            .bind(item.is_repeat)
            .bind(item.is_series)
            .bind(item.is_virtual_item)
            .bind(item.lufs)
            .bind(&item.media_type)
            .bind(&item.name)
            .bind(item.normalization_gain)
            .bind(&item.official_rating)
            .bind(&item.original_language)
            .bind(&item.original_title)
            .bind(&item.overview)
            .bind(&item.owner_id)
            .bind(&item.parent_id)
            .bind(item.parent_index_number)
            .bind(&item.path)
            .bind(&item.preferred_metadata_country_code)
            .bind(&item.preferred_metadata_language)
            .bind(item.premiere_date)
            .bind(&item.presentation_unique_key)
            .bind(&item.primary_version_id)
            .bind(&item.production_locations)
            .bind(item.production_year)
            .bind(item.run_time_ticks)
            .bind(&item.season_id)
            .bind(&item.season_name)
            .bind(&item.series_id)
            .bind(&item.series_name)
            .bind(&item.series_presentation_unique_key)
            .bind(&item.show_id)
            .bind(item.size)
            .bind(&item.sort_name)
            .bind(item.start_date)
            .bind(&item.studios)
            .bind(&item.tagline)
            .bind(&item.tags)
            .bind(&item.top_parent_id)
            .bind(item.total_bitrate)
            .bind(&item.type_)
            .bind(&item.unrated_type)
            .bind(item.width)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl ItemPersistenceService for HermitItemPersistenceService {
    async fn delete_items(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        for id in ids {
            if id.to_string() == PLACEHOLDER_ID {
                // Never delete the UserData placeholder row.
                continue;
            }
            // `LinkedChildren` is the one BaseItems FK without `ON DELETE CASCADE`
            // (it references the item as both parent and child), so clear those
            // links first — otherwise deleting a playlist/collection, or an item
            // that belongs to one, trips a FOREIGN KEY constraint (787).
            sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 OR "ChildId" = ?1"#)
                .bind(id.to_string())
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
            sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
        }
        Ok(())
    }

    async fn save_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item).await?;
        }
        Ok(())
    }

    async fn save_provider_id(
        &self,
        item_id: Uuid,
        provider: &str,
        value: &str,
    ) -> Result<(), ServiceError> {
        // One row per (item, provider key) — the table's primary key — so a
        // re-save replaces the value (the C# `ProviderIds[key] = value` write).
        sqlx::query(
            r#"INSERT OR REPLACE INTO "BaseItemProviders"
               ("ItemId", "ProviderId", "ProviderValue") VALUES (?1, ?2, ?3)"#,
        )
        .bind(item_id.to_string())
        .bind(provider)
        .bind(value)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn save_item_values(
        &self,
        item_id: Uuid,
        values: &[(i32, String)],
    ) -> Result<(), ServiceError> {
        let id = item_id.to_string();
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Rewrite this item's links; the shared ItemValues rows are kept.
        sqlx::query(r#"DELETE FROM "ItemValuesMap" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for (type_, value) in values {
            if value.is_empty() {
                continue;
            }
            let clean = crate::text_util::get_clean_value(value);
            // Get-or-create the (Type, Value) row (unique index on Type+Value).
            let new_id = Uuid::new_v4().to_string();
            sqlx::query(
                r#"INSERT OR IGNORE INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
                   VALUES (?1,?2,?3,?4)"#,
            )
            .bind(&new_id)
            .bind(&clean)
            .bind(type_)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            let value_id: String = sqlx::query_scalar(
                r#"SELECT "ItemValueId" FROM "ItemValues" WHERE "Type" = ?1 AND "Value" = ?2"#,
            )
            .bind(type_)
            .bind(value)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            sqlx::query(
                r#"INSERT OR IGNORE INTO "ItemValuesMap" ("ItemValueId","ItemId") VALUES (?1,?2)"#,
            )
            .bind(&value_id)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            // Materialize the browsable by-name item (genre/studio) sharing the
            // ItemValueId as its id, so the Genres/Studios tabs list it, the
            // /Genres/{name} lookup resolves it, and a `GenreIds=<id>` filter
            // (which resolves the id → BaseItems.CleanName) matches. Faithful to
            // Jellyfin, where genres/studios are BaseItems; here their id is the
            // shared value id the DTO layer already emits for genre_items.
            if let Some(type_name) = by_name_type_name(*type_) {
                sqlx::query(
                    r#"INSERT OR IGNORE INTO "BaseItems"
                       ("Id","Type","Name","CleanName","IsFolder","IsInMixedFolder",
                        "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                       VALUES (?1,?2,?3,?4,1,0,0,0,0,0,0)"#,
                )
                .bind(&value_id)
                .bind(type_name)
                .bind(value)
                .bind(&clean)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(exists.is_some())
    }

    async fn set_ancestors(
        &self,
        item_id: Uuid,
        ancestor_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        let id = item_id.to_string();
        // One transaction so the clear+rewrite is atomic on a single connection —
        // otherwise the DELETE and INSERTs land on different pool connections and
        // can interleave with a concurrent rebuild, and `INSERT OR IGNORE` makes
        // a duplicate ancestor (or a lost race) a no-op instead of a UNIQUE 500.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "AncestorIds" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for ancestor in ancestor_ids {
            sqlx::query(
                r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#,
            )
            .bind(&id)
            .bind(ancestor.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn save_images(&self, item: &BaseItemEntity) -> Result<(), ServiceError> {
        // The image rows live in BaseItemImageInfos and are owned by their own
        // repository; without the domain item's ImageInfos list on the entity
        // there is nothing to persist here beyond confirming the item exists.
        // (Full image persistence lands with the image repository unit.)
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&item.id)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        if exists.is_none() {
            return Err(ServiceError::not_found(format!("item {}", item.id)));
        }
        Ok(())
    }

    async fn save_item_images(
        &self,
        item_id: Uuid,
        images: &[ItemImageInfo],
    ) -> Result<(), ServiceError> {
        let item = item_id.to_string();
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Replace the item's image set (idempotent re-scan).
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#)
            .bind(&item)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for image in images {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                   ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "Blurhash", "DateModified")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            )
            .bind(uuid::Uuid::new_v4().hyphenated().to_string())
            .bind(&item)
            .bind(image_type_to_disc(image.image_type))
            .bind(&image.path)
            .bind(i64::from(image.width))
            .bind(i64::from(image.height))
            .bind(image.blur_hash.as_deref().map(str::as_bytes)) // BLOB of the hash's UTF-8 bytes
            .bind(image.date_modified)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn set_item_image(
        &self,
        item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Result<(), ServiceError> {
        let item = item_id.to_string();
        let disc = image_type_to_disc(image.image_type);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Replace any existing rows of this type (an uploaded image supersedes the
        // prior one of the same type).
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#)
            .bind(&item)
            .bind(disc)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
               ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "DateModified")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
        )
        .bind(uuid::Uuid::new_v4().hyphenated().to_string())
        .bind(&item)
        .bind(disc)
        .bind(&image.path)
        .bind(i64::from(image.width))
        .bind(i64::from(image.height))
        .bind(image.date_modified)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn delete_item_image(
        &self,
        item_id: Uuid,
        image_type: hermit_model::entities::ImageType,
        _index: Option<i32>,
    ) -> Result<Vec<String>, ServiceError> {
        let item = item_id.to_string();
        let disc = image_type_to_disc(image_type);
        // Collect the on-disk paths before deleting so the caller can remove files.
        let paths: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#,
        )
        .bind(&item)
        .bind(disc)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#)
            .bind(&item)
            .bind(disc)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(paths)
    }

    async fn reattach_user_data(&self, item: &BaseItemEntity) -> Result<(), ServiceError> {
        // Reattach user-data rows detached onto the placeholder item back to this
        // item when their CustomDataKey matches the item's presentation key
        // (C# `RetentionDate` reattachment keys user data by presentation key).
        let Some(key) = item.presentation_unique_key.as_ref() else {
            return Ok(());
        };
        sqlx::query(
            r#"UPDATE "UserData" SET "ItemId" = ?1
               WHERE "ItemId" = ?2 AND "CustomDataKey" = ?3"#,
        )
        .bind(&item.id)
        .bind(PLACEHOLDER_ID)
        .bind(key)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn update_inherited_values(&self) -> Result<(), ServiceError> {
        // Recomputing inherited parental-rating / tag values across the item tree
        // requires the AncestorIds closure traversal owned by the library manager;
        // deferred to that unit. No-op here so callers can invoke it safely.
        Ok(())
    }
}

/// The full-column upsert statement for a `BaseItems` row. Column order matches
/// the bind order in [`HermitItemPersistenceService::upsert_item`].
const UPSERT_SQL: &str = r#"INSERT INTO "BaseItems" (
    "Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName",
    "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated",
    "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified",
    "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId",
    "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber",
    "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder",
    "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem",
    "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating",
    "OriginalLanguage", "OriginalTitle", "Overview", "OwnerId", "ParentId",
    "ParentIndexNumber", "Path", "PreferredMetadataCountryCode",
    "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey",
    "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks",
    "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey",
    "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags",
    "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width"
) VALUES (
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
) ON CONFLICT("Id") DO UPDATE SET
    "Album" = excluded."Album", "AlbumArtists" = excluded."AlbumArtists",
    "Artists" = excluded."Artists", "Audio" = excluded."Audio",
    "ChannelId" = excluded."ChannelId", "CleanName" = excluded."CleanName",
    "CommunityRating" = excluded."CommunityRating", "CriticRating" = excluded."CriticRating",
    "CustomRating" = excluded."CustomRating", "Data" = excluded."Data",
    "DateCreated" = excluded."DateCreated", "DateLastMediaAdded" = excluded."DateLastMediaAdded",
    "DateLastRefreshed" = excluded."DateLastRefreshed", "DateLastSaved" = excluded."DateLastSaved",
    "DateModified" = excluded."DateModified", "EndDate" = excluded."EndDate",
    "EpisodeTitle" = excluded."EpisodeTitle", "ExternalId" = excluded."ExternalId",
    "ExternalSeriesId" = excluded."ExternalSeriesId", "ExternalServiceId" = excluded."ExternalServiceId",
    "ExtraType" = excluded."ExtraType", "ForcedSortName" = excluded."ForcedSortName",
    "Genres" = excluded."Genres", "Height" = excluded."Height",
    "IndexNumber" = excluded."IndexNumber",
    "InheritedParentalRatingSubValue" = excluded."InheritedParentalRatingSubValue",
    "InheritedParentalRatingValue" = excluded."InheritedParentalRatingValue",
    "IsFolder" = excluded."IsFolder", "IsInMixedFolder" = excluded."IsInMixedFolder",
    "IsLocked" = excluded."IsLocked", "IsMovie" = excluded."IsMovie",
    "IsRepeat" = excluded."IsRepeat", "IsSeries" = excluded."IsSeries",
    "IsVirtualItem" = excluded."IsVirtualItem", "LUFS" = excluded."LUFS",
    "MediaType" = excluded."MediaType", "Name" = excluded."Name",
    "NormalizationGain" = excluded."NormalizationGain", "OfficialRating" = excluded."OfficialRating",
    "OriginalLanguage" = excluded."OriginalLanguage", "OriginalTitle" = excluded."OriginalTitle",
    "Overview" = excluded."Overview", "OwnerId" = excluded."OwnerId",
    "ParentId" = excluded."ParentId", "ParentIndexNumber" = excluded."ParentIndexNumber",
    "Path" = excluded."Path",
    "PreferredMetadataCountryCode" = excluded."PreferredMetadataCountryCode",
    "PreferredMetadataLanguage" = excluded."PreferredMetadataLanguage",
    "PremiereDate" = excluded."PremiereDate",
    "PresentationUniqueKey" = excluded."PresentationUniqueKey",
    "PrimaryVersionId" = excluded."PrimaryVersionId",
    "ProductionLocations" = excluded."ProductionLocations",
    "ProductionYear" = excluded."ProductionYear", "RunTimeTicks" = excluded."RunTimeTicks",
    "SeasonId" = excluded."SeasonId", "SeasonName" = excluded."SeasonName",
    "SeriesId" = excluded."SeriesId", "SeriesName" = excluded."SeriesName",
    "SeriesPresentationUniqueKey" = excluded."SeriesPresentationUniqueKey",
    "ShowId" = excluded."ShowId", "Size" = excluded."Size", "SortName" = excluded."SortName",
    "StartDate" = excluded."StartDate", "Studios" = excluded."Studios",
    "Tagline" = excluded."Tagline", "Tags" = excluded."Tags",
    "TopParentId" = excluded."TopParentId", "TotalBitrate" = excluded."TotalBitrate",
    "Type" = excluded."Type", "UnratedType" = excluded."UnratedType", "Width" = excluded."Width"
"#;

#[cfg(test)]
mod tests {
    use hermit_model::data::BaseItemKind;
    use hermit_traits::persistence::{ItemPersistenceService, LinkedChildrenService};
    use uuid::Uuid;

    use crate::linked_children_service::HermitLinkedChildrenService;
    use crate::test_support::{seed_item, test_db};

    use super::HermitItemPersistenceService;

    // A playlist (parent) and one of its members (child) both live in
    // `LinkedChildren`, whose BaseItems FK lacks `ON DELETE CASCADE`. Deleting
    // either must clear those links first instead of tripping constraint 787.
    #[tokio::test]
    async fn delete_clears_linked_children_both_directions() {
        let db = test_db().await;
        let playlist = Uuid::new_v4();
        let (member_a, member_b) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, playlist, BaseItemKind::Playlist).await;
        seed_item(&db, member_a, BaseItemKind::Movie).await;
        seed_item(&db, member_b, BaseItemKind::Movie).await;

        let links = HermitLinkedChildrenService::new(db.clone());
        links
            .upsert_linked_child(playlist, member_a, 0)
            .await
            .expect("link a");
        links
            .upsert_linked_child(playlist, member_b, 0)
            .await
            .expect("link b");

        let svc = HermitItemPersistenceService::new(db.clone());

        // Delete a member (the ChildId FK direction): its link clears, the
        // playlist and other member survive — no FK 787.
        svc.delete_items(&[member_a])
            .await
            .expect("delete member_a");
        assert!(!svc.item_exists(member_a).await.expect("exists a"));
        assert!(svc.item_exists(playlist).await.expect("playlist survives"));

        // Delete the playlist (the ParentId FK direction) while member_b's link
        // still exists — must clear it instead of tripping FK 787.
        svc.delete_items(&[playlist])
            .await
            .expect("delete playlist");
        assert!(!svc.item_exists(playlist).await.expect("exists p"));
        assert!(svc.item_exists(member_b).await.expect("member_b survives"));

        let remaining: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(remaining, 0, "all links should be cleared");
    }

    // A provider id round-trips through the repository's by-provider lookup,
    // and a re-save for the same (item, key) replaces the value instead of
    // stacking rows (the table's composite primary key).
    #[tokio::test]
    async fn save_provider_id_upserts_the_row() {
        use hermit_traits::persistence::ItemRepository;

        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let svc = HermitItemPersistenceService::new(db.clone());
        let repo = crate::item_repository::HermitItemRepository::new(
            db.clone(),
            std::sync::Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );

        svc.save_provider_id(movie, "Tmdb", "603")
            .await
            .expect("save");
        svc.save_provider_id(movie, "Tmdb", "604")
            .await
            .expect("replace");

        let rows = repo
            .get_items_with_provider_id("Tmdb")
            .await
            .expect("lookup");
        assert_eq!(rows, vec![(movie, "604".to_owned())]);
    }

    // Saving an item must stamp the derived `CleanName` (C# `SaveItem` computes
    // `GetCleanValue(item.Name)` at write time). No scan path pre-computes it,
    // and the `searchTerm` filter queries `CleanName` — a NULL there makes the
    // item invisible to search (the web search page returned nothing).
    #[tokio::test]
    async fn save_items_stamps_derived_clean_name() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let svc = HermitItemPersistenceService::new(db.clone());

        let item = hermit_db::entities::base_items::BaseItemEntity {
            id: id.to_string(),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            name: Some("Amélie".to_owned()),
            ..hermit_db::entities::base_items::BaseItemEntity::default()
        };
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("save");

        let clean: Option<String> =
            sqlx::query_scalar(r#"SELECT "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .fetch_one(db.pool())
                .await
                .expect("query");
        assert_eq!(clean.as_deref(), Some("amelie"));
    }

    // Saving a movie's genre/studio values must also materialize the browsable
    // by-name BaseItems row (sharing the ItemValueId as its id) so the
    // Genres/Studios tabs list it and a `GenreIds=<id>` filter resolves.
    #[tokio::test]
    async fn save_item_values_materializes_by_name_items() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let svc = HermitItemPersistenceService::new(db.clone());

        // 2 = Genre, 3 = Studios, 4 = Tags (tags get no browse item).
        svc.save_item_values(
            movie,
            &[
                (2, "Horror".to_owned()),
                (3, "A24".to_owned()),
                (4, "4k".to_owned()),
            ],
        )
        .await
        .expect("save values");

        // The Genre by-name row exists, and its id equals the shared ItemValueId.
        let genre: Option<(String, String)> = sqlx::query_as(
            r#"SELECT bi."Id", iv."ItemValueId"
               FROM "BaseItems" bi
               JOIN "ItemValues" iv ON iv."Value" = bi."Name" AND iv."Type" = 2
               WHERE bi."Type" LIKE '%.Genre' AND bi."Name" = 'Horror'"#,
        )
        .fetch_optional(db.pool())
        .await
        .expect("query genre");
        let (genre_item_id, genre_value_id) = genre.expect("genre by-name row exists");
        assert_eq!(genre_item_id, genre_value_id, "id is the shared value id");

        // Studio row exists too; the tag does NOT get a by-name row.
        let studios: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%.Studio'"#)
                .fetch_one(db.pool())
                .await
                .expect("studio count");
        assert_eq!(studios, 1);
        let tags: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Name" = '4k' AND "Type" NOT LIKE '%.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("tag count");
        assert_eq!(tags, 0, "tags are not browsable by-name items");
    }
}
