//! The library box sets live in: the lookup and the provisioning of
//! `CollectionManager.EnsureLibraryFolder`
//! (v12.4 `Emby.Server.Implementations/Collections/CollectionManager.cs:80-118`).
//!
//! Upstream's Collections library is an ordinary library with one location,
//! `{data}/collections`, and two rows:
//! - its `CollectionFolder` at `{root}/default/Collections`, what users see and
//!   what their `EnabledFolders` name;
//! - the plain `Folder` for the location, a child of the aggregate root like any
//!   library's location, which is the folder [`find`](CollectionsLibrary::find)
//!   answers (`FindFolders`) and so the parent of every box set. Its id is the
//!   type `Folder` over `data\collections`, `8679d105…`.
//!
//! Ferrofin used to store a bare `CollectionFolder` at `{data}/collections`
//! instead, `6f929a39…`, in no library's listing and with no options. An older
//! database may still hold it: [`legacy_container`](CollectionsLibrary::legacy_container)
//! finds it, and [`crate::adoption_repairs::merge_legacy_collections_library`]
//! moves what it holds onto the two rows above.

use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::content_type_overrides::{contains_sub_path, paths_equal};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use uuid::Uuid;

use crate::item_type_lookup::{self, IdDerivation};
use crate::library_structure_repository::{self as repository, FolderRow};
use crate::virtual_paths::VirtualPathExpander;

/// The directory's `%AppDataPath%` spelling, as Jellyfin stores it.
fn token_path() -> String {
    format!(
        "{}/collections",
        crate::app_paths::FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH
    )
}

/// The folder box sets are created in, found or just provisioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectionsFolder {
    /// Its row.
    pub(crate) id: Uuid,
    /// Its directory, expanded: where a new box set's folder goes.
    pub(crate) path: String,
    /// Whether it is the bare `CollectionFolder` an older Ferrofin made, which
    /// the boot repair has not been able to retire yet. A box set made under it
    /// moves with the rest when the repair runs.
    pub(crate) legacy: bool,
}

/// The library at `{data}/collections`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryRef {
    /// Its `CollectionFolder` id.
    pub(crate) id: Uuid,
    /// Its name, which is its directory's and may not be what was asked for.
    pub(crate) name: String,
}

/// The Collections library of one database and data directory.
#[derive(Debug, Clone)]
pub(crate) struct CollectionsLibrary {
    db: Database,
    mode: IdDerivation,
    /// `{data}/collections`.
    path: String,
    /// The aggregate root's id, whose children `FindFolders` walks.
    aggregate: Option<Uuid>,
    virtual_paths: VirtualPathExpander,
}

impl CollectionsLibrary {
    /// The library under `data_path`, for the database `db` keyed by `mode`,
    /// with its aggregate root at `root_folder_path`.
    pub(crate) fn new(
        db: Database,
        mode: IdDerivation,
        data_path: &str,
        root_folder_path: &str,
        virtual_paths: VirtualPathExpander,
    ) -> Self {
        let aggregate = item_type_lookup::aggregate_folder_id(&mode, root_folder_path);
        Self {
            db,
            mode,
            path: format!("{data_path}/collections"),
            aggregate,
            virtual_paths,
        }
    }

    /// The aggregate root's id.
    pub(crate) fn aggregate(&self) -> Option<Uuid> {
        self.aggregate
    }

    /// `FindFolders(path).FirstOrDefault()`, then the folder as an older
    /// server left it, then the bare `CollectionFolder`, so a box set keeps
    /// finding its parent while that one waits for the boot repair. Should a
    /// repair stop between making the `Folder` and moving the box sets, new ones
    /// go under the `Folder` until the next boot brings the old ones over.
    pub(crate) async fn find(&self) -> Result<Option<CollectionsFolder>, ServiceError> {
        if let Some(folder) = self.physical_folder().await? {
            return Ok(Some(folder));
        }
        let Some(row) = self.legacy_container().await? else {
            return Ok(None);
        };
        Ok(Some(CollectionsFolder {
            id: parse_id(&row)?,
            path: self.path.clone(),
            legacy: true,
        }))
    }

    /// The `Folder` box sets hang off.
    ///
    /// First the children of the aggregate root whose path is the directory or
    /// holds it (`FindFolders`: `AreEqual(path, i.Path) ||
    /// ContainsSubPath(i.Path, path)`). A folder that holds it is a library
    /// location above `{data}/collections`, the only row the root keeps for
    /// that whole tree (`NormalizeRootPathList`).
    ///
    /// Then a `Folder` stored at the directory that is no child of the root,
    /// which an adopted database may carry, and last the row under the derived
    /// id of a data directory that moved (gl-922).
    async fn physical_folder(&self) -> Result<Option<CollectionsFolder>, ServiceError> {
        if let Some(aggregate) = self.aggregate {
            for child in repository::root_child_folders(&self.db, aggregate).await? {
                let Some(path) = child.path.as_deref().map(|p| self.virtual_paths.expand(p)) else {
                    continue;
                };
                if paths_equal(&self.path, &path) || contains_sub_path(&path, &self.path) {
                    return Ok(Some(CollectionsFolder {
                        id: parse_id(&child)?,
                        path,
                        legacy: false,
                    }));
                }
            }
        }
        let stored =
            repository::folder_at(&self.db, BaseItemKind::Folder, &self.path, &token_path())
                .await?;
        let id = match stored {
            Some(row) => Some(parse_id(&row)?),
            None => {
                crate::item_persistence_service::moved_container(
                    &self.db,
                    BaseItemKind::Folder,
                    &self.path,
                    &self.mode,
                )
                .await?
            }
        };
        Ok(id.map(|id| CollectionsFolder {
            id,
            path: self.path.clone(),
            legacy: false,
        }))
    }

    /// The bare `CollectionFolder` an older Ferrofin stored at the directory:
    /// by its path, then, for a data directory that moved, by the id that path
    /// derives. No library's row is ever stored there: those are at
    /// `{root}/default/<name>`.
    pub(crate) async fn legacy_container(&self) -> Result<Option<FolderRow>, ServiceError> {
        let kind = BaseItemKind::CollectionFolder;
        if let Some(row) = repository::folder_at(&self.db, kind, &self.path, &token_path()).await? {
            return Ok(Some(row));
        }
        match item_type_lookup::derive_item_id_with(&self.mode, kind, &self.path) {
            Some(id) => repository::folder_of_kind(&self.db, id, kind).await,
            None => Ok(None),
        }
    }

    /// The configured library that lists the directory, if any.
    pub(crate) async fn library_at(
        &self,
        virtual_folders: &dyn VirtualFolderManager,
    ) -> Result<Option<LibraryRef>, ServiceError> {
        let folders = virtual_folders.library_folders().await?;
        Ok(folders
            .iter()
            .find(|folder| {
                folder
                    .locations
                    .iter()
                    .any(|location| paths_equal(location, &self.path))
            })
            .and_then(|folder| {
                Some(LibraryRef {
                    id: Uuid::parse_str(folder.item_id.as_deref()?).ok()?,
                    name: folder.name.clone()?,
                })
            }))
    }

    /// The library that lists the directory, made as `EnsureLibraryFolder`
    /// makes it when there is none: `name`, `boxsets`, the directory as its one
    /// location, no realtime monitor and metadata saved beside the box sets.
    ///
    /// # Errors
    /// Returns [`ServiceError`] if the directory or the library cannot be made,
    /// or no library lists the directory afterwards: a `Folder` no library
    /// lists is deleted by the next scan with what hangs off it, so nothing may
    /// go on to store one.
    pub(crate) async fn ensure_library(
        &self,
        virtual_folders: &dyn VirtualFolderManager,
        name: &str,
    ) -> Result<LibraryRef, ServiceError> {
        self.create_directory().await?;
        if let Some(library) = self.library_at(virtual_folders).await? {
            return Ok(library);
        }
        let options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: self.path.clone(),
            }],
            enable_realtime_monitor: false,
            save_local_metadata: true,
            ..LibraryOptions::default()
        };
        virtual_folders
            .add_virtual_folder(name, Some(CollectionTypeOptions::boxsets), &options)
            .await?;
        self.library_at(virtual_folders).await?.ok_or_else(|| {
            ServiceError::backend(format!("no library lists {} once added", self.path))
        })
    }

    /// `Directory.CreateDirectory(path)`.
    async fn create_directory(&self) -> Result<(), ServiceError> {
        tokio::fs::create_dir_all(&self.path)
            .await
            .map_err(|e| ServiceError::backend(format!("could not create {}: {e}", self.path)))
    }

    /// The `Folder` box sets hang off, stored as `ValidateTopLibraryFolders`
    /// stores it if no row is there yet: `AddVirtualFolder` runs that before it
    /// returns, where Ferrofin's scan would only reach the location later. Its
    /// path is the `%AppDataPath%` form, as Jellyfin saves it, so it follows
    /// the data directory.
    ///
    /// # Errors
    /// Returns [`ServiceError`] if the directory or the row cannot be written.
    pub(crate) async fn ensure_folder(&self) -> Result<Option<CollectionsFolder>, ServiceError> {
        if let Some(folder) = self.physical_folder().await? {
            return Ok(Some(folder));
        }
        self.create_directory().await?;
        let Some(id) =
            item_type_lookup::derive_item_id_with(&self.mode, BaseItemKind::Folder, &self.path)
        else {
            return Ok(None);
        };
        let parent = match self.aggregate {
            Some(root) if crate::item_persistence_service::row_exists(&self.db, root).await? => {
                Some(root)
            }
            _ => None,
        };
        let now = chrono::Utc::now();
        let dates = tokio::fs::metadata(&self.path)
            .await
            .ok()
            .as_ref()
            .map_or((now, now), crate::library_scan::created_and_modified);
        let stored = self.virtual_paths.reverse(&self.path);
        repository::insert_location_folder(&self.db, id, parent, &stored, dates).await?;
        tracing::info!(item_id = %id, path = %self.path, "created the collections folder");
        Ok(Some(CollectionsFolder {
            id,
            path: self.path.clone(),
            legacy: false,
        }))
    }

    /// What a box set under `folder` names as its ancestors:
    /// `GetAncestorIds`, the folder, the root and the library.
    pub(crate) fn ancestors(
        &self,
        folder: &CollectionsFolder,
        library: Option<&LibraryRef>,
    ) -> Vec<Uuid> {
        [Some(folder.id), self.aggregate, library.map(|l| l.id)]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// A row's id. One that does not parse is refused rather than read as some
/// other id: what is moved or created next depends on it.
pub(crate) fn parse_id(row: &FolderRow) -> Result<Uuid, ServiceError> {
    Uuid::parse_str(&row.id).map_err(|e| {
        ServiceError::backend(format!("folder row {} has an unusable id: {e}", row.id))
    })
}

#[cfg(test)]
mod tests;
