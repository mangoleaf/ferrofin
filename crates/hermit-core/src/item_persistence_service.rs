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
use hermit_traits::persistence::ItemPersistenceService;

use crate::db_error::db_err;
use crate::translate_query::PLACEHOLDER_ID;

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
        sqlx::query(UPSERT_SQL)
            .bind(&item.id)
            .bind(&item.album)
            .bind(&item.album_artists)
            .bind(&item.artists)
            .bind(item.audio)
            .bind(&item.channel_id)
            .bind(&item.clean_name)
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
            .execute(self.db.pool())
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
            sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .execute(self.db.pool())
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
        .execute(self.db.pool())
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
