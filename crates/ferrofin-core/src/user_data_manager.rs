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
//!   notify the weak NFO listener with the source `UserDataSaved` reason, and
//!   the user's sessions in batches (`UserDataChangeNotifier`, see
//!   [`FerrofinUserDataManager::set_user_data_changed_sink`]).
//! - `UpdatePlayState` needs the item's runtime + kind (for the resume-point
//!   heuristics); those are read from the `BaseItems` row rather than a
//!   pre-loaded domain object.
//!
//! Every read-modify-write of a row runs under the writer
//! ([`FerrofinUserDataManager::mutate_row`]), so two saves of an item that
//! overlap both land. That goes past upstream, whose `GetUserData` hands each
//! caller a copy of the row and whose `SaveUserData` writes every column back.
//!
//! Resume-point thresholds (`MinResumePct`, `MaxResumePct`,
//! `MinResumeDurationSeconds`, `MinAudiobookResume`, `MaxAudiobookResume`) are
//! read live from the injected [`ServerConfigurationManager`].

use std::collections::{HashMap, HashSet};
use std::future::Future as _;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

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
use ferrofin_traits::library::{
    ContentPermissions, LibraryManager, UserDataManager, UserDataSaveListener,
};
use ferrofin_traits::persistence::ItemCountService;

use crate::db_error::db_err;
use crate::dto_service::parse_media_type;
use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::{supports_played_status, supports_position_ticks_resume};
use crate::user_data_key_repository::extra_type_name;
use crate::user_data_keys::{KeySource, user_data_keys, uses_provider_ids};

/// `RunTimeTicks`, `Type`, `MediaType` and `ExtraType` of one `BaseItems` row.
type PlayStateRow = (Option<i64>, String, Option<String>, Option<i32>);

/// What the play-state paths read about one item: its runtime and kind, and
/// the two C# virtuals, resolved from the row's `MediaType` (a playlist's
/// played state) and `ExtraType` (a video extra's resume).
struct PlayStateItem {
    runtime_ticks: i64,
    kind: BaseItemKind,
    /// `SupportsPlayedStatus`.
    played_status: bool,
    /// `SupportsPositionTicksResume`.
    resume: bool,
}

/// One tick is 100 nanoseconds; there are 10,000,000 ticks per second (the
/// .NET `TimeSpan.TicksPerSecond` the C# resume math uses).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The rating from which an item counts as liked (`UserItemData.MinLikeValue`).
const MIN_LIKE_VALUE: f64 = 6.5;

/// How long saves are gathered before each user's sessions hear one
/// `UserDataChanged` (`UserDataChangeNotifier.UpdateDuration`). The window
/// opens at a batch's first save and is never extended.
///
/// FLAGGED as a candidate setting — plausible range 100 ms…5 s, default
/// upstream's.
const USER_DATA_CHANGE_WINDOW: Duration = Duration::from_millis(500);

/// The batch size at which a batch is sent before its window closes
/// (`UserDataChangeNotifier.MaxBatchSize`). It counts the items saved;
/// upstream also counts the parents it adds, which are looked up here only
/// when the batch is sent.
///
/// FLAGGED as a candidate setting — plausible range 100…10 000, default
/// upstream's.
const USER_DATA_CHANGE_MAX_BATCH: usize = 2000;

/// The saves waiting for the next `UserDataChanged` of each user.
#[derive(Default)]
struct PendingChanges {
    /// User → the items saved, in save order, without repeats.
    users: HashMap<Uuid, (Vec<Uuid>, HashSet<Uuid>)>,
    /// How many items the batch holds, over every user.
    count: usize,
    /// The open batch's number, while its window runs.
    batch: Option<u64>,
    /// The number the next batch gets.
    next: u64,
}

impl PendingChanges {
    /// Ends the open batch and returns what it gathered.
    fn take(&mut self) -> HashMap<Uuid, Vec<Uuid>> {
        self.batch = None;
        self.count = 0;
        std::mem::take(&mut self.users)
            .into_iter()
            .map(|(user, (items, _))| (user, items))
            .collect()
    }
}

/// Where the user-data manager sends the `UserDataChanged` message of each
/// batch of saves to a user's sessions: upstream's `UserDataChangeNotifier`,
/// which hears every `UserDataSaved`.
#[async_trait]
pub trait UserDataChangedSink: Send + Sync {
    /// Sends one serialized `UserDataChanged` message (`data`) to every
    /// session of `user_id`. Best-effort: delivery never fails a save.
    async fn user_data_changed(&self, user_id: Uuid, data: &str);

    /// Whether `user_id` has a session that would hear the message, so a
    /// batch nobody would hear costs no reads. The default answers `true`.
    async fn has_listener(&self, _user_id: Uuid) -> bool {
        true
    }
}

/// [`UserDataChangedSink`] over the session manager, held weakly: the
/// session manager owns the user-data manager, so a strong handle back
/// would be a cycle. Once the session manager is gone nothing is sent.
pub struct SessionUserDataSink(pub std::sync::Weak<dyn ferrofin_traits::session::SessionManager>);

#[async_trait]
impl UserDataChangedSink for SessionUserDataSink {
    async fn user_data_changed(&self, user_id: Uuid, data: &str) {
        if let Some(sessions) = self.0.upgrade() {
            let _ = sessions
                .send_message_to_user_sessions(
                    &[user_id],
                    ferrofin_model::session::SessionMessageType::UserDataChanged,
                    data,
                )
                .await;
        }
    }

    async fn has_listener(&self, user_id: Uuid) -> bool {
        match self.0.upgrade() {
            Some(sessions) => sessions.has_listener_for_user(user_id).await,
            None => false,
        }
    }
}

/// The concrete user-data manager.
#[derive(Clone)]
pub struct FerrofinUserDataManager {
    db: Database,
    config: Arc<dyn ServerConfigurationManager>,
    save_listener: Arc<OnceLock<Weak<dyn UserDataSaveListener>>>,
    /// Where saves are announced, set by the composition root once the
    /// session manager exists ([`Self::set_user_data_changed_sink`]). Unset
    /// (unit tests), they are announced nowhere.
    changed: Arc<std::sync::OnceLock<Arc<dyn UserDataChangedSink>>>,
    /// The saves waiting to be announced.
    pending: Arc<Mutex<PendingChanges>>,
    /// Where a played mark on a container finds the items it applies to,
    /// set by the composition root once the library manager exists
    /// ([`Self::set_container_members`]), and where a user-data read finds
    /// the item and its counts. Unset (unit tests), every mark is the
    /// requested item's own and a read is the stored row.
    members: Arc<OnceLock<ContainerMembers>>,
}

/// What a container's played mark reads: the library manager that lists its
/// members (held weakly: the composition root owns it) and the counts its
/// rolled-up played state comes from.
struct ContainerMembers {
    library: Weak<dyn LibraryManager>,
    counts: Arc<dyn ItemCountService>,
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
            changed: Arc::new(std::sync::OnceLock::new()),
            pending: Arc::new(Mutex::new(PendingChanges::default())),
            members: Arc::new(OnceLock::new()),
        }
    }

    /// Lets a played mark on a container (a series, season, album, box set,
    /// playlist or folder) reach the items under it, as `Folder.MarkPlayed`
    /// and `BoxSet.MarkPlayed` do: `library` lists them and `counts` answers
    /// the container's rolled-up played state, on a mark and on every
    /// user-data read. The first call wins.
    pub fn set_container_members(
        &self,
        library: Weak<dyn LibraryManager>,
        counts: Arc<dyn ItemCountService>,
    ) {
        let _ = self.members.set(ContainerMembers { library, counts });
    }

    /// Sets where the user data this manager saves is announced, as
    /// upstream's `UserDataChangeNotifier` does: every save but a playback
    /// progress report, gathered per user for [`USER_DATA_CHANGE_WINDOW`],
    /// each item with its owner or parent, so a season's or series' unplayed
    /// count refreshes too. The first call wins; it closes the session
    /// manager ↔ user-data manager cycle after both exist.
    pub fn set_user_data_changed_sink(&self, sink: Arc<dyn UserDataChangedSink>) {
        let _ = self.changed.set(sink);
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
        self.record_change(user_id, item_id, reason);
        let Some(listener) = self.save_listener.get().and_then(Weak::upgrade) else {
            return;
        };
        let mut notification = Box::pin(ferrofin_util::current_culture::scope(
            ferrofin_util::current_culture::capture(),
            async move {
                if let Err(error) = listener.user_data_saved(user_id, item_id, reason).await {
                    // Source NfoUserDataSaver catches export errors after commit.
                    tracing::warn!(%user_id, %item_id, %error, "user-data NFO saver failed");
                }
            },
        ));
        // The pinned event handler is async void: it starts inline, then the
        // user-data caller returns at its first incomplete await. Own the
        // remainder in the runtime so slow NFO I/O does not delay HTTP writes.
        let first_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(notification.as_mut().poll(cx))).await;
        if first_poll.is_pending() {
            drop(tokio::spawn(notification));
        }
    }

    /// `OnUserDataManagerUserDataSaved`: adds a save to the user's batch,
    /// opening a batch window if none runs. A playback progress report is
    /// not announced, and a batch that reaches [`USER_DATA_CHANGE_MAX_BATCH`]
    /// goes out at once.
    fn record_change(&self, user_id: Uuid, item_id: Uuid, reason: UserDataSaveReason) {
        if reason == UserDataSaveReason::PlaybackProgress || self.changed.get().is_none() {
            return;
        }
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let (items, seen) = pending.users.entry(user_id).or_default();
        if seen.insert(item_id) {
            items.push(item_id);
            pending.count += 1;
        }
        if pending.batch.is_none() {
            let batch = pending.next;
            pending.next += 1;
            pending.batch = Some(batch);
            let this = self.clone();
            drop(tokio::spawn(async move {
                tokio::time::sleep(USER_DATA_CHANGE_WINDOW).await;
                this.send_batch(Some(batch)).await;
            }));
        }
        if pending.count >= USER_DATA_CHANGE_MAX_BATCH {
            let changes = pending.take();
            drop(pending);
            let this = self.clone();
            drop(tokio::spawn(
                async move { this.send_changes(changes).await },
            ));
        }
    }

    /// `UpdateTimerCallback`: sends batch `batch`, unless it already went out
    /// because it filled up.
    async fn send_batch(&self, batch: Option<u64>) {
        let changes = match self.pending.lock() {
            Ok(mut pending) if pending.batch == batch => pending.take(),
            _ => return,
        };
        self.send_changes(changes).await;
    }

    /// Sends each user one `UserDataChanged` listing their saved items and,
    /// one level up for the indicators, each item's owner or else its parent
    /// (`GetOwner() ?? GetParent()`), each as `GetUserDataDto(item, user)`
    /// answers it. The listed items are read together
    /// ([`Self::user_data_dtos_for_items`]); an item whose row was not read
    /// (it is gone, or the read failed) takes the single read, and so does
    /// every item when the batch read fails. Best-effort: a failed read
    /// sends what it can.
    async fn send_changes(&self, changes: HashMap<Uuid, Vec<Uuid>>) {
        use crate::video_versions::{DbVersionReader, VersionRowReader as _};
        let Some(sink) = self.changed.get() else {
            return;
        };
        let reader = DbVersionReader(&self.db);
        for (user_id, items) in changes {
            if !sink.has_listener(user_id).await {
                continue;
            }
            let mut rows = match reader.rows_by_id(&items).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(
                        %user_id,
                        %error,
                        "user-data push: reading the items failed; reading each item instead"
                    );
                    Vec::new()
                }
            };
            let up: HashMap<Uuid, Uuid> = rows
                .iter()
                .filter_map(|row| {
                    let id = Uuid::parse_str(&row.id).ok()?;
                    let above = [&row.owner_id, &row.parent_id]
                        .into_iter()
                        .find_map(|id| id.as_deref().and_then(|id| Uuid::parse_str(id).ok()))?;
                    Some((id, above))
                })
                .collect();
            let mut listed = Vec::with_capacity(items.len() * 2);
            let mut seen = HashSet::new();
            for item in items {
                for id in std::iter::once(item).chain(up.get(&item).copied()) {
                    if seen.insert(id) {
                        listed.push(id);
                    }
                }
            }
            let mut read: HashSet<Uuid> = rows
                .iter()
                .filter_map(|row| Uuid::parse_str(&row.id).ok())
                .collect();
            let mut parents: Vec<Uuid> = up
                .values()
                .copied()
                .filter(|id| !read.contains(id))
                .collect();
            parents.sort_unstable();
            parents.dedup();
            if !parents.is_empty() {
                match reader.rows_by_id(&parents).await {
                    Ok(found) => {
                        read.extend(found.iter().filter_map(|row| Uuid::parse_str(&row.id).ok()));
                        rows.extend(found);
                    }
                    Err(error) => {
                        tracing::warn!(
                            %user_id,
                            %error,
                            "user-data push: reading the parents failed; reading each instead"
                        );
                    }
                }
            }
            let mut dtos = match self.user_data_dtos_for_items(&rows, user_id).await {
                Ok(dtos) => dtos,
                Err(error) => {
                    tracing::warn!(
                        %user_id,
                        %error,
                        "user-data push: the batch read failed; reading each item instead"
                    );
                    read.clear();
                    HashMap::new()
                }
            };
            let mut user_data_list = Vec::with_capacity(listed.len());
            for id in listed {
                // An item not in `read` takes the single read. An item in
                // `read` with no answer had a fill read fail: leave it out,
                // as the single read would.
                if let Some(dto) = dtos.remove(&id) {
                    user_data_list.push(dto);
                } else if !read.contains(&id)
                    && let Ok(Some(dto)) = self.get_user_data_dto(id, user_id).await
                {
                    user_data_list.push(dto);
                }
            }
            if user_data_list.is_empty() {
                continue;
            }
            let info = ferrofin_model::session::UserDataChangeInfo {
                user_id,
                user_data_list,
            };
            if let Ok(data) = serde_json::to_string(&info) {
                sink.user_data_changed(user_id, &data).await;
            }
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
        let (rows, keys) = self.rows_and_keys(item_id, user_id).await?;
        Ok(Self::preferred_row(rows, &keys))
    }

    /// The user's stored rows on the item, and the item's current keys.
    async fn rows_and_keys(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Vec<UserDataEntity>, Vec<String>), ServiceError> {
        let mut conn = self.db.pool().acquire().await.map_err(db_err)?;
        Self::rows_and_keys_on(&mut conn, item_id, user_id).await
    }

    /// [`Self::rows_and_keys`] on `conn`: a reader, or the write transaction of
    /// [`Self::mutate_row`].
    ///
    /// The keys are the `CustomDataKey`s the item's rows are written under, a
    /// port of the `item.GetUserDataKeys()` that `UserDataManager.SaveUserData`
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
    /// reads keep the old one — the split-brain the single transaction of
    /// [`Self::mutate_row`] exists to prevent. A failed save the caller can
    /// retry beats a save that half-succeeded silently.
    async fn rows_and_keys_on(
        conn: &mut sqlx::SqliteConnection,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Vec<UserDataEntity>, Vec<String>), ServiceError> {
        let keys = crate::user_data_key_repository::load_keys(conn, item_id)
            .await?
            .unwrap_or_else(|| vec![item_id.to_string()]);
        let rows = sqlx::query_as::<_, UserDataEntity>(
            r#"SELECT * FROM "UserData" WHERE "ItemId" = ?1 AND "UserId" = ?2"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok((rows, keys))
    }

    /// [`Self::read_row`], or the row `GetUserData` answers for an item the
    /// user never wrote ([`Self::row_or_unwritten`]). One key derivation serves
    /// both the row lookup and the empty row's key.
    async fn read_or_unwritten_row(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<UserDataEntity, ServiceError> {
        let (rows, keys) = self.rows_and_keys(item_id, user_id).await?;
        Ok(Self::row_or_unwritten(rows, &keys, item_id, user_id))
    }

    /// The row [`Self::preferred_row`] picks, or the one `GetUserData` answers
    /// for an item the user never wrote (`UserDataManager.cs:373-376`): empty,
    /// keyed by the item's first user-data key, which is its id when it has no
    /// item row.
    fn row_or_unwritten(
        rows: Vec<UserDataEntity>,
        keys: &[String],
        item_id: Uuid,
        user_id: Uuid,
    ) -> UserDataEntity {
        Self::preferred_row(rows, keys).unwrap_or_else(|| {
            let mut row = Self::empty_row(item_id, user_id);
            if let Some(first) = keys.first() {
                row.custom_data_key.clone_from(first);
            }
            row
        })
    }

    /// Pinned ResolveUserDataRow: exact current-key priority, then the first
    /// attached candidate when none matches. No own-GUID override is applied.
    /// The chosen row is moved out of `rows`, which every caller owns.
    fn preferred_row(mut rows: Vec<UserDataEntity>, keys: &[String]) -> Option<UserDataEntity> {
        let index = keys
            .iter()
            .find_map(|key| rows.iter().position(|row| row.custom_data_key == *key))
            .unwrap_or(0);
        (index < rows.len()).then(|| rows.swap_remove(index))
    }

    /// Reads what the play-state heuristics need about an item: its runtime
    /// ticks and kind, and its `SupportsPlayedStatus`/`SupportsPositionTicksResume`
    /// resolved from the row's `MediaType` and `ExtraType`. A missing item
    /// yields `None`.
    async fn item_runtime_and_kind(
        &self,
        item_id: Uuid,
    ) -> Result<Option<PlayStateItem>, ServiceError> {
        let row: Option<PlayStateRow> = sqlx::query_as(
            r#"SELECT "RunTimeTicks", "Type", "MediaType", "ExtraType" FROM "BaseItems"
               WHERE "Id" = ?1 LIMIT 1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(row.map(|(ticks, type_name, media_type, extra_type)| {
            // Unknown stored types default to Folder (a conservative choice that
            // disables the position-resume heuristics).
            let kind = kind_from_type_name(&type_name).unwrap_or(BaseItemKind::Folder);
            PlayStateItem {
                runtime_ticks: ticks.unwrap_or(0),
                kind,
                played_status: supports_played_status(
                    kind,
                    parse_media_type(media_type.as_deref()),
                ),
                resume: supports_position_ticks_resume(
                    kind,
                    extra_type.and_then(crate::dto_service::extra_type_from_disc),
                ),
            }
        }))
    }

    /// Changes the user's data on an item: reads the row, hands it to `mutate`
    /// and writes it back, all in **one `BEGIN IMMEDIATE` transaction**, and
    /// answers the saved row with what `mutate` returned.
    ///
    /// The read is inside the transaction so that two saves that overlap both
    /// land. A save that read first and only then waited for the writer, for
    /// seconds while a scan holds it, would start from the row as it was and
    /// write every column back: a progress report would undo a favourite set
    /// meanwhile, and two play starts would count once (gl-142). That puts the
    /// key and row reads on the writer, the price of the busiest write path
    /// staying correct. `mutate` runs with the writer held, so it is synchronous
    /// and whatever it needs is read before this is called.
    ///
    /// The write is the port of `UserDataManager.SaveUserData`, **one row per
    /// `CustomDataKey`**, which does `foreach (var key in item.GetUserDataKeys())`
    /// inside one transaction. Writing only the row the caller named would leave
    /// an adopted database's provider-keyed rows holding stale values, and those
    /// are the rows Jellyfin reads. The derived set always ends with the item id,
    /// so the row a caller expected is always among those written. All keys go in
    /// one transaction: a favourite that reached the TMDB row but not the IMDb
    /// row is precisely the split-brain state this exists to prevent.
    async fn mutate_row<T: Send>(
        &self,
        item_id: Uuid,
        user_id: Uuid,
        mutate: impl FnOnce(&mut UserDataEntity) -> T + Send,
    ) -> Result<(UserDataEntity, T), ServiceError> {
        let mut tx = self
            .db
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        let (rows, keys) = Self::rows_and_keys_on(&mut tx, item_id, user_id).await?;
        let mut row = Self::row_or_unwritten(rows, &keys, item_id, user_id);
        let answer = mutate(&mut row);
        for key in &keys {
            Self::upsert_one(&mut tx, &row, key).await?;
        }
        Self::update_retired(&mut tx, &row, &keys).await?;
        tx.commit().await.map_err(db_err)?;
        Ok((row, answer))
    }

    /// Brings the user's rows of the item under keys it no longer reports to
    /// `row`'s values, as `SaveUserData` does after its per-key upserts (v12.2
    /// `UserDataManager.cs:79-92`). They are kept, not deleted: the key set
    /// is briefly incomplete mid-refresh, and a deleted and re-added item
    /// reattaches by them. Without this the any-row filters (`IsFavorite`,
    /// `IsPlayed`, `IsResumable`) match a stale row the item's DTO no longer
    /// reads. `RetentionDate` stays untouched, as on [`Self::upsert_one`].
    async fn update_retired(
        tx: &mut sqlx::SqliteConnection,
        row: &UserDataEntity,
        keys: &[String],
    ) -> Result<(), ServiceError> {
        let keys = serde_json::to_string(keys).map_err(|e| ServiceError::backend(e.to_string()))?;
        sqlx::query(
            r#"UPDATE "UserData" SET
                 "AudioStreamIndex" = ?4, "IsFavorite" = ?5, "LastPlayedDate" = ?6,
                 "Likes" = ?7, "PlayCount" = ?8, "PlaybackPositionTicks" = ?9,
                 "Played" = ?10, "Rating" = ?11, "SubtitleStreamIndex" = ?12
               WHERE "ItemId" = ?1 AND "UserId" = ?2
                 AND "CustomDataKey" NOT IN (SELECT value FROM json_each(?3))"#,
        )
        .bind(&row.item_id)
        .bind(&row.user_id)
        .bind(keys)
        .bind(row.audio_stream_index)
        .bind(row.is_favorite)
        .bind(opt_datetime_to_db(row.last_played_date))
        .bind(row.rating.map(|rating| rating >= MIN_LIKE_VALUE))
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
        .bind(row.rating.map(|rating| rating >= MIN_LIKE_VALUE))
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

    /// Saves a play-state report — the shared body of
    /// [`UserDataManager::update_play_state_with_reason`] and
    /// [`UserDataManager::update_play_progress`]. With `streams`, the reported
    /// tracks are kept as `UpdatePlaybackSettings` keeps them, in the same save,
    /// and a report with no position saves when they change (jellyfin#16501's
    /// `if (tracksChanged) changed = true`).
    ///
    /// A report that saves nothing finds that out on a reader, so it never
    /// queues for the writer: during a scan that would be seconds for nothing.
    async fn save_play_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        reason: UserDataSaveReason,
        streams: Option<ferrofin_traits::library::ReportedStreams>,
    ) -> Result<bool, ServiceError> {
        let config = self.config.configuration().await?;
        let item = self
            .item_runtime_and_kind(item_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("item {item_id}")))?;

        if reported_position_ticks.is_none() && reason != UserDataSaveReason::PlaybackFinished {
            let mut row = self.read_or_unwritten_row(item_id, user_id).await?;
            let tracks_changed = streams.is_some_and(|streams| {
                streams.apply_to(&mut row.audio_stream_index, &mut row.subtitle_stream_index)
            });
            if !tracks_changed {
                return Ok(false);
            }
        }

        let (row, played_to_completion) = self
            .mutate_row(item_id, user_id, |row| {
                if let Some(streams) = streams {
                    streams.apply_to(&mut row.audio_stream_index, &mut row.subtitle_stream_index);
                }
                apply_play_report(row, reported_position_ticks, reason, &item, &config)
            })
            .await?;
        self.notify_saved(user_id, item_id, reason).await;
        // "A completed version marks every alternate version played and
        // clears their resume points" — `if (data.Played == true && item is
        // Video playedVideo) playedVideo.PropagatePlayedState(user, true)`
        // after the progress and the stop save (`SessionManager.cs:
        // 1001-1006,1206-1211`).
        if row.played {
            self.propagate_played_state(user_id, item_id, true, true)
                .await?;
        }

        Ok(played_to_completion)
    }

    /// Gives every other version of a video the played state just set on
    /// it — port of `Video.PropagatePlayedState` (`Video.cs:370-410`), which
    /// `Video.MarkPlayed`/`MarkUnplayed` and the session's progress and stop
    /// reports (`SessionManager.cs:1001-1006,1206-1211`) run after saving
    /// the video's own data.
    ///
    /// Marking played sets `Played` on each version and, with
    /// `reset_position`, clears its resume point — only those fields, as the
    /// partial `UpdateUserItemDataDto` upstream saves (play count and the
    /// rest stay). Marking unplayed fully resets each version
    /// (`ResetPlayedState`: played, play count, position, last played) and
    /// saves it, data or none, as upstream's `GetUserData` hands back a new
    /// record for a version never played. Every version saved is announced
    /// (`UserDataChanged`). A video with no versions — and any item that is
    /// no video — is left alone after one indexed read (upstream's
    /// `!PrimaryVersionId.HasValue && LinkedAlternateVersions.Length == 0 &&
    /// !HasLocalAlternateVersions` guard); only a video with versions pays
    /// the two reads that find them
    /// ([`VersionRows`](crate::video_versions::VersionRows)), and one more
    /// per further level of versions it finds.
    async fn propagate_played_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        played: bool,
        reset_position: bool,
    ) -> Result<(), ServiceError> {
        use crate::video_versions::{DbVersionReader, VersionRowReader as _, VersionRows};
        // The cheap guard: a version, or a video some row names as its
        // primary (`IX_BaseItems_PrimaryVersionId`).
        let has_versions: Option<bool> = sqlx::query_scalar(
            r#"SELECT b."PrimaryVersionId" IS NOT NULL
                      OR EXISTS (SELECT 1 FROM "BaseItems" x
                                 WHERE x."PrimaryVersionId" = b."Id" AND x."Id" <> b."Id")
               FROM "BaseItems" b WHERE b."Id" = ?1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        if has_versions != Some(true) {
            return Ok(());
        }
        let reader = DbVersionReader(&self.db);
        let rows = reader.rows_by_id(&[item_id]).await?;
        let Some(item) = rows.first() else {
            return Ok(());
        };
        if !kind_from_type_name(&item.type_).is_some_and(crate::kinds::is_video) {
            return Ok(());
        }
        let versions = VersionRows::load(&reader, &[item]).await?;
        for version in versions.all_version_ids(item) {
            if version == item_id {
                continue;
            }
            self.mutate_row(version, user_id, |row| {
                if played {
                    row.played = true;
                    if reset_position {
                        row.playback_position_ticks = 0;
                    }
                } else {
                    row.played = false;
                    row.play_count = 0;
                    row.playback_position_ticks = 0;
                    row.last_played_date = None;
                }
            })
            .await?;
            // Each version's save is a `TogglePlayed` `SaveUserData`
            // (`Video.PropagatePlayedState`), which the NFO saver and the
            // user's sessions hear too.
            self.notify_saved(user_id, version, UserDataSaveReason::TogglePlayed)
                .await;
        }
        Ok(())
    }

    /// The items a played mark on `item_id` applies to, or `None` when it is
    /// no container and the mark is the item's own.
    ///
    /// Port of the container overrides of `MarkPlayed`/`MarkUnplayed`: a box
    /// set marks each of its members (`BoxSet.cs:205-232`, a member that is
    /// itself a container in turn), any other folder the recursive non-folder
    /// items its own `GetItemsInternal` lists for the user (`Folder.cs:
    /// 1956-2012`): a season its episodes, a series every episode under its
    /// key, a playlist or library what is under it. A virtual item is left
    /// out of a played mark unless the user displays missing episodes, and
    /// an unaired virtual episode always is. A legacy (folder-backed) box
    /// set's members are its children on disk, which reach the same items
    /// as its `base.MarkPlayed` does.
    async fn container_members(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        played: bool,
    ) -> Result<Option<Container>, ServiceError> {
        let Some(members) = self.members.get() else {
            return Ok(None);
        };
        let Some(library) = members.library.upgrade() else {
            tracing::warn!(%item_id, "library manager gone: the played mark is the item's own");
            return Ok(None);
        };
        let Some(root) = library.get_item_by_id(item_id).await? else {
            return Ok(None);
        };
        // `item is Folder`: the C# class decides, not the stored `IsFolder`
        // (a by-name artist row stores false).
        if !is_container(&root) {
            return Ok(None);
        }
        let Some(user) = crate::item_repository::query_user(&self.db, user_id).await? else {
            return Ok(None);
        };
        let mut seen = std::collections::HashSet::from([item_id]);
        let mut leaves = Vec::new();
        // A box set inside a box set (or a cycle of them) is walked once.
        let mut folders = vec![(item_id, root.clone())];
        while let Some((id, folder)) = folders.pop() {
            let box_set = kind_from_type_name(&folder.type_) == Some(BaseItemKind::BoxSet);
            let query = member_query(id, &folder, &user, played);
            for child in library.query_items(&query).await?.items {
                let Ok(child_id) = Uuid::parse_str(&child.id) else {
                    continue;
                };
                if !seen.insert(child_id) {
                    continue;
                }
                if is_container(&child) {
                    folders.push((child_id, child));
                } else if box_set
                    || !(played && child.is_virtual_item && is_unaired_episode(&child))
                {
                    leaves.push(child_id);
                }
            }
        }
        Ok(Some(Container {
            id: item_id,
            root,
            user,
            leaves,
        }))
    }

    /// Marks a container's members played or unplayed and answers with the
    /// container's user data: `UpdatePlayedStatus` → `item.MarkPlayed`/
    /// `MarkUnplayed`, then `GetUserDataDto(item, user)`.
    ///
    /// One write per member, as upstream's per-item `SaveUserData` — not one
    /// transaction around them all, which would hold the single writer for a
    /// whole library's mark and stall every playback report meanwhile. Each
    /// save is announced as it lands, so the members saved before a failing
    /// one are announced too.
    async fn mark_container(
        &self,
        user_id: Uuid,
        container: &Container,
        played: bool,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        for &leaf in &container.leaves {
            if played {
                self.mark_item_played(user_id, leaf, date_played).await?;
            } else {
                self.mark_item_unplayed(user_id, leaf).await?;
            }
        }
        if !played {
            self.clear_own_play_state(user_id, container.id).await?;
        }
        self.container_dto(user_id, container).await
    }

    /// Resets the play state a container's own rows carry, under every key,
    /// when the container is marked unplayed.
    ///
    /// Upstream's container mark never writes those rows, but earlier
    /// Ferrofin builds marked the container itself. Where a kind's played
    /// state IS its own row (a music album, a library) that stale state would
    /// otherwise show for good. Divergence: upstream's `MarkUnplayed` leaves
    /// the row alone, so a `Played` set on a container through the UserData
    /// API survives it there; here the user's unmark clears it. A played mark
    /// clears nothing: the row then agrees with it. Favourite and rating are
    /// kept, and a row with no play state is not written.
    async fn clear_own_play_state(&self, user_id: Uuid, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "UserData"
               SET "Played" = 0, "PlayCount" = 0, "PlaybackPositionTicks" = 0,
                   "LastPlayedDate" = NULL
               WHERE "ItemId" = ?1 AND "UserId" = ?2
                 AND ("Played" = 1 OR "PlayCount" <> 0 OR "PlaybackPositionTicks" <> 0
                      OR "LastPlayedDate" IS NOT NULL)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// `GetUserDataDto(container, user)`: the container's own user data (its
    /// key derived as the item DTO derives it), whose played numbers
    /// `Folder.FillUserDataDtoValues` takes from its leaves.
    async fn container_dto(
        &self,
        user_id: Uuid,
        container: &Container,
    ) -> Result<UserItemDataDto, ServiceError> {
        // Providers hydrated: the controller loads the item by id.
        let mut dto = self
            .get_user_data_dtos_for_rows(std::slice::from_ref(&container.root), user_id, true)
            .await?
            .remove(&container.id)
            .unwrap_or_else(|| to_dto(Self::empty_row(container.id, user_id), container.id));
        if let Some(members) = self.members.get()
            && kind_from_type_name(&container.root.type_).is_some_and(|kind| {
                crate::kinds::supports_user_data_from_children(kind)
                    && supports_played_status(
                        kind,
                        parse_media_type(container.root.media_type.as_deref()),
                    )
            })
        {
            let counts = self
                .container_counts(container.id, &container.user, &members.counts)
                .await?;
            crate::dto_service::fill_folder_played(&mut dto, counts);
        }
        Ok(dto)
    }

    /// `Folder.FillUserDataDtoValues`' counts for one container, with no
    /// precomputed batch (v12.2 Folder.cs:2048-2059): a folder with linked
    /// children (a box set, a playlist) counts through
    /// `GetPlayedAndTotalCountFromLinkedChildren`, any other through
    /// `GetPlayedAndTotalCount`.
    async fn container_counts(
        &self,
        id: Uuid,
        user: &ferrofin_db::entities::users::UserEntity,
        counts: &Arc<dyn ItemCountService>,
    ) -> Result<ferrofin_traits::persistence::PlayedAndTotal, ServiceError> {
        // `new InternalItemsQuery(user)`: `set_user` also carries the user's
        // parental-rating cap.
        let mut query = ferrofin_traits::options::InternalItemsQuery::default();
        query.set_user(user.clone());
        if counts
            .get_linked_children_count_batch(&[id])
            .await?
            .contains_key(&id)
        {
            return counts
                .get_played_and_total_count_from_linked_children(&query, id)
                .await;
        }
        counts.get_played_and_total_count(&query, id).await
    }

    /// Folds the completion of `item`'s version played last into `dto`, its
    /// own user data — `GetResumeUserData(user, item)?.ApplyTo(dto)`
    /// (v12.2 `UserDataManager.cs:263-357, 424`). A video that is a version
    /// itself, or that no row names as its primary, is left alone after at
    /// most one indexed read; only a video with versions reads its group and
    /// their user data.
    async fn apply_version_resume(
        &self,
        dto: &mut UserItemDataDto,
        item: &BaseItemEntity,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        use crate::video_versions::{DbVersionReader, VersionRows, resume_user_data};
        if !takes_version_resume(item) {
            return Ok(());
        }
        let has_versions: bool = sqlx::query_scalar(
            r#"SELECT EXISTS (SELECT 1 FROM "BaseItems" x
                              WHERE x."PrimaryVersionId" = ?1 AND x."Id" <> ?1)"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)?;
        if !has_versions {
            return Ok(());
        }
        let versions = VersionRows::load(&DbVersionReader(&self.db), &[item]).await?;
        let data = self
            .get_user_data_dtos(&versions.all_version_ids(item), user_id)
            .await?;
        if let Some(resume) = resume_user_data(&versions, item, |id| data.get(&id)) {
            resume.apply_to(dto);
        }
        Ok(())
    }

    /// `item.FillUserDataDtoValues(dto, userData, null, user, options)` as
    /// `GetUserDataDto(item, user)` runs it (v12.2 UserDataManager.cs:411-427),
    /// chosen by class ([`UserDataFill`]): a `Folder` (Folder.cs:2024-2081)
    /// takes its played numbers from its leaves, any other item
    /// (BaseItem.cs:2746-2767) turns a positive resume position over a known
    /// runtime into `PlayedPercentage`. Unwired (unit tests), the stored row
    /// is the whole answer.
    async fn fill_user_data_dto_values(
        &self,
        dto: &mut UserItemDataDto,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        let Some(members) = self.members.get() else {
            return Ok(());
        };
        let Some(library) = members.library.upgrade() else {
            return Ok(());
        };
        let Some(item) = library.get_item_by_id(item_id).await? else {
            return Ok(());
        };
        match UserDataFill::of(&item) {
            UserDataFill::Stored => Ok(()),
            UserDataFill::Leaf => {
                crate::dto_service::fill_leaf_played_percentage(dto, item.run_time_ticks);
                // `GetResumeUserData(user, item)?.ApplyTo(dto)` (UserDataManager.cs:424).
                self.apply_version_resume(dto, &item, item_id, user_id)
                    .await
            }
            UserDataFill::Counts => {
                let Some(user) = crate::item_repository::query_user(&self.db, user_id).await?
                else {
                    return Ok(());
                };
                let counts = self
                    .container_counts(item_id, &user, &members.counts)
                    .await?;
                crate::dto_service::fill_folder_played(dto, counts);
                Ok(())
            }
        }
    }

    /// [`UserDataManager::get_user_data_dto`] for each of `items`, read
    /// together for the `UserDataChanged` push: the rows and keys in one
    /// batch ([`UserDataManager::get_user_data_dtos_for_rows`], provider ids
    /// hydrated, as the single read loads its item by id), then each item's
    /// [`UserDataFill`] from the row in hand — a leaf's `PlayedPercentage`,
    /// the version resume of every video in one group read
    /// ([`Self::version_resume_batch`]) and a folder's counts, the one read
    /// left per item. An item whose fill read fails is left out, as the
    /// single read fails it; the version read is shared, so its failure
    /// leaves out every video that took part in it. Unwired (unit tests), the
    /// stored rows are the whole answer.
    ///
    /// # Errors
    /// [`ServiceError`] when the batch read fails.
    async fn user_data_dtos_for_items(
        &self,
        items: &[BaseItemEntity],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let mut dtos = self
            .get_user_data_dtos_for_rows(items, user_id, true)
            .await?;
        let Some(members) = self
            .members
            .get()
            .filter(|members| members.library.strong_count() > 0)
        else {
            return Ok(dtos);
        };
        let mut resumable = Vec::new();
        let mut folders = Vec::new();
        for item in items {
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            let Some(dto) = dtos.get_mut(&id) else {
                continue;
            };
            match UserDataFill::of(item) {
                UserDataFill::Stored => {}
                UserDataFill::Leaf => {
                    crate::dto_service::fill_leaf_played_percentage(dto, item.run_time_ticks);
                    if takes_version_resume(item) {
                        resumable.push(item);
                    }
                }
                UserDataFill::Counts => folders.push(id),
            }
        }
        match self.version_resume_batch(&resumable, user_id).await {
            Ok(resumes) => {
                for (id, resume) in resumes {
                    if let Some(dto) = dtos.get_mut(&id) {
                        resume.apply_to(dto);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    %user_id,
                    %error,
                    "user-data push: reading the version resume failed; leaving the videos out"
                );
                for item in resumable {
                    if let Ok(id) = Uuid::parse_str(&item.id) {
                        dtos.remove(&id);
                    }
                }
            }
        }
        if folders.is_empty() {
            return Ok(dtos);
        }
        match crate::item_repository::query_user(&self.db, user_id).await {
            Ok(Some(user)) => {
                for id in folders {
                    match self.container_counts(id, &user, &members.counts).await {
                        Ok(counts) => {
                            if let Some(dto) = dtos.get_mut(&id) {
                                crate::dto_service::fill_folder_played(dto, counts);
                            }
                        }
                        Err(error) => {
                            tracing::debug!(
                                %user_id,
                                item_id = %id,
                                %error,
                                "user-data push: counting the folder's leaves failed"
                            );
                            dtos.remove(&id);
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %user_id,
                    %error,
                    "user-data push: reading the user failed; leaving the folders out"
                );
                for id in folders {
                    dtos.remove(&id);
                }
            }
        }
        Ok(dtos)
    }

    /// [`Self::apply_version_resume`] for many videos at once
    /// (`GetResumeUserDataBatch`, v12.2 `UserDataManager.cs:263-357`): their
    /// version groups in one [`crate::video_versions::VersionRows::load`],
    /// the user data of every version in a group in one read, and each
    /// video's version played last, by video id. A video with no other
    /// version has none.
    ///
    /// # Errors
    /// [`ServiceError`] when a read fails.
    async fn version_resume_batch(
        &self,
        videos: &[&BaseItemEntity],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, crate::video_versions::VersionResumeData>, ServiceError> {
        use crate::video_versions::{DbVersionReader, VersionRows, resume_user_data};
        if videos.is_empty() {
            return Ok(HashMap::new());
        }
        let versions = VersionRows::load(&DbVersionReader(&self.db), videos).await?;
        let mut wanted: Vec<Uuid> = videos
            .iter()
            .map(|video| versions.all_version_ids(video))
            .filter(|group| group.len() > 1)
            .flatten()
            .collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        wanted.sort_unstable();
        wanted.dedup();
        let data = self.get_user_data_dtos(&wanted, user_id).await?;
        Ok(videos
            .iter()
            .filter_map(|video| {
                let id = Uuid::parse_str(&video.id).ok()?;
                resume_user_data(&versions, video, |id| data.get(&id)).map(|resume| (id, resume))
            })
            .collect())
    }

    /// Port of `BaseItem.MarkPlayed(user, datePlayed, resetPosition: true)`
    /// for one item that is no container.
    async fn mark_item_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        let (row, ()) = self
            .mutate_row(item_id, user_id, |row| {
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
            })
            .await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::TogglePlayed)
            .await;
        // `Video.MarkPlayed` → `PropagatePlayedState(user, true, resetPosition)`.
        self.propagate_played_state(user_id, item_id, true, true)
            .await?;
        Ok(to_dto(row, item_id))
    }

    /// Port of `BaseItem.MarkUnplayed` → `ResetPlayedState` for one item
    /// that is no container.
    async fn mark_item_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        let (row, ()) = self
            .mutate_row(item_id, user_id, |row| {
                row.play_count = 0;
                row.playback_position_ticks = 0;
                row.last_played_date = None;
                row.played = false;
            })
            .await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::TogglePlayed)
            .await;
        // `Video.MarkUnplayed` → `PropagatePlayedState(user, false, true)`.
        self.propagate_played_state(user_id, item_id, false, true)
            .await?;
        Ok(to_dto(row, item_id))
    }
}

/// What a play-state report does to the row: `UserDataManager.UpdatePlayState`
/// and the `LastPlayedDate` stamp `SessionManager` adds around it, or, for a stop
/// with no position, the completed play `OnPlaybackStopped` assumes. A report
/// with no position that is no stop changes nothing here. Answers whether the
/// item was played to completion.
fn apply_play_report(
    row: &mut UserDataEntity,
    reported_position_ticks: Option<i64>,
    reason: UserDataSaveReason,
    item: &PlayStateItem,
    config: &ferrofin_model::configuration::ServerConfiguration,
) -> bool {
    let Some(mut position_ticks) = reported_position_ticks else {
        if reason != UserDataSaveReason::PlaybackFinished {
            return false;
        }
        // Source SessionManager's successful stop without a position
        // counts a completed play before its PlaybackFinished event.
        row.play_count += 1;
        row.played = item.played_status;
        row.playback_position_ticks = 0;
        row.last_played_date = Some(chrono::Utc::now());
        return true;
    };
    let runtime_ticks = item.runtime_ticks;
    let has_runtime = runtime_ticks > 0;
    let is_audiobook = matches!(item.kind, BaseItemKind::AudioBook);
    let is_book = matches!(item.kind, BaseItemKind::Book);
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

    if !item.played_status {
        position_ticks = 0;
        row.played = false;
    }
    if !item.resume {
        position_ticks = 0;
    }

    row.playback_position_ticks = position_ticks;
    if played_to_completion || position_ticks > 0 {
        row.last_played_date = Some(chrono::Utc::now());
    }
    played_to_completion
}

/// A container a played mark applies to through its members.
struct Container {
    id: Uuid,
    root: BaseItemEntity,
    user: ferrofin_db::entities::users::UserEntity,
    /// The items the mark is written on, in the order they were found.
    leaves: Vec<Uuid>,
}

/// What `item.FillUserDataDtoValues(dto, userData, null, user, options)`
/// adds to an item's stored user data in `GetUserDataDto(item, user)` (v12.2
/// UserDataManager.cs:411-427), chosen by the item's class as the C# virtual
/// is, not by the stored `IsFolder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserDataFill {
    /// Nothing: a type with no class, or a folder whose played state is not
    /// its leaves' — with no item DTO, only `Folder`'s `SupportsPlayedStatus`
    /// half applies, and a by-name row (a by-name artist, a genre) never
    /// passes it.
    Stored,
    /// Any item but a folder (BaseItem.cs:2746-2767): a positive resume
    /// position over a known runtime becomes `PlayedPercentage`, then a video
    /// takes the completion of its version played last
    /// ([`takes_version_resume`]).
    Leaf,
    /// A `Folder` (Folder.cs:2024-2081) whose played numbers come from its
    /// leaves.
    Counts,
}

impl UserDataFill {
    fn of(item: &BaseItemEntity) -> Self {
        let Some(kind) = kind_from_type_name(&item.type_) else {
            return Self::Stored;
        };
        if !crate::kinds::is_folder(kind) {
            return Self::Leaf;
        }
        if crate::kinds::supports_user_data_from_children(kind)
            && supports_played_status(kind, parse_media_type(item.media_type.as_deref()))
        {
            Self::Counts
        } else {
            Self::Stored
        }
    }
}

/// Whether `GetResumeUserData(user, item)` can fold a version's completion
/// into `item`'s user data: a video that is no version itself.
fn takes_version_resume(item: &BaseItemEntity) -> bool {
    item.primary_version_id.is_none()
        && kind_from_type_name(&item.type_).is_some_and(crate::kinds::is_video)
}

/// Whether a played mark on this item goes to its members: a `Folder`
/// subclass upstream, whatever its stored `IsFolder`.
fn is_container(item: &BaseItemEntity) -> bool {
    kind_from_type_name(&item.type_).is_some_and(crate::kinds::is_folder)
}

/// The query listing a container's members for a played mark (see
/// [`FerrofinUserDataManager::container_members`]).
fn member_query(
    id: Uuid,
    folder: &BaseItemEntity,
    user: &ferrofin_db::entities::users::UserEntity,
    played: bool,
) -> ferrofin_traits::options::InternalItemsQuery {
    use ferrofin_traits::options::InternalItemsQuery;
    let user_entity = Some(user.clone());
    match kind_from_type_name(&folder.type_) {
        // `BoxSet.GetLinkedChildren(user)`: the parent query merges the
        // manual links (a legacy set's children are its rows on disk), and
        // the box set's own listing keeps only the members from libraries
        // the user can open (gl-571).
        Some(BaseItemKind::BoxSet) => InternalItemsQuery {
            parent_id: id,
            parent_type: Some(BaseItemKind::BoxSet),
            user: user_entity,
            apply_folder_display_options: true,
            enable_total_record_count: false,
            ..InternalItemsQuery::default()
        },
        // `Series.GetItemsInternal`'s recursive branch: everything under the
        // series key, never a virtual item.
        Some(BaseItemKind::Series) => InternalItemsQuery {
            recursive: true,
            is_folder: Some(false),
            is_virtual_item: Some(false),
            series_presentation_unique_key: Some(crate::library_manager::episode_presentation_key(
                folder,
            )),
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
            user: user_entity,
            enable_total_record_count: false,
            ..InternalItemsQuery::default()
        },
        // `Folder.MarkPlayed`'s query, through the folder's own
        // `GetItemsInternal` (a season's `GetEpisodes`, a playlist's links,
        // a library's top parents).
        kind => InternalItemsQuery {
            parent_id: id,
            parent_type: kind,
            recursive: true,
            is_folder: Some(false),
            is_virtual_item: (played && !user.display_missing_episodes).then_some(false),
            user: user_entity,
            apply_folder_display_options: true,
            enable_total_record_count: false,
            ..InternalItemsQuery::default()
        },
    }
}

/// `item is Episode { IsUnaired: true }` — `BaseItem.IsUnaired`
/// (BaseItem.cs:213): it premieres today or later, by the server's date.
fn is_unaired_episode(item: &BaseItemEntity) -> bool {
    kind_from_type_name(&item.type_) == Some(BaseItemKind::Episode)
        && item.premiere_date.is_some_and(|date| {
            date.with_timezone(&chrono::Local).date_naive() >= chrono::Local::now().date_naive()
        })
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
    // Only a season or an episode is keyed through its series, so no other
    // item formats its id to look one up.
    if !matches!(kind, BaseItemKind::Season | BaseItemKind::Episode) {
        return keys;
    }
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
        channel_name_key: crate::user_data_keys::live_tv_channel_name_key_enabled(),
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
/// (they are filled by the DTO service against the resolved item). The row's
/// key moves into the DTO.
fn to_dto(row: UserDataEntity, item_id: Uuid) -> UserItemDataDto {
    UserItemDataDto {
        rating: row.rating,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: row.playback_position_ticks,
        play_count: row.play_count,
        is_favorite: row.is_favorite,
        likes: row.rating.map(|rating| rating >= MIN_LIKE_VALUE),
        last_played_date: row.last_played_date,
        played: row.played,
        key: row.custom_data_key,
        item_id,
    }
}

/// [`to_dto`] of the row `GetUserData` answers for an item the user never
/// wrote ([`FerrofinUserDataManager::empty_row`]) keyed by `key`, without
/// building that row: its ids are never read.
fn unwritten_dto(item_id: Uuid, key: String) -> UserItemDataDto {
    UserItemDataDto {
        rating: None,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: 0,
        play_count: 0,
        is_favorite: false,
        likes: None,
        last_played_date: None,
        played: false,
        key,
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
        self.mutate_row(item_id, user_id, |row| {
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
        })
        .await?;
        self.notify_saved(user_id, item_id, reason).await;
        Ok(())
    }

    async fn get_stream_default_data(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<(Option<i32>, Option<i32>), ServiceError> {
        let row = self.read_row(item_id, user_id).await?;
        Ok(row.map_or((None, None), |r| {
            (r.audio_stream_index, r.subtitle_stream_index)
        }))
    }

    async fn get_stream_default_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, (Option<i32>, Option<i32>)>, ServiceError> {
        Ok(self
            .stored_preferred_rows(item_ids, user_id)
            .await?
            .into_iter()
            .map(|(id, row)| (id, (row.audio_stream_index, row.subtitle_stream_index)))
            .filter(|(_, remembered)| *remembered != (None, None))
            .collect())
    }

    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        let row = self.read_or_unwritten_row(item_id, user_id).await?;
        let mut dto = to_dto(row, item_id);
        self.fill_user_data_dto_values(&mut dto, item_id, user_id)
            .await?;
        Ok(Some(dto))
    }

    async fn get_user_data_dtos(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        let rows = self.stored_user_data_rows(item_ids, user_id).await?;
        // Every item's current keys in one batch, for the page: a stored row is picked by them,
        // and an item without one gets the empty-row DTO keyed by its first
        // (`GetUserDataBatch`'s `keys[0]`, `UserDataManager.cs:251`). An id with no item row
        // keeps the id as its key.
        let mut ids: Vec<String> = item_ids.iter().map(|&id| guid_to_db(id)).collect();
        ids.sort_unstable();
        ids.dedup();
        let keys = if ids.is_empty() {
            HashMap::new()
        } else {
            self.current_keys(&ids).await?
        };
        let mut map: HashMap<Uuid, UserItemDataDto> = Self::preferred_rows(rows, &keys)
            .into_iter()
            .map(|(id, row)| (id, to_dto(row, id)))
            .collect();
        for &item_id in item_ids {
            map.entry(item_id).or_insert_with(|| {
                let mut row = Self::empty_row(item_id, user_id);
                if let Some(first) = keys.get(&guid_to_db(item_id)).and_then(|keys| keys.first()) {
                    row.custom_data_key.clone_from(first);
                }
                to_dto(row, item_id)
            });
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
        let children: Vec<&BaseItemEntity> = items
            .iter()
            .filter(|item| {
                matches!(
                    kind_from_type_name(&item.type_),
                    Some(BaseItemKind::Season | BaseItemKind::Episode)
                )
            })
            .collect();
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
            let candidates = stored.remove(&id).unwrap_or_default();
            // `ResolveUserDataRow` answers with an only row whatever the
            // item's keys are, so they are derived only to choose between
            // rows, or to key the row of an item the user never wrote.
            let dto = match <[UserDataEntity; 1]>::try_from(candidates) {
                Ok([row]) => to_dto(row, id),
                Err(candidates) => {
                    let current = row_data_keys(item, id, &providers, &series_keys);
                    match Self::preferred_row(candidates, &current) {
                        Some(row) => to_dto(row, id),
                        None => unwritten_dto(id, current.into_iter().next().unwrap_or_default()),
                    }
                }
            };
            map.insert(id, dto);
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
        let (row, ()) = self
            .mutate_row(item_id, user_id, |row| {
                row.likes = likes;
                row.rating = likes.map(|likes| if likes { 10.0 } else { 1.0 });
            })
            .await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::UpdateUserRating)
            .await;
        Ok(to_dto(row, item_id))
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
        self.save_play_state(user_id, item_id, reported_position_ticks, reason, None)
            .await
    }

    async fn update_play_progress(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        streams: ferrofin_traits::library::ReportedStreams,
    ) -> Result<bool, ServiceError> {
        let played = self
            .save_play_state(
                user_id,
                item_id,
                reported_position_ticks,
                UserDataSaveReason::PlaybackProgress,
                Some(streams),
            )
            .await?;
        if streams.resets_selections() {
            self.reset_playback_stream_selections(user_id, item_id)
                .await?;
        }
        Ok(played)
    }

    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        match self.container_members(user_id, item_id, true).await? {
            Some(container) => {
                self.mark_container(user_id, &container, true, date_played)
                    .await
            }
            None => self.mark_item_played(user_id, item_id, date_played).await,
        }
    }

    async fn mark_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        match self.container_members(user_id, item_id, false).await? {
            Some(container) => self.mark_container(user_id, &container, false, None).await,
            None => self.mark_item_unplayed(user_id, item_id).await,
        }
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
        // PlayCount++, and non-resumable kinds (photos, books — anything
        // without position-ticks resume) are played outright.
        // LastPlayedDate is only set here for items that are unplayed or
        // non-resumable; a rewatch of a played, resumable item (episode/movie)
        // defers the stamp to the progress/stop path so that barely-started
        // rewatches don't advance Next Up.
        let item = self.item_runtime_and_kind(item_id).await?;
        let resumable = item.as_ref().is_some_and(|item| item.resume);
        let played_outright = item.is_some_and(|item| item.played_status && !item.resume);
        self.mutate_row(item_id, user_id, |row| {
            row.play_count += 1;
            if !row.played || !resumable {
                row.last_played_date = Some(chrono::Utc::now());
            }
            if played_outright {
                row.played = true;
            }
        })
        .await?;
        self.notify_saved(user_id, item_id, UserDataSaveReason::PlaybackStart)
            .await;
        Ok(())
    }

    async fn get_content_permissions(
        &self,
        user_id: Uuid,
    ) -> Result<Option<ContentPermissions>, ServiceError> {
        use ferrofin_db::enums::{PermissionKind, PreferenceKind};
        // The user's policy rows, from the cross-request cache (`Database::user_policy`): the
        // five permissions and the "Allow media deletion from" list (stored `,`-delimited, as
        // C# writes it).
        let policy = self
            .db
            .user_policy(&guid_to_db(user_id))
            .await
            .map_err(ServiceError::from)?;
        let has = |kind: PermissionKind| policy.permission(i32::from(kind));
        let content_deletion_folders = policy
            .preference(i32::from(PreferenceKind::EnableContentDeletionFromFolders))
            .into_iter()
            .flat_map(|value| value.split(','))
            .filter_map(ferrofin_util::guid_extensions::parse_dotnet_guid)
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
        // (`PermissionKind`), from the cached policy rows (`Database::user_policy`).
        let policy = self
            .db
            .user_policy(&guid_to_db(user_id))
            .await
            .map_err(ServiceError::from)?;
        let rows: Vec<(i32, bool)> = policy
            .permissions
            .iter()
            .filter(|(kind, _)| matches!(kind, 8 | 9 | 19))
            .copied()
            .collect();
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

    /// The row [`Self::read_row`] would pick for each of `item_ids`, in two
    /// reads for the whole set; an item with no stored row is absent.
    async fn stored_preferred_rows(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashMap<Uuid, UserDataEntity>, ServiceError> {
        let rows = self.stored_user_data_rows(item_ids, user_id).await?;
        if rows.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<_> = rows.keys().copied().map(guid_to_db).collect();
        let keys = self.current_keys(&ids).await?;
        Ok(Self::preferred_rows(rows, &keys))
    }

    /// Each item's current user-data keys (`GetUserDataKeys`), by its stored id, for the whole
    /// set at once.
    async fn current_keys(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>, ServiceError> {
        let mut connection = self.db.pool().acquire().await.map_err(db_err)?;
        Ok(
            crate::user_data_key_repository::load_keys_for_items(&mut connection, ids)
                .await?
                .into_iter()
                .collect(),
        )
    }

    /// The row [`Self::read_row`] would pick among each item's stored `rows`, by its current
    /// `keys` (its own id when it has none).
    fn preferred_rows(
        rows: HashMap<Uuid, Vec<UserDataEntity>>,
        keys: &HashMap<String, Vec<String>>,
    ) -> HashMap<Uuid, UserDataEntity> {
        rows.into_iter()
            .filter_map(|(id, candidates)| {
                let own = [id.to_string()];
                let current = keys
                    .get(&guid_to_db(id))
                    .map_or(own.as_slice(), Vec::as_slice);
                Self::preferred_row(candidates, current).map(|row| (id, row))
            })
            .collect()
    }
}

/// Test-only: sets the audio and subtitle stream indices `user` remembers
/// for `item` on its existing `UserData` rows.
#[cfg(test)]
pub(crate) async fn remember_streams(
    db: &Database,
    user: Uuid,
    item: Uuid,
    audio: Option<i32>,
    subtitle: Option<i32>,
) {
    sqlx::query(
        r#"UPDATE "UserData" SET "AudioStreamIndex" = ?3, "SubtitleStreamIndex" = ?4
           WHERE "UserId" = ?1 AND "ItemId" = ?2"#,
    )
    .bind(guid_to_db(user))
    .bind(guid_to_db(item))
    .bind(audio)
    .bind(subtitle)
    .execute(db.writer())
    .await
    .expect("remember streams");
}

/// Test-only: how many `UserData` rows item `item` holds.
#[cfg(test)]
pub(crate) async fn user_data_row_count(db: &Database, item: Uuid) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1"#)
        .bind(guid_to_db(item))
        .fetch_one(db.pool())
        .await
        .expect("count user data")
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
    mod lost_updates;

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

    /// The page read of remembered stream indices picks, per item, the row
    /// the per-item read picks — the current user-data key over a stale one —
    /// and leaves out items that remember nothing.
    #[tokio::test]
    async fn stream_default_batch_agrees_with_the_per_item_read() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let [remembers, rekeyed, forgets, unplayed] = [(); 4].map(|()| Uuid::new_v4());
        for item in [remembers, rekeyed, forgets, unplayed] {
            seed_named_item(&db, item, BaseItemKind::Movie, "Movie").await;
        }
        seed_provider_id(&db, rekeyed, "Tmdb", "603").await;
        for (item, key, audio, subtitle) in [
            (remembers, remembers.to_string(), Some(2), Some(-1)),
            (rekeyed, "stale".to_owned(), Some(7), Some(5)),
            (rekeyed, "603".to_owned(), None, Some(3)),
            (forgets, forgets.to_string(), None, None),
        ] {
            sqlx::query(
                r#"INSERT INTO "UserData"
                   ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                    "PlaybackPositionTicks", "Played", "AudioStreamIndex",
                    "SubtitleStreamIndex")
                   VALUES (?1, ?2, ?3, 0, 0, 0, 0, ?4, ?5)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(user))
            .bind(key)
            .bind(audio)
            .bind(subtitle)
            .execute(db.writer())
            .await
            .expect("user data");
        }
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        let ids = [remembers, rekeyed, forgets, unplayed];

        let batch = manager
            .get_stream_default_data_batch(&ids, user)
            .await
            .unwrap();

        assert_eq!(batch.len(), 2, "{batch:?}");
        for item in ids {
            let single = manager.get_stream_default_data(item, user).await.unwrap();
            assert_eq!(
                batch.get(&item).copied().unwrap_or_default(),
                single,
                "{item}"
            );
        }
        assert_eq!(batch[&rekeyed], (None, Some(3)), "the current key's row");
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

    /// `UserDataManagerTests.SaveUserData_RowUnderRetiredKey_IsKeptInAgreement`
    /// (v12.2): a save keeps the user's row under a key the item no longer
    /// reports, which a re-added item reattaches by, but brings it to the
    /// saved values, so a stale position cannot hold the item in Continue
    /// Watching once it is played.
    #[tokio::test]
    async fn a_save_keeps_a_retired_key_row_in_agreement() {
        let db = test_db().await;
        let (user, item, other) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        seed_user(&db, user).await;
        crate::test_support::seed_named_user(&db, other, "other").await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;
        for who in [user, other] {
            sqlx::query(
                r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite",
                       "PlayCount","PlaybackPositionTicks","Played")
                   VALUES (?1, ?2, 'Old Key', 0, 0, 111, 0)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(who))
            .execute(db.writer())
            .await
            .expect("seed retired row");
        }

        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.save_user_data(
            user,
            item,
            &UpdateUserItemDataDto {
                played: Some(true),
                playback_position_ticks: Some(0),
                ..UpdateUserItemDataDto::default()
            },
        )
        .await
        .expect("save");

        let rows: Vec<(String, String, bool, i64)> = sqlx::query_as(
            r#"SELECT "UserId", "CustomDataKey", "Played", "PlaybackPositionTicks"
               FROM "UserData" WHERE "ItemId" = ?1 ORDER BY 1, 2"#,
        )
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("rows");
        let (mine, theirs): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|row| row.0 == guid_to_db(user));
        let mut keys: Vec<&str> = mine.iter().map(|row| row.1.as_str()).collect();
        keys.sort_unstable();
        let mut expected = vec![item.to_string(), "700391".to_owned(), "Old Key".to_owned()];
        expected.sort_unstable();
        assert_eq!(keys, expected, "the retired row is kept");
        assert!(
            mine.iter().all(|row| row.2 && row.3 == 0),
            "every row agrees: {mine:?}"
        );
        assert_eq!(
            theirs,
            [(guid_to_db(other), "Old Key".to_owned(), false, 111)],
            "another user's row is left alone"
        );
    }

    /// The resume list matches any row of the user, so a retired-key row's
    /// stale position kept a finished movie in Continue Watching
    /// (upstream issue 12195) until the save brought that row along.
    #[tokio::test]
    async fn a_finished_item_leaves_resume_despite_a_retired_key_row() {
        use ferrofin_traits::options::InternalItemsQuery;
        use ferrofin_traits::persistence::ItemRepository as _;
        let db = test_db().await;
        let user_id = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        let user = crate::test_support::seed_user_with_defaults(&db, user_id).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;
        crate::test_support::seed_library_over(&db, &[item]).await;
        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite",
                   "PlayCount","PlaybackPositionTicks","Played")
               VALUES (?1, ?2, 'Old Key', 0, 0, 24000000000, 0)"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("seed retired row");
        let repository = crate::FerrofinItemRepository::new(
            db.clone(),
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );
        let resume = || async {
            repository
                .get_item_list(&InternalItemsQuery {
                    user: Some(user.clone()),
                    is_resumable: Some(true),
                    ..InternalItemsQuery::default()
                })
                .await
                .expect("resume")
                .len()
        };
        assert_eq!(resume().await, 1, "half watched under the old key");

        FerrofinUserDataManager::new(db.clone(), config())
            .save_user_data(
                user_id,
                item,
                &UpdateUserItemDataDto {
                    played: Some(true),
                    playback_position_ticks: Some(0),
                    ..UpdateUserItemDataDto::default()
                },
            )
            .await
            .expect("finish");
        assert_eq!(resume().await, 0, "finished");
    }

    /// An item the user never wrote reports its first user-data key, as
    /// `GetUserData`'s `new UserItemData { Key = item.GetUserDataKeys()[0] }`
    /// and `GetUserDataBatch`'s `keys[0]` do: the TMDb id for a movie that has
    /// one, its id for one that has none. An id with no item row keeps the id.
    #[rstest::rstest]
    #[case::provider_key(Some("700391"), true, "700391")]
    #[case::no_provider(None, true, "00000000-0000-0000-0000-000000000002")]
    #[case::no_item(None, false, "00000000-0000-0000-0000-000000000002")]
    #[tokio::test]
    async fn a_never_written_item_reports_its_first_user_data_key(
        #[case] tmdb: Option<&str>,
        #[case] stored: bool,
        #[case] key: &str,
    ) {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        if stored {
            seed_item(&db, item, BaseItemKind::Movie).await;
        }
        if let Some(tmdb) = tmdb {
            seed_provider_id(&db, item, "Tmdb", tmdb).await;
        }
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        let one = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("dto");
        assert_eq!(one.key, key);
        assert!(!one.is_favorite && !one.played);
        let batch = mgr.get_user_data_dtos(&[item], user).await.expect("batch");
        assert_eq!(batch[&item].key, key, "the batch read agrees");
    }

    /// An episode never written reports its series-derived first key, from
    /// the per-item and the batch read alike.
    #[tokio::test]
    async fn a_never_written_episode_reports_its_series_derived_key() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let series = Uuid::from_u128(2);
        let episode = Uuid::from_u128(3);
        seed_user(&db, user).await;
        seed_item(&db, series, BaseItemKind::Series).await;
        seed_provider_id(&db, series, "Tvdb", "343498").await;
        crate::test_support::seed_episode(&db, episode, "series-key", 1, 2, false, None).await;
        let mut row = fetch_item(&db, episode).await;
        row.series_id = Some(guid_to_db(series));
        crate::test_support::save_item(&db, &row).await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        let one = mgr
            .get_user_data_dto(episode, user)
            .await
            .expect("read")
            .expect("dto");
        // The series' keys are `[343498, <series id>]`; an episode drops the
        // series' own id and suffixes `SSSEEE`.
        assert_eq!(one.key, "343498001002");
        let batch = mgr
            .get_user_data_dtos(&[episode, episode], user)
            .await
            .expect("batch");
        assert_eq!(batch[&episode].key, "343498001002");
    }

    /// The writes that answer with the saved row's DTO.
    #[derive(Debug, Clone, Copy)]
    enum FirstWrite {
        MarkPlayed,
        MarkUnplayed,
        SetLikes,
    }

    /// A write on a never-written item answers with the key its rows were
    /// saved under, as upstream's re-read through `GetUserData` does, not
    /// the item id.
    #[rstest::rstest]
    #[case::mark_played(FirstWrite::MarkPlayed)]
    #[case::mark_unplayed(FirstWrite::MarkUnplayed)]
    #[case::set_likes(FirstWrite::SetLikes)]
    #[tokio::test]
    async fn a_first_write_answers_with_the_first_user_data_key(#[case] write: FirstWrite) {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(2);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        seed_provider_id(&db, item, "Tmdb", "700391").await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        let answered = match write {
            FirstWrite::MarkPlayed => mgr.mark_played(user, item, None).await,
            FirstWrite::MarkUnplayed => mgr.mark_unplayed(user, item).await,
            FirstWrite::SetLikes => mgr.set_likes(user, item, Some(true)).await,
        }
        .expect("write");
        assert_eq!(answered.key, "700391");
        let read = mgr
            .get_user_data_dto(item, user)
            .await
            .expect("read")
            .expect("dto");
        assert_eq!(read.key, answered.key, "the write and the read agree");
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
    /// succeed. `upsert_one` used to `SELECT EXISTS` and then branch on the
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

    /// A page builds an unwritten item's DTO without its empty row, so it
    /// must be the DTO of that row under the same key.
    #[test]
    fn an_unwritten_dto_is_the_empty_rows_dto() {
        let item = Uuid::from_u128(2);
        let mut row = FerrofinUserDataManager::empty_row(item, Uuid::from_u128(1));
        row.custom_data_key = "Year-2021".to_owned();
        assert_eq!(
            unwritten_dto(item, "Year-2021".to_owned()),
            to_dto(row, item)
        );
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

    /// Upstream's overrides reach the play-state paths: `Audio` never resumes
    /// (`BaseItem.cs:190`), so a song is played outright at start and drops a
    /// reported position; so does a trailer extra (`Video.cs:63-88`). A movie
    /// keeps its position (`update_play_state_midway_keeps_resume_point`).
    #[tokio::test]
    async fn songs_and_trailer_extras_never_keep_a_resume_point() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let runtime = 3600 * TICKS_PER_SECOND;
        let song = Uuid::from_u128(0x50);
        let trailer = Uuid::from_u128(0x51);
        seed_item(&db, song, BaseItemKind::Audio).await;
        seed_item(&db, trailer, BaseItemKind::Video).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = ?1"#)
            .bind(runtime)
            .execute(db.writer())
            .await
            .expect("set runtime");
        sqlx::query(r#"UPDATE "BaseItems" SET "ExtraType" = 2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(trailer))
            .execute(db.writer())
            .await
            .expect("mark trailer extra");
        let mgr = FerrofinUserDataManager::new(db, config());

        for item in [song, trailer] {
            mgr.record_playback_start(user, item).await.expect("start");
            let row = mgr.read_row(item, user).await.expect("read").expect("row");
            assert!(row.played, "played outright at start: {item}");
            mgr.update_play_state(user, item, Some(runtime / 2))
                .await
                .expect("stop");
            let row = mgr.read_row(item, user).await.expect("read").expect("row");
            assert_eq!(row.playback_position_ticks, 0, "no resume point: {item}");
        }
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
    async fn rewatch_of_played_episode_defers_last_played_date() {
        let db = test_db().await;
        let user = Uuid::from_u128(0xA);
        let item = Uuid::from_u128(0xB);
        seed_user(&db, user).await;
        let runtime = 3600 * TICKS_PER_SECOND;
        seed_movie_with_runtime(&db, item, runtime).await;
        // Re-type to Episode so it supports_position_ticks_resume.
        sqlx::query(r#"UPDATE "BaseItems" SET "Type" = 'MediaBrowser.Controller.Entities.TV.Episode' WHERE "Id" = ?1"#)
            .bind(guid_to_db(item))
            .execute(db.writer())
            .await
            .expect("retype");
        let mgr = FerrofinUserDataManager::new(db, config());

        // First play: stamp is set.
        mgr.record_playback_start(user, item).await.expect("start");
        let first = mgr.read_row(item, user).await.unwrap().unwrap();
        assert!(first.last_played_date.is_some());
        let first_date = first.last_played_date.unwrap();

        // Mark it played with a completion past 90%.
        mgr.update_play_state(user, item, Some(runtime * 95 / 100))
            .await
            .expect("complete");
        let after_complete = mgr.read_row(item, user).await.unwrap().unwrap();
        assert!(after_complete.played);
        assert!(after_complete.last_played_date.unwrap() >= first_date);

        let completed_date = after_complete.last_played_date.unwrap();

        // Rewatch: start does NOT advance LastPlayedDate.
        mgr.record_playback_start(user, item)
            .await
            .expect("rewatch start");
        let rewatch = mgr.read_row(item, user).await.unwrap().unwrap();
        assert_eq!(rewatch.last_played_date.unwrap(), completed_date);
        assert_eq!(rewatch.play_count, 2);

        // A barely-started stop (below MinResumePct, default 5%) does NOT
        // advance LastPlayedDate.
        mgr.update_play_state(user, item, Some(runtime / 100))
            .await
            .expect("barely started");
        let barely = mgr.read_row(item, user).await.unwrap().unwrap();
        assert_eq!(barely.last_played_date.unwrap(), completed_date);

        // A stop past the resume threshold DOES advance it.
        mgr.update_play_state(user, item, Some(runtime / 2))
            .await
            .expect("mid-episode");
        let mid = mgr.read_row(item, user).await.unwrap().unwrap();
        assert!(mid.last_played_date.unwrap() >= completed_date);
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
        // `GetPreferenceValues<Guid>` parses as `Guid.TryParse` does: the
        // `(…)` form is a GUID, `urn:uuid:` is not.
        .bind(format!(
            " {} ,junk,({library_b}),urn:uuid:{}",
            library_a.simple(),
            Uuid::from_u128(0xD)
        ))
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
        mgr.mutate_row(item, user, |row| {
            row.audio_stream_index = Some(2);
            row.subtitle_stream_index = Some(1);
        })
        .await
        .expect("seed row");

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
            &db,
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

    /// A page's rows are not read again for their series (gl-982): the series an episode row
    /// names is the one its keys derive from, read in the same walk as every other page row's,
    /// even when the stored row has since moved to another series.
    #[tokio::test]
    async fn page_rows_name_their_series_without_being_read_again() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let named = Uuid::from_u128(0xF030);
        let stored = Uuid::from_u128(0xF031);
        let episode = Uuid::from_u128(0xF032);
        seed_user(&db, user).await;
        for (id, kind) in [
            (named, BaseItemKind::Series),
            (stored, BaseItemKind::Series),
            (episode, BaseItemKind::Episode),
        ] {
            seed_item(&db, id, kind).await;
        }
        seed_provider_id(&db, named, "Tvdb", "named-series").await;
        seed_provider_id(&db, stored, "Tvdb", "stored-series").await;
        let point = |series: Uuid| {
            sqlx::query(r#"UPDATE "BaseItems" SET "SeriesId" = ?2, "ParentIndexNumber" = 1, "IndexNumber" = 2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(episode))
                .bind(guid_to_db(series))
                .execute(db.writer())
        };
        point(named).await.unwrap();
        let row = fetch_item(&db, episode).await;
        point(stored).await.unwrap();
        let manager = FerrofinUserDataManager::new(db.clone(), config());
        for hydrated in [false, true] {
            let selected = manager
                .get_user_data_dtos_for_rows(std::slice::from_ref(&row), user, hydrated)
                .await
                .unwrap();
            assert_eq!(selected[&episode].key, "named-series001002");
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

    struct CultureNfoObserver {
        captured: std::sync::Mutex<Vec<String>>,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        finished: tokio::sync::Notify,
    }

    #[async_trait]
    impl UserDataSaveListener for CultureNfoObserver {
        async fn user_data_saved(
            &self,
            _: Uuid,
            _: Uuid,
            _: UserDataSaveReason,
        ) -> Result<(), ServiceError> {
            self.captured
                .lock()
                .unwrap()
                .push(ferrofin_util::current_culture::capture());
            self.started.notify_one();
            self.release.notified().await;
            self.captured
                .lock()
                .unwrap()
                .push(ferrofin_util::current_culture::capture());
            self.finished.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn detached_nfo_notification_inherits_request_culture_after_the_first_yield() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        let item = Uuid::from_u128(0xF007);
        seed_user(&db, user).await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        let manager = FerrofinUserDataManager::new(db, config());
        let observer = Arc::new(CultureNfoObserver {
            captured: std::sync::Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
        });
        let listener: Arc<dyn UserDataSaveListener> = observer.clone();
        manager
            .set_save_listener(Arc::downgrade(&listener))
            .unwrap();
        let previous = ferrofin_util::current_culture::capture();
        ferrofin_util::current_culture::scope(
            "sv".into(),
            manager.save_user_data_with_reason(
                user,
                item,
                &favorite_dto(),
                UserDataSaveReason::UpdateUserRating,
            ),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            observer.started.notified(),
        )
        .await
        .unwrap();
        assert_eq!(ferrofin_util::current_culture::capture(), previous);
        observer.release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            observer.finished.notified(),
        )
        .await
        .unwrap();
        assert_eq!(*observer.captured.lock().unwrap(), ["sv", "sv"]);
    }

    // ---- `Video.PropagatePlayedState` (upstream `BaseItemTests`) ----------

    /// Upstream `SetupVersionGroup`: a primary `Movie.mkv` and two local
    /// alternates, owned by and pointing at it, as the scan stores them.
    async fn version_group(db: &Database) -> (Uuid, Uuid, Uuid) {
        let (primary, alt1, alt2) = (
            Uuid::from_u128(10),
            Uuid::from_u128(11),
            Uuid::from_u128(12),
        );
        for (id, path) in [
            (primary, "/Movies/Movie/Movie.mkv"),
            (alt1, "/Movies/Movie/Movie - 1080p.mkv"),
            (alt2, "/Movies/Movie/Movie - 4K.mkv"),
        ] {
            seed_item(db, id, BaseItemKind::Movie).await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "Path" = ?2, "RunTimeTicks" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(path)
            .bind(72_000_000_000_i64)
            .execute(db.pool())
            .await
            .unwrap();
        }
        for alt in [alt1, alt2] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(alt))
            .bind(guid_to_db(primary))
            .execute(db.pool())
            .await
            .unwrap();
        }
        (primary, alt1, alt2)
    }

    async fn data(mgr: &FerrofinUserDataManager, item: Uuid, user: Uuid) -> UserItemDataDto {
        mgr.get_user_data_dto(item, user)
            .await
            .unwrap()
            .expect("user data")
    }

    async fn resume_at(mgr: &FerrofinUserDataManager, item: Uuid, user: Uuid, ticks: i64) {
        mgr.save_user_data(
            user,
            item,
            &UpdateUserItemDataDto {
                playback_position_ticks: Some(ticks),
                play_count: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// A single read of a primary's user data — the one `UserDataChanged`
    /// pushes and the played/favorite responses carry — takes its completion
    /// from the version played last (`GetUserDataDto` →
    /// `GetResumeUserData(user, item)?.ApplyTo(dto)`, UserDataManager.cs:424):
    /// finished on the 4K copy after the versions were grouped, the movie
    /// reads played, its own resume bar cleared. The version read directly
    /// keeps its own data.
    #[tokio::test]
    async fn a_primary_read_takes_the_completion_of_the_version_played_last() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, _alt1, alt2) = version_group(&db).await;
        let (mgr, _library) = container_manager(&db);
        let finished = chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        mgr.save_user_data(
            user,
            primary,
            &UpdateUserItemDataDto {
                playback_position_ticks: Some(36_000_000_000),
                last_played_date: Some(finished - chrono::Duration::days(30)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        mgr.save_user_data(
            user,
            alt2,
            &UpdateUserItemDataDto {
                played: Some(true),
                last_played_date: Some(finished),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let movie = data(&mgr, primary, user).await;
        assert!(movie.played, "finished on the 4K version");
        assert_eq!(movie.last_played_date, Some(finished));
        assert_eq!(movie.playback_position_ticks, 0);
        assert_eq!(movie.played_percentage, None);
        let version = data(&mgr, alt2, user).await;
        assert_eq!(version.item_id, alt2);
        assert!(version.played);
    }

    /// Both alternate versions are marked played, and their positions reset
    /// so a watched version does not linger in "Continue Watching"; their
    /// other state (the play count) stays.
    #[tokio::test]
    async fn propagate_played_state_marks_alternate_versions_and_resets_position_by_default() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        resume_at(&mgr, alt1, user, 1000).await;

        mgr.mark_played(user, primary, None).await.unwrap();

        for alt in [alt1, alt2] {
            let dto = data(&mgr, alt, user).await;
            assert!(dto.played, "{alt}");
            assert_eq!(dto.playback_position_ticks, 0);
        }
        assert_eq!(
            data(&mgr, alt1, user).await.play_count,
            3,
            "only Played and the position"
        );
    }

    #[tokio::test]
    async fn propagate_played_state_without_reset_leaves_position_untouched() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        resume_at(&mgr, alt1, user, 1000).await;

        mgr.propagate_played_state(user, primary, true, false)
            .await
            .unwrap();

        let dto = data(&mgr, alt1, user).await;
        assert!(dto.played);
        assert_eq!(dto.playback_position_ticks, 1000);
        assert!(data(&mgr, alt2, user).await.played);
    }

    /// Every alternate is fully reset to an unwatched state, mirroring
    /// `MarkUnplayed`: the played flag, play count, resume point and
    /// last-played date are all cleared.
    #[tokio::test]
    async fn propagate_played_state_unwatched_clears_all_watched_state_on_versions() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        for alt in [alt1, alt2] {
            mgr.save_user_data(
                user,
                alt,
                &UpdateUserItemDataDto {
                    played: Some(true),
                    play_count: Some(3),
                    playback_position_ticks: Some(1000),
                    last_played_date: Some(chrono::Utc::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        mgr.mark_unplayed(user, primary).await.unwrap();

        for alt in [alt1, alt2] {
            let dto = data(&mgr, alt, user).await;
            assert!(!dto.played);
            assert_eq!(dto.play_count, 0);
            assert_eq!(dto.playback_position_ticks, 0);
            assert_eq!(dto.last_played_date, None);
        }
    }

    /// Records each announced `UserDataChanged` message; with `deaf`, the
    /// user has no session that would hear one.
    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<(Uuid, serde_json::Value)>>, bool);

    #[async_trait]
    impl UserDataChangedSink for RecordingSink {
        async fn user_data_changed(&self, user_id: Uuid, data: &str) {
            self.0
                .lock()
                .unwrap()
                .push((user_id, serde_json::from_str(data).unwrap()));
        }

        async fn has_listener(&self, _user_id: Uuid) -> bool {
            !self.1
        }
    }

    /// The messages sent once `count` of them have arrived, or after a
    /// deadline well past the window, so a loaded host does not fail the test.
    async fn sent_after_window(
        sink: &RecordingSink,
        count: usize,
    ) -> Vec<(Uuid, serde_json::Value)> {
        let deadline = tokio::time::Instant::now() + USER_DATA_CHANGE_WINDOW * 10;
        loop {
            let sent = sink.0.lock().unwrap().clone();
            if sent.len() >= count || tokio::time::Instant::now() >= deadline {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Whether nothing was sent through the window that would have sent it.
    async fn nothing_sent_after_window(sink: &RecordingSink) -> bool {
        tokio::time::sleep(USER_DATA_CHANGE_WINDOW + Duration::from_millis(250)).await;
        sink.0.lock().unwrap().is_empty()
    }

    /// The item ids a `UserDataChanged` message lists, in order.
    fn listed(info: &serde_json::Value) -> Vec<String> {
        info["UserDataList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["ItemId"].as_str().unwrap().to_owned())
            .collect()
    }

    /// `UserDataChangeNotifier`: the saves of one window reach each user's
    /// sessions as one `UserDataChanged`, each item followed by its parent
    /// (`GetOwner() ?? GetParent()`), and a playback progress report is
    /// never announced.
    #[tokio::test]
    async fn saves_are_announced_in_one_batch_with_their_parents_and_progress_is_not() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (series, season) = (Uuid::from_u128(0x5E0), Uuid::from_u128(0x5E1));
        let (ep1, ep2) = (Uuid::from_u128(0xE1), Uuid::from_u128(0xE2));
        seed_show(
            &db,
            series,
            season,
            &[(ep1, false, None), (ep2, false, None)],
        )
        .await;
        let (mgr, _library) = container_manager(&db);
        let sink = Arc::new(RecordingSink::default());
        mgr.set_user_data_changed_sink(sink.clone());

        mgr.update_play_state(user, ep1, Some(1_000)).await.unwrap();
        assert!(
            nothing_sent_after_window(&sink).await,
            "a progress report is not announced"
        );

        mgr.mark_item_played(user, ep1, None).await.unwrap();
        mgr.set_likes(user, ep2, Some(true)).await.unwrap();
        mgr.mark_item_played(user, ep1, None).await.unwrap();
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "nothing before the window closes"
        );
        let sent = sent_after_window(&sink, 1).await;
        assert_eq!(sent.len(), 1, "one message for the batch");
        assert_eq!(sent[0].0, user);
        assert_eq!(
            listed(&sent[0].1),
            [ep1, season, ep2].map(|id| id.simple().to_string()),
            "each item once, with its season"
        );
        let season_data = &sent[0].1["UserDataList"][1];
        assert_eq!(
            season_data["UnplayedItemCount"], 1,
            "the season's count refreshed"
        );
    }

    /// A primary `Other.mkv` and its 4K copy, owned by and pointing at it: a
    /// second group beside [`version_group`]'s.
    async fn version_pair(db: &Database) -> (Uuid, Uuid) {
        let (primary, copy) = (Uuid::from_u128(40), Uuid::from_u128(41));
        for (id, path) in [
            (primary, "/Movies/Other/Other.mkv"),
            (copy, "/Movies/Other/Other - 4K.mkv"),
        ] {
            seed_item(db, id, BaseItemKind::Movie).await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "Path" = ?2, "RunTimeTicks" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(path)
            .bind(72_000_000_000_i64)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(copy))
        .bind(guid_to_db(primary))
        .execute(db.pool())
        .await
        .unwrap();
        (primary, copy)
    }

    /// A box set linking `members`, in a library that holds them.
    async fn box_set_of(db: &Database, members: &[Uuid]) -> Uuid {
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        let boxset = Uuid::from_u128(0xB0);
        crate::test_support::seed_folder_item(db, boxset, BaseItemKind::BoxSet, "Set", None).await;
        crate::test_support::seed_library_over(db, members).await;
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        // 0 = `LinkedChildType.Manual`.
        for &member in members {
            links.upsert_linked_child(boxset, member, 0).await.unwrap();
        }
        boxset
    }

    /// gl-1005: the push reads its items together, yet answers each one as
    /// the single read (`get_user_data_dto`, upstream's per-item
    /// `GetUserDataDto(item, user)` in `GetUserDataChangeInfo`) does: an
    /// episode's resume `PlayedPercentage`, the first user-data key of a
    /// never-written episode and movie, a season's, series' and box set's
    /// counts, a primary's completion from the version finished last, and a
    /// gone item's empty row keyed by its id.
    #[tokio::test]
    async fn the_batched_push_answers_each_item_as_the_single_read_does() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let (series, season) = (Uuid::from_u128(0x5E0), Uuid::from_u128(0x5E1));
        let (ep1, ep2, ep3) = (
            Uuid::from_u128(0xE1),
            Uuid::from_u128(0xE2),
            Uuid::from_u128(0xE3),
        );
        seed_show(
            &db,
            series,
            season,
            &[(ep1, false, None), (ep2, false, None), (ep3, false, None)],
        )
        .await;
        seed_provider_id(&db, series, "Tvdb", "73739").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = 1000 WHERE "Id" = ?1"#)
            .bind(guid_to_db(ep2))
            .execute(db.pool())
            .await
            .unwrap();
        let (primary, alt1, alt2) = version_group(&db).await;
        // A second group, read in the same version batch.
        let (other, other_4k) = version_pair(&db).await;
        let fresh = Uuid::from_u128(30);
        seed_item(&db, fresh, BaseItemKind::Movie).await;
        seed_provider_id(&db, fresh, "Tmdb", "700391").await;
        let boxset = box_set_of(&db, &[fresh, primary]).await;
        let gone = Uuid::from_u128(0xD0);
        let (mgr, _library) = container_manager(&db);
        mgr.mark_item_played(user, ep1, None).await.unwrap();
        resume_at(&mgr, ep2, user, 250).await;
        mgr.save_user_data(user, series, &favorite_dto())
            .await
            .unwrap();
        resume_at(&mgr, primary, user, 1000).await;
        mgr.save_user_data(
            user,
            alt2,
            &UpdateUserItemDataDto {
                played: Some(true),
                last_played_date: Some(chrono::Utc::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        mgr.save_user_data(
            user,
            other_4k,
            &UpdateUserItemDataDto {
                playback_position_ticks: Some(5_000),
                last_played_date: Some(chrono::Utc::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Set after the saves above, so only the batch below is announced.
        let sink = Arc::new(RecordingSink::default());
        mgr.set_user_data_changed_sink(sink.clone());
        let saved = vec![
            ep1, ep2, ep3, series, primary, alt1, other, other_4k, fresh, boxset, gone,
        ];
        // Each saved item followed, once, by its owner or parent: the
        // episodes' season, and the primaries that own `alt1` and `other_4k`.
        let listed = [
            ep1, season, ep2, ep3, series, primary, alt1, other, other_4k, fresh, boxset, gone,
        ];
        let mut single = Vec::with_capacity(listed.len());
        for id in listed {
            single.push(serde_json::to_value(data(&mgr, id, user).await).unwrap());
        }

        mgr.send_changes(HashMap::from([(user, saved)])).await;

        let sent = sink.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one message for the batch");
        assert_eq!(
            sent[0].1["UserDataList"],
            serde_json::Value::Array(single.clone()),
            "each item as the single read answers it, in the listed order"
        );
        // The fixture reaches every fill, so the two agree on more than the
        // stored rows.
        let of = |id: Uuid| &single[listed.iter().position(|&l| l == id).unwrap()];
        assert_eq!(of(ep2)["PlayedPercentage"], 25.0);
        assert_eq!(of(ep3)["Key"], "73739001003");
        assert_eq!(of(season)["UnplayedItemCount"], 2);
        assert_eq!(of(series)["UnplayedItemCount"], 2);
        assert_eq!(of(series)["IsFavorite"], true);
        assert_eq!(of(primary)["Played"], true, "finished on the 4K copy");
        assert_eq!(of(primary)["PlaybackPositionTicks"], 0);
        assert_eq!(
            of(other)["LastPlayedDate"],
            of(other_4k)["LastPlayedDate"],
            "the second group's video takes its 4K copy's date"
        );
        assert_eq!(of(fresh)["Key"], "700391");
        assert!(of(boxset)["UnplayedItemCount"].is_u64(), "{}", of(boxset));
        assert_eq!(of(gone)["Key"], gone.to_string());
    }

    /// A user with no session that would hear the batch is sent nothing.
    #[tokio::test]
    async fn a_batch_nobody_would_hear_is_not_sent() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let item = Uuid::from_u128(30);
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        let sink = Arc::new(RecordingSink(std::sync::Mutex::default(), true));
        mgr.set_user_data_changed_sink(sink.clone());

        mgr.mark_item_played(user, item, None).await.unwrap();
        assert!(nothing_sent_after_window(&sink).await);
    }

    /// Each version the propagation saved is announced to the user's
    /// sessions with the video itself (upstream fires `UserDataSaved` per
    /// save, which the notifier batches).
    #[tokio::test]
    async fn propagated_played_state_is_announced_for_each_other_version() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        // Never-written versions with keys of their own: each announcement
        // carries the version's first user-data key, not its id.
        seed_provider_id(&db, alt1, "Tmdb", "1111").await;
        seed_provider_id(&db, alt2, "Tmdb", "2222").await;
        let mgr = FerrofinUserDataManager::new(db, config());
        let sink = Arc::new(RecordingSink::default());
        mgr.set_user_data_changed_sink(sink.clone());

        mgr.mark_played(user, primary, None).await.unwrap();

        let sent = sent_after_window(&sink, 1).await;
        assert_eq!(sent.len(), 1, "one message for the group");
        let (to, info) = &sent[0];
        assert_eq!(*to, user);
        let mut items: Vec<(String, bool, String)> = info["UserDataList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["ItemId"].as_str().unwrap().to_owned(),
                    d["Played"].as_bool().unwrap(),
                    d["Key"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        items.sort();
        let mut want = vec![
            // The primary has no provider id of its own, so its first key is
            // its id.
            (primary.simple().to_string(), true, primary.to_string()),
            (alt1.simple().to_string(), true, "1111".to_owned()),
            (alt2.simple().to_string(), true, "2222".to_owned()),
        ];
        want.sort();
        assert_eq!(items, want);

        // A video with no versions announces only itself.
        let solo = Uuid::from_u128(30);
        seed_item(&mgr.db, solo, BaseItemKind::Movie).await;
        mgr.mark_played(user, solo, None).await.unwrap();
        let sent = sent_after_window(&sink, 2).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(listed(&sent[1].1), [solo.simple().to_string()]);
    }

    #[tokio::test]
    async fn propagate_played_state_single_version_does_nothing() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let solo = Uuid::from_u128(20);
        seed_item(&db, solo, BaseItemKind::Movie).await;
        let mgr = FerrofinUserDataManager::new(db.clone(), config());

        mgr.mark_played(user, solo, None).await.unwrap();

        let rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(DISTINCT "ItemId") FROM "UserData""#)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(rows, 1, "only the video's own data is written");
    }

    /// A completed playback report on an alternate (`SessionManager.cs:
    /// 1206-1211`) marks the primary and the other alternate played.
    #[tokio::test]
    async fn a_version_played_to_completion_marks_its_whole_group_played() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        resume_at(&mgr, primary, user, 1000).await;

        assert!(
            mgr.update_play_state(user, alt1, Some(72_000_000_000))
                .await
                .unwrap()
        );

        for version in [primary, alt2] {
            let dto = data(&mgr, version, user).await;
            assert!(dto.played, "{version}");
            assert_eq!(dto.playback_position_ticks, 0);
        }
    }

    /// A stop with no position counts as played to the end, and a completed
    /// version marks its whole group played whichever way it got there
    /// (`SessionManager.cs:1183-1211`).
    #[tokio::test]
    async fn a_stop_with_no_position_marks_the_whole_group_played() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let mgr = FerrofinUserDataManager::new(db, config());
        resume_at(&mgr, primary, user, 1000).await;

        assert!(
            mgr.update_play_state_with_reason(
                user,
                alt1,
                None,
                UserDataSaveReason::PlaybackFinished
            )
            .await
            .unwrap()
        );

        for version in [primary, alt2] {
            let dto = data(&mgr, version, user).await;
            assert!(dto.played, "{version}");
            assert_eq!(dto.playback_position_ticks, 0);
        }
    }

    /// Across a merged (`Grouping`) group: marking the merged version played
    /// reaches its primary, the primary's local versions and its own local
    /// version (`GetAllItemsForMediaSources` spans them all); marking the
    /// primary unplayed saves and announces every other version, a version
    /// with no data of its own included.
    #[tokio::test]
    async fn propagation_spans_a_merged_group_and_its_local_versions() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (primary, alt1, alt2) = version_group(&db).await;
        let (merged, merged_local) = (Uuid::from_u128(13), Uuid::from_u128(14));
        for (id, path) in [
            (merged, "/Movies/Cut/Cut.mkv"),
            (merged_local, "/Movies/Cut/Cut - 720p.mkv"),
        ] {
            seed_item(&db, id, BaseItemKind::Movie).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(path)
                .execute(db.pool())
                .await
                .unwrap();
        }
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2,
               "Data" = '{"LocalAlternateVersions":["/Movies/Cut/Cut - 720p.mkv"]}' WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(merged))
        .bind(guid_to_db(primary))
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(merged_local))
        .bind(guid_to_db(merged))
        .execute(db.pool())
        .await
        .unwrap();
        let mgr = FerrofinUserDataManager::new(db, config());
        let sink = Arc::new(RecordingSink::default());
        mgr.set_user_data_changed_sink(sink.clone());

        mgr.mark_played(user, merged, None).await.unwrap();
        for version in [primary, alt1, alt2, merged_local] {
            assert!(data(&mgr, version, user).await.played, "{version}");
        }

        // A fresh user: no version has data, and unplaying still saves them.
        let other = Uuid::from_u128(2);
        crate::test_support::seed_named_user(&mgr.db, other, "other").await;
        mgr.mark_unplayed(other, primary).await.unwrap();
        let rows: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(DISTINCT "ItemId") FROM "UserData" WHERE "UserId" = ?1"#,
        )
        .bind(guid_to_db(other))
        .fetch_one(mgr.db.pool())
        .await
        .unwrap();
        assert_eq!(rows, 5, "the primary and its four other versions");
        // Both users' saves may share one window, whose users go out in no
        // set order.
        let sent = sent_after_window(&sink, 2).await;
        let (_, theirs) = sent.iter().find(|(to, _)| *to == other).unwrap();
        assert_eq!(
            theirs["UserDataList"].as_array().map(Vec::len),
            Some(5),
            "the primary and its four other versions"
        );
    }

    /// A user-data manager whose container marks reach their members through
    /// a library manager wired as the composition root wires it (with the
    /// dashboard configuration, so a season lists its episodes through
    /// `Season.GetItemsInternal`), kept alive by the caller.
    fn container_manager(
        db: &Database,
    ) -> (
        FerrofinUserDataManager,
        Arc<dyn ferrofin_traits::library::LibraryManager>,
    ) {
        let counts: Arc<dyn ferrofin_traits::persistence::ItemCountService> =
            Arc::new(crate::FerrofinItemCountService::new(db.clone()));
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> =
            Arc::new(crate::FerrofinItemRepository::new(
                db.clone(),
                Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
            ));
        let visibility = Arc::new(crate::item_visibility::ItemVisibility::new(
            db.clone(),
            Arc::clone(&items),
            Arc::new(crate::LocalizationManager::new("US")),
            "/nonexistent/ferrofin-test-data".to_owned(),
        ));
        let library: Arc<dyn ferrofin_traits::library::LibraryManager> = Arc::new(
            crate::FerrofinLibraryManager::new(
                items,
                Arc::clone(&counts),
                Arc::new(crate::FerrofinItemPersistenceService::new(db.clone())),
                Arc::new(crate::FerrofinPeopleRepository::new(db.clone())),
            )
            .with_dashboard_configuration(config())
            .with_visibility(visibility),
        );
        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.set_container_members(Arc::downgrade(&library), counts);
        (mgr, library)
    }

    /// A show as a scan stores it: a series, its season 1 and the episodes
    /// `(id, is_virtual, premiere)` under it, keyed and parented the way
    /// `Season.GetItemsInternal` and the series-key query read them.
    async fn seed_show(
        db: &Database,
        series: Uuid,
        season: Uuid,
        episodes: &[(Uuid, bool, Option<chrono::DateTime<chrono::Utc>>)],
    ) {
        use ferrofin_traits::persistence::ItemPersistenceService as _;
        let key = format!("show-{series}");
        let mut rows = vec![
            BaseItemEntity {
                id: guid_to_db(series),
                type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
                name: Some("Show".into()),
                is_folder: true,
                presentation_unique_key: Some(key.clone()),
                ..Default::default()
            },
            BaseItemEntity {
                id: guid_to_db(season),
                type_: "MediaBrowser.Controller.Entities.TV.Season".into(),
                name: Some("Season 1".into()),
                index_number: Some(1),
                parent_id: Some(guid_to_db(series)),
                series_id: Some(guid_to_db(series)),
                series_presentation_unique_key: Some(key.clone()),
                presentation_unique_key: Some(format!("{key}-season-1")),
                is_folder: true,
                ..Default::default()
            },
        ];
        for (number, &(id, is_virtual, premiere)) in (1..).zip(episodes) {
            rows.push(BaseItemEntity {
                id: guid_to_db(id),
                type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
                name: Some(format!("Episode {number}")),
                parent_id: Some(guid_to_db(season)),
                season_id: Some(guid_to_db(season)),
                series_id: Some(guid_to_db(series)),
                series_presentation_unique_key: Some(key.clone()),
                parent_index_number: Some(1),
                index_number: Some(number),
                media_type: Some("Video".into()),
                is_virtual_item: is_virtual,
                path: (!is_virtual).then(|| format!("/media/show/s01e{number:02}.mkv")),
                premiere_date: premiere,
                ..Default::default()
            });
        }
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        persistence.save_items(&rows).await.unwrap();
        persistence.set_ancestors(season, &[series]).await.unwrap();
        for &(id, _, _) in episodes {
            persistence
                .set_ancestors(id, &[season, series])
                .await
                .unwrap();
        }
    }

    /// `Folder.MarkPlayed`/`MarkUnplayed` (v12.2 Folder.cs:1956-2012) through
    /// `Series.GetItemsInternal`: marking a series marks its episodes —
    /// never a virtual one — and writes nothing on the series itself. The
    /// answer is `GetUserDataDto(series)`, whose played state comes from the
    /// episodes (`Folder.FillUserDataDtoValues`).
    #[tokio::test]
    async fn marking_a_series_marks_its_episodes_not_the_series() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (series, season) = (Uuid::from_u128(0x5E0), Uuid::from_u128(0x5E1));
        let (ep1, ep2, missing) = (
            Uuid::from_u128(0xE1),
            Uuid::from_u128(0xE2),
            Uuid::from_u128(0xE3),
        );
        seed_show(
            &db,
            series,
            season,
            &[
                (ep1, false, None),
                (ep2, false, None),
                (missing, true, None),
            ],
        )
        .await;
        seed_provider_id(&db, series, "Tvdb", "73739").await;
        let (mgr, _library) = container_manager(&db);
        let sink = Arc::new(RecordingSink::default());
        mgr.set_user_data_changed_sink(sink.clone());

        let dto = mgr.mark_played(user, series, None).await.unwrap();
        assert!(data(&mgr, ep1, user).await.played);
        assert!(data(&mgr, ep2, user).await.played);
        assert!(
            !data(&mgr, missing, user).await.played,
            "never a virtual episode"
        );
        assert!(
            mgr.read_row(series, user).await.unwrap().is_none(),
            "the series' own data is never written"
        );
        assert!(dto.played, "both episodes are played");
        assert_eq!(dto.unplayed_item_count, Some(0));
        assert_eq!(dto.played_percentage, Some(100.0));
        assert_eq!(
            dto.key, "73739",
            "the series' first user-data key, as its item DTO derives it, not its guid"
        );
        let sent = sent_after_window(&sink, 1).await;
        assert_eq!(sent.len(), 1, "one message for the episodes");
        assert_eq!(
            listed(&sent[0].1),
            [ep1, season, ep2].map(|id| id.simple().to_string()),
            "the episodes and, one level up, their season"
        );

        let dto = mgr.mark_unplayed(user, series).await.unwrap();
        assert!(!data(&mgr, ep1, user).await.played);
        assert!(!data(&mgr, ep2, user).await.played);
        assert!(mgr.read_row(series, user).await.unwrap().is_none());
        assert!(!dto.played);
        assert_eq!(dto.unplayed_item_count, Some(2));
    }

    /// `GetUserDataDto(item, user)` (v12.2 UserDataManager.cs:411-427) runs
    /// `Folder.FillUserDataDtoValues` on every read: a series' or season's
    /// user data counts its episodes, an episode's is its own row, and a
    /// favourite on the series keeps the roll-up.
    #[tokio::test]
    async fn folder_user_data_counts_its_leaves() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (series, season) = (Uuid::from_u128(0x5E0), Uuid::from_u128(0x5E1));
        let (ep1, ep2) = (Uuid::from_u128(0xE1), Uuid::from_u128(0xE2));
        seed_show(
            &db,
            series,
            season,
            &[(ep1, false, None), (ep2, false, None)],
        )
        .await;
        let (mgr, _library) = container_manager(&db);
        mgr.mark_played(user, ep1, None).await.unwrap();
        mgr.save_user_data(
            user,
            series,
            &UpdateUserItemDataDto {
                is_favorite: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        for folder in [series, season] {
            let dto = data(&mgr, folder, user).await;
            assert!(!dto.played, "{folder}: one episode of two is played");
            assert_eq!(dto.unplayed_item_count, Some(1), "{folder}");
            assert_eq!(dto.played_percentage, Some(50.0), "{folder}");
        }
        assert!(data(&mgr, series, user).await.is_favorite);

        let episode = data(&mgr, ep1, user).await;
        assert!(episode.played);
        assert_eq!(episode.unplayed_item_count, None, "a leaf has no count");

        // A leaf's resume position over its runtime, and none without one.
        resume_at(&mgr, ep2, user, 250).await;
        assert_eq!(data(&mgr, ep2, user).await.played_percentage, None);
        sqlx::query(r#"UPDATE "BaseItems" SET "RunTimeTicks" = 1000 WHERE "Id" = ?1"#)
            .bind(guid_to_db(ep2))
            .execute(db.pool())
            .await
            .unwrap();
        resume_at(&mgr, ep2, user, 250).await;
        assert_eq!(data(&mgr, ep2, user).await.played_percentage, Some(25.0));
    }

    /// `Folder.MarkPlayed` on a season, through `Season.GetItemsInternal`:
    /// a virtual episode is marked only when the user displays missing
    /// episodes, an unaired one never (`BaseItem.IsUnaired`), and
    /// `Folder.MarkUnplayed` resets every episode, virtual ones included. A
    /// series always leaves virtual episodes out (`Series.GetItemsInternal`
    /// sets `IsVirtualItem = false`), whatever the user displays.
    #[tokio::test]
    async fn marking_a_season_follows_display_missing_episodes() {
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (series, season) = (Uuid::from_u128(0x5E0), Uuid::from_u128(0x5E1));
        let (aired, missing, unaired) = (
            Uuid::from_u128(0xE1),
            Uuid::from_u128(0xE2),
            Uuid::from_u128(0xE3),
        );
        let now = chrono::Utc::now();
        seed_show(
            &db,
            series,
            season,
            &[
                (aired, false, Some(now - chrono::Duration::days(30))),
                (missing, true, Some(now - chrono::Duration::days(30))),
                (unaired, true, Some(now + chrono::Duration::days(30))),
            ],
        )
        .await;
        let (mgr, _library) = container_manager(&db);
        let played = |id| {
            let mgr = &mgr;
            async move { data(mgr, id, user).await.played }
        };

        mgr.mark_played(user, season, None).await.unwrap();
        assert!(played(aired).await);
        assert!(!played(missing).await, "missing episodes are not displayed");
        assert!(!played(unaired).await);

        mgr.mark_played(user, missing, None).await.unwrap();
        mgr.mark_unplayed(user, season).await.unwrap();
        assert!(!played(aired).await);
        assert!(!played(missing).await, "unplayed resets virtual ones too");

        sqlx::query(r#"UPDATE "Users" SET "DisplayMissingEpisodes" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(user))
            .execute(db.writer())
            .await
            .unwrap();
        mgr.mark_played(user, season, None).await.unwrap();
        assert!(played(aired).await);
        assert!(
            played(missing).await,
            "displayed missing episodes are marked"
        );
        assert!(!played(unaired).await, "never an unaired one");

        mgr.mark_unplayed(user, season).await.unwrap();
        mgr.mark_played(user, series, None).await.unwrap();
        assert!(played(aired).await);
        assert!(!played(missing).await, "a series never marks a virtual one");
        assert!(!played(unaired).await, "nor an unaired one");
    }

    /// `BoxSet.MarkPlayed` (v12.2 BoxSet.cs:205-232, jellyfin#16062): a box
    /// set marks each of its linked children — a movie directly, a linked
    /// series through its own `Folder.MarkPlayed`, a box set inside it once
    /// even when it links back — never itself. A playlist reaches its
    /// members through `Folder.MarkPlayed`'s recursive query.
    #[tokio::test]
    async fn marking_a_box_set_or_playlist_marks_its_linked_children() {
        use crate::test_support::seed_folder_item;
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let (boxset, inner, movie, series, season, episode) = (
            Uuid::from_u128(0xB0),
            Uuid::from_u128(0xB4),
            Uuid::from_u128(0xB1),
            Uuid::from_u128(0xB2),
            Uuid::from_u128(0xB5),
            Uuid::from_u128(0xB3),
        );
        let (playlist, track) = (Uuid::from_u128(0xC0), Uuid::from_u128(0xC1));
        seed_folder_item(&db, boxset, BaseItemKind::BoxSet, "Set", None).await;
        seed_folder_item(&db, inner, BaseItemKind::BoxSet, "Inner", None).await;
        seed_folder_item(&db, playlist, BaseItemKind::Playlist, "Mix", None).await;
        seed_item(&db, movie, BaseItemKind::Movie).await;
        seed_item(&db, track, BaseItemKind::Audio).await;
        seed_show(&db, series, season, &[(episode, false, None)]).await;
        // Linked members are scoped to the user's libraries.
        crate::test_support::seed_library_over(&db, &[movie, episode, track]).await;
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        // 0 = `LinkedChildType.Manual`.
        for (parent, child) in [
            (boxset, movie),
            (boxset, series),
            (boxset, inner),
            (inner, boxset),
        ] {
            links.upsert_linked_child(parent, child, 0).await.unwrap();
        }
        let (mgr, _library) = container_manager(&db);

        let dto = mgr.mark_played(user, boxset, None).await.unwrap();
        assert!(data(&mgr, movie, user).await.played);
        assert!(data(&mgr, episode, user).await.played);
        assert_eq!(data(&mgr, movie, user).await.play_count, 1, "marked once");
        for container in [boxset, inner, series] {
            assert!(mgr.read_row(container, user).await.unwrap().is_none());
        }
        // `GetPlayedAndTotalCountFromLinkedChildren`: the leaves below the
        // links, the linked series' episode included.
        assert!(dto.played);
        assert_eq!(dto.unplayed_item_count, Some(0));
        assert_eq!(dto.played_percentage, Some(100.0));
        let dto = mgr.mark_unplayed(user, boxset).await.unwrap();
        assert!(!data(&mgr, movie, user).await.played);
        assert!(!dto.played, "the answer follows the members it just reset");
        assert_eq!(dto.unplayed_item_count, Some(2));

        links.upsert_linked_child(playlist, track, 0).await.unwrap();
        mgr.mark_played(user, playlist, None).await.unwrap();
        assert!(data(&mgr, track, user).await.played);
        assert!(mgr.read_row(playlist, user).await.unwrap().is_none());
    }

    /// gl-571: a box set marks only the members `GetLinkedChildren(user)`
    /// keeps (v12.2 BoxSet.cs:205-232): never one from a library the user
    /// cannot open.
    #[tokio::test]
    async fn marking_a_box_set_skips_members_from_libraries_the_user_cannot_open() {
        let db = test_db().await;
        let fixture = crate::test_support::seed_box_set_across_libraries(&db).await;
        let library: Arc<dyn ferrofin_traits::library::LibraryManager> = Arc::new(
            crate::test_support::rooted_library_manager(&db, fixture.roots, config()),
        );
        let mgr = FerrofinUserDataManager::new(db.clone(), config());
        mgr.set_container_members(
            Arc::downgrade(&library),
            Arc::new(crate::FerrofinItemCountService::new(db.clone())),
        );
        let user = Uuid::parse_str(&fixture.user.id).unwrap();

        mgr.mark_played(user, fixture.set, None).await.unwrap();
        assert!(data(&mgr, fixture.shown, user).await.played);
        assert!(!data(&mgr, fixture.blocked, user).await.played);
    }

    /// Earlier builds wrote a container's own row when it was marked.
    /// Marking that container unplayed now clears the row's play state under
    /// every key (keeping the favourite), so a kind whose played state is its
    /// own row — here a music album — stops answering `Played` for good.
    #[tokio::test]
    async fn marking_a_container_clears_its_own_stale_play_state() {
        use crate::test_support::seed_folder_item;
        use ferrofin_traits::persistence::ItemPersistenceService as _;
        let db = test_db().await;
        let user = Uuid::from_u128(1);
        seed_user(&db, user).await;
        let (album, track) = (Uuid::from_u128(0xA0), Uuid::from_u128(0xA1));
        seed_folder_item(&db, album, BaseItemKind::MusicAlbum, "Album", None).await;
        seed_item(&db, track, BaseItemKind::Audio).await;
        crate::FerrofinItemPersistenceService::new(db.clone())
            .set_ancestors(track, &[album])
            .await
            .unwrap();
        // The rows an earlier build left: played and a favourite, plus a
        // played row under a key the album no longer derives.
        let leaf_only = FerrofinUserDataManager::new(db.clone(), config());
        leaf_only.mark_played(user, album, None).await.unwrap();
        leaf_only
            .save_user_data(user, album, &favorite_dto())
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
                   "PlayCount", "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, 'stale-key', 0, 1, 0, 1)"#,
        )
        .bind(guid_to_db(album))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .unwrap();
        leaf_only.mark_played(user, track, None).await.unwrap();
        let (mgr, _library) = container_manager(&db);

        let dto = mgr.mark_unplayed(user, album).await.unwrap();
        assert!(!dto.played, "the album's own row no longer says played");
        assert!(dto.is_favorite, "the favourite is kept");
        assert!(!data(&mgr, track, user).await.played, "the walk reset it");
        let row = mgr.read_row(album, user).await.unwrap().unwrap();
        assert_eq!((row.played, row.play_count), (false, 0));
        assert!(row.last_played_date.is_none());
        let played_rows: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1 AND "Played" = 1"#,
        )
        .bind(guid_to_db(album))
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(played_rows, 0, "no key keeps the album played");
    }
}
