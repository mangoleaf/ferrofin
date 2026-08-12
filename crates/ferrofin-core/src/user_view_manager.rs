//! [`FerrofinUserViewManager`] — the concrete [`UserViewManager`].
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

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserViewManager;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};

use crate::item_type_lookup;
use crate::resolvers::sort_name as create_sort_name;

/// How many latest items to return per view by default (C#
/// `GetLatestItems` uses the request's `Limit`, defaulting to 20 when unset).
const DEFAULT_LATEST_LIMIT: i32 = 20;

/// The display name of the auto-provisioned playlists media folder
/// (C# `ManualPlaylistsFolder.Name`).
const PLAYLISTS_FOLDER_NAME: &str = "Playlists";

/// The concrete user-view manager.
#[derive(Clone)]
pub struct FerrofinUserViewManager {
    items: Arc<dyn ItemRepository>,
    /// The item store, set by the composition root. When present (together with a
    /// [`playlists_path`](Self::playlists_path)), [`get_media_folders`] lazily
    /// provisions the [`BaseItemKind::ManualPlaylistsFolder`] row on first read —
    /// the same self-healing stance `FerrofinVirtualFolderManager` takes for a
    /// library's `CollectionFolder`. `None` in unit tests keeps the manager
    /// read-only.
    ///
    /// [`get_media_folders`]: UserViewManager::get_media_folders
    persistence: Option<Arc<dyn ItemPersistenceService>>,
    /// The on-disk playlists directory (`{data}/playlists`), the provisioned
    /// folder's `Path`. Only meaningful alongside [`persistence`](Self::persistence).
    playlists_path: Option<PathBuf>,
    /// The per-database item-id derivation mode (see
    /// [`item_type_lookup::IdDerivation`]).
    id_derivation: item_type_lookup::IdDerivation,
}

impl std::fmt::Debug for FerrofinUserViewManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinUserViewManager")
            .field("has_item_store", &self.persistence.is_some())
            .field("playlists_path", &self.playlists_path)
            .finish_non_exhaustive()
    }
}

impl FerrofinUserViewManager {
    /// Creates a user-view manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self {
            items,
            persistence: None,
            playlists_path: None,
            id_derivation: item_type_lookup::IdDerivation::LegacyLowercase,
        }
    }

    /// Sets the per-database id-derivation mode. Called once by the
    /// composition root (unit tests keep the legacy default).
    #[must_use]
    pub fn with_id_derivation(mut self, mode: item_type_lookup::IdDerivation) -> Self {
        self.id_derivation = mode;
        self
    }

    /// Attaches the item store and the playlists directory so
    /// [`get_media_folders`](UserViewManager::get_media_folders) can lazily
    /// provision the `ManualPlaylistsFolder` row (and create its directory) on
    /// first read. Called once by the composition root.
    #[must_use]
    pub fn with_playlists_store(
        mut self,
        persistence: Arc<dyn ItemPersistenceService>,
        playlists_path: impl Into<PathBuf>,
    ) -> Self {
        self.persistence = Some(persistence);
        self.playlists_path = Some(playlists_path.into());
        self
    }

    /// The deterministic `ManualPlaylistsFolder` item id (`GetNewItemIdInternal`
    /// over the folder path).
    fn playlists_folder_id(&self, playlists_path: &std::path::Path) -> Option<Uuid> {
        item_type_lookup::derive_item_id_with(
            &self.id_derivation,
            BaseItemKind::ManualPlaylistsFolder,
            &playlists_path.to_string_lossy(),
        )
    }

    /// Upserts the `ManualPlaylistsFolder` row (and its directory) when it is
    /// missing (idempotent). No-op without an item store + playlists path wired.
    ///
    /// Port of Jellyfin's lazy `GetUserRootFolder()` provisioning of its
    /// `ManualPlaylistsFolder` child: the folder is `Name="Playlists"`,
    /// `Path={data}/playlists`, and appears among the media folders.
    async fn ensure_playlists_folder(&self) -> Result<(), ServiceError> {
        let (Some(persistence), Some(playlists_path)) = (&self.persistence, &self.playlists_path)
        else {
            return Ok(());
        };
        let Some(id) = self.playlists_folder_id(playlists_path) else {
            return Ok(());
        };
        if persistence.item_exists(id).await? {
            return Ok(());
        }
        // Create the backing directory (C# `ManualPlaylistsFolder` lives on disk).
        tokio::fs::create_dir_all(playlists_path)
            .await
            .map_err(|e| ServiceError::backend(format!("create playlists directory: {e}")))?;
        let entity = BaseItemEntity {
            id: id.to_string(),
            type_: item_type_lookup::stored_type_name(BaseItemKind::ManualPlaylistsFolder)
                .unwrap_or_default()
                .to_owned(),
            name: Some(PLAYLISTS_FOLDER_NAME.to_owned()),
            sort_name: Some(create_sort_name(PLAYLISTS_FOLDER_NAME)),
            path: Some(playlists_path.to_string_lossy().into_owned()),
            is_folder: true,
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        Ok(())
    }
}

#[async_trait]
impl UserViewManager for FerrofinUserViewManager {
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

    async fn get_media_folders(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Jellyfin's LibraryController.GetMediaFolders returns
        // GetUserRootFolder().Children sorted by SortName — the library collection
        // folders plus the auto-provisioned ManualPlaylistsFolder. Provision the
        // playlists folder on first read (lazy, self-healing), then project the
        // user-root child kinds, name-sorted.
        self.ensure_playlists_folder().await?;
        let query = InternalItemsQuery {
            include_item_types: vec![
                BaseItemKind::CollectionFolder,
                BaseItemKind::UserView,
                BaseItemKind::ManualPlaylistsFolder,
            ],
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
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{seed_item, seed_named_item, test_db};
    use ferrofin_db::Database;

    fn manager(db: &Database) -> FerrofinUserViewManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinUserViewManager::new(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
    }

    fn manager_with_playlists(
        db: &Database,
        playlists_path: impl Into<PathBuf>,
    ) -> FerrofinUserViewManager {
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        manager(db).with_playlists_store(persistence, playlists_path)
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

    #[tokio::test]
    async fn media_folders_include_the_provisioned_playlists_folder() {
        let db = test_db().await;
        // Two libraries, one already present.
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
        let libraries = 2usize;
        let tmp = tempfile::tempdir().expect("tempdir");
        let playlists_path = tmp.path().join("data").join("playlists");
        let mgr = manager_with_playlists(&db, &playlists_path);

        let folders = mgr
            .get_media_folders(Uuid::from_u128(9))
            .await
            .expect("media folders");

        // The user-root children are the libraries plus the auto-provisioned
        // Playlists folder (C# GetUserRootFolder().Children).
        assert_eq!(folders.len(), libraries + 1);
        let playlists = folders
            .iter()
            .find(|f| {
                f.type_ == "Emby.Server.Implementations.Playlists.ManualPlaylistsFolder"
                    && f.name.as_deref() == Some("Playlists")
            })
            .expect("Playlists media folder present");
        assert!(
            playlists
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("/data/playlists")),
            "playlists path should end with /data/playlists, got {:?}",
            playlists.path
        );
        // The backing directory is created on disk.
        assert!(playlists_path.is_dir());

        // Provisioning is idempotent — a second read does not add a duplicate.
        let again = mgr
            .get_media_folders(Uuid::from_u128(9))
            .await
            .expect("media folders again");
        assert_eq!(again.len(), libraries + 1);
    }
}
