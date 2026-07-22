//! Image-processing **service** traits — the drawing seam.
//!
//! Ports of `IImageProcessor` and `IImageEncoder` in
//! `MediaBrowser.Controller.Drawing`. [`ImageProcessor`] is the high-level
//! service the API uses (dimensions, blurhash, cache tags, on-the-fly resize,
//! collages); [`ImageEncoder`] is the lower-level codec seam it drives (encode a
//! single file, build a collage/splashscreen/trickplay tile).
//!
//! Port rules applied:
//! - Item **identity** arguments become [`uuid::Uuid`]; the domain `BaseItem` /
//!   `BaseItemDto` / `User` receivers of the overloaded `GetImageDimensions` /
//!   `GetImageCacheTag` methods collapse onto one id-plus-[`ItemImageInfo`]
//!   form each (the C# `ChapterInfo` overloads fold into the same shape).
//! - `Task<T>` becomes `async fn`; the synchronous C# methods stay `async`
//!   here too, since encoding/probing is I/O behind the trait.
//! - Enums/value types are reused from `hermit-model`
//!   ([`ImageDimensions`]/[`ImageFormat`]/[`ImageOrientation`]).
//! - Property getters become `fn … (&self) -> _` accessors.
//!
//! Both traits are object-safe and carry `_assert_object_safe_*` assertions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::{ImageCollageOptions, ImageProcessingOptions, ItemImageInfo};

/// The result of processing an image on the fly: the produced file, its MIME
/// type (if known), and its last-modified time.
///
/// Port of the C# `(string Path, string? MimeType, DateTime DateModified)`
/// tuple returned by `IImageProcessor.ProcessImage`; a named struct reads
/// better across a trait boundary and can grow fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    /// The path to the processed image file.
    pub path: String,
    /// The MIME type of the processed image, if it could be determined.
    pub mime_type: Option<String>,
    /// The last-modified time of the processed image.
    pub date_modified: DateTime<Utc>,
}

/// High-level image service: dimensions, blurhash, cache tags, resize, collage.
///
/// Port of `IImageProcessor`. Implementations delegate the actual pixel work to
/// an [`ImageEncoder`].
#[async_trait]
pub trait ImageProcessor: Send + Sync {
    /// The container/extension formats the processor can read as input (e.g.
    /// `"jpg"`, `"png"`). Port of the `SupportedInputFormats` property.
    fn supported_input_formats(&self) -> Vec<String>;

    /// Whether this processor can composite images into a collage. Port of the
    /// `SupportsImageCollageCreation` property.
    fn supports_image_collage_creation(&self) -> bool;

    /// The image output formats this processor can produce. Port of
    /// `GetSupportedImageOutputFormats`.
    fn supported_image_output_formats(&self) -> Vec<ImageFormat>;

    /// Reads the pixel dimensions of the image at `path`. Port of the
    /// `GetImageDimensions(string path)` overload.
    async fn get_image_dimensions(&self, path: &str) -> Result<ImageDimensions, ServiceError>;

    /// Reads the pixel dimensions of an item's image row. Port of the
    /// `GetImageDimensions(BaseItem, ItemImageInfo)` overload; the domain item
    /// becomes an [`item_id`](Uuid).
    async fn get_item_image_dimensions(
        &self,
        item_id: Uuid,
        info: &ItemImageInfo,
    ) -> Result<ImageDimensions, ServiceError>;

    /// Computes the blurhash of the image at `path`. Port of the
    /// `GetImageBlurHash(string path)` overload.
    async fn get_image_blur_hash(&self, path: &str) -> Result<String, ServiceError>;

    /// Computes the blurhash of the image at `path` given its already-known
    /// dimensions. Port of the
    /// `GetImageBlurHash(string, ImageDimensions)` overload.
    async fn get_image_blur_hash_sized(
        &self,
        path: &str,
        image_dimensions: ImageDimensions,
    ) -> Result<String, ServiceError>;

    /// Computes the cache tag for an item's image row, or `None` when the image
    /// cannot be tagged. Port of the several `GetImageCacheTag` overloads
    /// (`BaseItem`/`BaseItemDto` + `ItemImageInfo`/`ChapterInfo`), collapsed to
    /// one id-plus-[`ItemImageInfo`] form.
    async fn get_image_cache_tag(
        &self,
        item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Result<Option<String>, ServiceError>;

    /// Computes the cache tag for an image path plus its modification time.
    /// Port of the `GetImageCacheTag(string baseItemPath, DateTime
    /// imageDateModified)` overload.
    async fn get_image_cache_tag_for_path(
        &self,
        base_item_path: &str,
        image_date_modified: DateTime<Utc>,
    ) -> Result<Option<String>, ServiceError>;

    /// Processes (resizes/overlays/re-encodes) an image on the fly, returning
    /// the produced file. Port of `ProcessImage(ImageProcessingOptions)`.
    async fn process_image(
        &self,
        options: &ImageProcessingOptions,
    ) -> Result<ProcessedImage, ServiceError>;

    /// Composites several images into a single collage, optionally drawing
    /// `library_name` onto it. Port of `CreateImageCollage(options,
    /// libraryName)`.
    async fn create_image_collage(
        &self,
        options: &ImageCollageOptions,
        library_name: Option<&str>,
    ) -> Result<(), ServiceError>;
}

/// Compile-time assertion that [`ImageProcessor`] is object-safe.
fn _assert_object_safe_image_processor(_: &dyn ImageProcessor) {}

/// Low-level image-codec seam: probe, blurhash, encode, collage, trickplay.
///
/// Port of `IImageEncoder`. A concrete encoder (SkiaSharp, ImageMagick, …)
/// implements this; [`ImageProcessor`] chooses one and drives it.
#[async_trait]
pub trait ImageEncoder: Send + Sync {
    /// The container/extension formats the encoder can read as input. Port of
    /// the `SupportedInputFormats` property.
    fn supported_input_formats(&self) -> Vec<String>;

    /// The image formats the encoder can produce. Port of the
    /// `SupportedOutputFormats` property.
    fn supported_output_formats(&self) -> Vec<ImageFormat>;

    /// A human-readable name for this encoder. Port of the `Name` property.
    fn name(&self) -> String;

    /// Whether this encoder can composite a collage. Port of the
    /// `SupportsImageCollageCreation` property.
    fn supports_image_collage_creation(&self) -> bool;

    /// Whether this encoder can encode images at all (some are decode-only).
    /// Port of the `SupportsImageEncoding` property.
    fn supports_image_encoding(&self) -> bool;

    /// Reads the pixel dimensions of the image at `path`. Port of
    /// `GetImageSize(string path)`.
    async fn get_image_size(&self, path: &str) -> Result<ImageDimensions, ServiceError>;

    /// Computes the blurhash of the image at `path` using `x_comp`×`y_comp` DCT
    /// components. Port of `GetImageBlurHash(int xComp, int yComp, string
    /// path)`.
    async fn get_image_blur_hash(
        &self,
        x_comp: i32,
        y_comp: i32,
        path: &str,
    ) -> Result<String, ServiceError>;

    /// Encodes one image file into `output_path`, returning the written path.
    /// Port of `EncodeImage(inputPath, dateModified, outputPath, autoOrient,
    /// orientation, quality, options, outputFormat)`.
    #[allow(clippy::too_many_arguments)]
    async fn encode_image(
        &self,
        input_path: &str,
        date_modified: DateTime<Utc>,
        output_path: &str,
        auto_orient: bool,
        orientation: Option<ImageOrientation>,
        quality: i32,
        options: &ImageProcessingOptions,
        output_format: ImageFormat,
    ) -> Result<String, ServiceError>;

    /// Composites a collage, optionally drawing `library_name`. Port of
    /// `CreateImageCollage(options, libraryName)`.
    async fn create_image_collage(
        &self,
        options: &ImageCollageOptions,
        library_name: Option<&str>,
    ) -> Result<(), ServiceError>;

    /// Builds a splashscreen from poster and backdrop image paths. Port of
    /// `CreateSplashscreen(posters, backdrops)`.
    async fn create_splashscreen(
        &self,
        posters: &[String],
        backdrops: &[String],
    ) -> Result<(), ServiceError>;

    /// Builds a trickplay tile image (here `options` width/height are a
    /// thumbnail *count*, not pixels), returning the decoded height of a single
    /// thumbnail. Port of `CreateTrickplayTile(options, quality, imgWidth,
    /// imgHeight)`.
    async fn create_trickplay_tile(
        &self,
        options: &ImageCollageOptions,
        quality: i32,
        img_width: i32,
        img_height: Option<i32>,
    ) -> Result<i32, ServiceError>;
}

/// Compile-time assertion that [`ImageEncoder`] is object-safe.
fn _assert_object_safe_image_encoder(_: &dyn ImageEncoder) {}
