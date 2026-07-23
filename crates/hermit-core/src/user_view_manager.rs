//! [`HermitUserViewManager`] — the concrete [`UserViewManager`].
//!
//! Port of `Emby.Server.Implementations.Library.UserViewManager` (the object-safe
//! subset). The C# manager assembles the per-user "views" — the top-level library
//! folders shown on the home screen — and the "latest" rows under each. It leans
//! on the whole `Folder`/`UserView` object tree; at this seam the views are the
//! persisted [`BaseItemKind::CollectionFolder`] / [`BaseItemKind::UserView`] rows,
//! and "latest" is a newest-first query scoped to each view, both served by the
//! injected [`ItemRepository`].
//!
//! Deferred (needs the un-ported scanner/grouping): the special "grouped" views
//! (all-movies/all-tv merges), channel views, and per-user view ordering from
//! display preferences. Those layer on top of the row set returned here.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::SortOrder;
use hermit_model::live_tv::ItemSortBy;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::UserViewManager;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::persistence::ItemRepository;

/// How many latest items to return per view by default (C#
/// `GetLatestItems` uses the request's `Limit`, defaulting to 20 when unset).
const DEFAULT_LATEST_LIMIT: i32 = 20;

/// The concrete user-view manager.
#[derive(Clone)]
pub struct HermitUserViewManager {
    items: Arc<dyn ItemRepository>,
}

impl std::fmt::Debug for HermitUserViewManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitUserViewManager")
            .finish_non_exhaustive()
    }
}

impl HermitUserViewManager {
    /// Creates a user-view manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self { items }
    }
}

#[async_trait]
impl UserViewManager for HermitUserViewManager {
    async fn get_user_views(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // The user's top-level views are the library collection folders. Per-user
        // access filtering (which libraries the user may see) rides on the
        // InternalItemsQuery.user field in the full pipeline; the base set is every
        // collection folder / user view, name-sorted.
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::CollectionFolder, BaseItemKind::UserView],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        self.items.get_item_list(&query).await
    }

    async fn get_latest_items(
        &self,
        user_id: Uuid,
        options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        let _ = options;
        let views = self.get_user_views(user_id).await?;
        let mut result = Vec::with_capacity(views.len());
        for view in views {
            let Ok(view_id) = Uuid::parse_str(&view.id) else {
                continue;
            };
            let latest_query = InternalItemsQuery {
                parent_id: view_id,
                recursive: true,
                is_folder: Some(false),
                limit: Some(DEFAULT_LATEST_LIMIT),
                order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
                ..Default::default()
            };
            let latest = self.items.get_item_list(&latest_query).await?;
            result.push((view, latest));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{seed_item, seed_named_item, test_db};
    use hermit_db::Database;

    fn manager(db: &Database) -> HermitUserViewManager {
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitUserViewManager::new(Arc::new(HermitItemRepository::new(db.clone(), lookup)))
    }

    #[tokio::test]
    async fn user_views_are_the_collection_folders() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        seed_named_item(
            &db,
            Uuid::from_u128(0x101),
            BaseItemKind::CollectionFolder,
            "Movies",
        )
        .await;
        seed_named_item(
            &db,
            Uuid::from_u128(0x102),
            BaseItemKind::CollectionFolder,
            "Shows",
        )
        .await;
        // A regular movie is not a view.
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let views = mgr.get_user_views(Uuid::from_u128(9)).await.expect("views");
        assert_eq!(views.len(), 2);
        assert!(
            views
                .iter()
                .all(|v| v.type_ != *"Movie" && v.name.is_some())
        );
    }

    #[tokio::test]
    async fn latest_items_group_under_each_view() {
        let db = test_db().await;
        let view = Uuid::from_u128(0x101);
        seed_named_item(&db, view, BaseItemKind::CollectionFolder, "Movies").await;
        let mgr = manager(&db);

        let grouped = mgr
            .get_latest_items(Uuid::from_u128(9), &DtoOptions::default())
            .await
            .expect("latest");
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0.id, view.to_string());
    }
}
