//! Image processing for Hermit — port of `Jellyfin.Drawing` + `Emby.Photos`.
//!
//! Implements the `hermit-traits` `ImageProcessor` / `ImageEncoder` traits via
//! the `image` crate (resize/crop/format; no native Skia).

pub mod error;
pub mod image_encoder;
pub mod null_encoder;
pub mod photo_provider;
pub mod processor;

pub use error::DrawingError;
pub use image_encoder::ImageCrateEncoder;
pub use null_encoder::NullImageEncoder;
pub use photo_provider::{DirectoryService, FileInfo, PhotoItem, PhotoProvider, is_exif_candidate};
pub use processor::{FileMeta, ImageProcessor, StdFileMeta};
