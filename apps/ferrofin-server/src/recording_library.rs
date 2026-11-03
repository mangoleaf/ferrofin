//! The library side of Live TV's recording folders: the composition of
//! `ILibraryManager` calls `RecordingsManager` makes, over the managers that
//! split them here.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    LibraryManager, LibraryMonitor, RecordingLibrary, ScanTarget, VirtualFolderManager,
};
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
}

impl ServerRecordingLibrary {
    /// Restarts the monitor so it watches the changed structure, then queues
    /// the scan `AddVirtualFolder` and `RemoveVirtualFolder` end with: of the
    /// new library alone, or of everything. Like the API's structure changes,
    /// best-effort: the change has already happened.
    async fn after_structure_change(&self, scope: Option<Uuid>) {
        if let Err(error) = self.monitor.stop().await {
            tracing::warn!(%error, "failed to stop library monitor");
        }
        if let Err(error) = self.monitor.start().await {
            tracing::warn!(%error, "failed to restart library monitor");
        }
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
        if let Err(error) = self.monitor.stop().await {
            tracing::warn!(%error, "failed to stop library monitor");
        }
        if let Err(error) = self.monitor.start().await {
            tracing::warn!(%error, "failed to restart library monitor");
        }
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
}
