//! Episode-specific local image discovery.
//!
//! Port of `MediaBrowser.LocalMetadata.Images.EpisodeLocalImageProvider` — an
//! episode's primary image is the file sharing the episode's name (or that name
//! with a `-thumb` suffix), searched in the episode's own folder and a sibling
//! `metadata/` subfolder.

use crate::container_types::{FileSystemMetadata, LocalImageInfo};
use crate::local_images::directory_service::{DirectoryService, is_supported_image_extension};
use crate::local_images::item::{ImageItem, ImageItemKind};
use crate::local_images::local_image_provider::file_name_without_extension;
use hermit_model::entities::ImageType;

/// The episode local image provider.
///
/// Port of `EpisodeLocalImageProvider`. `Name` is `"Local Images"` and `Order`
/// is `0` upstream.
#[derive(Debug, Default, Clone, Copy)]
pub struct EpisodeLocalImageProvider;

impl EpisodeLocalImageProvider {
    /// The provider name (`Name => "Local Images"`).
    pub const NAME: &'static str = "Local Images";

    /// The provider order (`Order => 0`).
    pub const ORDER: i32 = 0;

    /// Creates an episode local image provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether this provider handles `item` (`Supports`).
    ///
    /// Port of `EpisodeLocalImageProvider.Supports`: episodes only.
    #[must_use]
    pub fn supports(item: &ImageItem) -> bool {
        item.kind == ImageItemKind::Episode
    }

    /// Discovers the episode's primary image in its folder and `metadata/`
    /// subfolder.
    ///
    /// Port of `GetImages`.
    #[must_use]
    pub fn get_images<D: DirectoryService>(
        item: &ImageItem,
        directory_service: &D,
    ) -> Vec<LocalImageInfo> {
        let Some(item_path) = item.path.as_deref() else {
            return Vec::new();
        };
        let Some(parent_path) = parent_directory(item_path) else {
            return Vec::new();
        };

        let parent_files = directory_service.files(parent_path);
        let name_without_extension = file_name_without_extension(item_path);

        let mut images = Self::image_files_from_folder(name_without_extension, &parent_files);

        if let Some(metadata_dir) = directory_service
            .directories(parent_path)
            .into_iter()
            .find(|d| d.name == "metadata")
        {
            let files = directory_service.files(&metadata_dir.full_name);
            images.extend(Self::image_files_from_folder(
                name_without_extension,
                &files,
            ));
        }

        images
    }

    /// Matches image files in `file_paths` named `{filename}` or `{filename}-thumb`.
    ///
    /// Port of `GetImageFilesFromFolder`.
    fn image_files_from_folder(
        filename_without_extension: &str,
        file_paths: &[FileSystemMetadata],
    ) -> Vec<LocalImageInfo> {
        let thumb_name = format!("{filename_without_extension}-thumb");
        let mut list = Vec::new();

        for file in file_paths {
            if file.is_directory {
                continue;
            }
            if !file
                .extension
                .as_deref()
                .is_some_and(is_supported_image_extension)
            {
                continue;
            }
            let current = file_name_without_extension(&file.full_name);
            if current.eq_ignore_ascii_case(filename_without_extension)
                || current.eq_ignore_ascii_case(&thumb_name)
            {
                list.push(LocalImageInfo {
                    file_info: file.clone(),
                    type_: ImageType::Primary,
                });
            }
        }

        list
    }
}

/// Returns the parent directory of a path, or `None` for a bare filename.
///
/// Port of `Path.GetDirectoryName`.
fn parent_directory(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rfind(['/', '\\']).map(|idx| &trimmed[..idx])
}
