//! [`FerrofinUserDataManager`] — the concrete [`UserDataManager`] over `ferrofin-db`.
//!
//! Port of `Emby.Server.Implementations.Library.UserDataManager`. Per-user,
//! per-item playback state lives in the `UserData` table, keyed by the
//! `(ItemId, UserId, CustomDataKey)` triple.
//!
//! Port simplifications, all faithful to the trait's `Uuid`-identity surface:
//! - `GetUserDataKeys` derives keys from stored metadata through the shared
//!   user-data key repository, also used by retention recovery.
//! - Reads use the table rather than the C# in-memory cache. Successful writes
//!   notify the weak NFO listener with the source `UserDataSaved` reason.
//! - `UpdatePlayState` needs the item's runtime + kind (for the resume-point
//!   heuristics); those are read from the `BaseItems` row rather than a
//!   pre-loaded domain object.
//!
//! Resume-point thresholds (`MinResumePct`, `MaxResumePct`,
//! `MinResumeDurationSeconds`, `MinAudiobookResume`, `MaxAudiobookResume`) are
//! read live from the injected [`ServerConfigurationManager`].

use std::collections::HashMap;
use std::future::Future as _;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::playback::UserDataEntity;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{UpdateUserItemDataDto, UserItemDataDto};
use ferrofin_model::entities::UserDataSaveReason;
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ContentPermissions, UserDataManager, UserDataSaveListener};

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::{supports_played_status, supports_position_ticks_resume};
use crate::user_data_key_repository::extra_type_name;
use crate::user_data_keys::{KeySource, user_data_keys, uses_provider_ids};

/// One tick is 100 nanoseconds; there are 10,000,000 ticks per second (the
/// .NET `TimeSpan.TicksPerSecond` the C# resume math uses).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The concrete user-data manager.
#[derive(Clone)]
pub struct FerrofinUserDataManager {
    db: Database,
    config: Arc<dyn ServerConfigurationManager>,
    save_listener: Arc<OnceLock<Weak<dyn UserDataSaveListener>>>,
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
        Self {
            db,
            config,
            save_listener: Arc::new(OnceLock::new()),
        }
    }

    /// Attaches the NFO consumer without retaining a strong reference cycle.
    ///
    /// # Errors
    /// Returns an error if startup tries to attach a second listener.
    pub fn set_save_listener(
        &self,
        listener: Weak<dyn UserDataSaveListener>,
    ) -> Result<(), ServiceError> {
        self.save_listener
            .set(listener)
            .map_err(|_| ServiceError::backend("user-data save listener already attached"))
    }

    async fn notify_saved(&self, user_id: Uuid, item_id: Uuid, reason: UserDataSaveReason) {
        let Some(listener) = self.save_listener.get().and_then(Weak::upgrade) else {
            return;
        };
        let mut notification = Box::pin(async move {
            if let Err(error) = listener.user_data_saved(user_id, item_id, reason).await {
                // Source NfoUserDataSaver catches export errors after commit.
                tracing::warn!(%user_id, %item_id, %error, "user-data NFO saver failed");
            }
        });
        // The pinned event handler is async void: it starts inline, then the
        // user-data caller returns at its first incomplete await. Own the
        // remainder in the runtime so slow NFO I/O does not delay HTTP writes.
        let first_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(notification.as_mut().poll(cx))).await;
        if first_poll.is_pending() {
            drop(tokio::spawn(notification));
        }
    }

    /// Resolves attached rows using the current item's source key order.
    /// Provider keys precede the own GUID; an unmatched legacy row remains a
    /// fallback rather than being filtered out before resolution.
    async fn read_row(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserDataEntity>, ServiceError> {
        let keys = self.keys_for(item_id).await?;
        let rows = sqlx::query_as::<_, UserDataEntity>(
            r#"SELECT * FROM "UserData" WHERE "ItemId" = ?1 AND "UserId" = ?2"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(Self::preferred_row(&rows, &keys))
    }

    /// Pinned ResolveUserDataRow: exact current-key priority, then the first
    /// attached candidate when none matches. No own-GUID override is applied.
    fn preferred_row(rows: &[UserDataEntity], keys: &[String]) -> Option<UserDataEntity> {
        keys.iter()
            .find_map(|key| rows.iter().find(|row| row.custom_data_key == *key))
            .or_else(|| rows.first())
            .cloned()
    }

    /// The `CustomDataKey`s this item's rows are written under.
    ///
    /// Port of the `item.GetUserDataKeys()` that `UserDataManager.SaveUserData`
    /// iterates. The key is **not** the item id: Jellyfin derives a list from
    /// the item's metadata and writes one row per key, so an adopted database
    /// holds provider-keyed rows — a movie under its TMDB id, an episode under
    /// its series' TVDB id plus `SSSEEE`. Writing only the guid row leaves
    /// those stale, and Jellyfin reads them, which is how a favourite set here
    /// disappears on a swap back.
    ///
    /// An item with no `BaseItems` row has nothing to derive from and gets its
    /// id alone — the same single key this manager used before.
    ///
    /// A **database error propagates** rather than degrading to that fallback.
    /// Degrading looks safe and is not: on an adopted library it would write
    /// the new value to the guid row while the provider rows Jellyfin actually
    /// reads keep the old one — the split-brain the single transaction below
    /// exists to prevent. A failed save the caller can retry beats a save that
    /// half-succeeded silently.
    async fn keys_for(&self, item_id: Uuid) -> Result<Vec<String>, ServiceError> {
        let mut conn = self.db.pool().acquire().await.map_err(db_err)?;
        Ok(
            crate::user_data_key_repository::load_keys(&mut conn, item_id)
                .await?
                .unwrap_or_else(|| vec![item_id.to_string()]),
        )
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

    /// Inserts or updates the rows for an item/user pair from the supplied
    /// [`UserDataEntity`] — **one row per `CustomDataKey`**.
    ///
    /// Port of `UserDataManager.SaveUserData`, which does
    /// `foreach (var key in item.GetUserDataKeys())` inside one transaction.
    /// Writing only the row the caller named would leave an adopted database's
    /// provider-keyed rows holding stale values, and those are the rows
    /// Jellyfin reads. The caller's `custom_data_key` is ignored in favour of
    /// the derived set, which always ends with the item id — so the row a
    /// caller expected is always among those written.
    ///
    /// All keys go in **one transaction**: a favourite that reached the TMDB
    /// row but not the IMDb row is precisely the split-brain state this exists
    /// to prevent.
    async fn upsert_row(&self, row: &UserDataEntity) -> Result<(), ServiceError> {
        let item_id = Uuid::parse_str(&row.item_id).ok();
        let keys = match item_id {
            Some(id) => self.keys_for(id).await?,
            // An unparseable id cannot be looked up; honour what the caller
            // asked for rather than dropping the write.
            None => vec![row.custom_data_key.clone()],
        };

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for key in &keys {
            Self::upsert_one(&mut tx, row, key).await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    /// One `(ItemId, UserId, CustomDataKey)` row.
    ///
    /// ONE statement, not a `SELECT EXISTS` followed by a branch: this is the
    /// busiest write path on the server (every playback progress report, every
    /// favorite/rating toggle), so two requests routinely reach it for the same
    /// `(item, user)` at once. Read-then-branch let both see "absent" and both
    /// run the `INSERT`, and the loser failed `PK_UserData` — a 500 on a
    /// playback report. `ON CONFLICT … DO UPDATE` resolves that inside SQLite.
    /// `RetentionDate` stays untouched on the update leg, exactly as the
    /// previous `UPDATE` did.
    async fn upsert_one(
        tx: &mut sqlx::SqliteConnection,
        row: &UserDataEntity,
        custom_data_key: &str,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"INSERT INTO "UserData"
                ("ItemId", "UserId", "CustomDataKey", "AudioStreamIndex",
                 "IsFavorite", "LastPlayedDate", "Likes", "PlayCount",
                 "PlaybackPositionTicks", "Played", "Rating", "SubtitleStreamIndex")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
               ON CONFLICT("ItemId", "UserId", "CustomDataKey") DO UPDATE SET
                 "AudioStreamIndex" = excluded."AudioStreamIndex",
                 "IsFavorite" = excluded."IsFavorite",
                 "LastPlayedDate" = excluded."LastPlayedDate",
                 "Likes" = excluded."Likes",
                 "PlayCount" = excluded."PlayCount",
                 "PlaybackPositionTicks" = excluded."PlaybackPositionTicks",
                 "Played" = excluded."Played",
                 "Rating" = excluded."Rating",
                 "SubtitleStreamIndex" = excluded."SubtitleStreamIndex""#,
        )
        .bind(&row.item_id)
        .bind(&row.user_id)
        .bind(custom_data_key)
        .bind(row.audio_stream_index)
        .bind(row.is_favorite)
        .bind(opt_datetime_to_db(row.last_played_date))
        .bind(row.rating.map(|rating| rating >= 6.5))
        .bind(row.play_count)
        .bind(row.playback_position_ticks)
        .bind(row.played)
        .bind(row.rating)
        .bind(row.subtitle_stream_index)
        .execute(&mut *tx)
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

/// The synthetic `UserData.Key` for an item that has **no** stored row.
///
/// Port of the fallback in C# `UserDataManager.GetUserData(User, BaseItem)`:
///
/// ```csharp
/// return item.UserData?...FirstOrDefault()
///        ?? new UserItemData { Key = item.GetUserDataKeys()[0] };
/// ```
///
/// So a `Year` reports `"Year-2020"`, a `Studio` `"Studio-Acme"`, an `Audio` its
/// `<artist>-<album>-…` composite and an `Episode` its series' key with the
/// `SSSEEE` suffix — never the item guid, which is only ever the *last* key in
/// the list. Ferrofin used to answer with the guid on every row.
///
/// The item's own providers follow its query hydration. Episode and Season
/// resolve their actual Series independently: pinned Series uses GetItemById,
/// which loads providers even when this browse page omitted ProviderIds.
fn row_data_keys(
    item: &BaseItemEntity,
    item_id: Uuid,
    providers: &HashMap<Uuid, Vec<(String, String)>>,
    series_keys: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let kind = kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder);
    let own_providers = providers.get(&item_id).map_or(&[][..], Vec::as_slice);
    let source = key_source(item, item_id, kind, own_providers);
    let mut keys = user_data_keys(&source, None);
    let Some(series) = series_keys.get(&guid_to_db(item_id)) else {
        return keys;
    };
    let derived: Vec<String> = match kind {
        BaseItemKind::Season => series
            .iter()
            .map(|key| format!("{key}{:03}", item.index_number.unwrap_or(0)))
            .collect(),
        BaseItemKind::Episode => {
            let (Some(season), Some(episode)) = (item.parent_index_number, item.index_number)
            else {
                return keys;
            };
            // Episode omits the Series own GUID when provider keys exist;
            // Season keeps every Series key. Both retain the child's GUID.
            let take = if series.len() > 1 {
                series.len() - 1
            } else {
                series.len()
            };
            series
                .iter()
                .take(take)
                .map(|key| format!("{key}{season:03}{episode:03}"))
                .collect()
        }
        _ => return keys,
    };
    keys.splice(0..0, derived);
    keys
}

/// The key-derivation view of a `BaseItems` row.
fn key_source<'a>(
    item: &'a BaseItemEntity,
    item_id: Uuid,
    kind: BaseItemKind,
    providers: &'a [(String, String)],
) -> KeySource<'a> {
    KeySource {
        item_id,
        kind,
        tmdb: provider_value(providers, "Tmdb"),
        imdb: provider_value(providers, "Imdb"),
        tvdb: provider_value(providers, "Tvdb"),
        custom: provider_value(providers, "Custom"),
        musicbrainz_album: provider_value(providers, "MusicBrainzAlbum"),
        musicbrainz_release_group: provider_value(providers, "MusicBrainzReleaseGroup"),
        musicbrainz_artist: provider_value(providers, "MusicBrainzArtist"),
        episode_title: item.episode_title.as_deref(),
        is_series: item.is_series,
        index_number: item.index_number,
        parent_index_number: item.parent_index_number,
        name: item.name.as_deref(),
        album: item.album.as_deref(),
        // `AlbumArtists` is a delimited list; the C# key uses the first.
        album_artist: item
            .album_artists
            .as_deref()
            .and_then(|a| a.split('|').next())
            .filter(|a| !a.is_empty()),
        extra_type: item.extra_type.and_then(extra_type_name),
        run_time_ticks: item.run_time_ticks,
    }
}

/// Looks a provider id up case-insensitively, the way C# `TryGetProviderId` does.
fn provider_value<'a>(providers: &'a [(String, String)], want: &str) -> Option<&'a str> {
    providers
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(want))
        .map(|(_, v)| v.as_str())
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
        likes: row.rating.map(|rating| rating >= 6.5),
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
        self.save_user_data_with_reason(
            user_id,
            item_id,
            user_data,
            UserDataSaveReason::UpdateUserData,
        )
        .await
    }

    async fn save_user_data_with_reason(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &UpdateUserItemDataDto,
        reason: UserDataSaveReason,
    ) -> Result<(), ServiceError> {
        // Preserve the source comparisons, including its NaN behavior.
        #[allow(
            clippy::manual_range_contains,
            reason = "the source Rating setter allows NaN"
        )]
        let invalid_rating = user_data
            .rating
            .is_some_and(|rating| rating < 0.0 || rating > 10.0);
        if invalid_rating {
            return Err(ServiceError::invalid_input(
                "a 0 to 10 user rating is required",
            ));
        }
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
        if let Some(likes) = user_data.likes {
            row.likes = Some(likes);
            row.rating = Some(if likes { 10.0 } else { 1.0 });
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

        self.upsert_row(&row).await?;
        self.notify_saved(user_id, item_id, reason).await;
        Ok(())
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
        let mut map = self.stored_user_data_dtos(item_ids, user_id).await?;
        // Items without a stored row get the empty-row DTO, matching the
        // per-item path's `unwrap_or_else(empty_row)` fallback. With only an id
        // in hand the synthetic key can only be the guid; the row-aware form
        // (`get_user_data_dtos_for_rows`) derives the real one.
        for &item_id in item_ids {
            map.entry(item_id)
                .or_insert_with(|| to_dto(&Self::empty_row(item_id, user_id), item_id));
        }
        Ok(map)
    }

    async fn get_user_data_dtos_for_rows(
        &self,
        items: &[BaseItemEntity],
        user_id: Uuid,
        include_provider_ids: bool,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let ids: Vec<Uuid> = items
            .iter()
            .filter_map(|item| Uuid::parse_str(&item.id).ok())
            .collect();
        let mut stored = self.stored_user_data_rows(&ids, user_id).await?;
        let mut children: Vec<String> = items
            .iter()
            .filter(|item| {
                matches!(
                    kind_from_type_name(&item.type_),
                    Some(BaseItemKind::Season | BaseItemKind::Episode)
                )
            })
            .filter_map(|item| Uuid::parse_str(&item.id).ok().map(guid_to_db))
            .collect();
        children.sort_unstable();
        children.dedup();
        let series_keys = if children.is_empty() {
            HashMap::new()
        } else {
            let mut conn = self.db.pool().acquire().await.map_err(db_err)?;
            crate::user_data_key_repository::load_child_series_keys(&mut conn, &children).await?
        };
        let providers = if include_provider_ids {
            let mut wanted: Vec<Uuid> = items
                .iter()
                .filter(|item| {
                    uses_provider_ids(
                        kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder),
                    )
                })
                .filter_map(|item| Uuid::parse_str(&item.id).ok())
                .collect();
            wanted.sort_unstable();
            wanted.dedup();
            self.provider_ids_batch(&wanted).await?
        } else {
            HashMap::new()
        };
        let mut map = HashMap::with_capacity(items.len());
        for item in items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            let current = row_data_keys(item, id, &providers, &series_keys);
            let candidates = stored.remove(&id).unwrap_or_default();
            let row = Self::preferred_row(&candidates, &current).unwrap_or_else(|| {
                let mut row = Self::empty_row(id, user_id);
                row.custom_data_key = current.first().cloned().unwrap_or_default();
                row
            });
            map.insert(id, to_dto(&row, id));
        }
        Ok(map)
    }

    async fn get_user_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        // Identical semantics to the per-item loop this used to run (the stored
        // row when present, the empty row otherwise) in one chunked `IN` query —
        // the loop was an N+1 that issued one round trip per candidate item
        // (~100 per `/Items/Latest` request, which post-filters the whole
        // candidate set by played state).
        self.get_user_data_dtos(item_ids, user_id).await
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
        row.rating = likes.map(|likes| if likes { 10.0 } else { 1.0 });
        self.upsert_row(&row).await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::UpdateUserRating)
            .await;
        Ok(to_dto(&row, item_id))
    }

    async fn update_play_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        self.update_play_state_with_reason(
            user_id,
            item_id,
            reported_position_ticks,
            UserDataSaveReason::PlaybackProgress,
        )
        .await
    }

    async fn update_play_state_with_reason(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        reason: UserDataSaveReason,
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

        let Some(mut position_ticks) = reported_position_ticks else {
            if reason == UserDataSaveReason::PlaybackFinished {
                // Source SessionManager's successful stop without a position
                // counts a completed play before its PlaybackFinished event.
                row.play_count += 1;
                row.played = supports_played_status(kind);
                row.playback_position_ticks = 0;
                self.upsert_row(&row).await?;
                self.notify_saved(user_id, item_id, reason).await;
                return Ok(true);
            }
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
        self.notify_saved(user_id, item_id, reason).await;

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
        self.notify_saved(user_id, item_id, UserDataSaveReason::TogglePlayed)
            .await;
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
        self.notify_saved(user_id, item_id, UserDataSaveReason::TogglePlayed)
            .await;
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

    async fn record_playback_start(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Port of the user-data half of C# SessionManager.OnPlaybackStart:
        // PlayCount++, LastPlayedDate = now, and non-resumable kinds (photos,
        // books — anything without position-ticks resume) are played outright.
        // The LastPlayedDate stamp is what Next Up's recently-watched HAVING
        // filter reads; the stop-path `update_play_state` deliberately never
        // writes it, exactly like upstream.
        let mut row = self
            .read_row(item_id, user_id)
            .await?
            .unwrap_or_else(|| Self::empty_row(item_id, user_id));
        row.play_count += 1;
        row.last_played_date = Some(chrono::Utc::now());
        if let Some((_, kind)) = self.item_runtime_and_kind(item_id).await?
            && supports_played_status(kind)
            && !supports_position_ticks_resume(kind)
        {
            row.played = true;
        }
        self.upsert_row(&row).await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::PlaybackStart)
            .await;
        Ok(())
    }

    async fn get_content_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ContentPermissions>, ServiceError> {
        use ferrofin_db::enums::{PermissionKind, PreferenceKind};
        // One indexed read over both `(UserId, Kind)` tables, the shape
        // `user_manager::load_permission_and_preference_maps` uses: the five
        // permissions and the "Allow media deletion from" list (stored
        // `,`-delimited, as C# writes it).
        let rows: Vec<(i64, i32, String)> = sqlx::query_as(
            r#"SELECT 0, "Kind", CAST("Value" AS TEXT) FROM "Permissions"
               WHERE "UserId" = ?1 AND "Kind" IN (?2, ?3, ?4, ?5, ?6)
               UNION ALL
               SELECT 1, "Kind", "Value" FROM "Preferences"
               WHERE "UserId" = ?1 AND "Kind" = ?7"#,
        )
        .bind(guid_to_db(user_id))
        .bind(i32::from(PermissionKind::IsAdministrator))
        .bind(i32::from(PermissionKind::EnableContentDeletion))
        .bind(i32::from(PermissionKind::EnableContentDownloading))
        .bind(i32::from(PermissionKind::EnableCollectionManagement))
        .bind(i32::from(PermissionKind::EnableMediaPlayback))
        .bind(i32::from(PreferenceKind::EnableContentDeletionFromFolders))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let has = |kind: PermissionKind| {
            rows.iter()
                .any(|(src, k, v)| *src == 0 && *k == i32::from(kind) && v == "1")
        };
        let content_deletion_folders = rows
            .iter()
            .filter(|(src, ..)| *src == 1)
            .flat_map(|(_, _, value)| value.split(','))
            .filter_map(|id| Uuid::parse_str(id.trim()).ok())
            .collect();
        Ok(Some(ContentPermissions {
            is_administrator: has(PermissionKind::IsAdministrator),
            enable_media_playback: has(PermissionKind::EnableMediaPlayback),
            enable_content_deletion: has(PermissionKind::EnableContentDeletion),
            enable_content_downloading: has(PermissionKind::EnableContentDownloading),
            enable_collection_management: has(PermissionKind::EnableCollectionManagement),
            content_deletion_folders,
        }))
    }

    async fn get_playback_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ferrofin_traits::library::PlaybackPermissions>, ServiceError> {
        // Kind 8 = EnableAudioPlaybackTranscoding, 9 =
        // EnableVideoPlaybackTranscoding, 19 = EnablePlaybackRemuxing
        // (`PermissionKind`). One indexed read for all three, because the
        // overwrite reads all three on one request.
        let rows: Vec<(i32, bool)> = sqlx::query_as(
            r#"SELECT "Kind", "Value" FROM "Permissions"
               WHERE "UserId" = ?1 AND "Kind" IN (8, 9, 19)"#,
        )
        .bind(guid_to_db(user_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        if rows.is_empty() {
            // No permission rows at all is "no policy known" — an unknown user
            // id, whose sources upstream leaves untouched because `user` is
            // null. Reporting three `false`s here would instead tell a client
            // the item can neither be remuxed nor transcoded.
            return Ok(None);
        }
        let has = |kind: i32| rows.iter().any(|(k, v)| *k == kind && *v);
        Ok(Some(ferrofin_traits::library::PlaybackPermissions {
            video_transcoding: has(9),
            audio_transcoding: has(8),
            remuxing: has(19),
        }))
    }
}

impl FerrofinUserDataManager {
    /// The `BaseItemProviders` rows for `ids`, keyed by item id — one chunked
    /// `IN` read instead of the per-item query the write path makes.
    async fn provider_ids_batch(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<(String, String)>>, ServiceError> {
        let mut out: HashMap<Uuid, Vec<(String, String)>> = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" IN ({placeholders})"#,
            );
            let mut query = sqlx::query_as::<_, (String, String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (item_id, provider, value) in
                query.fetch_all(self.db.pool()).await.map_err(db_err)?
            {
                if let Ok(id) = Uuid::parse_str(&item_id) {
                    out.entry(id).or_default().push((provider, value));
                }
            }
        }
        Ok(out)
    }

    /// Attached candidate rows for a page, grouped without a per-item query.
    async fn stored_user_data_rows(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, Vec<UserDataEntity>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<UserDataEntity>> = HashMap::with_capacity(item_ids.len());
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (2..=chunk.len() + 1)
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT * FROM "UserData" WHERE "UserId" = ?1 AND "ItemId" IN ({placeholders})"#
            );
            let mut query = sqlx::query_as::<_, UserDataEntity>(sqlx::AssertSqlSafe(sql))
                .bind(guid_to_db(user_id));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
            for row in rows {
                if let Ok(id) = Uuid::parse_str(&row.item_id) {
                    map.entry(id).or_default().push(row);
                }
            }
        }
        Ok(map)
    }

    /// Id-only reads resolve current keys once per page through the existing
    /// metadata/provider batch seam, matching per-item selection without an N+1.
    async fn stored_user_data_dtos(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let rows = self.stored_user_data_rows(item_ids, user_id).await?;
        if rows.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<_> = rows.keys().copied().map(guid_to_db).collect();
        let mut connection = self.db.pool().acquire().await.map_err(db_err)?;
        let keys: HashMap<_, _> =
            crate::user_data_key_repository::load_keys_for_items(&mut connection, &ids)
                .await?
                .into_iter()
                .collect();
        Ok(rows
            .into_iter()
            .filter_map(|(id, candidates)| {
                let own = [id.to_string()];
                let current = keys
                    .get(&guid_to_db(id))
                    .map_or(own.as_slice(), Vec::as_slice);
                Self::preferred_row(&candidates, current).map(|row| (id, to_dto(&row, id)))
            })
            .collect())
    }
}

/// Test-only: grants `user` the given permission rows and, when `folders` is
/// not empty, the "Allow media deletion from" list — the rows
/// [`UserDataManager::get_content_permissions`] reads — keeping the raw SQL
/// inside the repository boundary.
#[cfg(test)]
pub(crate) async fn seed_content_permissions(
    db: &Database,
    user: Uuid,
    permissions: &[ferrofin_db::enums::PermissionKind],
    folders: &[Uuid],
) {
    for kind in permissions {
        sqlx::query(
            r#"INSERT INTO "Permissions" ("Kind", "Value", "UserId", "RowVersion")
               VALUES (?1, 1, ?2, 0)"#,
        )
        .bind(i32::from(*kind))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("seed permission");
    }
    if !folders.is_empty() {
        let value = folders
            .iter()
            .map(|f| f.simple().to_string())
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(
            r#"INSERT INTO "Preferences" ("Kind", "Value", "UserId", "RowVersion")
               VALUES (?1, ?2, ?3, 0)"#,
        )
        .bind(i32::from(
            ferrofin_db::enums::PreferenceKind::EnableContentDeletionFromFolders,
        ))
        .bind(value)
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("seed deletion folders");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use crate::test_support::{
        fetch_item, seed_item, seed_named_item, seed_provider_id, seed_user, test_db,
    };
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

        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(self.config.clone()))
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

    #[rstest::rstest]
    #[case::movie(BaseItemKind::Movie)]
    #[case::episode(BaseItemKind::Episode)]
    #[case::season(BaseItemKind::Season)]
    #[case::audio(BaseItemKind::Audio)]
    #[case::audiobook(BaseItemKind::AudioBook)]
    #[tokio::test]
    async fn retention_moves_agree_in_individual_and_batch_dtos(#[case] kind: BaseItemKind) {
        use ferrofin_traits::persistence::ItemPersistenceService;
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        for item in [old, new] {
            seed_item(&db, item, kind).await;
            let mut row = fetch_item(&db, item).await;
            row.path = Some(format!("/media/{item}/file"));
            row.name = Some("Track".to_owned());
            row.album = Some("Album".to_owned());
            row.album_artists = Some("Artist|Other".to_owned());
            row.index_number = Some(2);
            row.parent_index_number = Some(1);
            if matches!(kind, BaseItemKind::Episode | BaseItemKind::Season) {
                let series = Uuid::new_v4();
                seed_item(&db, series, BaseItemKind::Series).await;
                seed_provider_id(&db, series, "Tvdb", "retained-series").await;
                row.series_id = Some(guid_to_db(series));
            }
            persistence.save_items(&[row]).await.unwrap();
            if kind == BaseItemKind::Movie {
                seed_provider_id(&db, item, "Tmdb", "retained-movie").await;
            }
        }
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        manager
            .save_user_data(
                user,
                old,
                &UpdateUserItemDataDto {
                    played: Some(true),
                    is_favorite: Some(true),
                    play_count: Some(4),
                    playback_position_ticks: Some(1_234_567),
                    likes: Some(false),
                    rating: Some(8.5),
                    ..UpdateUserItemDataDto::default()
                },
            )
            .await
            .unwrap();
        persistence.delete_items(&[old]).await.unwrap();
        persistence.reattach_all_user_data().await.unwrap();
        let one = manager.get_user_data_dto(new, user).await.unwrap().unwrap();
        let batch = manager.get_user_data_dtos(&[new], user).await.unwrap();
        assert_eq!(one, batch[&new]);
        assert!(one.played && one.is_favorite);
        assert_eq!(one.play_count, 4);
        assert_eq!(one.playback_position_ticks, 1_234_567);
        // A subsequent write keeps every restored alias in sync.
        manager.mark_unplayed(user, new).await.unwrap();
        let next = manager.get_user_data_dto(new, user).await.unwrap().unwrap();
        assert!(!next.played);
        assert!(next.is_favorite);
        assert_eq!(
            next,
            manager.get_user_data_dtos(&[new], user).await.unwrap()[&new]
        );
    }

    #[rstest::rstest]
    #[case::source_first(false)]
    #[case::destination_first(true)]
    #[tokio::test]
    async fn retention_round_trip_does_not_revive_old_guid_state(#[case] destination_first: bool) {
        use ferrofin_traits::persistence::ItemPersistenceService;
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for item in [a, b] {
            seed_item(&db, item, BaseItemKind::Movie).await;
            seed_provider_id(&db, item, "Tmdb", "round-trip").await;
        }
        manager
            .save_user_data(
                user,
                a,
                &UpdateUserItemDataDto {
                    played: Some(true),
                    is_favorite: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        persistence.delete_items(&[a]).await.unwrap();
        persistence.reattach_all_user_data().await.unwrap();
        assert!(
            manager
                .get_user_data_dto(b, user)
                .await
                .unwrap()
                .unwrap()
                .played
        );
        manager.mark_unplayed(user, b).await.unwrap();

        if !destination_first {
            persistence.delete_items(&[b]).await.unwrap();
        }
        seed_item(&db, a, BaseItemKind::Movie).await;
        seed_provider_id(&db, a, "Tmdb", "round-trip").await;
        persistence
            .reattach_user_data(&fetch_item(&db, a).await)
            .await
            .unwrap();
        if destination_first {
            persistence.delete_items(&[b]).await.unwrap();
        }
        persistence.reattach_all_user_data().await.unwrap();
        let restored = manager.get_user_data_dto(a, user).await.unwrap().unwrap();
        assert!(
            !restored.played,
            "the later explicit unplayed state survives"
        );
        assert!(restored.is_favorite);
        let retained: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots""#)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(retained, 0, "consume the complete source snapshot");
    }

    #[rstest::rstest]
    #[case::same_page(false)]
    #[case::different_page(true)]
    #[tokio::test]
    async fn retention_existing_copy_does_not_consume_another_users_recovery(
        #[case] different_page: bool,
    ) {
        use ferrofin_traits::persistence::ItemPersistenceService;
        let db = test_db().await;
        let users = [Uuid::new_v4(), Uuid::new_v4()];
        for user in users {
            crate::test_support::seed_named_user(&db, user, &user.to_string()).await;
        }
        let existing = Uuid::from_u128(100);
        let destination = Uuid::from_u128(900);
        let old = Uuid::from_u128(1000);
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for item in [existing, destination, old] {
            seed_item(&db, item, BaseItemKind::Movie).await;
            seed_provider_id(&db, item, "Tmdb", "multiple-copies").await;
        }
        if different_page {
            for id in 101..=601 {
                seed_item(&db, Uuid::from_u128(id), BaseItemKind::Movie).await;
            }
        }
        for user in users {
            manager
                .save_user_data(
                    user,
                    old,
                    &UpdateUserItemDataDto {
                        played: Some(true),
                        is_favorite: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        manager.mark_unplayed(users[0], existing).await.unwrap();
        persistence.delete_items(&[old]).await.unwrap();
        persistence.reattach_all_user_data().await.unwrap();
        assert!(
            !manager
                .get_user_data_dto(existing, users[0])
                .await
                .unwrap()
                .unwrap()
                .played
        );
        assert!(
            manager
                .get_user_data_dto(existing, users[1])
                .await
                .unwrap()
                .unwrap()
                .played
        );
        let restored = manager
            .get_user_data_dto(destination, users[0])
            .await
            .unwrap()
            .unwrap();
        assert!(restored.played && restored.is_favorite);
        let retained: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots""#)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(retained, 0);
        persistence.reattach_all_user_data().await.unwrap();
        assert!(
            !manager
                .get_user_data_dto(existing, users[0])
                .await
                .unwrap()
                .unwrap()
                .played
        );
    }

    #[rstest::rstest]
    #[case::different_kind(BaseItemKind::Series, "Tvdb")]
    #[case::different_provider(BaseItemKind::Movie, "Imdb")]
    #[tokio::test]
    async fn retention_equal_provider_values_keep_distinct_snapshots(
        #[case] other_kind: BaseItemKind,
        #[case] other_provider: &str,
    ) {
        use ferrofin_traits::persistence::ItemPersistenceService;
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let [other_new, movie_new, movie_old, other_old] =
            [100, 900, 1000, 1100].map(Uuid::from_u128);
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for (item, kind, provider) in [
            (movie_old, BaseItemKind::Movie, "Tmdb"),
            (movie_new, BaseItemKind::Movie, "Tmdb"),
            (other_old, other_kind, other_provider),
            (other_new, other_kind, other_provider),
        ] {
            seed_item(&db, item, kind).await;
            seed_provider_id(&db, item, provider, "949").await;
        }
        manager
            .save_user_data(
                user,
                movie_old,
                &UpdateUserItemDataDto {
                    played: Some(true),
                    is_favorite: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        manager
            .save_user_data(
                user,
                other_old,
                &UpdateUserItemDataDto {
                    played: Some(false),
                    is_favorite: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        persistence.delete_items(&[movie_old]).await.unwrap();
        persistence
            .reattach_user_data(&fetch_item(&db, other_new).await)
            .await
            .unwrap();
        assert!(
            manager.read_row(other_new, user).await.unwrap().is_none(),
            "equal provider values must not cross media or provider namespaces"
        );
        // Both detached snapshots now have the same compatible bare key.
        persistence.delete_items(&[other_old]).await.unwrap();
        persistence.reattach_all_user_data().await.unwrap();
        let movie = manager.read_row(movie_new, user).await.unwrap().unwrap();
        let other = manager.read_row(other_new, user).await.unwrap().unwrap();
        assert!(movie.played && !movie.is_favorite);
        assert!(!other.played && other.is_favorite);
        assert!(movie.retention_date.is_none() && other.retention_date.is_none());
        let retained: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots""#)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(retained, 0);
    }

    #[tokio::test]
    async fn retention_legacy_provider_only_rows_are_not_guessed() {
        use ferrofin_traits::persistence::ItemPersistenceService;
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let movie = Uuid::new_v4();
        let series = Uuid::new_v4();
        for (item, kind, provider) in [
            (movie, BaseItemKind::Movie, "Tmdb"),
            (series, BaseItemKind::Series, "Tvdb"),
        ] {
            seed_item(&db, item, kind).await;
            seed_provider_id(&db, item, provider, "949").await;
        }
        crate::test_support::seed_user_data(&db, user, Uuid::from_u128(1), true, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "CustomDataKey" = '949'"#)
            .execute(db.writer())
            .await
            .unwrap();
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        persistence
            .reattach_user_data(&fetch_item(&db, series).await)
            .await
            .unwrap();
        persistence.reattach_all_user_data().await.unwrap();
        let owners: Vec<String> = sqlx::query_scalar(r#"SELECT "ItemId" FROM "UserData""#)
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(owners, vec![guid_to_db(Uuid::from_u128(1))]);
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

    /// The update a client sends when a user taps the heart.
    fn favorite_dto() -> UpdateUserItemDataDto {
        UpdateUserItemDataDto {
            is_favorite: Some(true),
            ..UpdateUserItemDataDto::default()
        }
    }

    /// Every `CustomDataKey` stored for an item, sorted.
    async fn stored_keys(db: &Database, item: Uuid) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            r#"SELECT "CustomDataKey" FROM "UserData" WHERE "ItemId" = ?1 ORDER BY 1"#,
        )
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("read keys")
    }

    /// A favourite must land on **every** key Jellyfin would read, not just the
    /// item's guid row.
    ///
    /// This is the drop-in data-loss bug: measured on a real library, Ferrofin
    /// wrote a third row under the guid while Jellyfin kept reading its TMDB
    /// and IMDb rows, so the favourite was invisible the moment the user
    /// swapped back.
    #[tokio::test]
    async fn a_favorite_is_written_under_every_provider_key() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;
        seed_provider_id(&db, item, "Imdb", "tt12261776").await;

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(user, item, &favorite_dto())
            .await
            .expect("favorite");

        // Sorted by key, so the all-zero test guid leads.
        assert_eq!(
            stored_keys(&db, item).await,
            vec![
                item.to_string(),
                "700391".to_owned(),
                "tt12261776".to_owned(),
            ]
        );
        let favorited: Vec<bool> =
            sqlx::query_scalar(r#"SELECT "IsFavorite" FROM "UserData" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item))
                .fetch_all(db.pool())
                .await
                .expect("read");
        assert!(favorited.iter().all(|f| *f), "every row carries it");
    }

    /// A row written by Jellyfin under a provider key alone must be readable.
    ///
    /// The guid row is normally present too (the id is the last key Jellyfin
    /// saves), so this is the case where it is not — an item whose default row
    /// was never written, which a guid-only lookup misses entirely.
    #[tokio::test]
    async fn a_provider_keyed_row_is_found_without_a_guid_row() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;

        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite",
                   "PlayCount","PlaybackPositionTicks","Played")
               VALUES (?1, ?2, '700391', 1, 4, 0, 1)"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("seed jellyfin row");

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("dto");
        assert!(dto.is_favorite, "the provider-keyed favourite is visible");
        assert_eq!(dto.play_count, 4);

        // And the batch/listing path must agree with the per-item one — they
        // disagreed while the batch filtered to `CustomDataKey = lower(ItemId)`.
        let batch = mgr.get_user_data_dtos(&[item], user).await.expect("batch");
        assert!(
            batch[&item].is_favorite,
            "listing agrees with the item view"
        );
        assert_eq!(batch[&item].play_count, 4);
    }

    /// An episode is keyed by its SERIES' provider ids plus `SSSEEE`, never its
    /// own — the shape a real Jellyfin database holds
    /// (`[<guid>, 273181001001, tt3032476001001]`).
    #[tokio::test]
    async fn an_episode_is_keyed_through_its_series() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let series = Uuid::from_u128(2);
        let episode = Uuid::from_u128(3);
        seed_user(&db, user).await;
        seed_item(&db, series, BaseItemKind::Series).await;
        seed_provider_id(&db, series, "Tvdb", "273181").await;
        seed_item(&db, episode, BaseItemKind::Episode).await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "ParentIndexNumber" = 1,
                   "IndexNumber" = 1 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(episode))
        .bind(guid_to_db(series))
        .execute(db.writer())
        .await
        .expect("link episode to series");

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(user, episode, &favorite_dto())
            .await
            .expect("favorite");

        // The episode's OWN provider ids are absent by construction — its keys
        // come from the series, suffixed with season/episode numbers.
        assert_eq!(
            stored_keys(&db, episode).await,
            vec![episode.to_string(), "273181001001".to_owned()]
        );
    }

    /// With several provider rows and no guid row, the highest-priority KEY
    /// wins — not whatever order SQLite happened to return.
    ///
    /// The rows are seeded so that key order and storage order disagree: TMDB
    /// leads the derived keys but `tt…` sorts first, so a `rows.first()` pick
    /// would return the IMDb row.
    #[tokio::test]
    async fn the_highest_priority_key_wins_when_rows_disagree() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;
        seed_provider_id(&db, item, "Imdb", "tt12261776").await;

        for (key, play_count) in [("700391", 7), ("tt12261776", 3)] {
            sqlx::query(
                r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite",
                       "PlayCount","PlaybackPositionTicks","Played")
                   VALUES (?1, ?2, ?3, 0, ?4, 0, 0)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(user))
            .bind(key)
            .bind(play_count)
            .execute(db.writer())
            .await
            .expect("seed row");
        }

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        let dto = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("dto");
        assert_eq!(dto.play_count, 7, "the TMDB row, which leads the keys");
    }

    /// A season is keyed through its series — the `SeriesId` hop plus the
    /// series' own provider fetch, neither of which any other test exercises.
    #[tokio::test]
    async fn a_season_is_keyed_through_its_series() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let series = Uuid::from_u128(2);
        let season = Uuid::from_u128(3);
        seed_user(&db, user).await;
        seed_item(&db, series, BaseItemKind::Series).await;
        seed_provider_id(&db, series, "Tvdb", "273181").await;
        seed_item(&db, season, BaseItemKind::Season).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "IndexNumber" = 2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(season))
            .bind(guid_to_db(series))
            .execute(db.writer())
            .await
            .expect("link season to series");

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(user, season, &favorite_dto())
            .await
            .expect("favorite");

        // A Season keeps the series' own guid key where an Episode drops it,
        // so all three keys are present.
        // Sorted by key: the series-derived guid key sorts before the season's
        // own id, which sorts before the numeric TVDB one.
        assert_eq!(
            stored_keys(&db, season).await,
            vec![
                format!("{series}002"),
                season.to_string(),
                "273181002".to_owned(),
            ]
        );
    }

    /// A by-name item is keyed by type and name, read out of `BaseItems`.
    ///
    /// Covers the `Name` column reaching the derivation at all — the ten
    /// by-name/music arms all depend on it and nothing else exercises it.
    #[tokio::test]
    async fn a_person_is_keyed_by_name_from_the_database() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let person = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, person, BaseItemKind::Person).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Name" = 'Beyoncé' WHERE "Id" = ?1"#)
            .bind(guid_to_db(person))
            .execute(db.writer())
            .await
            .expect("name the person");

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(user, person, &favorite_dto())
            .await
            .expect("favorite");

        // Diacritic-stripped, so this row is the one Jellyfin reads for
        // "Beyoncé". Sorted by key, so the all-zero test guid leads.
        assert_eq!(
            stored_keys(&db, person).await,
            vec![person.to_string(), "Person-Beyonce".to_owned()]
        );
    }

    /// An item with no providers still writes exactly one row, keyed by its id
    /// — the pre-existing behaviour, which must not regress.
    #[tokio::test]
    async fn a_provider_less_item_still_writes_one_row() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(user, item, &favorite_dto())
            .await
            .expect("favorite");
        assert_eq!(stored_keys(&db, item).await, vec![item.to_string()]);
    }

    /// Two concurrent first-time saves for the same `(item, user)` must both
    /// succeed. `upsert_row` used to `SELECT EXISTS` and then branch on the
    /// answer, so racing callers both saw "absent" and both ran the `INSERT` —
    /// the loser hit `PK_UserData` and the playback report 500'd.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_saves_do_not_collide() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = Arc::new(FerrofinUserDataManager::new(db, config()));

        let mut tasks = Vec::new();
        for i in 0..8_i64 {
            let mgr = Arc::clone(&mgr);
            tasks.push(tokio::spawn(async move {
                mgr.save_user_data(
                    user,
                    item,
                    &UpdateUserItemDataDto {
                        playback_position_ticks: Some(i * 100),
                        ..Default::default()
                    },
                )
                .await
            }));
        }
        for task in tasks {
            task.await
                .expect("join")
                .expect("a concurrent first save must not fail");
        }
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

        // `get_user_data_batch` is the same read (it was a per-item loop —
        // ~100 round trips per `/Items/Latest` request — and now delegates), so
        // it must return exactly the same map, empty rows included.
        let also_batch = mgr
            .get_user_data_batch(&[with_row, without_row], user)
            .await
            .expect("batch");
        assert_eq!(also_batch, batch);
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

    /// C# `GetUserData(User, BaseItem)` synthesizes the missing row with
    /// `Key = item.GetUserDataKeys()[0]` — `"Year-2020"` for a year, never the
    /// item guid. Ferrofin answered with the guid on every by-name row.
    #[tokio::test]
    async fn a_row_less_year_reports_its_derived_key_not_its_guid() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let year = Uuid::from_u128(0xF101);
        seed_user(&db, user).await;
        seed_named_item(&db, year, BaseItemKind::Year, "2020").await;
        let row = fetch_item(&db, year).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        let map = mgr
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, false)
            .await
            .expect("rows");
        assert_eq!(map[&year].key, "Year-2020");
        assert!(!map[&year].is_favorite);
    }

    /// The same rule across the by-name kinds — `Studio-…`, `Genre-…`,
    /// `Person-…` (`Genre.cs:37`, `Person.cs:40`, `Studio.cs:…`).
    #[tokio::test]
    async fn row_less_by_name_items_report_their_type_prefixed_keys() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let mut rows = Vec::new();
        for (n, kind, name, want) in [
            (0xF201_u128, BaseItemKind::Studio, "Acme", "Studio-Acme"),
            (0xF202, BaseItemKind::Genre, "Drama", "Genre-Drama"),
            (
                0xF203,
                BaseItemKind::Person,
                "Bob Parity",
                "Person-Bob Parity",
            ),
        ] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, kind, name).await;
            rows.push((id, fetch_item(&db, id).await, want));
        }
        let entities: Vec<_> = rows.iter().map(|(_, e, _)| e.clone()).collect();
        let mgr = FerrofinUserDataManager::new(db, config());

        let map = mgr
            .get_user_data_dtos_for_rows(&entities, user, false)
            .await
            .expect("rows");
        for (id, _, want) in &rows {
            assert_eq!(&map[id].key, want, "item {id}");
        }
    }

    /// An `Episode` is keyed through its SERIES plus `SSSEEE`
    /// (`TV/Episode.cs:158`), so a row-less episode reports
    /// `<series key>001002`, not its own guid.
    #[tokio::test]
    async fn a_row_less_episode_is_keyed_through_its_series() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let series = Uuid::from_u128(0xF401);
        let episode = Uuid::from_u128(0xF402);
        seed_user(&db, user).await;
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, episode, BaseItemKind::Episode, "Ep").await;
        sqlx::query(
            r#"UPDATE "BaseItems"
               SET "SeriesId" = ?2, "ParentIndexNumber" = 1, "IndexNumber" = 2
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(episode))
        .bind(guid_to_db(series))
        .execute(db.writer())
        .await
        .expect("link episode to series");
        let row = fetch_item(&db, episode).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        let map = mgr
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, false)
            .await
            .expect("rows");
        assert_eq!(map[&episode].key, format!("{series}001002"));
    }

    /// Provider ids join the derivation only when the caller says its query
    /// hydrated them (C# `.Include(e => e.Provider)`), which is what makes the
    /// same movie report its guid in a plain list and `tt…` by id.
    #[tokio::test]
    async fn provider_keys_appear_only_when_provider_ids_were_hydrated() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let movie = Uuid::from_u128(0xF501);
        seed_user(&db, user).await;
        seed_item(&db, movie, BaseItemKind::Movie).await;
        seed_provider_id(&db, movie, "Imdb", "tt0111161").await;
        let row = fetch_item(&db, movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        let listed = mgr
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, false)
            .await
            .expect("list");
        assert_eq!(listed[&movie].key, movie.to_string());

        let by_id = mgr
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, true)
            .await
            .expect("by id");
        assert_eq!(by_id[&movie].key, "tt0111161");
    }

    /// A stored row reports the first matching current key, including the
    /// name-derived key before the own GUID.
    #[tokio::test]
    async fn a_stored_row_uses_the_first_current_key() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let year = Uuid::from_u128(0xF301);
        seed_user(&db, user).await;
        seed_named_item(&db, year, BaseItemKind::Year, "2021").await;
        let row = fetch_item(&db, year).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        mgr.save_user_data(
            user,
            year,
            &UpdateUserItemDataDto {
                is_favorite: Some(true),
                ..UpdateUserItemDataDto::default()
            },
        )
        .await
        .expect("save");

        let map = mgr
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, false)
            .await
            .expect("rows");
        assert_eq!(map[&year].key, "Year-2021");
        assert!(map[&year].is_favorite);
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
    async fn record_playback_start_stamps_last_played_and_play_count() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x9);
        let item = Uuid::from_u128(0x51);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());

        // Two starts: PlayCount accumulates and LastPlayedDate lands — the
        // column Next Up's recently-watched filter reads (the bug was that a
        // normally-watched series never got the stamp, so Next Up was empty).
        mgr.record_playback_start(user, item).await.expect("start");
        mgr.record_playback_start(user, item).await.expect("start");
        let row = mgr.read_row(item, user).await.expect("read").expect("row");
        assert_eq!(row.play_count, 2);
        assert!(row.last_played_date.is_some());
        // A movie resumes by position, so a start alone never marks it played.
        assert!(!row.played);
    }

    #[tokio::test]
    async fn content_permissions_read_the_permission_rows() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x77);
        seed_user(&db, user).await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        // No rows: permissions known, all denied (falsy rows == absent rows).
        let perms = mgr
            .get_content_permissions(user)
            .await
            .expect("read")
            .expect("policy known");
        assert_eq!(perms, ContentPermissions::default());

        // Kind 10 = EnableContentDeletion granted, 11 = downloading denied,
        // 0 = IsAdministrator and 21 = EnableCollectionManagement granted, and
        // another user's rows ignored.
        let other = Uuid::from_u128(0x78);
        crate::test_support::seed_named_user(&db, other, "other").await;
        sqlx::query(
            r#"INSERT INTO "Permissions" ("Kind", "Value", "UserId", "RowVersion")
               VALUES (10, 1, ?1, 0), (11, 0, ?1, 0), (0, 1, ?1, 0), (21, 1, ?1, 0),
                      (7, 1, ?1, 0), (11, 1, ?2, 0)"#,
        )
        .bind(ferrofin_db::store::guid_to_db(user))
        .bind(ferrofin_db::store::guid_to_db(other))
        .execute(db.writer())
        .await
        .expect("seed permissions");
        // The "Allow media deletion from" list, `,`-delimited as C# writes it;
        // a value that is not a GUID is dropped.
        let library_a = Uuid::from_u128(0xA);
        let library_b = Uuid::from_u128(0xB);
        sqlx::query(
            r#"INSERT INTO "Preferences" ("Kind", "Value", "UserId", "RowVersion")
               VALUES (6, ?2, ?1, 0), (5, ?3, ?1, 0)"#,
        )
        .bind(ferrofin_db::store::guid_to_db(user))
        .bind(format!("{},junk,{}", library_a.simple(), library_b))
        .bind(Uuid::from_u128(0xC).to_string())
        .execute(db.writer())
        .await
        .expect("seed preferences");
        let perms = mgr
            .get_content_permissions(user)
            .await
            .expect("read")
            .expect("policy known");
        assert_eq!(
            perms,
            ContentPermissions {
                is_administrator: true,
                enable_media_playback: true,
                enable_content_deletion: true,
                enable_content_downloading: false,
                enable_collection_management: true,
                content_deletion_folders: vec![library_a, library_b],
            }
        );
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

    // Two clients reporting on the SAME item/user with no row yet must both
    // succeed. The read-then-INSERT-or-UPDATE this replaced could have both
    // callers observe "absent" and the loser's INSERT hit `PK_UserData` — a 500
    // mid-playback. The single `ON CONFLICT` upsert makes that unrepresentable.
    #[tokio::test]
    async fn concurrent_first_writes_to_one_row_all_succeed() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = Arc::new(FerrofinUserDataManager::new(db.clone(), config()));

        let mut writes = tokio::task::JoinSet::new();
        for i in 0..16 {
            let mgr = Arc::clone(&mgr);
            writes.spawn(async move {
                mgr.save_user_data(
                    user,
                    item,
                    &UpdateUserItemDataDto {
                        play_count: Some(i),
                        ..Default::default()
                    },
                )
                .await
            });
        }
        while let Some(joined) = writes.join_next().await {
            joined
                .expect("task panicked")
                .expect("concurrent save must not collide on the primary key");
        }

        // …and exactly one row exists afterwards.
        let rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "UserData""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(rows, 1);
    }

    /// The three playback permissions the media-source manager overwrites
    /// `SupportsTranscoding`/`SupportsDirectStream` from. A user id nobody
    /// knows must answer `None` — "no policy", which leaves the source alone —
    /// and never three `false`s, which would tell a client the item can be
    /// neither remuxed nor transcoded.
    #[tokio::test]
    async fn playback_permissions_come_from_the_users_own_rows() {
        use ferrofin_db::enums::PermissionKind;
        use ferrofin_traits::library::UserManager as _;

        let db = test_db().await;
        let users = crate::user_manager::FerrofinUserManager::new(db.clone());
        let user = users.create_user("erin").await.expect("create");
        let id = Uuid::parse_str(&user.id).expect("uuid");
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        // `AddDefaultPermissions` grants all three.
        let granted = mgr
            .get_playback_permissions(id)
            .await
            .expect("read")
            .expect("a known user has a policy");
        assert!(granted.video_transcoding && granted.audio_transcoding && granted.remuxing);

        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::EnablePlaybackRemuxing,
            false,
        )
        .await
        .expect("revoke");
        let revoked = mgr
            .get_playback_permissions(id)
            .await
            .expect("read")
            .expect("policy");
        assert!(!revoked.remuxing, "only the revoked one flips");
        assert!(revoked.video_transcoding && revoked.audio_transcoding);

        assert!(
            mgr.get_playback_permissions(Uuid::from_u128(0xdead))
                .await
                .expect("read")
                .is_none(),
            "an unknown user is 'no policy', not 'nothing permitted'"
        );
    }
    struct NfoSaveObserver {
        manager: Arc<FerrofinUserDataManager>,
        seen: std::sync::Mutex<Vec<(UserDataSaveReason, UserItemDataDto)>>,
        fail: bool,
        changed: tokio::sync::Notify,
    }

    impl NfoSaveObserver {
        async fn wait_seen(&self, count: usize) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let notified = self.changed.notified();
                    if self.seen.lock().unwrap().len() >= count {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .expect("owned NFO notification finishes");
        }
    }

    #[async_trait]
    impl UserDataSaveListener for NfoSaveObserver {
        async fn user_data_saved(
            &self,
            user: Uuid,
            item: Uuid,
            reason: UserDataSaveReason,
        ) -> Result<(), ServiceError> {
            let persisted = self.manager.get_user_data_dto(item, user).await?.unwrap();
            self.seen.lock().unwrap().push((reason, persisted));
            self.changed.notify_one();
            if self.fail {
                Err(ServiceError::backend("NFO destination is unavailable"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn nfo_notifications_keep_source_reasons_after_persistence_and_weak_ownership() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF001);
        seed_user(&db, user).await;
        seed_movie_with_runtime(&db, item, 12_000_000_000).await;
        let manager = Arc::new(FerrofinUserDataManager::new(db, config()));
        let observer = Arc::new(NfoSaveObserver {
            manager: Arc::clone(&manager),
            seen: std::sync::Mutex::new(Vec::new()),
            fail: false,
            changed: tokio::sync::Notify::new(),
        });
        let listener: Arc<dyn UserDataSaveListener> = observer.clone();
        let weak = Arc::downgrade(&listener);
        manager.set_save_listener(weak.clone()).unwrap();
        assert!(manager.set_save_listener(weak.clone()).is_err());
        manager
            .save_user_data(user, item, &favorite_dto())
            .await
            .unwrap();
        observer.wait_seen(1).await;
        manager.set_likes(user, item, Some(true)).await.unwrap();
        observer.wait_seen(2).await;
        manager.set_likes(user, item, Some(false)).await.unwrap();
        observer.wait_seen(3).await;
        manager.set_likes(user, item, None).await.unwrap();
        observer.wait_seen(4).await;
        manager.mark_played(user, item, None).await.unwrap();
        observer.wait_seen(5).await;
        manager.mark_unplayed(user, item).await.unwrap();
        observer.wait_seen(6).await;
        manager.record_playback_start(user, item).await.unwrap();
        observer.wait_seen(7).await;
        manager
            .update_play_state(user, item, Some(6_000_000_000))
            .await
            .unwrap();
        observer.wait_seen(8).await;
        manager
            .update_play_state_with_reason(
                user,
                item,
                Some(6_000_000_000),
                UserDataSaveReason::PlaybackFinished,
            )
            .await
            .unwrap();
        observer.wait_seen(9).await;
        manager
            .update_play_state_with_reason(user, item, None, UserDataSaveReason::PlaybackFinished)
            .await
            .unwrap();
        observer.wait_seen(10).await;
        {
            let seen = observer.seen.lock().unwrap();
            assert_eq!(
                seen.iter().map(|(reason, _)| *reason).collect::<Vec<_>>(),
                vec![
                    UserDataSaveReason::UpdateUserData,
                    UserDataSaveReason::UpdateUserRating,
                    UserDataSaveReason::UpdateUserRating,
                    UserDataSaveReason::UpdateUserRating,
                    UserDataSaveReason::TogglePlayed,
                    UserDataSaveReason::TogglePlayed,
                    UserDataSaveReason::PlaybackStart,
                    UserDataSaveReason::PlaybackProgress,
                    UserDataSaveReason::PlaybackFinished,
                    UserDataSaveReason::PlaybackFinished,
                ]
            );
            assert!(seen[0].1.is_favorite, "listener reads the committed write");
            assert_eq!(seen[1].1.rating, Some(10.0));
            assert_eq!(seen[2].1.rating, Some(1.0));
            assert_eq!(seen[3].1.rating, None);
            assert!(seen[4].1.played);
            assert!(!seen[5].1.played);
            assert_eq!(seen[7].1.playback_position_ticks, 6_000_000_000);
            assert!(seen[9].1.played);
            assert_eq!(seen[9].1.play_count, 2);
            assert_eq!(seen[9].1.playback_position_ticks, 0);
        }
        drop(listener);
        drop(observer);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned notification releases the weak listener");
        manager.mark_played(user, item, None).await.unwrap();
    }

    #[tokio::test]
    async fn nfo_listener_errors_do_not_undo_committed_data_and_failed_writes_do_not_notify() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF002);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let manager = Arc::new(FerrofinUserDataManager::new(db.clone(), config()));
        let observer = Arc::new(NfoSaveObserver {
            manager: Arc::clone(&manager),
            seen: std::sync::Mutex::new(Vec::new()),
            fail: true,
            changed: tokio::sync::Notify::new(),
        });
        let listener: Arc<dyn UserDataSaveListener> = observer.clone();
        manager
            .set_save_listener(Arc::downgrade(&listener))
            .unwrap();
        manager
            .save_user_data_with_reason(
                user,
                item,
                &favorite_dto(),
                UserDataSaveReason::UpdateUserRating,
            )
            .await
            .unwrap();
        observer.wait_seen(1).await;
        assert!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .is_favorite
        );
        assert_eq!(observer.seen.lock().unwrap().len(), 1);
        sqlx::query(r#"CREATE TRIGGER nfo_reject_userdata BEFORE INSERT ON "UserData" BEGIN SELECT RAISE(ABORT, 'fixture write rejection'); END"#).execute(db.writer()).await.unwrap();
        assert!(manager.mark_unplayed(user, item).await.is_err());
        assert_eq!(
            observer.seen.lock().unwrap().len(),
            1,
            "no notification before a failed transaction commits"
        );
        assert!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .is_favorite
        );
    }

    #[tokio::test]
    async fn selected_nfo_reads_prefer_current_provider_keys_and_keep_legacy_candidates() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF003);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "current-tmdb").await;
        seed_provider_id(&db, item, "Imdb", "current-imdb").await;
        for (key, count, favorite, rating) in [
            (item.to_string(), 1, false, 2.5),
            ("current-imdb".to_owned(), 3, false, 1.0),
            ("current-tmdb".to_owned(), 7, true, 8.5),
        ] {
            sqlx::query(r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "PlayCount", "IsFavorite", "Rating", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)"#).bind(guid_to_db(item)).bind(guid_to_db(user)).bind(key).bind(count).bind(favorite).bind(rating).execute(db.writer()).await.unwrap();
        }
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        let selected = manager
            .get_user_data_dto(item, user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.key, "current-tmdb");
        assert_eq!(selected.play_count, 7);
        assert!(selected.is_favorite);
        assert_eq!(selected.rating, Some(8.5));
        assert_eq!(
            manager.get_user_data_dtos(&[item], user).await.unwrap()[&item],
            selected
        );
        let row = fetch_item(&db, item).await;
        assert_eq!(
            manager
                .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, true)
                .await
                .unwrap()[&item],
            selected
        );
        let unhydrated = manager
            .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, false)
            .await
            .unwrap();
        assert_eq!(unhydrated[&item].key, item.to_string());
        assert_eq!(unhydrated[&item].play_count, 1);
        sqlx::query(r#"DELETE FROM "UserData" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item))
            .execute(db.writer())
            .await
            .unwrap();
        sqlx::query(r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "PlayCount", "IsFavorite", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, 'legacy-retained-key', 13, 1, 0, 0)"#).bind(guid_to_db(item)).bind(guid_to_db(user)).execute(db.writer()).await.unwrap();
        let retained = manager
            .get_user_data_dto(item, user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.key, "legacy-retained-key");
        assert_eq!(retained.play_count, 13);
        assert!(retained.is_favorite);
        assert_eq!(
            manager.get_user_data_dtos(&[item], user).await.unwrap()[&item],
            retained
        );
    }
    #[tokio::test]
    async fn user_rating_updates_follow_source_setter_order_and_validation() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF004);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let manager = FerrofinUserDataManager::new(db, config());
        manager
            .save_user_data(
                user,
                item,
                &UpdateUserItemDataDto {
                    likes: Some(false),
                    rating: Some(8.5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let rated = manager
            .get_user_data_dto(item, user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rated.rating, Some(8.5));
        assert_eq!(
            rated.likes,
            Some(true),
            "explicit Rating is applied after Likes and controls its getter"
        );
        for rating in [-0.01, 10.01] {
            assert!(
                manager
                    .save_user_data(
                        user,
                        item,
                        &UpdateUserItemDataDto {
                            rating: Some(rating),
                            ..Default::default()
                        }
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                manager
                    .get_user_data_dto(item, user)
                    .await
                    .unwrap()
                    .unwrap(),
                rated
            );
        }
        manager
            .save_user_data(
                user,
                item,
                &UpdateUserItemDataDto {
                    likes: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .rating,
            Some(1.0)
        );
        manager.set_likes(user, item, None).await.unwrap();
        let cleared = manager
            .get_user_data_dto(item, user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared.rating, None);
        assert_eq!(cleared.likes, None);
    }
    struct HeldNfoObserver {
        manager: Arc<FerrofinUserDataManager>,
        entered: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        finished: tokio::sync::Notify,
    }

    #[async_trait]
    impl UserDataSaveListener for HeldNfoObserver {
        async fn user_data_saved(
            &self,
            user: Uuid,
            item: Uuid,
            _reason: UserDataSaveReason,
        ) -> Result<(), ServiceError> {
            self.entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                self.manager
                    .get_user_data_dto(item, user)
                    .await?
                    .unwrap()
                    .is_favorite
            );
            self.started.notify_one();
            self.release.notified().await;
            self.finished.notify_one();
            Err(ServiceError::backend("held NFO destination failed"))
        }
    }

    #[tokio::test]
    async fn nfo_async_notification_returns_after_first_yield_with_committed_state_readable() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF005);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let manager = Arc::new(FerrofinUserDataManager::new(db, config()));
        let observer = Arc::new(HeldNfoObserver {
            manager: Arc::clone(&manager),
            entered: std::sync::atomic::AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
        });
        let listener: Arc<dyn UserDataSaveListener> = observer.clone();
        let weak = Arc::downgrade(&listener);
        manager.set_save_listener(weak.clone()).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            manager.save_user_data_with_reason(
                user,
                item,
                &favorite_dto(),
                UserDataSaveReason::UpdateUserRating,
            ),
        )
        .await
        .expect("user-data write cannot wait for held NFO I/O")
        .unwrap();
        assert!(
            observer.entered.load(std::sync::atomic::Ordering::SeqCst),
            "async void listener starts before return"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            observer.started.notified(),
        )
        .await
        .unwrap();
        assert!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .is_favorite,
            "a held saver cannot hide committed data"
        );
        observer.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            observer.finished.notified(),
        )
        .await
        .unwrap();
        assert!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .is_favorite,
            "asynchronous export failure cannot roll back user data"
        );
        drop(listener);
        drop(observer);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime releases the listener when notification finishes");
    }

    #[tokio::test]
    async fn user_rating_likes_matches_source_six_point_five_boundary_in_every_read_path() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF006);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let row = fetch_item(&db, item).await;
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for (rating, expected) in [
            (0.0, false),
            (5.0, false),
            (6.49, false),
            (6.5, true),
            (10.0, true),
        ] {
            manager
                .save_user_data(
                    user,
                    item,
                    &UpdateUserItemDataDto {
                        rating: Some(rating),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let single = manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(single.likes, Some(expected), "rating {rating}");
            assert_eq!(
                manager.get_user_data_dtos(&[item], user).await.unwrap()[&item],
                single
            );
            for hydrated in [false, true] {
                assert_eq!(
                    manager
                        .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, hydrated)
                        .await
                        .unwrap()[&item],
                    single
                );
            }
            let stored: Option<bool> = sqlx::query_scalar(
                r#"SELECT "Likes" FROM "UserData" WHERE "ItemId" = ?1 AND "UserId" = ?2"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(user))
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(stored, Some(expected));
        }
        sqlx::query(r#"UPDATE "UserData" SET "Rating" = NULL, "Likes" = 1 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item))
            .execute(db.writer())
            .await
            .unwrap();
        assert_eq!(
            manager
                .get_user_data_dto(item, user)
                .await
                .unwrap()
                .unwrap()
                .likes,
            None,
            "Likes getter derives from Rating rather than a stale serialized Likes column"
        );
    }

    #[tokio::test]
    async fn browse_episode_and_season_use_resolved_series_keys_without_provider_ids_field() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let series = Uuid::from_u128(0xF007);
        let episode = Uuid::from_u128(0xF008);
        let season = Uuid::from_u128(0xF009);
        seed_user(&db, user).await;
        seed_named_item(&db, series, BaseItemKind::Series, "Resolved Series").await;
        for (provider, key) in [
            ("Imdb", "series-imdb"),
            ("Tvdb", "series-tvdb"),
            ("Custom", "series-custom"),
        ] {
            seed_provider_id(&db, series, provider, key).await;
        }
        let mut rows = Vec::new();
        for (item, kind, suffix) in [
            (episode, BaseItemKind::Episode, "001002"),
            (season, BaseItemKind::Season, "002"),
        ] {
            seed_item(&db, item, kind).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "ParentIndexNumber" = 1, "IndexNumber" = 2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(item)).bind(guid_to_db(series)).execute(db.writer()).await.unwrap();
            for (key, count) in [
                (item.to_string(), 1),
                (format!("{series}{suffix}"), 2),
                (format!("series-tvdb{suffix}"), 3),
                (format!("series-custom{suffix}"), 7),
            ] {
                sqlx::query(r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "PlayCount", "IsFavorite", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?3, ?4, 0, 0, 0)"#)
                    .bind(guid_to_db(item)).bind(guid_to_db(user)).bind(key).bind(count).execute(db.writer()).await.unwrap();
            }
            rows.push(fetch_item(&db, item).await);
        }
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for hydrated in [false, true] {
            let selected = manager
                .get_user_data_dtos_for_rows(&rows, user, hydrated)
                .await
                .unwrap();
            assert_eq!(selected[&episode].key, "series-custom001002");
            assert_eq!(selected[&season].key, "series-custom002");
            assert_eq!(selected[&episode].play_count, 7);
            assert_eq!(selected[&season].play_count, 7);
            assert_eq!(
                manager
                    .get_user_data_dtos(&[episode, season], user)
                    .await
                    .unwrap(),
                selected
            );
        }
        sqlx::query(r#"DELETE FROM "UserData" WHERE "UserId" = ?1"#)
            .bind(guid_to_db(user))
            .execute(db.writer())
            .await
            .unwrap();
        let synthetic = manager
            .get_user_data_dtos_for_rows(&rows, user, false)
            .await
            .unwrap();
        assert_eq!(synthetic[&episode].key, "series-custom001002");
        assert_eq!(synthetic[&season].key, "series-custom002");
        // Once providers exist, Episode omits Series GUID; Season retains it.
        for (item, suffix) in [(episode, "001002"), (season, "002")] {
            for (key, count) in [(item.to_string(), 1), (format!("{series}{suffix}"), 2)] {
                sqlx::query(r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "PlayCount", "IsFavorite", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?3, ?4, 0, 0, 0)"#)
                    .bind(guid_to_db(item)).bind(guid_to_db(user)).bind(key).bind(count).execute(db.writer()).await.unwrap();
            }
        }
        let selected = manager
            .get_user_data_dtos_for_rows(&rows, user, false)
            .await
            .unwrap();
        assert_eq!(selected[&episode].key, episode.to_string());
        assert_eq!(selected[&season].key, format!("{series}002"));
    }

    #[tokio::test]
    async fn unresolved_or_non_series_child_pointer_does_not_invent_series_suffix_keys() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let episode = Uuid::from_u128(0xF00A);
        let wrong_kind = Uuid::from_u128(0xF00B);
        let missing = Uuid::from_u128(0xF00C);
        seed_user(&db, user).await;
        seed_item(&db, episode, BaseItemKind::Episode).await;
        seed_item(&db, wrong_kind, BaseItemKind::Movie).await;
        seed_provider_id(&db, wrong_kind, "Tmdb", "not-a-series").await;
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for pointer in [missing, wrong_kind] {
            sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "ParentIndexNumber" = 1, "IndexNumber" = 2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(episode)).bind(guid_to_db(pointer)).execute(db.writer()).await.unwrap();
            let row = fetch_item(&db, episode).await;
            for hydrated in [false, true] {
                let selected = manager
                    .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, hydrated)
                    .await
                    .unwrap();
                assert_eq!(selected[&episode].key, episode.to_string());
            }
            manager.set_likes(user, episode, Some(true)).await.unwrap();
            let keys: Vec<String> =
                sqlx::query_scalar(r#"SELECT "CustomDataKey" FROM "UserData" WHERE "ItemId" = ?1"#)
                    .bind(guid_to_db(episode))
                    .fetch_all(db.pool())
                    .await
                    .unwrap();
            assert_eq!(keys, vec![episode.to_string()]);
        }
    }
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one source-contract regression checks the complete sequence"
    )]
    async fn child_user_data_resolves_nearest_series_parent_only_for_empty_or_nil_series_id() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let outer = Uuid::from_u128(0xF010);
        let inner = Uuid::from_u128(0xF011);
        let intermediate = Uuid::from_u128(0xF012);
        let folder = Uuid::from_u128(0xF013);
        let episode = Uuid::from_u128(0xF014);
        let season = Uuid::from_u128(0xF015);
        let missing = Uuid::from_u128(0xF016);
        seed_user(&db, user).await;
        for (id, kind) in [
            (outer, BaseItemKind::Series),
            (inner, BaseItemKind::Series),
            (intermediate, BaseItemKind::Season),
            (folder, BaseItemKind::Folder),
            (episode, BaseItemKind::Episode),
            (season, BaseItemKind::Season),
        ] {
            seed_item(&db, id, kind).await;
        }
        seed_provider_id(&db, outer, "Custom", "outer-series").await;
        seed_provider_id(&db, inner, "Tvdb", "nearest-series").await;
        for (id, parent) in [
            (inner, outer),
            (intermediate, inner),
            (folder, intermediate),
            (episode, folder),
            (season, folder),
        ] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .unwrap();
        }
        // FindParent walks actual parents, not an intermediate Season's SeriesId.
        sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(intermediate))
            .bind(guid_to_db(outer))
            .execute(db.writer())
            .await
            .unwrap();
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for pointer in [None, Some(Uuid::nil())] {
            let mut rows = Vec::new();
            for item in [episode, season] {
                sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "ParentIndexNumber" = 1, "IndexNumber" = 2 WHERE "Id" = ?1"#)
                    .bind(guid_to_db(item)).bind(pointer.map(guid_to_db)).execute(db.writer()).await.unwrap();
                rows.push(fetch_item(&db, item).await);
            }
            for hydrated in [false, true] {
                let selected = manager
                    .get_user_data_dtos_for_rows(&rows, user, hydrated)
                    .await
                    .unwrap();
                assert_eq!(selected[&episode].key, "nearest-series001002");
                assert_eq!(selected[&season].key, "nearest-series002");
            }
            for (item, suffix) in [(episode, "001002"), (season, "002")] {
                manager
                    .save_user_data_with_reason(
                        user,
                        item,
                        &favorite_dto(),
                        UserDataSaveReason::UpdateUserRating,
                    )
                    .await
                    .unwrap();
                let selected = manager
                    .get_user_data_dto(item, user)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(selected.key, format!("nearest-series{suffix}"));
                assert!(selected.is_favorite);
                let keys: Vec<String> = sqlx::query_scalar(
                    r#"SELECT "CustomDataKey" FROM "UserData" WHERE "ItemId" = ?1"#,
                )
                .bind(guid_to_db(item))
                .fetch_all(db.pool())
                .await
                .unwrap();
                assert!(keys.contains(&item.to_string()));
                assert!(keys.contains(&format!("nearest-series{suffix}")));
                assert!(!keys.iter().any(|key| key.starts_with("outer-series")));
                assert_eq!(keys.contains(&format!("{inner}{suffix}")), item == season);
            }
            let selected = manager
                .get_user_data_dtos_for_rows(&rows, user, false)
                .await
                .unwrap();
            assert_eq!(
                manager
                    .get_user_data_dtos(&[episode, season], user)
                    .await
                    .unwrap(),
                selected
            );
        }
        sqlx::query(r#"DELETE FROM "UserData" WHERE "UserId" = ?1"#)
            .bind(guid_to_db(user))
            .execute(db.writer())
            .await
            .unwrap();
        for pointer in [missing, folder] {
            let mut rows = Vec::new();
            for item in [episode, season] {
                sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2 WHERE "Id" = ?1"#)
                    .bind(guid_to_db(item))
                    .bind(guid_to_db(pointer))
                    .execute(db.writer())
                    .await
                    .unwrap();
                rows.push(fetch_item(&db, item).await);
            }
            for hydrated in [false, true] {
                let selected = manager
                    .get_user_data_dtos_for_rows(&rows, user, hydrated)
                    .await
                    .unwrap();
                for item in [episode, season] {
                    assert_eq!(
                        selected[&item].key,
                        item.to_string(),
                        "a nonempty missing/wrong-kind pointer cannot trigger FindSeriesId fallback"
                    );
                    manager.set_likes(user, item, Some(true)).await.unwrap();
                    assert_eq!(
                        manager
                            .get_user_data_dto(item, user)
                            .await
                            .unwrap()
                            .unwrap()
                            .key,
                        item.to_string()
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn child_series_parent_resolution_terminates_on_self_and_cyclic_parent_graphs() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let episode = Uuid::from_u128(0xF020);
        let season = Uuid::from_u128(0xF021);
        let folder = Uuid::from_u128(0xF022);
        seed_user(&db, user).await;
        for (item, kind) in [
            (episode, BaseItemKind::Episode),
            (season, BaseItemKind::Season),
            (folder, BaseItemKind::Folder),
        ] {
            seed_item(&db, item, kind).await;
        }
        for (item, parent) in [(episode, episode), (season, folder), (folder, season)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2, "SeriesId" = ?3, "ParentIndexNumber" = 1, "IndexNumber" = 2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(item)).bind(guid_to_db(parent)).bind(guid_to_db(Uuid::nil())).execute(db.writer()).await.unwrap();
        }
        let rows = vec![
            fetch_item(&db, episode).await,
            fetch_item(&db, season).await,
        ];
        let manager = FerrofinUserDataManager::new(db, config());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for hydrated in [false, true] {
                let selected = manager
                    .get_user_data_dtos_for_rows(&rows, user, hydrated)
                    .await
                    .unwrap();
                assert_eq!(selected[&episode].key, episode.to_string());
                assert_eq!(selected[&season].key, season.to_string());
            }
            for item in [episode, season] {
                manager.set_likes(user, item, Some(true)).await.unwrap();
                assert_eq!(
                    manager
                        .get_user_data_dto(item, user)
                        .await
                        .unwrap()
                        .unwrap()
                        .key,
                    item.to_string()
                );
            }
        })
        .await
        .expect("finite cycle-safe SQL and parent walks cannot hang user-data reads or writes");
    }
}
