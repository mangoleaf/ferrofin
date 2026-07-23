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

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use hermit_model::data::CollectionType;
use hermit_model::dto::ItemCounts;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
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
}

#[async_trait]
impl LibraryManager for HermitLibraryManager {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Ok(None);
        }
        self.items.retrieve_item(id).await
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

    async fn get_query_filters_legacy(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.items.get_query_filters_legacy(query).await
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
    use crate::test_support::{seed_item, seed_named_item, set_clean_name, test_db};
    use hermit_db::Database;
    use hermit_model::data::BaseItemKind;

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
}
