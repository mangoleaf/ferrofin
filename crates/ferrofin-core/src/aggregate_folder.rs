//! [`AggregateFolderStore`] — materializes the `AggregateFolder` row, the
//! *physical* root of the library tree, and the plugin folders that hang off
//! it.
//!
//! Port of `LibraryManager.CreateRootFolder()`
//! (v12.2 `Emby.Server.Implementations/Library/LibraryManager.cs:1101-1150`).
//! Upstream:
//!
//! 1. resolves (or creates) an `AggregateFolder` at
//!    `ApplicationPaths.RootFolderPath` (`{program data}/root`) whose id is
//!    `GetNewItemId(rootFolderPath, typeof(AggregateFolder))`, resetting its
//!    `Path` when the program data directory moved;
//! 2. creates `{program data}/data/playlists` and a `PlaylistsFolder` for it,
//!    keeping the row stored under that folder's id only while it is at that
//!    directory, and saving the folder at the directory otherwise;
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

use crate::item_persistence_service::{container_row, row_exists};
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

    /// Provisions the aggregate root and the playlists folder, moves both to
    /// this server's directories when the stored rows are elsewhere, and
    /// re-parents the aggregate's virtual children, returning both root ids.
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
        let root_path = self.root_folder_path.to_string_lossy();
        match container_row(&self.db, aggregate).await? {
            None => {
                if let Err(e) = tokio::fs::create_dir_all(&self.root_folder_path).await {
                    tracing::warn!(path = %self.root_folder_path.display(), %e,
                        "could not create the physical root directory");
                }
                self.insert_aggregate(aggregate).await?;
                tracing::info!(item_id = %aggregate, path = %root_path,
                    "created the aggregate (physical) root folder");
            }
            // "In case program data folder was moved": the stored path,
            // expanded, against this server's, compared ordinally
            // (LibraryManager.cs:1109-1113). The id is derived from the
            // program-data-relative key, so it survives the move; the path
            // does not.
            Some(row)
                if row.path.as_deref().map(|p| self.expand(p)).as_deref()
                    != Some(root_path.as_ref()) =>
            {
                crate::item_persistence_service::repath_root_folder(
                    &self.db, aggregate, &root_path,
                )
                .await?;
                tracing::info!(item_id = %aggregate, from = row.path.as_deref().unwrap_or_default(),
                    path = %root_path, "reset the root folder's path");
            }
            Some(_) => {}
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

    /// A path as `BaseItemRepository.Map` reads it back: Jellyfin's
    /// `%AppDataPath%` token, in any case, replaced by this server's data
    /// directory (`ExpandVirtualPath`, ApplicationHost.cs:286-297). Only the
    /// data token: neither folder this store keeps lives under the metadata
    /// directory.
    fn expand(&self, stored: &str) -> String {
        ferrofin_util::string_extensions::replace_ignore_ascii_case(
            stored,
            crate::app_paths::FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
            &self.data_path.to_string_lossy(),
        )
    }

    /// `folder.ParentId = rootFolder.Id` for the playlists folder, plus the
    /// one-shot rewrite of a legacy `ManualPlaylistsFolder` row onto 10.11.8's
    /// identity, the folder's row itself when there is none, its move to the
    /// data directory when the row is somewhere else, the retype of a folder
    /// kept at the directory under an id of its own, and the playlists an
    /// older Ferrofin left without a parent put into the folder.
    ///
    /// Each is idempotent, run once per process.
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
        // made through the API. This is the one place that provisions it:
        // upstream runs `CreateRootFolder` once per process
        // (`LibraryManager.RootFolder`, LibraryManager.cs:226), and no request
        // path here makes the row either.
        //
        // Upstream looks the row up by id (`GetItemById(folder.Id)`,
        // LibraryManager.cs:1132) and keeps it only while its path, expanded,
        // is the folder's (`OrdinalIgnoreCase`, :1134); otherwise it saves the
        // folder at its directory under the same id (:1139-1144). That id is
        // derived from the program-data-relative key `data\playlists`, so a
        // database adopted from a Jellyfin whose data directory was elsewhere,
        // or one whose data directory moved, still carries the row under this
        // id, and Jellyfin never writes it under another. The path fallback
        // (`container_at` accepts the resolved path and the `%AppDataPath%`
        // token) is defensive: a row already at the folder's path under some
        // other id is kept, rather than given a second folder beside it that
        // would split the playlists and list two Playlists views.
        let path = self.playlists_path();
        let path_str = path.to_string_lossy();
        match container_row(&self.db, correct).await? {
            Some(row) => {
                let at_folder = row.path.as_deref().is_some_and(|stored| {
                    ferrofin_util::string_extensions::equals_ordinal_ignore_case(
                        &self.expand(stored),
                        &path_str,
                    )
                });
                if !at_folder {
                    self.repath_playlists_folder(correct, aggregate, row.path.as_deref())
                        .await?;
                }
            }
            None => match crate::item_persistence_service::container_at(&self.db, &path_str).await?
            {
                None => {
                    if let Err(e) = tokio::fs::create_dir_all(&path).await {
                        tracing::warn!(path = %path.display(), %e,
                            "could not create the playlists directory");
                    }
                    let dir = tokio::fs::metadata(&path).await.ok();
                    let row = playlists_folder_row(correct, &path, Some(aggregate), dir.as_ref());
                    crate::item_persistence_service::FerrofinItemPersistenceService::new(
                        self.db.clone(),
                    )
                    .save_items(std::slice::from_ref(&row))
                    .await?;
                    tracing::info!(item_id = %correct, path = %path.display(),
                        "created the playlists folder");
                }
                // A playlists folder kept at the folder's path is the folder in
                // use, so it is made what a create looks up by type (gl-921):
                // a `PlaylistsFolder` (an earlier Ferrofin stored it as
                // `ManualPlaylistsFolder`) under the aggregate. Its id is its
                // own, not derived from its type, so it stays. A row of any
                // other type there is not this server's to change.
                Some(found) => {
                    if crate::item_persistence_service::retype_container(
                        &self.db,
                        found,
                        BaseItemKind::ManualPlaylistsFolder,
                        BaseItemKind::PlaylistsFolder,
                    )
                    .await?
                    {
                        crate::item_persistence_service::reparent_virtual_child(
                            &self.db, found, aggregate, user_root,
                        )
                        .await?;
                    } else {
                        tracing::warn!(item_id = %found, path = %path.display(),
                            "a row that is not a playlists folder holds the playlists directory; \
                             no playlists folder is provisioned");
                    }
                }
            },
        }
        // A row A4 parented straight to the `UserRootFolder` (or one left
        // parentless) moves onto the aggregate, where `CreateRootFolder` puts
        // it. Scoped to the two states this server can have produced, so a row
        // deliberately parented elsewhere is never moved.
        crate::item_persistence_service::reparent_virtual_child(
            &self.db, correct, aggregate, user_root,
        )
        .await?;
        // Playlists an older Ferrofin saved with neither parent nor top parent
        // go into the folder a create finds. Startup's repair, once per
        // process: upstream's lookup (`GetPlaylistsFolder`) writes nothing.
        if let Some(folder) = crate::item_persistence_service::playlists_folder_by_type(
            &self.db,
            Some(aggregate),
            Some(user_root),
        )
        .await?
        {
            crate::item_persistence_service::adopt_orphans(
                &self.db,
                BaseItemKind::Playlist,
                folder,
            )
            .await?;
        }
        Ok(())
    }

    /// Saves the playlists folder `id`, stored at `stored`, at this server's
    /// `{data}/playlists` under the aggregate, dated from that directory as a
    /// new folder is: what `CreateRootFolder` does when the data directory
    /// moved (LibraryManager.cs:1116-1144).
    ///
    /// Not onto a directory another row already holds: that would list two
    /// Playlists folders at one path, so both rows are left as they are.
    async fn repath_playlists_folder(
        &self,
        id: Uuid,
        aggregate: Uuid,
        stored: Option<&str>,
    ) -> Result<(), ServiceError> {
        let path = self.playlists_path();
        let path_str = path.to_string_lossy();
        if let Some(other) =
            crate::item_persistence_service::container_at(&self.db, &path_str).await?
        {
            tracing::warn!(item_id = %id, other = %other, from = stored.unwrap_or_default(),
                path = %path_str,
                "playlists folder left at its old path: another row holds its directory");
            return Ok(());
        }
        if let Err(e) = tokio::fs::create_dir_all(&path).await {
            tracing::warn!(path = %path_str, %e, "could not create the playlists directory");
        }
        let dir = tokio::fs::metadata(&path).await.ok();
        let now = chrono::Utc::now();
        let dates = dir
            .as_ref()
            .map_or((now, now), crate::library_scan::created_and_modified);
        crate::item_persistence_service::repath_virtual_child(
            &self.db, id, &path_str, aggregate, dates, now,
        )
        .await?;
        tracing::info!(item_id = %id, from = stored.unwrap_or_default(), path = %path_str,
            "moved the playlists folder to the data directory");
        Ok(())
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
    use crate::test_support::{fetch_item, seed_folder_item, set_item_path, test_db};
    use ferrofin_db::store::guid_to_db as db_guid;
    use rstest::rstest;

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

    /// gl-343: a playlists folder already at the folder's path under another
    /// id, stored resolved or with Jellyfin's `%AppDataPath%` token, is kept
    /// as it is: startup provisions no second folder beside it.
    #[rstest]
    #[case::resolved_path(false)]
    #[case::app_data_token(true)]
    #[tokio::test]
    async fn ensure_keeps_a_playlists_folder_found_by_path(#[case] token: bool) {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let db = test_db().await;
            let tmp = tempfile::tempdir().expect("tempdir");
            let store = store(&db, &tmp);
            let stored_path = if token {
                "%AppDataPath%/playlists".to_owned()
            } else {
                store.playlists_path().to_string_lossy().into_owned()
            };
            let foreign = Uuid::from_u128(0xADA);
            seed_folder_item(
                &db,
                foreign,
                BaseItemKind::PlaylistsFolder,
                "Playlists",
                None,
            )
            .await;
            set_item_path(&db, foreign, &stored_path).await;

            store.ensure().await.expect("ensure");
            store.ensure().await.expect("again");

            let derived = store.playlists_id().expect("id");
            assert_ne!(derived, foreign);
            assert!(
                !row_exists(&db, derived).await.expect("probe"),
                "no second playlists folder under the derived id"
            );
            let kept = fetch_item(&db, foreign).await;
            assert_eq!(
                kept.path.as_deref(),
                Some(stored_path.as_str()),
                "the row found by path is kept at its path"
            );
            assert_eq!(
                kept.parent_id,
                Some(db_guid(store.id())),
                "and hangs off the aggregate, where a create looks for it"
            );
        })
        .await
        .expect("the test finishes");
    }

    /// gl-921: a create finds the playlists folder by its type under the
    /// aggregate, so a row an earlier Ferrofin kept at the folder's path as a
    /// `ManualPlaylistsFolder` under an id of its own becomes the
    /// `PlaylistsFolder` child of the aggregate, keeping its id.
    #[tokio::test]
    async fn ensure_makes_a_manual_playlists_folder_at_the_path_a_playlists_folder() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let foreign = Uuid::from_u128(0xF0DE);
        seed_folder_item(
            &db,
            foreign,
            BaseItemKind::ManualPlaylistsFolder,
            "Playlists",
            None,
        )
        .await;
        set_item_path(&db, foreign, &store.playlists_path().to_string_lossy()).await;

        store.ensure().await.expect("ensure");

        let kept = fetch_item(&db, foreign).await;
        assert_eq!(
            Some(kept.type_.as_str()),
            item_type_lookup::stored_type_name(BaseItemKind::PlaylistsFolder)
        );
        assert_eq!(kept.parent_id, Some(db_guid(store.id())));
        assert_eq!(
            crate::item_persistence_service::playlists_folder_by_type(
                &db,
                Some(store.id()),
                Some(store.user_root_id()),
            )
            .await
            .expect("lookup"),
            Some(foreign)
        );
    }

    /// A playlist an older Ferrofin saved with neither parent nor top parent
    /// is put into the playlists folder at startup, once: the create path's
    /// lookup writes nothing, as `GetPlaylistsFolder` does not. A playlist
    /// that has a parent (every one an adopted Jellyfin database carries)
    /// stays where it is.
    #[tokio::test]
    async fn ensure_puts_orphaned_playlists_into_the_playlists_folder() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let orphan = Uuid::from_u128(0x0A9);
        seed_folder_item(&db, orphan, BaseItemKind::Playlist, "Orphan", None).await;
        let elsewhere = Uuid::from_u128(0xE15);
        seed_folder_item(&db, elsewhere, BaseItemKind::Folder, "Elsewhere", None).await;
        let placed = Uuid::from_u128(0x91A);
        seed_folder_item(
            &db,
            placed,
            BaseItemKind::Playlist,
            "Placed",
            Some(elsewhere),
        )
        .await;

        store.ensure().await.expect("ensure");

        let playlists = db_guid(store.playlists_id().expect("playlists id"));
        let row = fetch_item(&db, orphan).await;
        assert_eq!(row.parent_id.as_deref(), Some(playlists.as_str()));
        assert_eq!(row.top_parent_id.as_deref(), Some(playlists.as_str()));
        assert_eq!(
            fetch_item(&db, placed).await.parent_id,
            Some(db_guid(elsewhere)),
            "a playlist with a parent is left alone"
        );
    }

    /// A row that is not a playlists folder at the folder's directory (here
    /// a library) is not this server's to change: startup leaves its type and
    /// parent alone and provisions no second folder beside it.
    #[tokio::test]
    async fn ensure_leaves_a_foreign_row_at_the_playlists_path_alone() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let library = Uuid::from_u128(0xC0F);
        seed_folder_item(&db, library, BaseItemKind::CollectionFolder, "Lib", None).await;
        set_item_path(&db, library, &store.playlists_path().to_string_lossy()).await;

        store.ensure().await.expect("ensure");

        let row = fetch_item(&db, library).await;
        assert_eq!(
            Some(row.type_.as_str()),
            item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder)
        );
        assert_eq!(row.parent_id, None);
        assert!(
            !row_exists(&db, store.playlists_id().expect("id"))
                .await
                .expect("probe")
        );
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

    /// gl-910: a data directory moved under an existing database leaves both
    /// rows at the old paths, under the same ids (both derive from
    /// program-data-relative keys). `CreateRootFolder` resets the root
    /// folder's path and saves the playlists folder at the new one, under the
    /// aggregate and dated from its directory (LibraryManager.cs:1109-1144),
    /// and the next start finds nothing to do.
    #[tokio::test]
    async fn ensure_repaths_the_root_and_playlists_folders_after_a_move() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let user_root = store.user_root_id();
        let aggregate = store.id();
        let playlists = store.playlists_id().expect("playlists id");
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        set_item_path(&db, aggregate, "/moved-from/root").await;
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
        set_item_path(&db, playlists, "/moved-from/data/playlists").await;
        let dir = store.playlists_path();
        std::fs::create_dir_all(&dir).expect("dir");
        let written = chrono::DateTime::parse_from_rfc3339("2021-02-03T04:05:06Z")
            .expect("date")
            .with_timezone(&chrono::Utc);
        std::fs::File::open(&dir)
            .expect("open dir")
            .set_modified(written.into())
            .expect("set mtime");

        let before = chrono::Utc::now();
        let ids = store.ensure().await.expect("ensure");
        let after = chrono::Utc::now();

        assert_eq!(ids.aggregate, aggregate);
        assert_eq!(
            fetch_item(&db, aggregate).await.path.as_deref(),
            Some(store.path().to_string_lossy().as_ref()),
            "Resetting root folder path"
        );
        let row = fetch_item(&db, playlists).await;
        assert_eq!(
            row.path.as_deref(),
            Some(dir.to_string_lossy().as_ref()),
            "the playlists folder is saved at the new path"
        );
        assert_eq!(row.parent_id, Some(db_guid(aggregate)));
        assert_eq!(row.top_parent_id, Some(db_guid(playlists)));
        assert_eq!(row.date_modified, Some(written), "LastWriteTimeUtc");
        assert_eq!(row.date_created, Some(written), "CreationTimeUtc");
        let saved = row.date_last_saved.expect("saved");
        assert!((before..=after).contains(&saved), "{saved}");

        // The next start is repair-free: neither row is written again.
        let root_row = fetch_item(&db, aggregate).await;
        store.ensure().await.expect("again");
        assert_eq!(fetch_item(&db, aggregate).await, root_row);
        assert_eq!(fetch_item(&db, playlists).await, row);
    }

    /// The re-path is for a moved folder only. A row already at the folder,
    /// stored resolved, with Jellyfin's `%AppDataPath%` token, or differing
    /// only in case (`CreateRootFolder` compares the expanded path
    /// `OrdinalIgnoreCase`), is kept exactly as it is, and so is the root.
    #[rstest]
    #[case::resolved(None)]
    #[case::app_data_token(Some("%AppDataPath%/playlists"))]
    #[case::token_in_another_case(Some("%APPDATAPATH%/Playlists"))]
    #[tokio::test]
    async fn ensure_keeps_a_playlists_folder_already_at_its_path(#[case] stored: Option<&str>) {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let ids = store.ensure().await.expect("provision");
        let playlists = store.playlists_id().expect("playlists id");
        if let Some(stored) = stored {
            set_item_path(&db, playlists, stored).await;
        }
        let (root_row, row) = (
            fetch_item(&db, ids.aggregate).await,
            fetch_item(&db, playlists).await,
        );

        store.ensure().await.expect("again");

        assert_eq!(fetch_item(&db, ids.aggregate).await, root_row);
        assert_eq!(fetch_item(&db, playlists).await, row);
    }

    /// A root folder row with no path at all is given this server's, as
    /// `CreateRootFolder`'s ordinal compare treats a null path as moved.
    #[tokio::test]
    async fn ensure_sets_the_root_folder_path_when_it_has_none() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        let aggregate = store.id();
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        assert_eq!(fetch_item(&db, aggregate).await.path, None);

        store.ensure().await.expect("ensure");

        assert_eq!(
            fetch_item(&db, aggregate).await.path.as_deref(),
            Some(store.path().to_string_lossy().as_ref())
        );
    }

    /// A moved playlists folder is not saved onto a path another row already
    /// holds: that would list two Playlists folders at one directory.
    #[tokio::test]
    async fn ensure_does_not_repath_onto_a_folder_another_row_holds() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store(&db, &tmp);
        store.ensure().await.expect("provision");
        let playlists = store.playlists_id().expect("playlists id");
        set_item_path(&db, playlists, "/moved-from/data/playlists").await;
        let foreign = Uuid::from_u128(0xADA);
        seed_folder_item(
            &db,
            foreign,
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            None,
        )
        .await;
        set_item_path(&db, foreign, &store.playlists_path().to_string_lossy()).await;

        store.ensure().await.expect("again");

        assert_eq!(
            fetch_item(&db, playlists).await.path.as_deref(),
            Some("/moved-from/data/playlists")
        );
    }
}
