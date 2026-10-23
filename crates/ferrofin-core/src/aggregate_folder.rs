//! [`AggregateFolderStore`] — materializes the `AggregateFolder` row, the
//! *physical* root of the library tree, and the plugin folders that hang off
//! it.
//!
//! Port of `LibraryManager.CreateRootFolder()`
//! (`Emby.Server.Implementations/Library/LibraryManager.cs:838-888`). Upstream:
//!
//! 1. resolves (or creates) an `AggregateFolder` at
//!    `ApplicationPaths.RootFolderPath` (`{program data}/root`) whose id is
//!    `GetNewItemId(rootFolderPath, typeof(AggregateFolder))`;
//! 2. creates `{program data}/data/playlists` and a `PlaylistsFolder` for it;
//! 3. sets `folder.ParentId = rootFolder.Id` and calls
//!    `rootFolder.AddVirtualChild(folder)`.
//!
//! That third step is the whole point: the playlists folder is a child of the
//! **aggregate**, not of the `UserRootFolder`, which is why Jellyfin answers
//! `GET /Items/{playlistsId}/Ancestors` with `[AggregateFolder]`. The
//! `UserRootFolder` still *lists* it, because
//! `UserRootFolder.GetEligibleChildrenForRecursiveChildren`
//! (`UserRootFolder.cs:96-102`) concatenates
//! `LibraryManager.RootFolder.VirtualChildren` onto its own children — the
//! concat this crate ports in [`crate::item_repository`] (browse) and
//! [`crate::item_count_service`] (`ChildCount`).
//!
//! Everything here runs **once**, from the composition root, never from a
//! request path: it is a small pile of writes and existence probes, and a
//! previous version of this provisioning ran per request and serialized
//! `GET /Library/MediaFolders` to 1355 ms p50 with 31% errors.

use std::path::{Path, PathBuf};

use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use uuid::Uuid;

use crate::item_persistence_service::row_exists;
use crate::item_type_lookup::{self, IdDerivation};

/// The display name Jellyfin gives the aggregate (physical) root folder — the
/// directory's own name, which `UserRootFolder` renames for itself but
/// `AggregateFolder` keeps.
pub const AGGREGATE_FOLDER_NAME: &str = "root";

/// The ids of the two root rows, resolved once at startup and injected into the
/// query and count paths as plain constants (no lookup, no query).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFolderIds {
    /// `GetUserRootFolder()` — `{program data}/root/default`.
    pub user_root: Uuid,
    /// `CreateRootFolder()` — `{program data}/root`.
    pub aggregate: Uuid,
}

/// Creates and repairs the `AggregateFolder` row and its virtual children.
#[derive(Clone)]
pub struct AggregateFolderStore {
    db: Database,
    id_derivation: IdDerivation,
    root_folder_path: PathBuf,
    data_path: PathBuf,
}

impl std::fmt::Debug for AggregateFolderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateFolderStore")
            .field("root_folder_path", &self.root_folder_path)
            .finish_non_exhaustive()
    }
}

impl AggregateFolderStore {
    /// Creates the store over the database, the database's id-derivation mode,
    /// the physical root directory (`{program data}/root`) and the data
    /// directory (whose `playlists` child is the one virtual child 10.11.8
    /// registers).
    #[must_use]
    pub fn new(
        db: Database,
        id_derivation: IdDerivation,
        root_folder_path: impl Into<PathBuf>,
        data_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            db,
            id_derivation,
            root_folder_path: root_folder_path.into(),
            data_path: data_path.into(),
        }
    }

    /// The directory the aggregate row represents.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root_folder_path
    }

    /// The deterministic `AggregateFolder` id.
    #[must_use]
    pub fn id(&self) -> Uuid {
        item_type_lookup::aggregate_folder_id(
            &self.id_derivation,
            &self.root_folder_path.to_string_lossy(),
        )
        .unwrap_or_default()
    }

    /// The deterministic `UserRootFolder` id (`{root}/default`), the other half
    /// of [`RootFolderIds`].
    #[must_use]
    pub fn user_root_id(&self) -> Uuid {
        item_type_lookup::user_root_folder_id(
            &self.id_derivation,
            &self.root_folder_path.join("default").to_string_lossy(),
        )
        .unwrap_or_default()
    }

    /// The `{data}/playlists` directory the one virtual child lives at.
    #[must_use]
    pub fn playlists_path(&self) -> PathBuf {
        self.data_path.join("playlists")
    }

    /// The deterministic `PlaylistsFolder` id — 10.11.8's stored type name, so
    /// this is the id an adopted database already carries.
    #[must_use]
    pub fn playlists_id(&self) -> Option<Uuid> {
        item_type_lookup::derive_item_id_with(
            &self.id_derivation,
            BaseItemKind::PlaylistsFolder,
            &self.playlists_path().to_string_lossy(),
        )
    }

    /// The id a pre-`PlaylistsFolder` Ferrofin wrote for the same directory.
    ///
    /// Ferrofin used to persist the folder as
    /// `Emby.Server.Implementations.Playlists.ManualPlaylistsFolder`, a class
    /// 10.11.8 does not have (that spelling is only
    /// `PlaylistsFolder.GetClientTypeName()`). Because `GetNewItemIdInternal`
    /// hashes `type.FullName + key`, the fabricated name produced a different
    /// id for the same path, so an adopted Jellyfin database would grow a
    /// second playlists folder beside its own. [`Self::ensure`] rewrites such a
    /// row onto the correct identity, once.
    #[must_use]
    pub fn legacy_playlists_id(&self) -> Option<Uuid> {
        item_type_lookup::derive_item_id_with(
            &self.id_derivation,
            BaseItemKind::ManualPlaylistsFolder,
            &self.playlists_path().to_string_lossy(),
        )
    }

    /// Provisions the aggregate root and re-parents its virtual children,
    /// returning both root ids.
    ///
    /// Idempotent, and meant to be called **once per process** from the
    /// composition root — it is startup work, not request work.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory cannot be created or a row cannot be
    /// written.
    pub async fn ensure(&self) -> Result<RootFolderIds, ServiceError> {
        let aggregate = self.id();
        let user_root = self.user_root_id();
        if !row_exists(&self.db, aggregate).await? {
            if let Err(e) = tokio::fs::create_dir_all(&self.root_folder_path).await {
                tracing::warn!(path = %self.root_folder_path.display(), %e,
                    "could not create the physical root directory");
            }
            self.insert_aggregate(aggregate).await?;
            tracing::info!(item_id = %aggregate, path = %self.root_folder_path.display(),
                "created the aggregate (physical) root folder");
        }
        self.repair_playlists_folder(aggregate, user_root).await?;
        Ok(RootFolderIds {
            user_root,
            aggregate,
        })
    }

    /// The aggregate row exactly as a 10.11.8 database carries it: `Name`,
    /// `SortName` and `CleanName` all `root`, no parent, no top parent, the
    /// presentation key every folder gets.
    async fn insert_aggregate(&self, id: Uuid) -> Result<(), ServiceError> {
        crate::item_persistence_service::insert_root_folder(
            &self.db,
            id,
            BaseItemKind::AggregateFolder,
            AGGREGATE_FOLDER_NAME,
            &self.root_folder_path.to_string_lossy(),
        )
        .await
    }

    /// `folder.ParentId = rootFolder.Id` for the playlists folder, plus the
    /// one-shot rewrite of a legacy `ManualPlaylistsFolder` row onto 10.11.8's
    /// identity, and the folder's row itself when there is none.
    ///
    /// Each is a write to one row, run once per process.
    async fn repair_playlists_folder(
        &self,
        aggregate: Uuid,
        user_root: Uuid,
    ) -> Result<(), ServiceError> {
        let (Some(correct), Some(legacy)) = (self.playlists_id(), self.legacy_playlists_id())
        else {
            return Ok(());
        };
        if legacy != correct
            && row_exists(&self.db, legacy).await?
            && !row_exists(&self.db, correct).await?
        {
            crate::item_persistence_service::rekey_container(
                &self.db,
                legacy,
                correct,
                BaseItemKind::PlaylistsFolder,
            )
            .await?;
            tracing::info!(from = %legacy, to = %correct,
                "rewrote the playlists folder onto Jellyfin's PlaylistsFolder identity");
        }
        // `CreateRootFolder` saves the folder whenever its row is missing, so
        // a fresh server has it — and a user who can see a playlist the
        // library scan found has a Playlists view — before any playlist is
        // made through the API. A row Jellyfin wrote with a moved data
        // directory is found by its `%AppDataPath%` path and kept.
        let path = self.playlists_path();
        if !row_exists(&self.db, correct).await?
            && crate::item_persistence_service::container_at(&self.db, &path.to_string_lossy())
                .await?
                .is_none()
        {
            if let Err(e) = tokio::fs::create_dir_all(&path).await {
                tracing::warn!(path = %path.display(), %e,
                    "could not create the playlists directory");
            }
            let dir = tokio::fs::metadata(&path).await.ok();
            let row = playlists_folder_row(correct, &path, Some(aggregate), dir.as_ref());
            crate::item_persistence_service::FerrofinItemPersistenceService::new(self.db.clone())
                .save_items(std::slice::from_ref(&row))
                .await?;
            tracing::info!(item_id = %correct, path = %path.display(),
                "created the playlists folder");
        }
        // A row A4 parented straight to the `UserRootFolder` (or one left
        // parentless) moves onto the aggregate, where `CreateRootFolder` puts
        // it. Scoped to the two states this server can have produced, so a row
        // deliberately parented elsewhere is never moved.
        crate::item_persistence_service::reparent_virtual_child(
            &self.db, correct, aggregate, user_root,
        )
        .await
    }
}

/// The `PlaylistsFolder` row `CreateRootFolder` saves for `path` under
/// `parent` (the aggregate): `Name` "Playlists", its own top parent, created
/// and modified when `dir` (the directory's metadata) says, else now.
///
/// The id goes in through `guid_to_db`, never `to_string()`: Jellyfin stores
/// GUID columns uppercase-hyphenated and `BaseItems."Id"` has no `COLLATE
/// NOCASE`, so a lowercase id is another row to SQLite. Writing one once made
/// the existence probe before this miss the row it had just written, and
/// every `GET /Library/MediaFolders` re-ran the provisioning through the
/// single writer connection (1355 ms p50 and 31% errors under load, against
/// Jellyfin's 0.23 ms), with a lowercase `Id` in the response.
pub(crate) fn playlists_folder_row(
    id: Uuid,
    path: &Path,
    parent: Option<Uuid>,
    dir: Option<&std::fs::Metadata>,
) -> ferrofin_db::entities::base_items::BaseItemEntity {
    let name = crate::user_view_manager::PLAYLISTS_FOLDER_NAME;
    // `CreateRootFolder` dates the folder from the directory it just made,
    // and the `UpdateToRepositoryAsync` that inserts it stamps the save.
    let now = chrono::Utc::now();
    let (created, modified) = dir.map_or((now, now), crate::library_scan::created_and_modified);
    ferrofin_db::entities::base_items::BaseItemEntity {
        id: ferrofin_db::store::guid_to_db(id),
        type_: item_type_lookup::stored_type_name(BaseItemKind::PlaylistsFolder)
            .unwrap_or_default()
            .to_owned(),
        name: Some(name.to_owned()),
        sort_name: Some(ferrofin_util::sort_name::create_sort_name(name)),
        path: Some(path.to_string_lossy().into_owned()),
        parent_id: parent.map(ferrofin_db::store::guid_to_db),
        top_parent_id: Some(ferrofin_db::store::guid_to_db(id)),
        is_folder: true,
        date_created: Some(created),
        date_modified: Some(modified),
        date_last_saved: Some(now),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fetch_item, seed_folder_item, test_db};
    use ferrofin_db::store::guid_to_db as db_guid;

    fn store(db: &Database, tmp: &tempfile::TempDir) -> AggregateFolderStore {
        AggregateFolderStore::new(
            db.clone(),
            IdDerivation::Jellyfin {
                program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
            },
            tmp.path().join("root"),
            tmp.path().join("data"),
        )
    }

    /// The row `CreateRootFolder()` materializes, with the identity a real
    /// 10.11.8 database carries.
    #[tokio::test]
    async fn ensure_creates_the_aggregate_root_once() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);

        let ids = store.ensure().await.expect("ensure");

        assert_eq!(
            ids.aggregate,
            item_type_lookup::aggregate_folder_id(
                &IdDerivation::Jellyfin {
                    program_data_path: Some("/config".to_owned())
                },
                "/config/root",
            )
            .expect("derived"),
            "the id is data-dir relative, so it is Jellyfin's on any install"
        );
        let row = fetch_item(&db, ids.aggregate).await;
        assert_eq!(
            row.type_,
            "MediaBrowser.Controller.Entities.AggregateFolder"
        );
        assert_eq!(row.name.as_deref(), Some("root"));
        assert_eq!(row.parent_id, None, "the physical root has no parent");
        assert_eq!(row.top_parent_id, None);
        assert!(row.is_folder);
        assert_eq!(
            row.path.as_deref(),
            Some(tmp.path().join("root").to_string_lossy().as_ref())
        );
        assert_eq!(
            row.presentation_unique_key.as_deref(),
            Some(ids.aggregate.as_simple().to_string().as_str())
        );
        // Idempotent: a second call finds the row and changes nothing.
        assert_eq!(store.ensure().await.expect("again"), ids);
    }

    /// `CreateRootFolder` saves the playlists folder on every start where it
    /// is missing: under the aggregate, at `{data}/playlists`, with
    /// Jellyfin's `PlaylistsFolder` identity.
    #[tokio::test]
    async fn ensure_creates_the_playlists_folder() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);

        let ids = store.ensure().await.expect("ensure");

        let id = store.playlists_id().expect("id");
        let row = fetch_item(&db, id).await;
        assert_eq!(
            row.type_,
            "Emby.Server.Implementations.Playlists.PlaylistsFolder"
        );
        assert_eq!(row.name.as_deref(), Some("Playlists"));
        assert_eq!(row.parent_id, Some(db_guid(ids.aggregate)));
        assert_eq!(row.top_parent_id, Some(db_guid(id)), "its own top parent");
        assert_eq!(
            row.path.as_deref(),
            Some(store.playlists_path().to_string_lossy().as_ref())
        );
        assert!(store.playlists_path().is_dir());
        assert_eq!(store.ensure().await.expect("again"), ids);
    }

    /// The folder carries `CreateRootFolder`'s dates: created and modified
    /// from its directory (`info.CreationTimeUtc`, `info.LastWriteTimeUtc`),
    /// and saved now (the `UpdateToRepositoryAsync` that inserts it).
    #[tokio::test]
    async fn the_playlists_folder_is_dated_from_its_directory() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let dir = store.playlists_path();
        std::fs::create_dir_all(&dir).expect("dir");
        let written = chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .expect("date")
            .with_timezone(&chrono::Utc);
        std::fs::File::open(&dir)
            .expect("open dir")
            .set_modified(written.into())
            .expect("set mtime");

        let before = chrono::Utc::now();
        store.ensure().await.expect("ensure");
        let after = chrono::Utc::now();

        let row = fetch_item(&db, store.playlists_id().expect("id")).await;
        assert_eq!(row.date_modified, Some(written), "LastWriteTimeUtc");
        assert_eq!(
            row.date_created,
            Some(written),
            "CreationTimeUtc: the older of ctime and mtime"
        );
        let saved = row.date_last_saved.expect("saved");
        assert!((before..=after).contains(&saved), "{saved}");
    }

    /// A playlists folder an earlier Ferrofin hung off the `UserRootFolder`
    /// moves onto the aggregate — `folder.ParentId = rootFolder.Id`.
    #[tokio::test]
    async fn ensure_reparents_a_user_root_playlists_folder() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let user_root = store.user_root_id();
        let playlists = store.playlists_id().expect("playlists id");
        seed_folder_item(
            &db,
            user_root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
            None,
        )
        .await;
        seed_folder_item(
            &db,
            playlists,
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(user_root),
        )
        .await;

        let ids = store.ensure().await.expect("ensure");

        let row = fetch_item(&db, playlists).await;
        assert_eq!(
            row.parent_id.as_deref(),
            Some(db_guid(ids.aggregate).as_str()),
            "the playlists folder is a child of the AggregateFolder, not the user root"
        );
        assert_eq!(
            row.top_parent_id.as_deref(),
            Some(db_guid(playlists).as_str()),
            "…and its own top parent, as 10.11.8 stores it"
        );
    }

    /// The legacy `ManualPlaylistsFolder` row (a class 10.11.8 does not have)
    /// is rewritten onto `PlaylistsFolder`'s id, carrying its playlists with it.
    #[tokio::test]
    async fn ensure_rekeys_a_legacy_manual_playlists_folder() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let legacy = store.legacy_playlists_id().expect("legacy id");
        let correct = store.playlists_id().expect("correct id");
        assert_ne!(legacy, correct, "the two type names hash differently");
        let playlist = Uuid::from_u128(0x9001);
        seed_folder_item(
            &db,
            legacy,
            BaseItemKind::ManualPlaylistsFolder,
            "Playlists",
            None,
        )
        .await;
        seed_folder_item(
            &db,
            playlist,
            BaseItemKind::Playlist,
            "My Playlist",
            Some(legacy),
        )
        .await;

        let ids = store.ensure().await.expect("ensure");

        let row = fetch_item(&db, correct).await;
        assert_eq!(
            row.type_, "Emby.Server.Implementations.Playlists.PlaylistsFolder",
            "10.11.8's stored type name"
        );
        assert_eq!(row.name.as_deref(), Some("Playlists"));
        assert_eq!(
            row.parent_id.as_deref(),
            Some(db_guid(ids.aggregate).as_str())
        );
        let moved = fetch_item(&db, playlist).await;
        assert_eq!(
            moved.parent_id.as_deref(),
            Some(db_guid(correct).as_str()),
            "the playlists move with the folder rather than being cascaded away"
        );
        assert!(
            !row_exists(&db, legacy).await.expect("probe"),
            "the legacy row is gone, not duplicated"
        );
    }

    /// A database that already carries Jellyfin's row is left alone — the
    /// rekey must never create a second folder beside it.
    #[tokio::test]
    async fn ensure_does_not_rekey_when_the_correct_row_already_exists() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let legacy = store.legacy_playlists_id().expect("legacy id");
        let correct = store.playlists_id().expect("correct id");
        seed_folder_item(&db, legacy, BaseItemKind::ManualPlaylistsFolder, "P", None).await;
        seed_folder_item(
            &db,
            correct,
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            None,
        )
        .await;

        store.ensure().await.expect("ensure");

        assert_eq!(fetch_item(&db, legacy).await.id, db_guid(legacy));
        assert_eq!(fetch_item(&db, correct).await.id, db_guid(correct));
    }
}
