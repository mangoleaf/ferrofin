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
//! folder at `{DataPath}/playlists`, parents it to the root
//! (`folder.ParentId = rootFolder.Id`) and adds it as a virtual child, and
//! `UserRootFolder.GetChildCount(user)` counts it — so upstream's root has one
//! more child than the libraries alone. This store provisions it for the same
//! reason and at the same moment.
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

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemPersistenceService;

use crate::item_type_lookup::{self, IdDerivation};
use ferrofin_util::sort_name::create_sort_name;

/// The display name Jellyfin gives the user root folder.
pub const USER_ROOT_FOLDER_NAME: &str = "Media Folders";

/// The display name Jellyfin gives the playlists plugin folder
/// (`PlaylistsFolder.Name`).
pub const PLAYLISTS_FOLDER_NAME: &str = "Playlists";

/// Creates and resolves the one `UserRootFolder` row.
#[derive(Clone)]
pub struct UserRootFolderStore {
    persistence: Arc<dyn ItemPersistenceService>,
    id_derivation: IdDerivation,
    path: PathBuf,
    /// The database and the `{data}/playlists` directory, so [`ensure`] can
    /// provision the playlists plugin folder the way `CreateRootFolder()`
    /// does. `None` in unit tests that only want the root row.
    ///
    /// [`ensure`]: Self::ensure
    playlists: Option<(Database, PathBuf)>,
    /// Memoizes the resolved id, so the whole provisioning pass runs **once**
    /// per process — the port of C#'s `_userRootFolder` field, which
    /// `GetUserRootFolder()` builds under a lock and then hands out forever.
    ///
    /// Load-bearing for latency, not just tidiness: `ensure()` is on the hot
    /// path of `GET /Items/Root`, `GET /Library/MediaFolders`,
    /// `GET /Library/VirtualFolders` and the scan, and the playlists pass ends
    /// in a writer statement. Re-running that per request is exactly what once
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
            playlists: None,
            resolved: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Attaches the database and the `{data}/playlists` directory, so
    /// [`ensure`](Self::ensure) also provisions the playlists plugin folder as
    /// a child of the root. Called once by the composition root.
    #[must_use]
    pub fn with_playlists(mut self, db: Database, playlists_path: impl Into<PathBuf>) -> Self {
        self.playlists = Some((db, playlists_path.into()));
        self
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

    /// Ensures the directory, the row and the playlists plugin folder exist,
    /// returning the root row's id.
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
        self.ensure_playlists_folder(id).await?;
        Ok(id)
    }

    /// Creates — or repairs — the playlists plugin folder as a child of the
    /// root.
    ///
    /// Port of the `CreateRootFolder()` tail: the folder lives at
    /// `{DataPath}/playlists`, its id is
    /// `GetNewItemId(path, typeof(PlaylistsFolder))`, and if its parent is not
    /// the root the row is rewritten so that it is. That last arm is the whole
    /// point — `UserRootFolder.GetChildCount(user)` counts the root's
    /// children, so a parentless playlists row makes `GET /Items/Root` report
    /// one child too few.
    ///
    /// `ensure_container` is what does it, the same helper a created playlist
    /// or collection goes through: it matches the row **by path**, so a
    /// database that already carries one — Jellyfin's, or the
    /// `ManualPlaylistsFolder` an older Ferrofin wrote at the same directory —
    /// is adopted and parented rather than duplicated beside a second row.
    async fn ensure_playlists_folder(&self, root_id: Uuid) -> Result<(), ServiceError> {
        let Some((db, path)) = &self.playlists else {
            return Ok(());
        };
        // `PlaylistsFolder`, not `ManualPlaylistsFolder`: 10.11.8 has no class
        // of the latter name — it is only `PlaylistsFolder.GetClientTypeName()`
        // — so `GetNewItemId` hashes the FQN
        // `Emby.Server.Implementations.Playlists.PlaylistsFolder`. Deriving
        // from the other spelling gave the row an id no adopted Jellyfin
        // database recognises as its own.
        crate::item_persistence_service::ensure_container(
            db,
            BaseItemKind::PlaylistsFolder,
            PLAYLISTS_FOLDER_NAME,
            &path.to_string_lossy(),
            &self.id_derivation,
            Some(root_id),
        )
        .await?;
        Ok(())
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

    /// The playlists directory, and a store that provisions its folder.
    fn store_with_playlists(
        db: &ferrofin_db::Database,
        tmp: &tempfile::TempDir,
    ) -> (PathBuf, UserRootFolderStore) {
        let playlists = tmp.path().join("data/playlists");
        let s = store_over(db, tmp).with_playlists(db.clone(), playlists.clone());
        (playlists, s)
    }

    /// The id Jellyfin derives for a playlists folder of `kind` at `path`.
    fn playlists_id(tmp: &tempfile::TempDir, path: &Path, kind: BaseItemKind) -> Uuid {
        item_type_lookup::derive_item_id_with(
            &IdDerivation::Jellyfin {
                program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
            },
            kind,
            &path.to_string_lossy(),
        )
        .expect("derived")
    }

    /// `CreateRootFolder()` parents the playlists plugin folder to the root and
    /// `UserRootFolder.GetChildCount(user)` counts it, so a parentless row makes
    /// `GET /Items/Root` report one child too few.
    #[tokio::test]
    async fn ensure_parents_the_playlists_folder_to_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = test_db().await;
        let (playlists, store) = store_with_playlists(&db, &tmp);

        let root = store.ensure().await.expect("ensure");

        assert!(playlists.is_dir(), "the backing directory is created");
        // Found at the id Jellyfin derives for the same directory — under
        // `PlaylistsFolder`, the real 10.11.8 class — so an adopted database
        // recognises the row as its own instead of gaining a second one.
        let expected = playlists_id(&tmp, &playlists, BaseItemKind::PlaylistsFolder);
        let row = fetch_item(&db, expected).await;
        assert_eq!(
            row.parent_id.as_deref(),
            Some(guid_to_db(root).as_str()),
            "the playlists folder must hang off the user root"
        );
        assert_eq!(
            row.type_, "Emby.Server.Implementations.Playlists.PlaylistsFolder",
            "10.11.8 has no `ManualPlaylistsFolder` class — that name is only \
             `PlaylistsFolder.GetClientTypeName()`"
        );
        assert_eq!(row.name.as_deref(), Some(PLAYLISTS_FOLDER_NAME));
        assert!(row.is_folder);
        assert_eq!(
            row.path.as_deref(),
            Some(playlists.to_string_lossy().as_ref())
        );
    }

    /// The direct port of `LibraryManager.CreateRootFolder()`'s repair arm
    /// (`if (!folder.ParentId.Equals(rootFolder.Id)) { folder.ParentId = … }`):
    /// a database that already carries a parentless playlists row — every
    /// server an older Ferrofin provisioned — is fixed in place, not duplicated
    /// beside a second row.
    #[tokio::test]
    async fn ensure_repairs_a_parentless_playlists_row_it_did_not_create() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = test_db().await;
        let playlists = tmp.path().join("data/playlists");
        // What the old provisioner left behind: a row at the same directory
        // under the OTHER spelling, and with no parent. Seeded through the same
        // helper that created it, so the fixture cannot drift from reality.
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let legacy = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::ManualPlaylistsFolder,
            PLAYLISTS_FOLDER_NAME,
            &playlists.to_string_lossy(),
            &mode,
            None,
        )
        .await
        .expect("seed the legacy row")
        .expect("an id");
        assert_eq!(
            legacy,
            playlists_id(&tmp, &playlists, BaseItemKind::ManualPlaylistsFolder)
        );
        assert_eq!(
            fetch_item(&db, legacy).await.parent_id,
            None,
            "starts orphaned"
        );

        let (_, store) = store_with_playlists(&db, &tmp);
        let root = store.ensure().await.expect("ensure");

        assert_eq!(
            fetch_item(&db, legacy).await.parent_id.as_deref(),
            Some(guid_to_db(root).as_str()),
            "the parentless row is repaired in place"
        );
        // …and NOT by creating a second row beside it at the other derivation.
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        assert!(
            !persistence
                .item_exists(playlists_id(
                    &tmp,
                    &playlists,
                    BaseItemKind::PlaylistsFolder
                ))
                .await
                .expect("exists"),
            "the existing row is adopted, not duplicated"
        );
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
