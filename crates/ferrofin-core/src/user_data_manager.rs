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
use crate::user_data_keys::{KeySource, user_data_keys, uses_provider_ids};

/// One item's identity fields, read once and reused for the item and (for a
/// `Season`/`Episode`) its series.
///
/// Owns its strings because it outlives the query; [`Self::as_source`] hands
/// the derivation the borrowed view it wants.
#[derive(Debug, Clone)]
struct KeyRow {
    item_id: Uuid,
    kind: BaseItemKind,
    tmdb: Option<String>,
    imdb: Option<String>,
    tvdb: Option<String>,
    custom: Option<String>,
    musicbrainz_album: Option<String>,
    musicbrainz_release_group: Option<String>,
    musicbrainz_artist: Option<String>,
    episode_title: Option<String>,
    is_series: bool,
    index_number: Option<i64>,
    parent_index_number: Option<i64>,
    name: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    extra_type: Option<String>,
    run_time_ticks: Option<i64>,
    series_id: Option<Uuid>,
}

impl KeyRow {
    fn as_source(&self) -> KeySource<'_> {
        KeySource {
            item_id: self.item_id,
            kind: self.kind,
            tmdb: self.tmdb.as_deref(),
            imdb: self.imdb.as_deref(),
            tvdb: self.tvdb.as_deref(),
            custom: self.custom.as_deref(),
            musicbrainz_album: self.musicbrainz_album.as_deref(),
            musicbrainz_release_group: self.musicbrainz_release_group.as_deref(),
            musicbrainz_artist: self.musicbrainz_artist.as_deref(),
            episode_title: self.episode_title.as_deref(),
            is_series: self.is_series,
            index_number: self.index_number,
            parent_index_number: self.parent_index_number,
            name: self.name.as_deref(),
            album: self.album.as_deref(),
            album_artist: self.album_artist.as_deref(),
            extra_type: self.extra_type.as_deref(),
            run_time_ticks: self.run_time_ticks,
        }
    }
}

/// The lowercase `ExtraType` name the C# key builder appends
/// (`ExtraType.ToString().ToLowerInvariant()`), for a stored discriminant.
fn extra_type_name(disc: i32) -> Option<&'static str> {
    Some(match disc {
        1 => "clip",
        2 => "trailer",
        3 => "behindthescenes",
        4 => "deletedscene",
        5 => "interview",
        6 => "scene",
        7 => "sample",
        8 => "themesong",
        9 => "themevideo",
        10 => "featurette",
        11 => "short",
        // 0 is `Unknown`, which the C# never reaches: `ExtraType.HasValue` is
        // false for a non-extra, so no key is built at all.
        _ => return None,
    })
}

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

    /// Reads the user-data row for an item/user pair, or `None`.
    ///
    /// Port of `UserDataManager.GetUserDataInternal`: match **any** of the
    /// item's derived keys, then prefer the row keyed by the item's own id and
    /// fall back to the first match. An adopted item carries several rows (one
    /// per key) and they can disagree — a stale provider-keyed row from before
    /// a metadata change, say — so which one wins is not arbitrary.
    ///
    /// The guid row alone would answer correctly on a database Jellyfin wrote,
    /// because the item id is always among the keys it saves. It would miss on
    /// one where only a provider-keyed row exists.
    async fn read_row(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserDataEntity>, ServiceError> {
        let keys = self.keys_for(item_id).await?;
        let placeholders = (3..3 + keys.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        // ItemId/UserId are stored uppercase (Jellyfin's GUID casing) while
        // CustomDataKey keeps the lowercase hyphenated form — exactly what a
        // real 10.11.8 database contains, so the two need separate binds.
        let sql = format!(
            r#"SELECT * FROM "UserData"
               WHERE "ItemId" = ?1 AND "UserId" = ?2
                 AND "CustomDataKey" IN ({placeholders})"#,
        );
        let mut query = sqlx::query_as::<_, UserDataEntity>(&sql)
            .bind(guid_to_db(item_id))
            .bind(guid_to_db(user_id));
        for key in &keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        Ok(Self::preferred_row(&rows, &keys, item_id))
    }

    /// Picks the row to answer with when an item carries several.
    ///
    /// The guid row first, then the highest-priority *key* that has a row —
    /// **not** `rows.first()`, which is whatever order SQLite returned (PK
    /// index order, i.e. alphabetical by `CustomDataKey`) and would make the
    /// answer depend on how the provider ids happen to sort.
    ///
    /// A deliberate divergence: upstream's `directDataReference` compares
    /// against `itemId.ToString("N")` while the keys it just built carry the
    /// hyphenated `"D"` form, so that preference never actually fires and it
    /// always falls through to `userData.First()`. Preferring the guid row is
    /// what upstream evidently *meant*, and it is stable.
    fn preferred_row(
        rows: &[UserDataEntity],
        keys: &[String],
        item_id: Uuid,
    ) -> Option<UserDataEntity> {
        let own = item_id.to_string();
        if let Some(row) = rows.iter().find(|r| r.custom_data_key == own) {
            return Some(row.clone());
        }
        keys.iter()
            .find_map(|key| rows.iter().find(|r| &r.custom_data_key == key))
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
        Ok(self
            .load_keys(item_id)
            .await?
            .unwrap_or_else(|| vec![item_id.to_string()]))
    }

    /// Reads the rows behind [`Self::keys_for`] — the item, and for a
    /// `Season`/`Episode` its series — and derives the keys.
    async fn load_keys(&self, item_id: Uuid) -> Result<Option<Vec<String>>, ServiceError> {
        let Some(item) = self.key_row(item_id).await? else {
            return Ok(None);
        };
        // Only a Season or an Episode consults its series, so only then is the
        // second query worth making.
        let series = match (item.kind, item.series_id) {
            (BaseItemKind::Season | BaseItemKind::Episode, Some(series_id)) => {
                self.key_row(series_id).await?
            }
            _ => None,
        };
        let series_source = series.as_ref().map(KeyRow::as_source);
        Ok(Some(user_data_keys(
            &item.as_source(),
            series_source.as_ref(),
        )))
    }

    /// One item's identity fields plus its provider ids.
    #[allow(
        clippy::type_complexity,
        reason = "one row read positionally; naming a struct for it would not \
                  be read anywhere else"
    )]
    async fn key_row(&self, item_id: Uuid) -> Result<Option<KeyRow>, ServiceError> {
        let row: Option<(
            String,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i32>,
            Option<i64>,
            Option<String>,
            Option<bool>,
        )> = sqlx::query_as(
            r#"SELECT "Type", "IndexNumber", "ParentIndexNumber", "Name", "Album",
                      "AlbumArtists", "SeriesId", "ExtraType", "RunTimeTicks",
                      "EpisodeTitle", "IsSeries"
               FROM "BaseItems" WHERE "Id" = ?1 LIMIT 1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        let Some((
            type_name,
            index_number,
            parent_index_number,
            name,
            album,
            album_artists,
            series_id,
            extra_type,
            run_time_ticks,
            episode_title,
            is_series,
        )) = row
        else {
            return Ok(None);
        };

        let kind = kind_from_type_name(&type_name).unwrap_or(BaseItemKind::Folder);
        // Most kinds never look at a provider id, and this runs on the busiest
        // write path, so do not pay for the second query unless the derivation
        // will read it. An Episode is deliberately in the "no" list: it takes
        // its keys from the series and ignores its own providers entirely
        // (`EnableDefaultVideoUserDataKeys => false`), and episodes are the
        // bulk of a TV library.
        let providers: Vec<(String, String)> = if uses_provider_ids(kind) {
            sqlx::query_as(
                r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" = ?1"#,
            )
            .bind(guid_to_db(item_id))
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?
        } else {
            Vec::new()
        };
        let provider = |want: &str| {
            providers
                .iter()
                .find(|(id, _)| id.eq_ignore_ascii_case(want))
                .map(|(_, v)| v.clone())
        };

        Ok(Some(KeyRow {
            item_id,
            kind,
            tmdb: provider("Tmdb"),
            imdb: provider("Imdb"),
            tvdb: provider("Tvdb"),
            custom: provider("Custom"),
            musicbrainz_album: provider("MusicBrainzAlbum"),
            musicbrainz_release_group: provider("MusicBrainzReleaseGroup"),
            musicbrainz_artist: provider("MusicBrainzArtist"),
            episode_title,
            is_series: is_series.unwrap_or(false),
            index_number,
            parent_index_number,
            name,
            album,
            // `AlbumArtists` is a delimited list; the C# key uses the first.
            album_artist: album_artists
                .as_deref()
                .and_then(|a| a.split('|').next())
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
            extra_type: extra_type
                .and_then(extra_type_name)
                .map(std::borrow::ToOwned::to_owned),
            run_time_ticks,
            series_id: series_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()),
        }))
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
        .bind(row.likes)
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
        // The bool tracks whether the stored DTO came from the item's own guid
        // row, so a later one can displace a provider-keyed stand-in.
        let mut map: std::collections::HashMap<Uuid, (UserItemDataDto, bool)> =
            std::collections::HashMap::with_capacity(item_ids.len());
        // One IN-query per chunk instead of one query per item.
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            // Every row for these items, not just the guid-keyed one. An
            // adopted item carries a row per `CustomDataKey`, and filtering to
            // `lower(ItemId)` here made a page disagree with the per-item
            // endpoint about the same item whenever the default row was absent
            // — favourite on `/Items/{id}`, not favourite in the listing.
            //
            // The per-item path resolves ties by derived-key priority; doing
            // that here would mean deriving keys for a whole page, which is the
            // N+1 this batch exists to avoid. Instead: prefer the guid row,
            // else take the lowest key deterministically. The two agree
            // wherever a guid row exists, which is every item either server has
            // ever written — the id is always the last key saved.
            let sql = format!(
                r#"SELECT * FROM "UserData"
                   WHERE "UserId" = ?1 AND "ItemId" IN ({placeholders})
                   ORDER BY "CustomDataKey""#,
            );
            let mut query = sqlx::query_as::<_, UserDataEntity>(&sql).bind(guid_to_db(user_id));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
            for row in rows {
                let Ok(item_id) = Uuid::parse_str(&row.item_id) else {
                    continue;
                };
                let is_default = row.custom_data_key == item_id.to_string();
                match map.entry(item_id) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert((to_dto(&row, item_id), is_default));
                    }
                    // A later guid row displaces an earlier provider-keyed one;
                    // nothing displaces the guid row.
                    std::collections::hash_map::Entry::Occupied(mut slot) if is_default => {
                        slot.insert((to_dto(&row, item_id), true));
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }
        // Items without a stored row get the empty-row DTO, matching the
        // per-item path's `unwrap_or_else(empty_row)` fallback.
        for &item_id in item_ids {
            map.entry(item_id)
                .or_insert_with(|| (to_dto(&Self::empty_row(item_id, user_id), item_id), true));
        }
        Ok(map.into_iter().map(|(id, (dto, _))| (id, dto)).collect())
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
        // Identical semantics to the per-item loop this used to run (the stored
        // row when present, the empty row otherwise) in one chunked `IN` query —
        // the loop was an N+1 that issued one round trip per candidate item
        // (~100 per `/Items/Latest` request, which post-filters the whole
        // candidate set by played state).
        self.get_user_data_dtos(item_ids, user_id).await
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
        self.upsert_row(&row).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use crate::test_support::{seed_item, seed_provider_id, seed_user, test_db};
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
}
