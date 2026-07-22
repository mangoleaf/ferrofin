//! The collection-folder and internal-metadata local image providers.
//!
//! Ports of `CollectionFolderLocalImageProvider` and
//! `InternalMetadataFolderImageProvider`, both thin wrappers that delegate to
//! [`LocalImageProvider::get_images_from_paths`] with a different set of scan
//! paths.

use crate::container_types::LocalImageInfo;
use crate::local_images::directory_service::DirectoryService;
use crate::local_images::item::{ImageItem, ImageItemKind};
use crate::local_images::local_image_provider::LocalImageProvider;

/// Local image provider for collection folders.
///
/// Port of `CollectionFolderLocalImageProvider`: scans the collection folder's
/// physical locations. `Name` is `"Collection Folder Images"` and `Order` is
/// `1` upstream (runs after [`LocalImageProvider`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct CollectionFolderLocalImageProvider;

impl CollectionFolderLocalImageProvider {
    /// The provider name (`Name => "Collection Folder Images"`).
    pub const NAME: &'static str = "Collection Folder Images";

    /// The provider order (`Order => 1`).
    pub const ORDER: i32 = 1;

    /// Creates a collection-folder local image provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether this provider handles `item` (`Supports`).
    ///
    /// Port of `CollectionFolderLocalImageProvider.Supports`: collection folders
    /// only.
    #[must_use]
    pub fn supports(item: &ImageItem) -> bool {
        item.kind == ImageItemKind::CollectionFolder
    }

    /// Discovers images across the collection folder's physical locations.
    ///
    /// Port of `GetImages`.
    #[must_use]
    pub fn get_images<D: DirectoryService>(
        item: &ImageItem,
        directory_service: &D,
    ) -> Vec<LocalImageInfo> {
        LocalImageProvider::get_images_from_paths(item, &item.physical_locations, directory_service)
    }
}

/// Local image provider for the internal metadata folder.
///
/// Port of `InternalMetadataFolderImageProvider`: scans the item's internal
/// metadata path (where extracted images are stored). `Name` is
/// `"Internal Images"` and `Order` is `1000` upstream (runs last).
#[derive(Debug, Default, Clone, Copy)]
pub struct InternalMetadataFolderImageProvider;

impl InternalMetadataFolderImageProvider {
    /// The provider name (`Name => "Internal Images"`).
    pub const NAME: &'static str = "Internal Images";

    /// The provider order (`Order => 1000`).
    pub const ORDER: i32 = 1000;

    /// Creates an internal-metadata-folder local image provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether this provider handles `item` (`Supports`).
    ///
    /// Port of `InternalMetadataFolderImageProvider.Supports`. The C# decision
    /// depends on server-side flags (`IsSaveLocalMetadataEnabled`,
    /// `AlwaysScanInternalMetadataPath`) not modelled here; the two firm rules
    /// are ported: never for a photo, always for audio.
    #[must_use]
    pub fn supports(item: &ImageItem) -> bool {
        // Never for a photo; always for audio; otherwise the C# fall-through
        // `return true` (the save-local-metadata / always-scan flags that would
        // narrow this are server-side and not modelled here).
        item.kind != ImageItemKind::Photo
    }

    /// Discovers images under `internal_metadata_path`.
    ///
    /// Port of `GetImages`. The caller supplies the resolved internal-metadata
    /// path (`item.GetInternalMetadataPath()`), which has no field on
    /// [`ImageItem`]; a missing / empty path yields no images.
    #[must_use]
    pub fn get_images<D: DirectoryService>(
        item: &ImageItem,
        internal_metadata_path: &str,
        directory_service: &D,
    ) -> Vec<LocalImageInfo> {
        if internal_metadata_path.is_empty() {
            return Vec::new();
        }
        LocalImageProvider::get_images_from_paths(
            item,
            &[internal_metadata_path.to_owned()],
            directory_service,
        )
    }
}
