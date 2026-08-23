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
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery, LatestItemsQuery};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};

use crate::item_type_lookup;
use ferrofin_util::sort_name::create_sort_name;

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
            // `guid_to_db`, NOT `to_string()`. Jellyfin stores Guid columns
            // UPPERCASE-hyphenated and `BaseItems."Id"` is plain TEXT with no
            // COLLATE NOCASE, so a lowercase id is a different row as far as
            // SQLite is concerned. Writing one here meant the `item_exists`
            // check above — which binds `guid_to_db(id)` — could never see the
            // row it had just written, so EVERY `GET /Library/MediaFolders`
            // re-ran `create_dir_all` plus this upsert through the single
            // writer connection. Under load that serialized the endpoint:
            // 1355 ms p50 and 31% errors in the benchmark against Jellyfin's
            // 0.23 ms. It also leaked into the response — the folder came back
            // with a lowercase `Id` where Jellyfin sends uppercase, which is
            // why the suite scored this operation as diverging from upstream.
            id: ferrofin_db::store::guid_to_db(id),
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
        query: &LatestItemsQuery,
        options: &DtoOptions,
    ) -> Result<Vec<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        let _ = options;
        let views = self.get_user_views(query.user_id).await?;
        let limit = if query.limit > 0 {
            query.limit
        } else {
            DEFAULT_LATEST_LIMIT
        };
        let mut result = Vec::with_capacity(views.len());
        for view in views {
            let Ok(view_id) = Uuid::parse_str(&view.id) else {
                continue;
            };
            // A parent-scoped request keeps only that view's group. Stored ids
            // are uppercase-hyphenated (`guid_to_db`), so compare as Uuids.
            if let Some(parent) = query.parent_id
                && view_id != parent
            {
                continue;
            }
            let latest_query = InternalItemsQuery {
                // A view is a `CollectionFolder`/`UserView`, i.e. a *top parent*:
                // C# `LibraryManager.SetTopParentOrAncestorIds` swaps an ancestor
                // scope for `TopParentIds` whenever every ancestor is one of those
                // two kinds, and the rows here are exactly that set (see
                // `get_user_views`). The swap is semantically identical — the
                // scanner stamps every scanned entity's `TopParentId` with its
                // collection folder — but it turns a full `BaseItems` scan with a
                // correlated `AncestorIds` lookup per row plus a temp-B-tree sort
                // into an index seek on
                // `FerrofinIX_BaseItems_TopParentId_IsFolder_IsVirtualItem_DateCreated`.
                top_parent_ids: vec![view_id],
                is_folder: Some(false),
                // C# also excludes virtual rows (NFO-declared missing episodes).
                is_virtual_item: Some(false),
                include_item_types: query.include_item_types.clone(),
                // C# fetches `limit * 2` before grouping so the caller's
                // played/type post-filters don't starve the page.
                limit: Some(limit.saturating_mul(2)),
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
    use crate::test_support::{seed_episode, seed_item, seed_named_item, test_db};
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

        let query = LatestItemsQuery {
            user_id: Uuid::from_u128(9),
            ..LatestItemsQuery::default()
        };
        let grouped = mgr
            .get_latest_items(&query, &DtoOptions::default())
            .await
            .expect("latest");
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0.id, view.to_string());
    }

    #[tokio::test]
    async fn latest_items_are_scoped_to_their_view_by_top_parent() {
        let db = test_db().await;
        let movies = Uuid::from_u128(0x101);
        let shows = Uuid::from_u128(0x102);
        seed_named_item(&db, movies, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, shows, BaseItemKind::CollectionFolder, "Shows").await;
        // Every scanned row carries its collection folder in `TopParentId`, and
        // that is the ONLY scope the per-view latest query applies (C#
        // `SetTopParentOrAncestorIds` swaps an ancestor scope for the top-parent
        // one when the ancestors are views) — no `AncestorIds` closure row is
        // seeded here, and each view must still see exactly its own item.
        let a_movie = Uuid::from_u128(0x201);
        let an_episode = Uuid::from_u128(0x202);
        seed_episode(&db, a_movie, "movies-key", 1, 1, false, Some(movies)).await;
        seed_episode(&db, an_episode, "shows-key", 1, 1, false, Some(shows)).await;
        let mgr = manager(&db);

        let query = LatestItemsQuery {
            user_id: Uuid::from_u128(9),
            ..LatestItemsQuery::default()
        };
        let grouped = mgr
            .get_latest_items(&query, &DtoOptions::default())
            .await
            .expect("latest");

        assert_eq!(grouped.len(), 2, "one group per view");
        for (view, items) in grouped {
            let expected = if view.id.eq_ignore_ascii_case(&movies.to_string()) {
                a_movie
            } else {
                an_episode
            };
            let ids: Vec<String> = items.iter().map(|i| i.id.to_lowercase()).collect();
            assert_eq!(
                ids,
                vec![expected.to_string()],
                "view {:?} leaked another library's items",
                view.name
            );
        }
    }

    #[tokio::test]
    async fn latest_items_parent_scoping_is_case_insensitive() {
        let db = test_db().await;
        // Ids with hex letters, so the stored uppercase form actually differs
        // from `Uuid::to_string()` (an all-digit id can't catch a casing bug).
        let movies = Uuid::from_u128(0xABCD_EF01);
        let shows = Uuid::from_u128(0xABCD_EF02);
        seed_named_item(&db, movies, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, shows, BaseItemKind::CollectionFolder, "Shows").await;
        let mgr = manager(&db);

        // Stored view ids are uppercase-hyphenated (`guid_to_db`); the request's
        // `parentId` must still match — the regression was a string compare of
        // the uppercase row id against `Uuid::to_string()` (lowercase), which
        // silently emptied every parent-scoped "Latest" row.
        let query = LatestItemsQuery {
            user_id: Uuid::from_u128(9),
            parent_id: Some(movies),
            ..LatestItemsQuery::default()
        };
        let grouped = mgr
            .get_latest_items(&query, &DtoOptions::default())
            .await
            .expect("latest");
        assert_eq!(grouped.len(), 1);
        assert!(grouped[0].0.id.eq_ignore_ascii_case(&movies.to_string()));
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

        // The id is stored the way Jellyfin stores Guid columns: UPPERCASE
        // hyphenated. `BaseItems."Id"` is plain TEXT with no COLLATE NOCASE, so
        // a lowercase id is a different row to SQLite — the existence check
        // below would never match it, and the folder would also come back to
        // clients with a lowercase `Id` where Jellyfin sends uppercase.
        assert_eq!(
            playlists.id,
            playlists.id.to_uppercase(),
            "the provisioned id must be stored in guid_to_db form, got {}",
            playlists.id
        );

        // Provisioning is idempotent — and this checks that the second read
        // does not WRITE, not merely that it does not duplicate. An upsert on
        // the same id can never duplicate, so a row count proves nothing; the
        // bug this guards against re-ran the upsert on every single request and
        // still left exactly one row. Renaming the row out from under the
        // manager makes a rewrite observable: if provisioning runs again it
        // stamps `Name` back to "Playlists".
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let mut renamed_row = playlists.clone();
        renamed_row.name = Some("SENTINEL".to_owned());
        persistence
            .save_items(std::slice::from_ref(&renamed_row))
            .await
            .expect("rename the provisioned row");

        let again = mgr
            .get_media_folders(Uuid::from_u128(9))
            .await
            .expect("media folders again");
        assert_eq!(again.len(), libraries + 1);
        let renamed = again
            .iter()
            .find(|f| f.id == playlists.id)
            .expect("the provisioned row is still there");
        assert_eq!(
            renamed.name.as_deref(),
            Some("SENTINEL"),
            "a second read re-provisioned the folder — the existence check did \
             not match the row it wrote, so every request pays a filesystem \
             call and a write through the single writer connection"
        );
    }
}
