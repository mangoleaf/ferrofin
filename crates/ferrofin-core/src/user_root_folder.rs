//! [`UserRootFolderStore`] — materializes the `UserRootFolder` row, the
//! synthetic top of the library tree.
//!
//! Port of `LibraryManager.GetUserRootFolder()`: the folder lives at
//! `ApplicationPaths.DefaultUserViewsPath` (`{program data}/root/default`),
//! the directory is created, its id is `GetNewItemId(path,
//! typeof(UserRootFolder))`, and the row is persisted on first use. Every
//! library's `CollectionFolder` (a directory *under* `root/default/`) is its
//! child, which is what makes `GET /Items/{id}/Ancestors` climb past the
//! library to the root and `GET /Items/Root` resolve a real row.
//!
//! `LibraryManager.CreateRootFolder()` also creates the **playlists** plugin
//! folder at `{DataPath}/playlists`, but it parents that folder to the
//! **`AggregateFolder`** (`folder.ParentId = rootFolder.Id`, then
//! `rootFolder.AddVirtualChild(folder)`), not to this row — the user root only
//! picks it up at read time, through
//! `UserRootFolder.GetEligibleChildrenForRecursiveChildren`
//! (`UserRootFolder.cs:96-102`). Ferrofin ports that split in
//! [`crate::aggregate_folder::AggregateFolderStore`] (provisioning) and in
//! [`crate::item_repository`]/[`crate::item_count_service`] (the read-time
//! concat), so this store owns the `UserRootFolder` row and nothing else.
//!
//! Jellyfin renames the row from its directory name (`default`) to
//! `Media Folders` on its first metadata refresh (`UserRootFolder.
//! BeforeMetadataRefresh`); a 10.11.8 database carries that name, so the row
//! is created with it directly. The derived id is identical to the one an
//! adopted Jellyfin database already holds, so on such a database this store
//! finds the existing row and writes nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemPersistenceService;

use crate::item_type_lookup::{self, IdDerivation};
use ferrofin_util::sort_name::create_sort_name;

/// The display name Jellyfin gives the user root folder.
pub const USER_ROOT_FOLDER_NAME: &str = "Media Folders";

/// Creates and resolves the one `UserRootFolder` row.
#[derive(Clone)]
pub struct UserRootFolderStore {
    persistence: Arc<dyn ItemPersistenceService>,
    id_derivation: IdDerivation,
    path: PathBuf,
    /// Memoizes the resolved id, so the whole provisioning pass runs **once**
    /// per process — the port of C#'s `_userRootFolder` field, which
    /// `GetUserRootFolder()` builds under a lock and then hands out forever.
    ///
    /// Load-bearing for latency, not just tidiness: `ensure()` is on the hot
    /// path of `GET /Items/Root`, `GET /Library/MediaFolders`,
    /// `GET /Library/VirtualFolders` and the scan, and a cold pass ends in a
    /// writer statement. Re-running that per request is exactly what once
    /// serialized `/Library/MediaFolders` to 1355 ms p50 (see the id-casing
    /// note in `item_persistence_service`). Shared across clones via the
    /// `Arc`, so every holder of the store settles it together.
    resolved: Arc<tokio::sync::OnceCell<Uuid>>,
}

impl std::fmt::Debug for UserRootFolderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserRootFolderStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl UserRootFolderStore {
    /// Creates the store over the item writer, the database's id derivation
    /// mode, and the default user-views directory (`{program data}/root/default`).
    #[must_use]
    pub fn new(
        persistence: Arc<dyn ItemPersistenceService>,
        id_derivation: IdDerivation,
        default_user_views_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            persistence,
            id_derivation,
            path: default_user_views_path.into(),
            resolved: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// The directory the row represents.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The deterministic `UserRootFolder` id (`GetNewItemId(path,
    /// typeof(UserRootFolder))`).
    #[must_use]
    pub fn id(&self) -> Uuid {
        // `UserRootFolder` has a stored type name, so derivation cannot fail;
        // the nil id is only reachable if the kind table is ever edited.
        item_type_lookup::user_root_folder_id(&self.id_derivation, &self.path.to_string_lossy())
            .unwrap_or_default()
    }

    /// The row as it is created: `Name = "Media Folders"`, `IsFolder`, no
    /// parent, the presentation key Jellyfin stamps on folders.
    fn entity(&self, id: Uuid) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::UserRootFolder)
                .unwrap_or_default()
                .to_owned(),
            name: Some(USER_ROOT_FOLDER_NAME.to_owned()),
            sort_name: Some(create_sort_name(USER_ROOT_FOLDER_NAME)),
            path: Some(self.path.to_string_lossy().into_owned()),
            presentation_unique_key: Some(id.as_simple().to_string()),
            is_folder: true,
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        }
    }

    /// Ensures the directory and the row exist, returning the root row's id.
    ///
    /// Runs at most **once** per process: the result is memoized the way C#
    /// caches `_userRootFolder`, so every later call is a field read with no
    /// database traffic at all. That matters because this sits on the hot path
    /// of `GET /Items/Root`, `GET /Library/MediaFolders`,
    /// `GET /Library/VirtualFolders` and the scan.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or a row cannot be
    /// written. A failed pass is not cached, so the next call retries.
    pub async fn ensure(&self) -> Result<Uuid, ServiceError> {
        self.resolved
            .get_or_try_init(|| self.provision())
            .await
            .copied()
    }

    /// The one-shot body behind [`ensure`](Self::ensure).
    async fn provision(&self) -> Result<Uuid, ServiceError> {
        let id = self.id();
        if !self.persistence.item_exists(id).await? {
            tokio::fs::create_dir_all(&self.path)
                .await
                .map_err(|e| ServiceError::backend(format!("create user root directory: {e}")))?;
            self.persistence
                .save_items(std::slice::from_ref(&self.entity(id)))
                .await?;
            tracing::info!(item_id = %id, path = %self.path.display(), "created the user root folder");
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::test_support::{fetch_item, item_repository_over, test_db};
    use ferrofin_traits::options::InternalItemsQuery;

    async fn store(tmp: &tempfile::TempDir) -> (ferrofin_db::Database, UserRootFolderStore) {
        let db = test_db().await;
        (db.clone(), store_over(&db, tmp))
    }

    fn store_over(db: &ferrofin_db::Database, tmp: &tempfile::TempDir) -> UserRootFolderStore {
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        UserRootFolderStore::new(persistence, mode, tmp.path().join("root/default"))
    }

    #[tokio::test]
    async fn ensure_creates_the_row_and_directory_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;
        let id = store.ensure().await.expect("ensure");
        assert!(tmp.path().join("root/default").is_dir());
        // Same id as Jellyfin derives for `root\default` under the data dir.
        let expected = item_type_lookup::derive_item_id_with(
            &IdDerivation::Jellyfin {
                program_data_path: Some("/x".to_owned()),
            },
            BaseItemKind::UserRootFolder,
            "/x/root/default",
        )
        .expect("derived");
        assert_eq!(
            id, expected,
            "the id is data-dir relative, so it matches any install"
        );
        let row = fetch_item(&db, id).await;
        assert_eq!(row.type_, "MediaBrowser.Controller.Entities.UserRootFolder");
        assert_eq!(row.name.as_deref(), Some(USER_ROOT_FOLDER_NAME));
        assert_eq!(row.parent_id, None, "the root has no parent");
        assert!(row.is_folder);
        assert_eq!(
            row.presentation_unique_key.as_deref(),
            Some(id.as_simple().to_string().as_str())
        );
        // A second call finds the row and writes nothing new.
        assert_eq!(store.ensure().await.expect("ensure again"), id);
        let roots = item_repository_over(db.clone())
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::UserRootFolder],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("list");
        assert_eq!(roots.len(), 1);
    }
}
