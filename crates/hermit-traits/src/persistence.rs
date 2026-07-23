//! Persistence-layer **repository** traits — the storage seam.
//!
//! Port of the `IItemRepository` / `I*Repository` / `I*Service` interfaces in
//! `MediaBrowser.Controller.Persistence`. These sit *below* the manager traits
//! in [`crate::library`]: managers orchestrate business logic and delegate raw
//! row access to these repositories, whose implementations (Wave 6,
//! `hermit-core`) talk to `hermit-db`.
//!
//! Port rules applied throughout:
//! - Item **identity** arguments become [`uuid::Uuid`].
//! - Items **out of a repository** are `hermit-db` entities (the persisted row
//!   structs), never the un-ported C# `BaseItem` domain object. So
//!   `RetrieveItem(Guid) -> BaseItem` becomes
//!   `retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, _>`.
//! - `QueryResult<T>` is reused from [`hermit_model::querying::QueryResult`].
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
use hermit_db::entities::base_items::{
    AttachmentStreamInfoEntity, BaseItemEntity, ChapterEntity, KeyframeDataEntity,
    MediaStreamInfoEntity, PeopleEntity,
};
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::ItemCounts;
use hermit_model::entities::MediaStreamType;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
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

/// Reads and queries persisted [`BaseItemEntity`] rows.
///
/// Port of `IItemRepository`. Generic C# helpers (`GetGenres`/`GetStudios`/…)
/// collapse to concrete methods returning [`ItemWithCounts`]; the domain
/// `BaseItem` return type becomes [`BaseItemEntity`].
#[async_trait]
pub trait ItemRepository: Send + Sync {
    /// Retrieves a single item row by id, or `None` if it does not exist.
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError>;

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

    /// Returns the latest item rows for the given collection type (Latest API).
    async fn get_latest_item_list(
        &self,
        filter: &InternalItemsQuery,
        collection_type: hermit_model::data::CollectionType,
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

    /// Gets the image rows attached to an item, ordered by image type then by
    /// their stored order within a type.
    ///
    /// Port of the read side of `BaseItemRepository` image persistence
    /// (`BaseItemImageInfos`): each row becomes an [`ItemImageInfo`] the API
    /// layer serves or projects into an `ImageInfo` DTO. An item with no images
    /// yields an empty vector.
    async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError>;

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

    /// Gets aggregated legacy query-filter values for the matching items.
    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError>;

    /// Reports whether all children of `id` are played for the given user.
    async fn get_is_played(
        &self,
        user: &UserEntity,
        id: Uuid,
        recursive: bool,
    ) -> Result<bool, ServiceError>;
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

    /// Persists the image info attached to an item.
    async fn save_images(&self, item: &BaseItemEntity) -> Result<(), ServiceError>;

    /// Reattaches user-data rows to the correct item after an id change.
    async fn reattach_user_data(&self, item: &BaseItemEntity) -> Result<(), ServiceError>;

    /// Recomputes and persists inherited values across the item tree.
    async fn update_inherited_values(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_item_persistence_service(_: &dyn ItemPersistenceService) {}

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

    /// Replaces an item's media attachments with the given set.
    async fn save_media_attachments(
        &self,
        item_id: Uuid,
        attachments: &[AttachmentStreamInfoEntity],
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_media_attachment_repository(_: &dyn MediaAttachmentRepository) {}

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

    /// Replaces an item's people with the given set.
    async fn update_people(
        &self,
        item_id: Uuid,
        people: &[PeopleEntity],
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
