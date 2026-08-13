//! [`FerrofinUserDataManager`] — the concrete [`UserDataManager`] over `ferrofin-db`.
//!
//! Port of `Emby.Server.Implementations.Library.UserDataManager`. Per-user,
//! per-item playback state lives in the `UserData` table, keyed by the
//! `(ItemId, UserId, CustomDataKey)` triple.
//!
//! Port simplifications, all faithful to the trait's `Uuid`-identity surface:
//! - The C# path derives multiple `CustomDataKey`s from an item's metadata
//!   (`GetUserDataKeys`); the trait works purely with item ids, so this port
//!   uses the item id as the single `CustomDataKey` (matching how the landed
//!   `test_support::seed_user_data` and the next-up service already key rows).
//! - The C# in-memory `FastConcurrentLru` cache and the `UserDataSaved` event
//!   are dropped; every read hits the table.
//! - `UpdatePlayState` needs the item's runtime + kind (for the resume-point
//!   heuristics); those are read from the `BaseItems` row rather than a
//!   pre-loaded domain object.
//!
//! Resume-point thresholds (`MinResumePct`, `MaxResumePct`,
//! `MinResumeDurationSeconds`, `MinAudiobookResume`, `MaxAudiobookResume`) are
//! read live from the injected [`ServerConfigurationManager`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::playback::UserDataEntity;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{UpdateUserItemDataDto, UserItemDataDto};
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserDataManager;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::{supports_played_status, supports_position_ticks_resume};

/// One tick is 100 nanoseconds; there are 10,000,000 ticks per second (the
/// .NET `TimeSpan.TicksPerSecond` the C# resume math uses).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The concrete user-data manager.
#[derive(Clone)]
pub struct FerrofinUserDataManager {
    db: Database,
    config: Arc<dyn ServerConfigurationManager>,
}

impl std::fmt::Debug for FerrofinUserDataManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinUserDataManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinUserDataManager {
    /// Creates a user-data manager over the given database and configuration.
    #[must_use]
    pub fn new(db: Database, config: Arc<dyn ServerConfigurationManager>) -> Self {
        Self { db, config }
    }

    /// Reads the single user-data row for an item/user pair, keyed by the item
    /// id (this port's `CustomDataKey`), or `None`.
    async fn read_row(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserDataEntity>, ServiceError> {
        // ItemId/UserId are stored uppercase (Jellyfin's GUID casing) while
        // CustomDataKey keeps the lowercase hyphenated form — exactly what a
        // real 10.11.8 database contains, so the two need separate binds.
        sqlx::query_as::<_, UserDataEntity>(
            r#"SELECT * FROM "UserData"
               WHERE "ItemId" = ?1 AND "UserId" = ?2 AND "CustomDataKey" = ?3 LIMIT 1"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .bind(item_id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// Reads the item's runtime ticks and [`BaseItemKind`], for the play-state
    /// heuristics. A missing item yields `None`.
    async fn item_runtime_and_kind(
        &self,
        item_id: Uuid,
    ) -> Result<Option<(i64, BaseItemKind)>, ServiceError> {
        let row: Option<(Option<i64>, String)> = sqlx::query_as(
            r#"SELECT "RunTimeTicks", "Type" FROM "BaseItems" WHERE "Id" = ?1 LIMIT 1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(row.map(|(ticks, type_name)| {
            (
                ticks.unwrap_or(0),
                // Unknown stored types default to Folder (a conservative choice that
                // disables the position-resume heuristics).
                kind_from_type_name(&type_name).unwrap_or(BaseItemKind::Folder),
            )
        }))
    }

    /// Inserts or updates the row for an item/user pair from the supplied
    /// [`UserDataEntity`] (keyed by the item id).
    async fn upsert_row(&self, row: &UserDataEntity) -> Result<(), ServiceError> {
        let exists: bool = sqlx::query_scalar(
            r#"SELECT EXISTS(SELECT 1 FROM "UserData"
               WHERE "ItemId" = ?1 AND "UserId" = ?2 AND "CustomDataKey" = ?3)"#,
        )
        .bind(&row.item_id)
        .bind(&row.user_id)
        .bind(&row.custom_data_key)
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)?;

        if exists {
            sqlx::query(
                r#"UPDATE "UserData" SET
                    "AudioStreamIndex" = ?4, "IsFavorite" = ?5, "LastPlayedDate" = ?6,
                    "Likes" = ?7, "PlayCount" = ?8, "PlaybackPositionTicks" = ?9,
                    "Played" = ?10, "Rating" = ?11, "SubtitleStreamIndex" = ?12
                   WHERE "ItemId" = ?1 AND "UserId" = ?2 AND "CustomDataKey" = ?3"#,
            )
        } else {
            sqlx::query(
                r#"INSERT INTO "UserData"
                    ("ItemId", "UserId", "CustomDataKey", "AudioStreamIndex",
                     "IsFavorite", "LastPlayedDate", "Likes", "PlayCount",
                     "PlaybackPositionTicks", "Played", "Rating", "SubtitleStreamIndex")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
            )
        }
        .bind(&row.item_id)
        .bind(&row.user_id)
        .bind(&row.custom_data_key)
        .bind(row.audio_stream_index)
        .bind(row.is_favorite)
        .bind(opt_datetime_to_db(row.last_played_date))
        .bind(row.likes)
        .bind(row.play_count)
        .bind(row.playback_position_ticks)
        .bind(row.played)
        .bind(row.rating)
        .bind(row.subtitle_stream_index)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// A default (empty) user-data row for an item/user, used when none exists.
    fn empty_row(item_id: Uuid, user_id: Uuid) -> UserDataEntity {
        UserDataEntity {
            item_id: guid_to_db(item_id),
            user_id: guid_to_db(user_id),
            // Jellyfin stores the key in the LOWERCASE hyphenated form even
            // though ItemId is uppercase (verified against a real 10.11.8 DB).
            custom_data_key: item_id.to_string(),
            audio_stream_index: None,
            is_favorite: false,
            last_played_date: None,
            likes: None,
            play_count: 0,
            playback_position_ticks: 0,
            played: false,
            rating: None,
            retention_date: None,
            subtitle_stream_index: None,
        }
    }
}

/// Maps a [`UserDataEntity`] row to the presentation DTO (C#
/// `GetUserItemDataDto`). Playback fields carry over verbatim; the
/// item-dependent `PlayedPercentage`/`UnplayedItemCount` are left unset here
/// (they are filled by the DTO service against the resolved item).
fn to_dto(row: &UserDataEntity, item_id: Uuid) -> UserItemDataDto {
    UserItemDataDto {
        rating: row.rating,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: row.playback_position_ticks,
        play_count: row.play_count,
        is_favorite: row.is_favorite,
        likes: row.likes,
        last_played_date: row.last_played_date,
        played: row.played,
        key: row.custom_data_key.clone(),
        item_id,
    }
}

#[async_trait]
impl UserDataManager for FerrofinUserDataManager {
    async fn save_user_data(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        // C# loads the existing row, applies only the set fields, then saves.
        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));

        if let Some(v) = user_data.playback_position_ticks {
            row.playback_position_ticks = v;
        }
        if let Some(v) = user_data.play_count {
            row.play_count = v;
        }
        if let Some(v) = user_data.is_favorite {
            row.is_favorite = v;
        }
        if user_data.likes.is_some() {
            row.likes = user_data.likes;
        }
        if let Some(v) = user_data.played {
            row.played = v;
        }
        if user_data.last_played_date.is_some() {
            row.last_played_date = user_data.last_played_date;
        }
        if user_data.rating.is_some() {
            row.rating = user_data.rating;
        }

        self.upsert_row(&row).await
    }

    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        let row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));
        Ok(Some(to_dto(&row, item_id)))
    }

    async fn get_user_data_dtos(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        // One IN-query per chunk instead of one query per item. 500 stays far
        // below SQLite's conservative 999-host-variable floor.
        for chunk in item_ids.chunks(500) {
            let placeholders = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            // `CustomDataKey = lower(ItemId)` selects the item's DEFAULT row
            // (not alternate provider keys). A column-to-column equality would
            // never match real Jellyfin rows: ItemId is stored uppercase but
            // CustomDataKey lowercase; lower() is ASCII-only, which is exact
            // for hex GUID text.
            let sql = format!(
                r#"SELECT * FROM "UserData"
                   WHERE "UserId" = ?1 AND "ItemId" IN ({placeholders})
                     AND "CustomDataKey" = lower("ItemId")"#,
            );
            let mut query = sqlx::query_as::<_, UserDataEntity>(&sql).bind(guid_to_db(user_id));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
            for row in rows {
                if let Ok(item_id) = Uuid::parse_str(&row.item_id) {
                    map.insert(item_id, to_dto(&row, item_id));
                }
            }
        }
        // Items without a stored row get the empty-row DTO, matching the
        // per-item path's `unwrap_or_else(empty_row)` fallback.
        for &item_id in item_ids {
            map.entry(item_id)
                .or_insert_with(|| to_dto(&Self::empty_row(item_id, user_id), item_id));
        }
        Ok(map)
    }

    async fn set_likes(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        likes: Option<bool>,
    ) -> Result<UserItemDataDto, ServiceError> {
        // Assign the like directly (including `None`) so a clear persists — the
        // merge path in `save_user_data` can only ever *set* a like. Port of C#
        // `UpdateUserItemRatingInternal` (`userData.Likes = likes; Save(...)`).
        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));
        row.likes = likes;
        self.upsert_row(&row).await?;
        Ok(to_dto(&row, item_id))
    }

    async fn get_user_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let mut result = HashMap::with_capacity(item_ids.len());
        for &item_id in item_ids {
            let row = self
                .read_row(item_id, user_id)
                .await?
                .unwrap_or_else(|| Self::empty_row(item_id, user_id));
            result.insert(item_id, to_dto(&row, item_id));
        }
        Ok(result)
    }

    async fn update_play_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        let config = self.config.configuration().await?;
        let (runtime_ticks, kind) = self
            .item_runtime_and_kind(item_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("item {item_id}")))?;

        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));

        // A report with no position (an interrupted/failed start where the client
        // can't say where it was) tells us nothing. Jellyfin assumes "finished"
        // here, which marks the item played and wipes a still-valid resume point;
        // we preserve existing play-state instead so a hung resume doesn't destroy
        // progress. ponytail: deliberate divergence from Jellyfin, see bug notes.
        let Some(mut position_ticks) = reported_position_ticks else {
            return Ok(false);
        };
        let has_runtime = runtime_ticks > 0;
        let is_audiobook = matches!(kind, BaseItemKind::AudioBook);
        let is_book = matches!(kind, BaseItemKind::Book);
        let mut played_to_completion = false;

        if position_ticks > 0 && has_runtime && !is_audiobook && !is_book {
            #[allow(clippy::cast_precision_loss)]
            let pct_in = (position_ticks as f64 / runtime_ticks as f64) * 100.0;

            if pct_in < f64::from(config.min_resume_pct) {
                position_ticks = 0;
            } else if pct_in > f64::from(config.max_resume_pct)
                || position_ticks >= runtime_ticks - TICKS_PER_SECOND
            {
                position_ticks = 0;
                row.played = true;
                played_to_completion = true;
            } else {
                #[allow(clippy::cast_precision_loss)]
                let duration_seconds = runtime_ticks as f64 / TICKS_PER_SECOND as f64;
                if duration_seconds < f64::from(config.min_resume_duration_seconds) {
                    position_ticks = 0;
                    row.played = true;
                    played_to_completion = true;
                }
            }
        } else if position_ticks > 0 && has_runtime && is_audiobook {
            #[allow(clippy::cast_precision_loss)]
            let position_minutes = position_ticks as f64 / TICKS_PER_SECOND as f64 / 60.0;
            #[allow(clippy::cast_precision_loss)]
            let remaining_minutes =
                (runtime_ticks - position_ticks) as f64 / TICKS_PER_SECOND as f64 / 60.0;
            if position_minutes < f64::from(config.min_audiobook_resume) {
                position_ticks = 0;
            } else if remaining_minutes < f64::from(config.max_audiobook_resume)
                || position_ticks >= runtime_ticks
            {
                position_ticks = 0;
                row.played = true;
                played_to_completion = true;
            }
        } else if !has_runtime {
            row.played = true;
            played_to_completion = true;
            position_ticks = 0;
        }

        if !supports_played_status(kind) {
            position_ticks = 0;
            row.played = false;
        }
        if !supports_position_ticks_resume(kind) {
            position_ticks = 0;
        }

        row.playback_position_ticks = position_ticks;
        self.upsert_row(&row).await?;

        Ok(played_to_completion)
    }

    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        // Port of `BaseItem.MarkPlayed(user, datePlayed, resetPosition: true)`.
        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));

        // A supplied date is a fresh play → increment the count.
        if date_played.is_some() {
            row.play_count += 1;
        }
        // Ensure it is at least one.
        row.play_count = row.play_count.max(1);
        // `resetPosition` is always true from the controller.
        row.playback_position_ticks = 0;
        row.last_played_date = Some(
            date_played
                .or(row.last_played_date)
                .unwrap_or_else(chrono::Utc::now),
        );
        row.played = true;

        self.upsert_row(&row).await?;
        Ok(to_dto(&row, item_id))
    }

    async fn mark_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        // Port of `BaseItem.MarkUnplayed` → `ResetPlayedState`.
        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));

        row.play_count = 0;
        row.playback_position_ticks = 0;
        row.last_played_date = None;
        row.played = false;

        self.upsert_row(&row).await?;
        Ok(to_dto(&row, item_id))
    }

    async fn reset_playback_stream_selections(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "UserData"
               SET "AudioStreamIndex" = NULL, "SubtitleStreamIndex" = NULL
               WHERE "ItemId" = ?1 AND "UserId" = ?2"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_content_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<(bool, bool)>, ServiceError> {
        // Kind 10 = EnableContentDeletion, 11 = EnableContentDownloading.
        let rows: Vec<(i32, bool)> = sqlx::query_as(
            r#"SELECT "Kind", "Value" FROM "Permissions"
               WHERE "UserId" = ?1 AND "Kind" IN (10, 11)"#,
        )
        .bind(guid_to_db(user_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let has = |kind: i32| rows.iter().any(|(k, v)| *k == kind && *v);
        Ok(Some((has(10), has(11))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use crate::test_support::{seed_item, seed_user, test_db};
    use ferrofin_model::configuration::ServerConfiguration;

    /// A config manager whose configuration is the factory default.
    struct FixedConfig {
        config: ServerConfiguration,
    }

    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("not used in these tests")
        }

        async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
            Ok(self.config.clone())
        }

        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }

        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn config() -> Arc<dyn ServerConfigurationManager> {
        Arc::new(FixedConfig {
            config: default_server_configuration(),
        })
    }

    /// Seeds a movie with a runtime, for the play-state heuristics.
    async fn seed_movie_with_runtime(db: &Database, id: Uuid, runtime_ticks: i64) {
        seed_item(db, id, BaseItemKind::Movie).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(runtime_ticks)
            .execute(db.writer())
            .await
            .expect("set runtime");
    }

    #[tokio::test]
    async fn save_then_read_round_trips() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        mgr.save_user_data(
            user,
            item,
            &UpdateUserItemDataDto {
                is_favorite: Some(true),
                play_count: Some(3),
                ..Default::default()
            },
        )
        .await
        .expect("save");

        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("some");
        assert!(dto.is_favorite);
        assert_eq!(dto.play_count, 3);
    }

    // The batch read must agree with the per-item read for stored rows AND
    // fabricate the same empty-row DTO for items with no row (the list-endpoint
    // prefetch replaces the per-item N+1, so any divergence is a play-state bug).
    #[tokio::test]
    async fn batch_read_matches_per_item_reads() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let (with_row, without_row) = (Uuid::from_u128(2), Uuid::from_u128(3));
        seed_user(&db, user).await;
        seed_item(&db, with_row, BaseItemKind::Movie).await;
        seed_item(&db, without_row, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        mgr.save_user_data(
            user,
            with_row,
            &UpdateUserItemDataDto {
                is_favorite: Some(true),
                playback_position_ticks: Some(42),
                ..Default::default()
            },
        )
        .await
        .expect("save");

        let batch = mgr
            .get_user_data_dtos(&[with_row, without_row], user)
            .await
            .expect("batch");
        assert_eq!(batch.len(), 2);
        for id in [with_row, without_row] {
            let single = mgr.get_user_data_dto(id, user).await.expect("read");
            assert_eq!(batch.get(&id), single.as_ref(), "item {id}");
        }
        assert!(batch[&with_row].is_favorite);
        assert!(!batch[&without_row].is_favorite);
    }

    #[tokio::test]
    async fn set_likes_sets_and_clears() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        // Set a like.
        let dto = mgr.set_likes(user, item, Some(true)).await.expect("like");
        assert_eq!(dto.likes, Some(true));
        assert_eq!(
            mgr.get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .likes,
            Some(true),
            "like persisted"
        );

        // Clear it — must stick (the bug: a merge-save could not clear).
        let dto = mgr.set_likes(user, item, None).await.expect("clear");
        assert_eq!(dto.likes, None);
        assert_eq!(
            mgr.get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .likes,
            None,
            "cleared like persisted"
        );
    }

    #[tokio::test]
    async fn missing_row_reads_as_empty_dto() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        let mgr = FerrofinUserDataManager::new(db, config());
        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("some");
        assert!(!dto.is_favorite);
        assert_eq!(dto.play_count, 0);
    }

    #[tokio::test]
    async fn update_play_state_near_end_marks_played() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        // 1 hour runtime.
        let runtime = 3600 * TICKS_PER_SECOND;
        seed_movie_with_runtime(&db, item, runtime).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        // Reporting a position past MaxResumePct (default 90%) marks completion.
        let played = mgr
            .update_play_state(user, item, Some(runtime * 95 / 100))
            .await
            .expect("update");
        assert!(played);

        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("some");
        assert!(dto.played);
        assert_eq!(dto.playback_position_ticks, 0);
    }

    #[tokio::test]
    async fn update_play_state_none_position_preserves_resume_point() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        let runtime = 3600 * TICKS_PER_SECOND;
        seed_movie_with_runtime(&db, item, runtime).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        // Establish a mid-video resume point.
        let position = runtime / 2;
        mgr.update_play_state(user, item, Some(position))
            .await
            .expect("seed resume point");

        // A stop report with no position (failed/hung resume) must NOT mark the
        // item played or wipe the resume point.
        let played = mgr
            .update_play_state(user, item, None)
            .await
            .expect("update");
        assert!(!played);

        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("some");
        assert!(!dto.played);
        assert_eq!(dto.playback_position_ticks, position);
    }

    #[tokio::test]
    async fn update_play_state_midway_keeps_resume_point() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        let runtime = 3600 * TICKS_PER_SECOND;
        seed_movie_with_runtime(&db, item, runtime).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        let position = runtime / 2;
        let played = mgr
            .update_play_state(user, item, Some(position))
            .await
            .expect("update");
        assert!(!played);
        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("some");
        assert_eq!(dto.playback_position_ticks, position);
    }

    #[tokio::test]
    async fn content_permissions_read_the_permission_rows() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x77);
        seed_user(&db, user).await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        // No rows: permissions known, both denied (falsy rows == absent rows).
        let perms = mgr
            .get_content_permissions(user)
            .await
            .expect("read")
            .expect("policy known");
        assert_eq!(perms, (false, false));

        // Kind 10 = EnableContentDeletion granted, 11 = downloading denied.
        sqlx::query(
            r#"INSERT INTO "Permissions" ("Kind", "Value", "UserId", "RowVersion")
               VALUES (10, 1, ?1, 0), (11, 0, ?1, 0)"#,
        )
        .bind(ferrofin_db::store::guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("seed permissions");
        let perms = mgr
            .get_content_permissions(user)
            .await
            .expect("read")
            .expect("policy known");
        assert_eq!(perms, (true, false));
    }

    #[tokio::test]
    async fn reset_stream_selections_clears_indices() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        // Seed a row with stream indices set.
        let mut row = FerrofinUserDataManager::empty_row(item, user);
        row.audio_stream_index = Some(2);
        row.subtitle_stream_index = Some(1);
        mgr.upsert_row(&row).await.expect("seed row");

        mgr.reset_playback_stream_selections(user, item)
            .await
            .expect("reset");

        let cleared = mgr.read_row(item, user).await.expect("read").expect("some");
        assert_eq!(cleared.audio_stream_index, None);
        assert_eq!(cleared.subtitle_stream_index, None);
    }

    #[tokio::test]
    async fn mark_played_sets_played_and_increments_count() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        // Seed a resume position so we can prove it is reset.
        mgr.save_user_data(
            user,
            item,
            &UpdateUserItemDataDto {
                playback_position_ticks: Some(500),
                ..Default::default()
            },
        )
        .await
        .expect("seed");

        let when = chrono::Utc::now();
        let dto = mgr.mark_played(user, item, Some(when)).await.expect("mark");
        assert!(dto.played);
        assert_eq!(dto.play_count, 1);
        assert_eq!(dto.playback_position_ticks, 0);
        assert_eq!(dto.last_played_date, Some(when));

        // A second play with a date increments the count again.
        let dto = mgr
            .mark_played(user, item, Some(chrono::Utc::now()))
            .await
            .expect("mark");
        assert_eq!(dto.play_count, 2);
    }

    #[tokio::test]
    async fn mark_played_without_date_keeps_count_at_least_one() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        let dto = mgr.mark_played(user, item, None).await.expect("mark");
        assert!(dto.played);
        assert_eq!(dto.play_count, 1);
        assert!(dto.last_played_date.is_some());
    }

    #[tokio::test]
    async fn mark_unplayed_resets_play_state() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        mgr.mark_played(user, item, Some(chrono::Utc::now()))
            .await
            .expect("mark played");

        let dto = mgr.mark_unplayed(user, item).await.expect("mark unplayed");
        assert!(!dto.played);
        assert_eq!(dto.play_count, 0);
        assert_eq!(dto.playback_position_ticks, 0);
        assert_eq!(dto.last_played_date, None);
    }
}
