//! Image drawing types — port of `MediaBrowser.Model.Drawing`.
//!
//! `ImageFormat` (output formats + mime/extension helpers), `ImageOrientation`
//! (EXIF orientation values), `ImageResolution` (standard tiers),
//! `ImageDimensions` (a width/height pair), and `DrawingUtils`
//! (aspect-preserving resize math).

pub mod drawing_utils;
mod image_dimensions;
mod image_format;
mod image_orientation;
mod image_resolution;

pub use image_dimensions::ImageDimensions;
pub use image_format::{ImageFormat, UnknownImageFormat};
pub use image_orientation::ImageOrientation;
pub use image_resolution::ImageResolution;
