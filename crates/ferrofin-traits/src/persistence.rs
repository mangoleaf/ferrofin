//! Persistence-layer **repository** traits — the storage seam.
//!
//! Port of the `IItemRepository` / `I*Repository` / `I*Service` interfaces in
//! `MediaBrowser.Controller.Persistence`. These sit *below* the manager traits
//! in [`crate::library`]: managers orchestrate business logic and delegate raw
//! row access to these repositories, whose implementations (Wave 6,
//! `ferrofin-core`) talk to `ferrofin-db`.
//!
//! Port rules applied throughout:
//! - Item **identity** arguments become [`uuid::Uuid`].
//! - Items **out of a repository** are `ferrofin-db` entities (the persisted row
//!   structs), never the un-ported C# `BaseItem` domain object. So
//!   `RetrieveItem(Guid) -> BaseItem` becomes
//!   `retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, _>`.
//! - `QueryResult<T>` is reused from [`ferrofin_model::querying::QueryResult`].
//! - `Task<T>` methods become `async fn -> Result<T, ServiceError>`; the
//!   synchronous C# methods stay `async` here too, since a real repository does
//!   I/O regardless of the C# signature's blocking shape.
//! - `IProgress`/`CancellationToken` parameters are dropped for v1.
//!
//! Every trait is object-safe (no generic methods, no `impl Trait` returns, no
//! `Self`-by-value) because `AppState` stores them behind `Arc<dyn _>`; each
//! carries a `_assert_object_safe_*` compile-time assertion.

use std::collections::HashMap;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::{
    AttachmentStreamInfoEntity, BaseItemEntity, ChapterEntity, ItemTextRow, KeyframeDataEntity,
    MediaStreamInfoEntity, PeopleEntity,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::{ImageType, MediaStreamType};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::{InternalItemsQuery, InternalPeopleQuery, ItemImageInfo};

/// A genre/studio/artist row paired with its aggregated item counts.
///
/// Port of the C# `(BaseItem Item, ItemCounts ItemCounts)` value tuple returned
/// by `IItemRepository.GetGenres`/`GetStudios`/`GetArtists`/…; a named struct
/// reads better across a trait boundary than a bare tuple and can grow fields.
#[derive(Debug, Clone)]
pub struct ItemWithCounts {
    /// The by-name item row (a genre, studio, artist, …).
    pub item: BaseItemEntity,
    /// Its aggregated counts by child item kind.
    pub counts: ItemCounts,
}

/// Played/total pair returned by the descendant-count queries.
///
/// Port of the C# `(int Played, int Total)` value tuple used across
/// [`ItemCountService`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayedAndTotal {
    /// Number of played descendant items.
    pub played: i32,
    /// Total number of descendant items.
    pub total: i32,
}

/// Filter selecting media streams to fetch.
///
/// Port of `MediaBrowser.Controller.Persistence.MediaStreamQuery`.
#[derive(Debug, Clone, Default)]
pub struct MediaStreamQuery {
    /// The owning item's identifier.
    pub item_id: Uuid,
    /// Restrict to a single stream type, when set.
    pub stream_type: Option<MediaStreamType>,
    /// Restrict to a single stream index, when set.
    pub index: Option<i32>,
}

/// Filter selecting media attachments to fetch.
///
/// Port of `MediaBrowser.Controller.Persistence.MediaAttachmentQuery`.
#[derive(Debug, Clone, Default)]
pub struct MediaAttachmentQuery {
    /// The owning item's identifier.
    pub item_id: Uuid,
    /// Restrict to a single attachment index, when set.
    pub index: Option<i32>,
}

/// Result of a batched next-up query for a single series.
///
/// Port of `MediaBrowser.Controller.Persistence.NextUpEpisodeBatchResult`. Each
/// `BaseItem?` field becomes an `Option<BaseItemEntity>` (repository rows), and
/// the specials list becomes `Vec<BaseItemEntity>`.
#[derive(Debug, Clone, Default)]
pub struct NextUpEpisodeBatchResult {
    /// The highest played season/episode.
    pub last_watched: Option<BaseItemEntity>,
    /// The next unwatched episode after the last watched position.
    pub next_up: Option<BaseItemEntity>,
    /// Specials that may air between episodes (only when specials were requested).
    pub specials: Vec<BaseItemEntity>,
    /// The most recently played episode, for rewatching mode.
    pub last_watched_for_rewatching: Option<BaseItemEntity>,
    /// The next played episode, for rewatching mode.
    pub next_played_for_rewatching: Option<BaseItemEntity>,
}

/// The playlist-access columns a playlist read carries alongside its member
/// rows: the `FerrofinPlaylists` meta row (owner + open-access) and the caller's
/// own `FerrofinPlaylistShares` row, exactly as stored.
///
/// Left as raw storage values because the visibility rules (C#
/// `Playlist.IsVisible` plus the owner/`CanEdit` split) are the playlist
/// manager's to apply — the repository only reads.
#[derive(Debug, Clone)]
pub struct PlaylistAccessColumns {
    /// `FerrofinPlaylists.OwnerUserId` (`None` for a legacy or API-key playlist).
    pub owner_user_id: Option<String>,
    /// `FerrofinPlaylists.OpenAccess` (`None` only when the meta row is absent).
    pub open_access: Option<i64>,
    /// The caller's `FerrofinPlaylistShares.CanEdit` (`None` when not shared).
    pub share_can_edit: Option<i64>,
}

/// A playlist's member rows in link order, plus the caller's access columns.
///
/// `access` is `None` exactly when the playlist has no member rows — the join
/// then cannot tell "empty playlist" from "missing/invisible playlist", so the
/// caller resolves access separately.
#[derive(Debug, Clone)]
pub struct PlaylistItemsWithAccess {
    /// The member item rows, in playlist (link) order.
    pub items: Vec<BaseItemEntity>,
    /// The access columns repeated on every member row (`None` if there are none).
    pub access: Option<PlaylistAccessColumns>,
}

/// Reads and queries persisted [`BaseItemEntity`] rows.
///
/// Port of `IItemRepository`. Generic C# helpers (`GetGenres`/`GetStudios`/…)
/// collapse to concrete methods returning [`ItemWithCounts`]; the domain
/// `BaseItem` return type becomes [`BaseItemEntity`].
#[async_trait]
pub trait ItemRepository: Send + Sync {
    /// Retrieves a single item row by id, or `None` if it does not exist.
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError>;

    /// Returns the ids of every item whose metadata the user has **locked**
    /// (`IsLocked = 1`).
    ///
    /// The library scan needs each item's lock state, and asking per item cost
    /// it a full `SELECT *` row hydration for one boolean. Locked items are
    /// rare — usually none — so the whole answer is one small query, and the
    /// scan reads it from the returned set instead.
    async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError>;

    /// The stored `Name`/`SortName`/`Overview`/`Path` of the given items of
    /// `kind`, chunked into as few queries as the host-variable limit allows.
    ///
    /// The episode metadata providers gate a re-fetch on what a previous scan
    /// already achieved, which only the stored row knows — `Planned.entity` is
    /// rebuilt from the filesystem every scan, so its name is always the file
    /// stem and its overview always `None`. Asking per item would reinstate the
    /// `SELECT *`-per-item cost that
    /// [`locked_item_ids`](Self::locked_item_ids) removed, so the scan asks
    /// once for the set it planned.
    ///
    /// **Scoped by `ids` on purpose.** Reading every row of `kind` instead
    /// looks similar but is not: `locked_item_ids` rides a partial index and
    /// touches no rows in the normal case, whereas a whole-kind read of these
    /// four text columns is a non-covering index scan — ~113 ms and ~30 MB for
    /// 60k episodes, paid even by `scan_paths`, the library-monitor path that
    /// runs for a single changed file. Keep this O(planned).
    ///
    /// Returns rows only for ids that exist; callers must not assume a row per
    /// id.
    ///
    /// # Errors
    /// Propagates the repository failure.
    async fn item_text_rows(
        &self,
        kind: BaseItemKind,
        ids: &[Uuid],
    ) -> Result<Vec<ItemTextRow>, ServiceError>;

    /// Walks the `ParentId` chain from `item_id` upward in a single query
    /// (recursive CTE), returning ancestors nearest-first. Returns `None` if
    /// the starting item does not exist.
    async fn get_ancestor_chain(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError>;

    /// Runs a query and returns a page of item rows plus the total count.
    async fn get_items(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError>;

    /// Returns just the ids of the items matching the query.
    async fn get_item_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError>;

    /// Returns the full (unpaginated) list of item rows matching the query.
    async fn get_item_list(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Returns `(Id, CleanName)` for every row matching the query, in the same
    /// order — and under the same predicates, ordering and paging — as
    /// [`Self::get_item_list`].
    ///
    /// The by-name resolvers ([`crate::library::LibraryManager::get_named_item_ids`])
    /// join a page's names against `CleanName` and then read nothing but the id,
    /// so materializing a full item row per name is pure waste: on a cast-heavy
    /// page that is hundreds of 72-column rows decoded and dropped. The default
    /// delegates to [`Self::get_item_list`], so every implementation keeps
    /// working; the concrete repository overrides it with a two-column
    /// projection over the identical query.
    async fn get_item_id_clean_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
        Ok(self
            .get_item_list(filter)
            .await?
            .into_iter()
            .map(|row| (row.id, row.clean_name))
            .collect())
    }

    /// The "latest media" rows of a **tvshows or music** library — port of C#
    /// `BaseItemRepository.GetLatestItemList`.
    ///
    /// One grouped-threshold statement: the filter's predicates are grouped by
    /// `SeriesName` (tvshows) or `Album` (music), the newest `filter.limit`
    /// groups' `MAX(DateCreated)` are taken, and every row at or above the
    /// *smallest* of those maxima is returned in the filter's `order_by`,
    /// unpaged — so `limit` caps groups, not rows, and the caller buckets the
    /// rows by container afterwards. Any other collection type returns an
    /// empty list (the C# early exit); the caller uses
    /// [`get_item_list`](Self::get_item_list) for those.
    async fn get_latest_item_list(
        &self,
        filter: &InternalItemsQuery,
        collection_type: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Reports whether an item with the given id has been persisted.
    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError>;

    /// Returns the item rows whose `PrimaryVersionId` equals `primary_id`.
    ///
    /// The alternate versions of a version group point at the group's primary via
    /// this column; used to enumerate a group (e.g. to clear its links). The
    /// primary row itself is *not* included (its own `PrimaryVersionId` is null).
    async fn get_items_by_primary_version(
        &self,
        primary_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Batch form of [`Self::get_items_by_primary_version`] for a page of
    /// primary ids, keyed by primary id; primaries with no alternates are
    /// absent. The default loops the single-item form; the concrete repository
    /// overrides it with one query.
    async fn get_items_by_primary_version_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        let mut map = HashMap::new();
        for &id in primary_ids {
            let alternates = self.get_items_by_primary_version(id).await?;
            if !alternates.is_empty() {
                map.insert(id, alternates);
            }
        }
        Ok(map)
    }

    /// Returns every `(item_id, value)` pair stored under `provider_key` in
    /// `BaseItemProviders`.
    ///
    /// Used to group items by an external id (e.g. `Tmdb`) when merging duplicate
    /// versions. The provider key is matched case-insensitively (Jellyfin stores a
    /// canonical casing like `Tmdb`, but callers pass it as a literal).
    async fn get_items_with_provider_id(
        &self,
        provider_key: &str,
    ) -> Result<Vec<(Uuid, String)>, ServiceError>;

    /// Gets the image rows attached to an item, ordered by image type then by
    /// their stored order within a type.
    ///
    /// Port of the read side of `BaseItemRepository` image persistence
    /// (`BaseItemImageInfos`): each row becomes an [`ItemImageInfo`] the API
    /// layer serves or projects into an `ImageInfo` DTO. An item with no images
    /// yields an empty vector.
    async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError>;

    /// Swaps the two `image_type` images at `index1` and `index2` for an item,
    /// reordering them.
    ///
    /// Port of the persistence side of `BaseItem.SwapImagesAsync`: C# swaps the
    /// on-disk files backing the two rows and clears their cached dimensions;
    /// against the stored image rows the equivalent is to exchange the two rows'
    /// `Path` (and reset `Width`/`Height` to `0` and stamp `DateModified`), so the
    /// image that was at `index1` now resolves at `index2` and vice versa.
    ///
    /// Indices are `0`-based positions **within** the `image_type` group, in the
    /// same order [`get_image_infos`](Self::get_image_infos) returns. When either
    /// index is out of range the swap is a no-op (mirroring C#'s "nothing to do"
    /// when `GetImageInfo` returns `null`).
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn swap_item_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError>;

    /// Gets genres with their item counts.
    async fn get_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets music genres with their item counts.
    async fn get_music_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets studios with their item counts.
    async fn get_studios(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets artists with their item counts.
    async fn get_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets album artists with their item counts.
    async fn get_album_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets all artists with their item counts.
    async fn get_all_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError>;

    /// Gets all distinct music genre names.
    async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError>;

    /// Gets all distinct studio names.
    async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError>;

    /// Gets all distinct genre names.
    async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError>;

    /// Gets all distinct artist names.
    async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError>;

    /// Gets the distinct language codes of matching items' streams of a type.
    async fn get_media_stream_languages(
        &self,
        filter: &InternalItemsQuery,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError>;

    /// Gets the distinct language codes for several stream types at once, keyed
    /// by type. The default loops [`Self::get_media_stream_languages`]; the
    /// concrete repository overrides it to resolve the item set once and read
    /// every requested type in a single query (`Items/Filters2` asks for audio
    /// and subtitle together, which otherwise doubles the id-materialization).
    async fn get_media_stream_languages_by_type(
        &self,
        filter: &InternalItemsQuery,
        stream_types: &[MediaStreamType],
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(stream_types.len());
        for &t in stream_types {
            map.insert(t, self.get_media_stream_languages(filter, t).await?);
        }
        Ok(map)
    }

    /// Gets aggregated legacy query-filter values for the matching items.
    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError>;

    /// Gets just the distinct production years of the matching items,
    /// ascending — the one facet `/Years` needs.
    ///
    /// The default reads the whole legacy filter aggregate and throws three
    /// quarters of it away; an implementation that can answer the years alone
    /// should override this. See [`LibraryManager::get_distinct_years`].
    ///
    /// [`LibraryManager::get_distinct_years`]: crate::library::LibraryManager::get_distinct_years
    async fn get_distinct_years(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<i32>, ServiceError> {
        Ok(self.get_query_filters_legacy(filter).await?.years)
    }

    /// Reports whether all children of `id` are played for the given user.
    async fn get_is_played(
        &self,
        user: &UserEntity,
        id: Uuid,
        recursive: bool,
    ) -> Result<bool, ServiceError>;

    /// Reads a playlist's linked members of `child_type` in link order, each
    /// joined with the caller's playlist-access columns, in a **single**
    /// statement (`GET /Playlists/{id}/Items`).
    ///
    /// One round-trip is load-bearing: the previous shape took one reader-pool
    /// connection for the access check, another for the child-id list, and
    /// another for the detail fetch, so one request queued three times on a
    /// pool sized to the core count.
    async fn get_playlist_items_with_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        child_type: i32,
    ) -> Result<PlaylistItemsWithAccess, ServiceError>;
}

fn _assert_object_safe_item_repository(_: &dyn ItemRepository) {}

/// Writes item rows and their derived data.
///
/// Port of `IItemPersistenceService`. The C# `BaseItem` arguments become
/// [`BaseItemEntity`] rows; every method is `async` since it touches storage.
#[async_trait]
pub trait ItemPersistenceService: Send + Sync {
    /// Deletes the items with the given ids.
    async fn delete_items(&self, ids: &[Uuid]) -> Result<(), ServiceError>;

    /// Persists (inserts or updates) the given item rows.
    async fn save_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError>;

    /// Persists item rows rebuilt from disk by the library scan, preserving the
    /// columns the scanner does not own on already-stored rows:
    ///
    /// - `PrimaryVersionId` — merge-versions links; a scanned entity always
    ///   carries `None`, and a plain [`save_items`](Self::save_items) upsert
    ///   would erase every merged alternate version on each scan.
    /// - `DateCreated` — the item's first-import timestamp; re-stamping it with
    ///   the scan time breaks "date added" ordering.
    ///
    /// The default delegates to [`save_items`](Self::save_items) (for stub/fake
    /// services); the real service uses a scan-specific upsert.
    async fn save_scanned_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        self.save_items(items).await
    }

    /// Sets (or clears, with `None`) an item's `PrimaryVersionId` merge link
    /// without touching any other column.
    ///
    /// Merge/split write through this instead of a full-row save: they load
    /// rows only to decide the linkage, and saving those loaded copies back
    /// wholesale would revert every other column to its load-time value if a
    /// scan, metadata refresh, or user edit landed in between.
    async fn set_primary_version_id(
        &self,
        item_id: Uuid,
        primary_version_id: Option<Uuid>,
    ) -> Result<(), ServiceError>;

    /// Points an item's `ParentId` at `parent_id` without touching any other
    /// column, and without a write at all when it already does.
    ///
    /// The library tree's self-healing reads use this to re-parent a
    /// `CollectionFolder` row created before the `UserRootFolder` existed: a
    /// full-row save would revert columns a scan or edit changed in between,
    /// and an unconditional `UPDATE` would take the writer lock on every
    /// `GET /Library/VirtualFolders`.
    async fn set_parent_id(&self, item_id: Uuid, parent_id: Uuid) -> Result<(), ServiceError>;

    /// Records a library folder's collection type (`movies`, `tvshows`, …) in
    /// the row's `Data` blob, leaving every other column and every other key in
    /// the blob alone — a no-op when the value is already there.
    ///
    /// `Data.CollectionType` is where Jellyfin keeps a `CollectionFolder`'s
    /// type, and `DtoService` emits it for every `IHasCollectionType` item on
    /// every endpoint (DtoService.cs:1061-1064). It is therefore the row's job
    /// to carry it: reading the on-disk `<type>.collection` marker is not
    /// available to the DTO projection, and a per-handler backfill leaves the
    /// other endpoints (`/Library/MediaFolders`, `/Items`, `/Items/{id}`,
    /// `/Items/{id}/Ancestors`) reporting a null type.
    ///
    /// Column-scoped like [`set_parent_id`](Self::set_parent_id) and for the
    /// same reason: the callers hold no full row, so a whole-row save would
    /// revert whatever a scan or edit changed in between.
    async fn set_collection_type(
        &self,
        item_id: Uuid,
        collection_type: &str,
    ) -> Result<(), ServiceError>;

    /// Replaces an item's `ItemValues` links (genres/studios/tags) with `values`,
    /// each a `(type discriminant, display value)` pair. Get-or-creates the shared
    /// `ItemValues` row per (type, value) and rewrites this item's `ItemValuesMap`
    /// — which is what the genre/studio/tag *filters* (e.g. "More Like This") query.
    async fn save_item_values(
        &self,
        item_id: Uuid,
        values: &[(i32, String)],
    ) -> Result<(), ServiceError>;

    /// Whether an item row with this id exists (used to self-heal a library's
    /// `CollectionFolder` row before parenting scanned children to it).
    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError>;

    /// Replaces an item's ancestor-closure rows (`AncestorIds`) — the recursive
    /// `ParentId` chain up to and including the library's `CollectionFolder`.
    ///
    /// `save_items` writes only the item row (its `ParentId`/`TopParentId`); the
    /// recursive item query (`?ParentId=<library>&Recursive=true`) joins the
    /// `AncestorIds` closure table, so a scanned item appears in the library
    /// listing only once its ancestors are registered here.
    async fn set_ancestors(&self, item_id: Uuid, ancestor_ids: &[Uuid])
    -> Result<(), ServiceError>;

    /// Persists the image info attached to an item.
    async fn save_images(&self, item: &BaseItemEntity) -> Result<(), ServiceError>;

    /// Replaces the item's stored image rows (`BaseItemImageInfos`) with
    /// `images` — the write path the library scan uses to persist discovered
    /// artwork (posters/backdrops/…) so the image routes can serve it.
    ///
    /// The default is a no-op (for stub/fake services); the real service deletes
    /// the item's existing rows and inserts the given set.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn save_item_images(
        &self,
        item_id: Uuid,
        images: &[ItemImageInfo],
    ) -> Result<(), ServiceError> {
        let _ = (item_id, images);
        Ok(())
    }

    /// The stored dimensions/blurhash of every image row attached to
    /// `item_ids`, read in one batch.
    ///
    /// This is the read half of C#'s `LibraryManager.ImageNeedsRefresh`: an
    /// image whose stored `Width`/`Height`/`BlurHash` are already filled and
    /// whose stored `DateModified` still matches the file's mtime is *not*
    /// refreshed, so upstream never re-decodes an unchanged poster. Ferrofin
    /// holds its image rows in the database rather than on an in-memory
    /// `BaseItem`, so the scan reads them back — once for the whole run, in the
    /// same shape as the other scan prereads, never once per item.
    ///
    /// The default returns nothing (stub/fake services), which is safe: a
    /// caller that finds no stored metadata simply recomputes.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn image_metadata_for_items(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<StoredImageMetadata>, ServiceError> {
        let _ = item_ids;
        Ok(Vec::new())
    }

    /// Sets a single image (`image`) on an item, replacing any existing rows of
    /// the same [`ImageType`](ferrofin_model::entities::ImageType) — the write path
    /// for an uploaded poster/backdrop/logo (`ImageController.SetItemImage`).
    ///
    /// The default is a no-op; the real service deletes the item's rows of that
    /// type and inserts the given one.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn set_item_image(
        &self,
        item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Result<(), ServiceError> {
        let _ = (item_id, image);
        Ok(())
    }

    /// Deletes an item's image(s) of `image_type`, returning the on-disk paths of
    /// the removed rows so the caller can delete the files
    /// (`ImageController.DeleteItemImage`). `index` is reserved for per-index
    /// deletes; the current store deletes every row of the type.
    ///
    /// The default removes nothing.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn delete_item_image(
        &self,
        item_id: Uuid,
        image_type: ferrofin_model::entities::ImageType,
        index: Option<i32>,
    ) -> Result<Vec<String>, ServiceError> {
        let _ = (item_id, image_type, index);
        Ok(Vec::new())
    }

    /// The external ids already recorded for each of `item_ids`, keyed by item.
    ///
    /// A scanner reads these once up front so a re-scan resolves each item by
    /// the id a previous pass matched — C# `info.GetProviderId` — instead of
    /// searching by title again. The default is an empty map, for stub
    /// services with no store behind them.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn provider_ids_for_items(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<(String, String)>>, ServiceError> {
        let _ = item_ids;
        Ok(std::collections::HashMap::new())
    }

    /// Upserts one `(ProviderId, ProviderValue)` external-id row for an item
    /// (`BaseItemProviders`) — the write path behind provider-id lookups such
    /// as [`ItemRepository::get_items_with_provider_id`].
    ///
    /// The default is a no-op (for stub/fake services); the real service
    /// replaces the item's existing row of the same provider key.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn save_provider_id(
        &self,
        item_id: Uuid,
        provider: &str,
        value: &str,
    ) -> Result<(), ServiceError> {
        let _ = (item_id, provider, value);
        Ok(())
    }

    /// Replaces an item's whole external-id set (`BaseItemProviders`) with
    /// `ids` — the C# `item.ProviderIds = searchResult.ProviderIds` assignment
    /// behind "Identify → Apply": rows the new set lacks are removed, the rest
    /// upserted, atomically.
    ///
    /// The default is a no-op (for stub/fake services).
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn replace_provider_ids(
        &self,
        item_id: Uuid,
        ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        let _ = (item_id, ids);
        Ok(())
    }

    /// Reattaches user-data rows to the correct item after an id change.
    async fn reattach_user_data(&self, item: &BaseItemEntity) -> Result<(), ServiceError>;

    /// Recomputes and persists inherited values across the item tree.
    async fn update_inherited_values(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_item_persistence_service(_: &dyn ItemPersistenceService) {}

/// One by-name row for [`ItemCountService::get_item_counts_for_name_items`],
/// carrying the name columns the count queries key on.
///
/// Both names are `Option` so a `NULL` column stays distinguishable from an
/// empty string, exactly as the row reads out of `BaseItems`.
#[derive(Debug, Clone, Copy)]
pub struct NameItemRow<'a> {
    /// The by-name item's id.
    pub id: Uuid,
    /// The row's `Name` — the key a `Person`'s credits are counted by
    /// (upstream matches `m.People.Name == item.Name`).
    pub name: Option<&'a str>,
    /// The row's `CleanName` — the key every other by-name kind counts by,
    /// through `ItemValues`.
    pub clean_name: Option<&'a str>,
}

/// Counts items and played descendants.
///
/// Port of `IItemCountService`. The C# value tuples become [`PlayedAndTotal`];
/// the `User` argument becomes a [`UserEntity`] reference.
#[async_trait]
pub trait ItemCountService: Send + Sync {
    /// Counts the items matching the filter.
    async fn get_count(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError>;

    /// Gets item counts grouped by kind for the filter.
    async fn get_item_counts(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError>;

    /// Gets item counts for a by-name item via the optimized path.
    async fn get_item_counts_for_name_item(
        &self,
        kind: BaseItemKind,
        id: Uuid,
        related_item_kinds: &[BaseItemKind],
        access_filter: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError>;

    /// Batch form of [`Self::get_item_counts_for_name_item`] for a page of
    /// by-name items of the same `kind`: one entry per row (a row without the
    /// name column its kind counts by reports zeros). The default loops the
    /// per-item form; the concrete service overrides it with a grouped query.
    ///
    /// Takes whole [`NameItemRow`]s rather than ids because the caller is
    /// projecting those very rows: passing the name columns along saves the
    /// service a `SELECT "Id","Name"` / `SELECT "Id","CleanName"` round trip
    /// that re-reads what the caller already holds.
    async fn get_item_counts_for_name_items(
        &self,
        kind: BaseItemKind,
        rows: &[NameItemRow<'_>],
        related_item_kinds: &[BaseItemKind],
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        let mut out = HashMap::with_capacity(rows.len());
        for row in rows {
            out.insert(
                row.id,
                self.get_item_counts_for_name_item(kind, row.id, related_item_kinds, access_filter)
                    .await?,
            );
        }
        Ok(out)
    }

    /// Counts played descendants of `ancestor_id`.
    async fn get_played_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError>;

    /// Counts all descendants of `ancestor_id`.
    async fn get_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError>;

    /// Gets both played and total descendant counts of `ancestor_id`.
    async fn get_played_and_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError>;

    /// Gets played/total counts from the linked-children of `parent_id`.
    async fn get_played_and_total_count_from_linked_children(
        &self,
        filter: &InternalItemsQuery,
        parent_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError>;

    /// Batch-fetches played/total counts for several folders.
    async fn get_played_and_total_count_batch(
        &self,
        folder_ids: &[Uuid],
        user: &UserEntity,
    ) -> Result<HashMap<Uuid, PlayedAndTotal>, ServiceError>;

    /// Batch-fetches child counts for several parent folders.
    async fn get_child_count_batch(
        &self,
        parent_ids: &[Uuid],
        user_id: Option<Uuid>,
    ) -> Result<HashMap<Uuid, i32>, ServiceError>;
}

fn _assert_object_safe_item_count_service(_: &dyn ItemCountService) {}

/// Reads and writes an item's chapter rows.
///
/// Port of `IChapterRepository`. C# `ChapterInfo` becomes [`ChapterEntity`].
#[async_trait]
pub trait ChapterRepository: Send + Sync {
    /// Deletes all chapters of an item.
    async fn delete_chapters(&self, item_id: Uuid) -> Result<(), ServiceError>;

    /// Replaces an item's chapters with the given set.
    async fn save_chapters(
        &self,
        item_id: Uuid,
        chapters: &[ChapterEntity],
    ) -> Result<(), ServiceError>;

    /// Gets all chapters of an item, in order.
    async fn get_chapters(&self, item_id: Uuid) -> Result<Vec<ChapterEntity>, ServiceError>;

    /// Gets every chapter for a set of item ids in one query, keyed by item. The
    /// default loops [`Self::get_chapters`]; the concrete repository overrides it
    /// with a single `ItemId IN (…)` query for list DTO projection.
    async fn get_chapters_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ChapterEntity>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            map.insert(id, self.get_chapters(id).await?);
        }
        Ok(map)
    }

    /// Gets a single chapter of an item by index, or `None`.
    async fn get_chapter(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ChapterEntity>, ServiceError>;
}

fn _assert_object_safe_chapter_repository(_: &dyn ChapterRepository) {}

/// Reads and writes an item's media-stream rows.
///
/// Port of `IMediaStreamRepository`. C# `MediaStream` becomes
/// [`MediaStreamInfoEntity`] at this persistence layer.
#[async_trait]
pub trait MediaStreamRepository: Send + Sync {
    /// Gets the media streams matching a filter.
    async fn get_media_streams(
        &self,
        filter: &MediaStreamQuery,
    ) -> Result<Vec<MediaStreamInfoEntity>, ServiceError>;

    /// Gets every media stream for a set of item ids in one query, keyed by item.
    ///
    /// The batch form used to project a whole page of DTOs without an N+1 — the
    /// 2-connection SQLite pool makes query count the dominant cost. The default
    /// loops [`Self::get_media_streams`] so alternate impls compile unchanged; the
    /// concrete repository overrides it with a single `ItemId IN (…)` query.
    async fn get_media_streams_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<MediaStreamInfoEntity>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            map.insert(
                id,
                self.get_media_streams(&MediaStreamQuery {
                    item_id: id,
                    ..MediaStreamQuery::default()
                })
                .await?,
            );
        }
        Ok(map)
    }

    /// The subset of `item_ids` with at least one subtitle stream row.
    ///
    /// Backs the DTO builder's per-page `HasSubtitles` (C# stores the flag on
    /// the video entity; here it derives from `MediaStreamInfos`, which both
    /// Jellyfin and Ferrofin scans populate). The default filters the full
    /// stream batch; the concrete repository overrides it with an ids-only
    /// query so list pages don't materialize stream rows.
    async fn get_item_ids_with_subtitles(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, ServiceError> {
        let map = self.get_media_streams_batch(item_ids).await?;
        // Stored `StreamType` discriminant 2 = Subtitle.
        Ok(map
            .into_iter()
            .filter(|(_, rows)| rows.iter().any(|r| r.stream_type == 2))
            .map(|(id, _)| id)
            .collect())
    }

    /// Gets the distinct language codes for a stream type across the library.
    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError>;

    /// Replaces an item's media streams with the given set.
    async fn save_media_streams(
        &self,
        item_id: Uuid,
        streams: &[MediaStreamInfoEntity],
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_media_stream_repository(_: &dyn MediaStreamRepository) {}

/// Reads and writes an item's media-attachment rows.
///
/// Port of `IMediaAttachmentRepository`. C# `MediaAttachment` becomes
/// [`AttachmentStreamInfoEntity`].
#[async_trait]
pub trait MediaAttachmentRepository: Send + Sync {
    /// Gets the media attachments matching a filter.
    async fn get_media_attachments(
        &self,
        filter: &MediaAttachmentQuery,
    ) -> Result<Vec<AttachmentStreamInfoEntity>, ServiceError>;

    /// Batch form of [`Self::get_media_attachments`] for a page of items, keyed
    /// by item; items with no attachments are absent. The default loops the
    /// single-item form; the concrete repository runs one `IN (…)` query.
    async fn get_media_attachments_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<AttachmentStreamInfoEntity>>, ServiceError>
    {
        let mut map = std::collections::HashMap::new();
        for &item_id in item_ids {
            let rows = self
                .get_media_attachments(&MediaAttachmentQuery {
                    item_id,
                    index: None,
                })
                .await?;
            if !rows.is_empty() {
                map.insert(item_id, rows);
            }
        }
        Ok(map)
    }

    /// Replaces an item's media attachments with the given set.
    async fn save_media_attachments(
        &self,
        item_id: Uuid,
        attachments: &[AttachmentStreamInfoEntity],
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_media_attachment_repository(_: &dyn MediaAttachmentRepository) {}

/// What a previous scan already recorded about one image file: the probed
/// dimensions, the blurhash, and the file mtime those were computed from.
///
/// Returned by
/// [`ItemPersistenceService::image_metadata_for_items`] and consumed by the
/// scan's port of C#'s `LibraryManager.ImageNeedsRefresh`.
#[derive(Debug, Clone)]
pub struct StoredImageMetadata {
    /// The image file's path — the identity of the artwork on disk.
    pub path: String,
    /// The stored pixel width (`0` when never probed).
    pub width: i32,
    /// The stored pixel height (`0` when never probed).
    pub height: i32,
    /// The stored blurhash, absent when never computed.
    pub blur_hash: Option<String>,
    /// The file mtime the stored values were computed from.
    pub date_modified: chrono::DateTime<chrono::Utc>,
}

/// The outcome of writing one credited person via
/// [`PeopleRepository::update_people`] — enough for the caller to fetch that
/// person's remote artwork and biography.
#[derive(Debug, Clone)]
pub struct WrittenPerson {
    /// The materialized `Person` item id.
    pub id: Uuid,
    /// Whether the person's item still lacks a biography — the caller fetches
    /// details only for these, so a re-scan backfills people scanned before the
    /// biography feature and never re-fetches an already-enriched person.
    pub needs_details: bool,
    /// The remote profile-image URL to download, when present.
    pub image_url: Option<String>,
    /// The remote provider id (TMDB person id) for a biography lookup.
    pub provider_id: Option<i64>,
}

/// A `Person` item's biographical fields.
#[derive(Debug, Clone, Default)]
pub struct PersonMetadata {
    /// The biography text.
    pub overview: Option<String>,
    /// The birthday.
    pub premiere_date: Option<chrono::DateTime<chrono::Utc>>,
    /// The date of death.
    pub end_date: Option<chrono::DateTime<chrono::Utc>>,
    /// The place of birth.
    pub birthplace: Option<String>,
}

/// Reads and writes an item's people rows.
///
/// Port of `IPeopleRepository`. C# `PersonInfo` becomes [`PeopleEntity`].
#[async_trait]
pub trait PeopleRepository: Send + Sync {
    /// Gets the people matching a filter, paginated.
    async fn get_people(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<QueryResult<PeopleEntity>, ServiceError>;

    /// Gets the full credited cast/crew for a set of item ids in one query, keyed
    /// by item and in each item's credit order — the batch form used to project a
    /// page of DTOs without a `get_people` per item. The default loops the
    /// single-item form; the concrete repository overrides it.
    async fn get_people_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<PeopleEntity>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            let people = self
                .get_people(&InternalPeopleQuery {
                    item_id: id,
                    ..InternalPeopleQuery::default()
                })
                .await?
                .items;
            map.insert(id, people);
        }
        Ok(map)
    }

    /// Replaces an item's people with the given set, materializing a browsable
    /// `Person` item per credit.
    ///
    /// Returns a [`WrittenPerson`] per credited person so the caller (which has
    /// the HTTP/filesystem access the repository lacks) can download artwork and
    /// fetch biographies — the `is_new` flag lets it enrich only freshly-created
    /// people, keeping re-scans cheap.
    async fn update_people(
        &self,
        item_id: Uuid,
        people: &[PeopleEntity],
    ) -> Result<Vec<WrittenPerson>, ServiceError>;

    /// Sets a `Person` item's biographical fields (C# person-provider write).
    async fn set_person_metadata(
        &self,
        person_id: Uuid,
        metadata: PersonMetadata,
    ) -> Result<(), ServiceError>;

    /// Gets the distinct people names matching a filter.
    async fn get_people_names(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError>;

    /// Batch-fetches the distinct people names per item, filtered by type.
    async fn get_people_names_by_items(
        &self,
        item_ids: &[Uuid],
        person_types: &[String],
    ) -> Result<HashMap<Uuid, Vec<String>>, ServiceError>;
}

fn _assert_object_safe_people_repository(_: &dyn PeopleRepository) {}

/// Reads and writes an item's keyframe rows.
///
/// Port of `IKeyframeRepository`. C# `KeyframeData` becomes
/// [`KeyframeDataEntity`].
#[async_trait]
pub trait KeyframeRepository: Send + Sync {
    /// Gets the keyframe data rows for an item.
    async fn get_keyframe_data(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<KeyframeDataEntity>, ServiceError>;

    /// Persists keyframe data for an item.
    async fn save_keyframe_data(
        &self,
        item_id: Uuid,
        data: &KeyframeDataEntity,
    ) -> Result<(), ServiceError>;

    /// Deletes an item's keyframe data.
    async fn delete_keyframe_data(&self, item_id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_keyframe_repository(_: &dyn KeyframeRepository) {}

/// Queries and mutates linked-children relationships.
///
/// Port of `ILinkedChildrenService`. The `FindArtists` result's `MusicArtist[]`
/// becomes `Vec<BaseItemEntity>` (artists are item rows), and the
/// `LinkedChildType`/parent-type filters become plain `i32`/[`BaseItemKind`].
#[async_trait]
pub trait LinkedChildrenService: Send + Sync {
    /// Gets the ids of a parent's linked children, optionally filtered by type.
    async fn get_linked_children_ids(
        &self,
        parent_id: Uuid,
        child_type: Option<i32>,
    ) -> Result<Vec<Uuid>, ServiceError>;

    /// Resolves artist names to their candidate matching item rows.
    async fn find_artists(
        &self,
        artist_names: &[String],
    ) -> Result<HashMap<String, Vec<BaseItemEntity>>, ServiceError>;

    /// Gets parents that manually reference `child_id`, optionally by type.
    async fn get_manual_linked_parent_ids(
        &self,
        child_id: Uuid,
        parent_type: Option<BaseItemKind>,
    ) -> Result<Vec<Uuid>, ServiceError>;

    /// Re-routes linked-children references from one child to another, returning
    /// the parent ids that were modified.
    async fn reroute_linked_children(
        &self,
        from_child_id: Uuid,
        to_child_id: Uuid,
    ) -> Result<Vec<Uuid>, ServiceError>;

    /// Creates or updates a single linked-child entry.
    async fn upsert_linked_child(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
        child_type: i32,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_linked_children_service(_: &dyn LinkedChildrenService) {}

/// Computes next-up episodes.
///
/// Port of `INextUpService`. The C# `DateTime dateCutoff` becomes a
/// [`chrono::DateTime<Utc>`](chrono::DateTime).
#[async_trait]
pub trait NextUpService: Send + Sync {
    /// Gets the series presentation keys eligible for next-up.
    async fn get_next_up_series_keys(
        &self,
        filter: &InternalItemsQuery,
        date_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<String>, ServiceError>;

    /// Batch-computes next-up episodes for several series keys.
    async fn get_next_up_episodes_batch(
        &self,
        filter: &InternalItemsQuery,
        series_keys: &[String],
        include_specials: bool,
        include_watched_for_rewatching: bool,
    ) -> Result<HashMap<String, NextUpEpisodeBatchResult>, ServiceError>;
}

fn _assert_object_safe_next_up_service(_: &dyn NextUpService) {}

/// Static lookup tables mapping item kinds to serialization targets.
///
/// Port of `IItemTypeLookup`. The C# read-only properties become getters (a
/// property with no setter is just a `fn(&self) -> _`); this keeps the trait
/// object-safe.
pub trait ItemTypeLookup: Send + Sync {
    /// The serialization target type names for music-related kinds.
    fn music_genre_types(&self) -> Vec<String>;

    /// The mapping of every [`BaseItemKind`] to its serialization target name.
    fn base_item_kind_names(&self) -> HashMap<BaseItemKind, String>;
}

fn _assert_object_safe_item_type_lookup(_: &dyn ItemTypeLookup) {}

#[cfg(test)]
mod tests {
    use super::{MediaAttachmentQuery, MediaStreamQuery, NextUpEpisodeBatchResult, PlayedAndTotal};

    #[test]
    fn played_and_total_defaults_to_zero() {
        let pt = PlayedAndTotal::default();
        assert_eq!(pt.played, 0);
        assert_eq!(pt.total, 0);
    }

    #[test]
    fn stream_and_attachment_queries_default_to_no_filters() {
        let s = MediaStreamQuery::default();
        assert!(s.stream_type.is_none());
        assert!(s.index.is_none());
        let a = MediaAttachmentQuery::default();
        assert!(a.index.is_none());
    }

    #[test]
    fn next_up_batch_result_defaults_are_empty() {
        let r = NextUpEpisodeBatchResult::default();
        assert!(r.last_watched.is_none());
        assert!(r.next_up.is_none());
        assert!(r.specials.is_empty());
    }
}
