//! A disabled [`VirtualFolderManager`] for the `AppState` default.
//!
//! The [`VirtualFolderManager`] seam is a **real** subsystem backed by the
//! on-disk user-views tree (`ferrofin-core`'s `FerrofinVirtualFolderManager`), but
//! `AppState` must name a non-optional `Arc<dyn VirtualFolderManager>` and every
//! pre-seam test constructor predates it. This stub satisfies the field: the
//! read routes return an empty set (a faithful "no libraries configured"
//! response) and the mutation routes report the subsystem as unconfigured
//! ([`ServiceError::Backend`]). The composition root replaces it with the
//! concrete filesystem-backed manager.

use async_trait::async_trait;

use crate::error::ServiceError;
use crate::library::VirtualFolderManager;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;

/// A no-op [`VirtualFolderManager`]: reads are empty, mutations are rejected.
///
/// Used as the `AppState` default so pre-seam test constructors keep compiling.
/// Never touches the filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledVirtualFolderManager;

impl DisabledVirtualFolderManager {
    /// The uniform "the virtual-folder store is not configured" error returned
    /// by every mutating method.
    fn unconfigured() -> ServiceError {
        ServiceError::Backend(
            "the virtual-folder store is not configured on this server".to_owned(),
        )
    }
}

#[async_trait]
impl VirtualFolderManager for DisabledVirtualFolderManager {
    async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        Ok(Vec::new())
    }

    async fn add_virtual_folder(
        &self,
        _name: &str,
        _collection_type: Option<CollectionTypeOptions>,
        _options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn rename_virtual_folder(
        &self,
        _name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn add_media_path(
        &self,
        _virtual_folder_name: &str,
        _path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn update_media_path(
        &self,
        _virtual_folder_name: &str,
        _path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn remove_media_path(
        &self,
        _virtual_folder_name: &str,
        _path: &str,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }

    async fn update_library_options(
        &self,
        _virtual_folder_name: &str,
        _options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        Err(Self::unconfigured())
    }
}

#[cfg(test)]
mod tests {
    use super::DisabledVirtualFolderManager;
    use crate::library::VirtualFolderManager;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};

    #[tokio::test]
    async fn reads_are_empty_and_mutations_fail() {
        let mgr = DisabledVirtualFolderManager;
        assert!(mgr.get_virtual_folders().await.unwrap().is_empty());
        assert!(mgr.get_physical_paths().await.unwrap().is_empty());
        assert!(
            mgr.add_virtual_folder("x", None, &LibraryOptions::default())
                .await
                .is_err()
        );
        assert!(mgr.remove_virtual_folder("x").await.is_err());
        assert!(mgr.rename_virtual_folder("x", "y").await.is_err());
        let mpi = MediaPathInfo {
            path: "/x".to_owned(),
        };
        assert!(mgr.add_media_path("x", &mpi).await.is_err());
        assert!(mgr.update_media_path("x", &mpi).await.is_err());
        assert!(mgr.remove_media_path("x", "/x").await.is_err());
        assert!(
            mgr.update_library_options("x", &LibraryOptions::default())
                .await
                .is_err()
        );
    }
}
