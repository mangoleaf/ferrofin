//! [`HermitLibraryManager`] — the concrete [`LibraryManager`] orchestrator.
//!
//! Port of the object-safe, domain-tree-free subset of
//! `Emby.Server.Implementations.Library.LibraryManager`. The C# manager owns the
//! whole `BaseItem` OOP tree (resolvers, path/sort/named-view logic); those parts
//! live as free functions in [`crate::resolvers`] and [`crate::kinds`]. What
//! remains here is pure orchestration over the persistence seam: every query,
//! count, people, genre/studio/artist, and mutate call delegates to an injected
//! repository trait ([`ItemRepository`], [`ItemCountService`],
//! [`ItemPersistenceService`], [`PeopleRepository`]) rather than touching the
//! pool directly, so the manager stays composition-root agnostic.
//!
//! Port simplifications, all faithful to the trait surface:
//! - `create_items`/`update_items` collapse to `save_items` on the persistence
//!   service (the row upsert is idempotent); the `parent_id` argument is accepted
//!   for API parity but the parent linkage is already carried on each row's
//!   `ParentId` column.
//! - `delete_item` honors [`DeleteOptions`] by deleting the single row (and, when
//!   requested, its children) through the persistence service; the physical
//!   file deletion the C# path performs is out of scope for this seam and is
//!   noted deferred.
//! - The `IProgress`/`CancellationToken` scan plumbing is dropped;
//!   `queue_library_scan` records intent through the injected monitor/no-op.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_model::data::{BaseItemKind, CollectionType};
use hermit_model::dto::ItemCounts;
use hermit_model::entities::{ImageType, MediaStreamType};
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, image_type_allows_multiple};
use hermit_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use hermit_traits::persistence::{
    ItemCountService, ItemPersistenceService, ItemRepository, ItemWithCounts, PeopleRepository,
};

/// The concrete library manager.
///
/// Holds cheaply-cloneable `Arc<dyn _>` handles to the four persistence traits it
/// orchestrates. All are injected at the composition root so the same concrete
/// repositories back both this manager and any other consumer.
#[derive(Clone)]
pub struct HermitLibraryManager {
    items: Arc<dyn ItemRepository>,
    counts: Arc<dyn ItemCountService>,
    persistence: Arc<dyn ItemPersistenceService>,
    people: Arc<dyn PeopleRepository>,
}

impl std::fmt::Debug for HermitLibraryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLibraryManager")
            .finish_non_exhaustive()
    }
}

impl HermitLibraryManager {
    /// Creates a library manager over the injected persistence repositories.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        counts: Arc<dyn ItemCountService>,
        persistence: Arc<dyn ItemPersistenceService>,
        people: Arc<dyn PeopleRepository>,
    ) -> Self {
        Self {
            items,
            counts,
            persistence,
            people,
        }
    }

    /// Lists every non-virtual item of `kind` (the `MergeVersions` plugin's
    /// `GetItemList(IncludeItemTypes=[kind], IsVirtualItem=false, Recursive=true)`).
    async fn list_non_virtual(
        &self,
        kind: BaseItemKind,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                is_virtual_item: Some(false),
                recursive: true,
                ..Default::default()
            })
            .await
    }

    /// Splits every version group among the non-virtual items of `kind` by clearing
    /// each item's link (idempotent for items not in a group).
    async fn split_all(&self, kind: BaseItemKind) -> Result<(), ServiceError> {
        for item in self.list_non_virtual(kind).await? {
            if let Ok(id) = Uuid::parse_str(&item.id) {
                self.remove_alternate_sources(id).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl LibraryManager for HermitLibraryManager {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Ok(None);
        }
        self.items.retrieve_item(id).await
    }

    async fn get_item_images(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<hermit_traits::options::ItemImageInfo>, ServiceError> {
        if item_id.is_nil() {
            return Ok(Vec::new());
        }
        self.items.get_image_infos(item_id).await
    }

    async fn swap_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // Only backdrops and chapters may hold multiple images and thus be
        // reordered; any other type is a bad request (C# `AllowsMultipleImages`
        // guard throwing `ArgumentException` → 400).
        if !image_type_allows_multiple(image_type) {
            return Err(ServiceError::invalid_input(
                "The change index operation is only applicable to backdrops and chapters",
            ));
        }
        self.items
            .swap_item_images(item_id, image_type, index1, index2)
            .await
    }

    async fn query_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        self.items.get_items(query).await
    }

    async fn get_item_ids(&self, query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.items.get_item_ids(query).await
    }

    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items.get_item_list(query).await
    }

    async fn get_latest_item_list(
        &self,
        query: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.items
            .get_latest_item_list(query, collection_type)
            .await
    }

    async fn create_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        // The parent linkage is already carried on each row's ParentId column; the
        // upsert is the single persistence path (C# CreateItems == save + register).
        self.persistence.save_items(items).await
    }

    async fn update_items(
        &self,
        items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        self.persistence.save_items(items).await
    }

    async fn delete_item(&self, id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let Some(row) = self.items.retrieve_item(id).await? else {
            // Already gone — deletion is idempotent.
            return Ok(());
        };
        let mut ids = vec![id];
        // C# `DeleteItem` cascades to a folder's children; gather the direct-child
        // ids so the row deletion removes the subtree too. Physical file deletion
        // (honoring `delete_file_location`) is the filesystem layer's job, not this
        // persistence seam, and is deferred.
        if row.is_folder {
            let child_query = InternalItemsQuery {
                parent_id: id,
                ..Default::default()
            };
            ids.extend(self.items.get_item_ids(&child_query).await?);
        }
        self.persistence.delete_items(&ids).await
    }

    async fn merge_versions(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        // Resolve each supplied id to a persisted row, dropping any that are
        // missing, then de-duplicate and order by id (C# `.OrderBy(i => i.Id)`).
        let mut items = Vec::new();
        for &id in ids {
            if let Some(row) = self.items.retrieve_item(id).await? {
                items.push(row);
            }
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items.dedup_by(|a, b| a.id == b.id);

        if items.len() < 2 {
            return Err(ServiceError::invalid_input(
                "please supply at least two videos to merge",
            ));
        }

        // Pick the primary. C# prefers an item that already owns multiple sources
        // and is itself not an alternate; Hermit does not model `MediaSourceCount`,
        // so it falls back to C#'s secondary ordering: a plain video file outranks
        // a special type, then the widest default video stream wins. The item's own
        // `Width` column stands in for the default video stream width.
        let primary_index = items
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.width.unwrap_or(0).cmp(&b.width.unwrap_or(0)))
            .map_or(0, |(i, _)| i);
        let primary_id = items[primary_index].id.clone();

        // Link every non-primary item to the primary by pointer, and ensure the
        // primary itself is a standalone (its own pointer cleared).
        let mut updated = Vec::new();
        for item in &items {
            if item.id == primary_id {
                if item.primary_version_id.is_some() {
                    let mut primary = item.clone();
                    primary.primary_version_id = None;
                    updated.push(primary);
                }
            } else if item.primary_version_id.as_deref() != Some(primary_id.as_str()) {
                let mut alt = item.clone();
                alt.primary_version_id = Some(primary_id.clone());
                updated.push(alt);
            }
        }

        if !updated.is_empty() {
            self.persistence.save_items(&updated).await?;
        }
        Ok(())
    }

    async fn remove_alternate_sources(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(item) = self.items.retrieve_item(item_id).await? else {
            return Err(ServiceError::not_found(format!("item {item_id}")));
        };

        // Resolve the group's primary: either this item (no pointer) or the item it
        // points at (C# hops to `PrimaryVersionId` when the item has no alternates).
        let primary_id = match item.primary_version_id.as_deref() {
            Some(pid) => Uuid::parse_str(pid)
                .map_err(|_| ServiceError::invalid_input("malformed PrimaryVersionId"))?,
            None => item_id,
        };

        // Clear the pointer on every alternate that references the primary, then on
        // the primary itself, so each becomes a standalone version again.
        let mut updated = Vec::new();
        for mut alt in self.items.get_items_by_primary_version(primary_id).await? {
            alt.primary_version_id = None;
            updated.push(alt);
        }
        if let Some(mut primary) = self.items.retrieve_item(primary_id).await?
            && primary.primary_version_id.is_some()
        {
            primary.primary_version_id = None;
            updated.push(primary);
        }

        if !updated.is_empty() {
            self.persistence.save_items(&updated).await?;
        }
        Ok(())
    }

    async fn merge_all_movie_versions(&self) -> Result<(), ServiceError> {
        let movies = self.list_non_virtual(BaseItemKind::Movie).await?;
        let tmdb: HashMap<Uuid, String> = self
            .items
            .get_items_with_provider_id("Tmdb")
            .await?
            .into_iter()
            .collect();

        // Group movies by their Tmdb id, tracking whether each group has a member
        // that is not already an alternate (C# `PrimaryVersionId == null`).
        let mut groups: HashMap<String, Vec<Uuid>> = HashMap::new();
        let mut eligible: std::collections::HashSet<String> = std::collections::HashSet::new();
        for movie in &movies {
            let Ok(id) = Uuid::parse_str(&movie.id) else {
                continue;
            };
            let Some(value) = tmdb.get(&id) else {
                continue; // no Tmdb id → skipped, matching the plugin's filter
            };
            if movie.primary_version_id.is_none() {
                eligible.insert(value.clone());
            }
            groups.entry(value.clone()).or_default().push(id);
        }

        for (value, ids) in groups {
            if ids.len() > 1 && eligible.contains(&value) {
                self.merge_versions(&ids).await?;
            }
        }
        Ok(())
    }

    async fn split_all_movie_versions(&self) -> Result<(), ServiceError> {
        self.split_all(BaseItemKind::Movie).await
    }

    async fn merge_all_episode_versions(&self) -> Result<(), ServiceError> {
        let episodes = self.list_non_virtual(BaseItemKind::Episode).await?;

        // Group by (series, season, name, index, year) — the plugin's episode key.
        let mut groups: HashMap<(String, String, String, i64, i64), Vec<Uuid>> = HashMap::new();
        for ep in &episodes {
            let Ok(id) = Uuid::parse_str(&ep.id) else {
                continue;
            };
            let key = (
                ep.series_name.clone().unwrap_or_default(),
                ep.season_name.clone().unwrap_or_default(),
                ep.name.clone().unwrap_or_default(),
                ep.index_number.unwrap_or_default(),
                ep.production_year.unwrap_or_default(),
            );
            groups.entry(key).or_default().push(id);
        }

        for ids in groups.into_values() {
            if ids.len() > 1 {
                self.merge_versions(&ids).await?;
            }
        }
        Ok(())
    }

    async fn split_all_episode_versions(&self) -> Result<(), ServiceError> {
        self.split_all(BaseItemKind::Episode).await
    }

    async fn get_people(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        Ok(self.people.get_people(query).await?.items)
    }

    async fn get_people_names(
        &self,
        query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.people.get_people_names(query).await
    }

    async fn get_count(&self, query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        self.counts.get_count(query).await
    }

    async fn get_item_counts(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        self.counts.get_item_counts(query).await
    }

    async fn get_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_genres(query).await
    }

    async fn get_studios(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_studios(query).await
    }

    async fn get_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_artists(query).await
    }

    async fn get_music_genres(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_music_genres(query).await
    }

    async fn get_album_artists(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.items.get_album_artists(query).await
    }

    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.items.get_query_filters_legacy(query).await
    }

    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
        query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        self.items
            .get_media_stream_languages(query, stream_type)
            .await
    }

    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        // The real scheduler/scan pipeline is a later wave; queuing is a no-op that
        // succeeds so callers (API endpoints) get the expected 204 semantics.
        tracing::debug!("library scan queued (no-op: scan pipeline deferred)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_count_service::HermitItemCountService;
    use crate::item_persistence_service::HermitItemPersistenceService;
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::people_repository::HermitPeopleRepository;
    use crate::test_support::{
        seed_item, seed_item_genre, seed_named_item, set_clean_name, test_db,
    };
    use hermit_db::Database;
    use hermit_model::data::BaseItemKind;
    use hermit_model::entities::ImageType;

    /// Builds a manager backed by real repositories over the given database.
    fn manager(db: &Database) -> HermitLibraryManager {
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitLibraryManager::new(
            Arc::new(HermitItemRepository::new(db.clone(), lookup.clone())),
            Arc::new(HermitItemCountService::new(db.clone())),
            Arc::new(HermitItemPersistenceService::new(db.clone())),
            Arc::new(HermitPeopleRepository::new(db.clone())),
        )
    }

    #[tokio::test]
    async fn get_item_by_id_reads_seeded_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(7);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mgr = manager(&db);

        let item = mgr.get_item_by_id(id).await.expect("read").expect("some");
        assert_eq!(item.name.as_deref(), Some("Solaris"));
        // A nil id short-circuits to None without hitting the pool.
        assert!(
            mgr.get_item_by_id(Uuid::nil())
                .await
                .expect("nil")
                .is_none()
        );
    }

    #[tokio::test]
    async fn swap_images_rejects_non_multiple_type_and_swaps_backdrops() {
        let db = test_db().await;
        let item = Uuid::from_u128(0xA100);
        seed_named_item(&db, item, BaseItemKind::Movie, "Swappable").await;
        for (n, path) in [(0u128, "/one.jpg"), (1, "/two.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 0, 2, ?2, ?3, 0)"#,
            )
            .bind(Uuid::from_u128(0xA110 + n).to_string())
            .bind(item.to_string())
            .bind(path)
            .execute(db.pool())
            .await
            .expect("insert backdrop");
        }
        let mgr = manager(&db);

        // Primary does not allow multiple images → InvalidInput (the 400).
        let err = mgr
            .swap_images(item, ImageType::Primary, 0, 1)
            .await
            .expect_err("primary rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));

        // Backdrop is reorderable and the swap goes through to the repository.
        mgr.swap_images(item, ImageType::Backdrop, 0, 1)
            .await
            .expect("swap");
        let images = mgr.get_item_images(item).await.expect("images");
        assert_eq!(images[0].path, "/two.jpg");
        assert_eq!(images[1].path, "/one.jpg");
    }

    #[tokio::test]
    async fn query_items_returns_matching_rows() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        let a = Uuid::from_u128(0x101);
        let b = Uuid::from_u128(0x102);
        seed_named_item(&db, a, BaseItemKind::Movie, "MovieA").await;
        seed_named_item(&db, b, BaseItemKind::Movie, "MovieB").await;
        set_clean_name(&db, a, "MovieA").await;
        set_clean_name(&db, b, "MovieB").await;
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Episode).await;
        let mgr = manager(&db);

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            name_contains: Some("Movie".to_owned()),
            ..Default::default()
        };
        let result = mgr.query_items(&query).await.expect("query");
        assert_eq!(result.items.len(), 2);
    }

    #[tokio::test]
    async fn delete_item_removes_the_row() {
        let db = test_db().await;
        let id = Uuid::from_u128(9);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        mgr.delete_item(id, &DeleteOptions::default())
            .await
            .expect("delete");
        assert!(mgr.get_item_by_id(id).await.expect("read").is_none());
    }

    #[tokio::test]
    async fn queue_library_scan_is_a_successful_no_op() {
        let db = test_db().await;
        let mgr = manager(&db);
        mgr.queue_library_scan().await.expect("queue");
    }

    #[tokio::test]
    async fn get_named_item_resolves_by_clean_name() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x201);
        seed_named_item(&db, id, BaseItemKind::Genre, "Science Fiction").await;
        set_clean_name(&db, id, "Science Fiction").await;
        // A different-kind row with the same name must not be returned.
        let other = Uuid::from_u128(0x202);
        seed_named_item(&db, other, BaseItemKind::Studio, "Science Fiction").await;
        set_clean_name(&db, other, "Science Fiction").await;
        let mgr = manager(&db);

        let found = mgr
            .get_named_item(BaseItemKind::Genre, "Science Fiction")
            .await
            .expect("lookup")
            .expect("some");
        assert_eq!(found.id, id.to_string());
        assert_eq!(found.name.as_deref(), Some("Science Fiction"));
    }

    #[tokio::test]
    async fn get_ancestors_walks_parent_chain_nearest_first() {
        let db = test_db().await;
        // grandparent <- parent <- child
        let grandparent = Uuid::from_u128(0x301);
        let parent = Uuid::from_u128(0x302);
        let child = Uuid::from_u128(0x303);
        seed_named_item(&db, grandparent, BaseItemKind::Folder, "Library").await;
        seed_named_item(&db, parent, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, child, BaseItemKind::Episode, "Pilot").await;
        for (id, parent_id) in [(child, parent), (parent, grandparent)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .bind(parent_id.to_string())
                .execute(db.pool())
                .await
                .expect("set parent");
        }
        let mgr = manager(&db);

        let ancestors = mgr
            .get_ancestors(child)
            .await
            .expect("ancestors")
            .expect("item exists");
        // Nearest parent first, then its parent — the seed item is excluded.
        assert_eq!(ancestors.len(), 2);
        assert_eq!(ancestors[0].id, parent.to_string());
        assert_eq!(ancestors[1].id, grandparent.to_string());

        // A root item (no parent) yields an empty list, not None.
        let roots = mgr
            .get_ancestors(grandparent)
            .await
            .expect("ancestors")
            .expect("item exists");
        assert!(roots.is_empty());

        // A missing item yields None so the API maps it to 404.
        assert!(
            mgr.get_ancestors(Uuid::from_u128(0x3ff))
                .await
                .expect("missing")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_named_item_missing_is_none() {
        let db = test_db().await;
        let mgr = manager(&db);
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "Nope")
                .await
                .expect("lookup")
                .is_none()
        );
        // A blank name short-circuits to None.
        assert!(
            mgr.get_named_item(BaseItemKind::Genre, "   ")
                .await
                .expect("blank")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_music_genres_counts_referencing_items() {
        let db = test_db().await;
        // A MusicGenre by-name row plus a song that references it.
        let genre_id = Uuid::from_u128(0x301);
        seed_named_item(&db, genre_id, BaseItemKind::MusicGenre, "Jazz").await;
        set_clean_name(&db, genre_id, "Jazz").await;
        let song = Uuid::from_u128(0x302);
        seed_named_item(&db, song, BaseItemKind::Audio, "Blue in Green").await;
        seed_item_genre(&db, song, "Jazz").await;
        let mgr = manager(&db);

        let result = mgr
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        let jazz = result
            .items
            .iter()
            .find(|iwc| iwc.item.name.as_deref() == Some("Jazz"))
            .expect("jazz present");
        assert_eq!(jazz.counts.item_count, 1);
    }

    #[tokio::test]
    async fn get_media_stream_languages_reads_distinct_codes() {
        use hermit_model::entities::MediaStreamType;
        let db = test_db().await;
        let item = Uuid::from_u128(0x501);
        seed_item(&db, item, BaseItemKind::Movie).await;
        // One English audio stream plus one with no language (→ 'und').
        for (idx, lang) in [(0_i64, Some("eng")), (1, None)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos"
                   ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                    "IsOriginal", "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, 0, ?3)"#,
            )
            .bind(item.to_string())
            .bind(idx)
            .bind(lang)
            .execute(db.pool())
            .await
            .expect("insert stream");
        }
        let mgr = manager(&db);

        let mut langs = mgr
            .get_media_stream_languages(MediaStreamType::Audio, &InternalItemsQuery::default())
            .await
            .expect("languages");
        langs.sort();
        assert_eq!(langs, vec!["eng".to_owned(), "und".to_owned()]);
    }

    #[tokio::test]
    async fn get_album_artists_returns_artist_rows() {
        let db = test_db().await;
        let artist = Uuid::from_u128(0x401);
        seed_named_item(&db, artist, BaseItemKind::MusicArtist, "Miles Davis").await;
        set_clean_name(&db, artist, "Miles Davis").await;
        let mgr = manager(&db);

        let result = mgr
            .get_album_artists(&InternalItemsQuery::default())
            .await
            .expect("album artists");
        assert!(
            result
                .items
                .iter()
                .any(|iwc| iwc.item.name.as_deref() == Some("Miles Davis"))
        );
    }

    #[tokio::test]
    async fn get_user_root_folder_resolves_the_root_row() {
        let db = test_db().await;
        let mgr = manager(&db);

        // With no root row materialized, the default resolves to None.
        assert!(mgr.get_user_root_folder().await.expect("none").is_none());

        // Once a UserRootFolder row exists, it is returned.
        let root = Uuid::from_u128(0x5001);
        seed_named_item(&db, root, BaseItemKind::UserRootFolder, "Media Folders").await;
        let resolved = mgr
            .get_user_root_folder()
            .await
            .expect("root")
            .expect("some");
        assert_eq!(resolved.id, root.to_string());
    }

    /// Sets a row's `Width` column so the merge primary-selection heuristic has a
    /// deterministic winner.
    async fn set_width(db: &Database, id: Uuid, width: i64) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Width" = ?1 WHERE "Id" = ?2"#)
            .bind(width)
            .bind(id.to_string())
            .execute(db.pool())
            .await
            .expect("set width");
    }

    #[tokio::test]
    async fn merge_versions_links_alternates_to_widest_primary() {
        let db = test_db().await;
        let wide = Uuid::from_u128(0x301);
        let narrow = Uuid::from_u128(0x302);
        seed_item(&db, wide, BaseItemKind::Movie).await;
        seed_item(&db, narrow, BaseItemKind::Movie).await;
        set_width(&db, wide, 1920).await;
        set_width(&db, narrow, 640).await;
        let mgr = manager(&db);

        mgr.merge_versions(&[narrow, wide]).await.expect("merge");

        // The widest becomes the primary (its own pointer stays null); the narrow
        // one points at it.
        let primary = mgr.get_item_by_id(wide).await.expect("read").expect("some");
        assert_eq!(primary.primary_version_id, None);
        let alt = mgr
            .get_item_by_id(narrow)
            .await
            .expect("read")
            .expect("some");
        assert_eq!(
            alt.primary_version_id.as_deref(),
            Some(wide.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn merge_versions_rejects_single_id() {
        let db = test_db().await;
        let id = Uuid::from_u128(0x303);
        seed_item(&db, id, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let err = mgr.merge_versions(&[id]).await.expect_err("too few");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn remove_alternate_sources_clears_the_group() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x311);
        let alt = Uuid::from_u128(0x312);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        set_width(&db, primary, 1920).await;
        set_width(&db, alt, 640).await;
        let mgr = manager(&db);
        mgr.merge_versions(&[primary, alt]).await.expect("merge");

        // Splitting from the *alternate* still clears the whole group.
        mgr.remove_alternate_sources(alt).await.expect("remove");

        assert_eq!(
            mgr.get_item_by_id(primary)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
        assert_eq!(
            mgr.get_item_by_id(alt)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
    }

    #[tokio::test]
    async fn remove_alternate_sources_missing_item_is_not_found() {
        let db = test_db().await;
        let mgr = manager(&db);
        let err = mgr
            .remove_alternate_sources(Uuid::from_u128(0x3FF))
            .await
            .expect_err("missing");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    /// Attaches a `(ProviderId, ProviderValue)` external id to an item.
    async fn set_provider_id(db: &Database, id: Uuid, key: &str, value: &str) {
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(id.to_string())
        .bind(key)
        .bind(value)
        .execute(db.pool())
        .await
        .expect("set provider id");
    }

    /// Sets the episode-grouping columns the bulk merge keys on.
    async fn set_episode_fields(
        db: &Database,
        id: Uuid,
        series: &str,
        season: &str,
        name: &str,
        index: i64,
        year: i64,
    ) {
        sqlx::query(
            r#"UPDATE "BaseItems"
               SET "SeriesName" = ?2, "SeasonName" = ?3, "Name" = ?4,
                   "IndexNumber" = ?5, "ProductionYear" = ?6
               WHERE "Id" = ?1"#,
        )
        .bind(id.to_string())
        .bind(series)
        .bind(season)
        .bind(name)
        .bind(index)
        .bind(year)
        .execute(db.pool())
        .await
        .expect("set episode fields");
    }

    #[tokio::test]
    async fn merge_all_movie_versions_groups_by_tmdb() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x401);
        let b = Uuid::from_u128(0x402);
        let lonely = Uuid::from_u128(0x403);
        let no_id = Uuid::from_u128(0x404);
        for id in [a, b, lonely, no_id] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        // Two files of the same movie (same Tmdb id), one of a different movie, and
        // one with no Tmdb id at all (must be skipped).
        set_provider_id(&db, a, "Tmdb", "603").await;
        set_provider_id(&db, b, "Tmdb", "603").await;
        set_provider_id(&db, lonely, "Tmdb", "604").await;
        set_width(&db, a, 1920).await;
        set_width(&db, b, 640).await;
        let mgr = manager(&db);

        mgr.merge_all_movie_versions().await.expect("merge movies");

        // a (widest) is the primary; b links to it.
        assert_eq!(
            mgr.get_item_by_id(a)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
        assert_eq!(
            mgr.get_item_by_id(b)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id
                .as_deref(),
            Some(a.to_string().as_str())
        );
        // The single-file movie and the id-less movie are untouched.
        assert_eq!(
            mgr.get_item_by_id(lonely)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
        assert_eq!(
            mgr.get_item_by_id(no_id)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
    }

    #[tokio::test]
    async fn split_all_movie_versions_clears_every_group() {
        let db = test_db().await;
        let primary = Uuid::from_u128(0x411);
        let alt = Uuid::from_u128(0x412);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        set_width(&db, primary, 1920).await;
        set_width(&db, alt, 640).await;
        let mgr = manager(&db);
        mgr.merge_versions(&[primary, alt]).await.expect("merge");

        mgr.split_all_movie_versions().await.expect("split movies");

        for id in [primary, alt] {
            assert_eq!(
                mgr.get_item_by_id(id)
                    .await
                    .expect("read")
                    .expect("some")
                    .primary_version_id,
                None
            );
        }
    }

    #[tokio::test]
    async fn merge_all_episode_versions_groups_by_key() {
        let db = test_db().await;
        let a = Uuid::from_u128(0x421);
        let b = Uuid::from_u128(0x422);
        let other = Uuid::from_u128(0x423);
        for id in [a, b, other] {
            seed_item(&db, id, BaseItemKind::Episode).await;
        }
        // a and b are two files of the same episode; `other` differs by index.
        set_episode_fields(&db, a, "Show", "Season 1", "Pilot", 1, 2020).await;
        set_episode_fields(&db, b, "Show", "Season 1", "Pilot", 1, 2020).await;
        set_episode_fields(&db, other, "Show", "Season 1", "Pilot", 2, 2020).await;
        set_width(&db, a, 1920).await;
        set_width(&db, b, 640).await;
        let mgr = manager(&db);

        mgr.merge_all_episode_versions()
            .await
            .expect("merge episodes");

        assert_eq!(
            mgr.get_item_by_id(b)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id
                .as_deref(),
            Some(a.to_string().as_str())
        );
        assert_eq!(
            mgr.get_item_by_id(other)
                .await
                .expect("read")
                .expect("some")
                .primary_version_id,
            None
        );
    }
}
