//! Local (filesystem-convention) image discovery.
//!
//! Port of `MediaBrowser.LocalMetadata.Images` — the family of providers that
//! map files sitting alongside a library item (`poster.png`, `folder.jpg`,
//! `movie-fanart.jpg`, `season01-poster.png`, an `extrafanart/` folder, …) to
//! typed [`crate::container_types::LocalImageInfo`] entries by filename
//! convention.
//!
//! - [`LocalImageProvider`] — the main convention scanner.
//! - [`EpisodeLocalImageProvider`] — an episode's own image plus `-thumb`.
//! - [`CollectionFolderLocalImageProvider`] — scans a collection's physical
//!   locations.
//! - [`InternalMetadataFolderImageProvider`] — scans the internal metadata path.
//!
//! All filesystem access goes through the [`DirectoryService`] seam so the unit
//! tests drive a temp-dir / in-memory fake and the un-mockable I/O
//! ([`FsDirectoryService`]) stays out of the coverage numbers.

pub mod directory_service;
pub mod episode_image_provider;
pub mod item;
pub mod local_image_provider;
pub mod other_providers;

pub use directory_service::{
    DirectoryService, FsDirectoryService, SUPPORTED_IMAGE_EXTENSIONS, is_supported_image_extension,
    supported_image_extension_index,
};
pub use episode_image_provider::EpisodeLocalImageProvider;
pub use item::{ImageItem, ImageItemKind};
pub use local_image_provider::LocalImageProvider;
pub use other_providers::{
    CollectionFolderLocalImageProvider, InternalMetadataFolderImageProvider,
};

#[cfg(test)]
mod tests;
