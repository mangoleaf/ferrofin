//! The library side of Live TV's recording folders: the composition of
//! `ILibraryManager` calls `RecordingsManager` makes, over the managers that
//! split them here.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::media_info_resolver::is_file_protocol;
use ferrofin_core::virtual_paths::VirtualPathExpander;
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    LibraryManager, LibraryMonitor, RecordingLibrary, ScanTarget, VirtualFolderManager,
};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery};
use ferrofin_traits::providers::MetadataRefreshOptions;
use uuid::Uuid;

/// [`RecordingLibrary`] over the virtual-folder manager, the library manager
/// and the library monitor.
pub(crate) struct ServerRecordingLibrary {
    /// The on-disk libraries.
    pub(crate) virtual_folders: Arc<dyn VirtualFolderManager>,
    /// Queues the scans.
    pub(crate) library: Arc<dyn LibraryManager>,
    /// Watches the libraries' locations.
    pub(crate) monitor: Arc<dyn LibraryMonitor>,
    /// The stored form of a path under the data or metadata directory
    /// (`ReverseVirtualPath`), which a by-path lookup matches.
    pub(crate) virtual_paths: VirtualPathExpander,
}

impl ServerRecordingLibrary {
    /// Asks the monitor to watch the changed structure (it restarts on its
    /// own task), then queues the scan `AddVirtualFolder` and
    /// `RemoveVirtualFolder` end with: of the new library alone, or of
    /// everything. Like the API's structure changes, best-effort: the change
    /// has already happened.
    async fn after_structure_change(&self, scope: Option<Uuid>) {
        self.monitor.restart();
        let queued = match scope {
            Some(library_id) => self.library.queue_library_scan_scoped(library_id).await,
            None => self.library.queue_library_scan().await,
        };
        if let Err(error) = queued {
            tracing::warn!(%error, "failed to queue the scan of a recording library");
        }
    }
}

#[async_trait]
impl RecordingLibrary for ServerRecordingLibrary {
    async fn virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        self.virtual_folders.get_virtual_folders().await
    }

    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<CollectionTypeOptions>,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let library_id = self
            .virtual_folders
            .add_virtual_folder(name, collection_type, options)
            .await?;
        self.after_structure_change(library_id).await;
        Ok(())
    }

    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        self.virtual_folders.remove_virtual_folder(name).await?;
        self.after_structure_change(None).await;
        Ok(())
    }

    async fn remove_media_path(&self, name: &str, path: &str) -> Result<(), ServiceError> {
        self.virtual_folders.remove_media_path(name, path).await?;
        self.monitor.restart();
        Ok(())
    }

    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        self.library.queue_library_scan().await
    }

    async fn refresh_path(&self, path: &str) -> Result<(), ServiceError> {
        self.library
            .queue_refresh_scan(
                ScanTarget::Changed(vec![path.to_owned()]),
                &MetadataRefreshOptions::default(),
            )
            .await
    }

    async fn find_by_path(
        &self,
        path: &str,
        is_folder: bool,
    ) -> Result<Option<Uuid>, ServiceError> {
        // Upstream throws on an empty path; a blank one must never match.
        if path.trim().is_empty() {
            return Ok(None);
        }
        // `TranslateQuery` matches `GetPathToSave(path)`, the stored form (an
        // adopted row's `%AppDataPath%/…`); a row Ferrofin saved resolved is
        // found by the path itself.
        let stored = self.virtual_paths.reverse(path);
        let forms = if stored == path {
            vec![stored]
        } else {
            vec![stored, path.to_owned()]
        };
        for form in forms {
            let query = InternalItemsQuery {
                path: Some(form),
                is_folder: Some(is_folder),
                order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
                limit: Some(1),
                ..InternalItemsQuery::default()
            };
            let found = self
                .library
                .get_item_list(&query)
                .await?
                .into_iter()
                .find_map(|item| Uuid::parse_str(&item.id).ok());
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    async fn files_under(&self, folder_id: Uuid) -> Result<Vec<(Uuid, String)>, ServiceError> {
        // `Folder.GetItemList` sets the folder as the parent, and a recursive
        // parent is an ancestor query.
        let query = InternalItemsQuery {
            parent_id: folder_id,
            recursive: true,
            is_folder: Some(false),
            is_virtual_item: Some(false),
            order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
            ..InternalItemsQuery::default()
        };
        Ok(self
            .library
            .get_item_list(&query)
            .await?
            .into_iter()
            .filter_map(|item| {
                let path = item.path.filter(|path| is_file_protocol(path))?;
                Some((Uuid::parse_str(&item.id).ok()?, path))
            })
            .collect())
    }

    async fn delete_item(&self, id: Uuid) -> Result<(), ServiceError> {
        self.library
            .delete_item(
                id,
                &DeleteOptions {
                    delete_file_location: true,
                    ..DeleteOptions::default()
                },
            )
            .await
    }
}
