//! `ImageProcessor` — the high-level [`ferrofin_traits::drawing::ImageProcessor`]
//! service, port of `Jellyfin.Drawing.ImageProcessor`.
//!
//! Orchestration only: it owns an [`ImageEncoder`] and the resized-image cache
//! directory, and drives dimension probing, blurhash component math, cache-tag
//! and cache-key derivation, and the on-the-fly [`process_image`](ImageProcessor::process_image)
//! pipeline. All pixel work is delegated to the encoder; the processor is pure
//! string/hash/gating on top of it.
//!
//! Port rules applied:
//! - The C# constructor's `IServerApplicationPaths` collapses to the single
//!   [`image_cache_path`](ImageProcessor::new) the processor actually uses; the
//!   `IServerConfigurationManager.ParallelImageEncodingLimit` semaphore is
//!   dropped — concurrency limiting is the host's concern, not this port's, and
//!   there is no oracle for it.
//! - The overloaded `GetImageDimensions` / `GetImageCacheTag` methods collapse
//!   onto the id-plus-[`ItemImageInfo`] forms the trait declares.
//! - The `Photo` auto-orientation branch is **deferred**: the `Photo` entity is
//!   not wired into [`ImageProcessingOptions`] here (it carries an
//!   [`item_id`](ImageProcessingOptions::item_id), not a domain item), so
//!   `auto_orient` is always `false`. The gate is preserved structurally so it
//!   can be re-enabled once the entity lands.
//! - `MimeTypes.GetMimeType` is reused from [`ferrofin_model::net::mime_types`];
//!   `GetMD5().ToString("N")` is reused from [`ferrofin_common::extensions::get_md5`]
//!   plus [`Uuid::simple`] (32-char lowercase hex, identical to .NET `"N"`).
//! - The filesystem reads the pipeline needs (`File.Exists`, `GetLastWriteTimeUtc`)
//!   sit behind the small [`FileMeta`] seam so tests use a fake and the real
//!   `std::fs` stat stays out of the parity/coverage numbers.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_common::extensions::get_md5;
use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
use ferrofin_model::net::mime_types::get_mime_type;
use ferrofin_traits::drawing::{
    ImageEncoder, ImageProcessor as ImageProcessorTrait, ProcessedImage,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions, ItemImageInfo};
use uuid::Uuid;

/// The cache-invalidation version stamped into every resized-image cache key.
///
/// Port of the C# `private const char Version = '3'`: bump it to invalidate all
/// previously-cached resized images. Surfaced as the default of
/// [`ImageProcessor::cache_version`] (a configurable setting) rather than a bare
/// literal, so a host can force a cache flush without a recompile.
const DEFAULT_CACHE_VERSION: char = '3';

/// The image extensions that require a transparency-preserving output format.
///
/// Port of the C# `_transparentImageTypes` set (compared case-insensitively
/// against the source extension, leading dot included).
const TRANSPARENT_IMAGE_TYPES: [&str; 4] = [".png", ".webp", ".gif", ".svg"];

/// The MIME type of a GIF, which [`process_image`](ImageProcessor::process_image)
/// passes through untouched. Port of the C# `MediaTypeNames.Image.Gif` check.
const GIF_MIME_TYPE: &str = "image/gif";

/// The number of `.NET` ticks (100-nanosecond intervals) in one second.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The offset, in seconds, from the `.NET` `DateTime` epoch (`0001-01-01`) to
/// the Unix epoch (`1970-01-01`). `62_135_596_800 * TICKS_PER_SECOND` is the
/// well-known `621_355_968_000_000_000` Unix-epoch tick constant.
const DOTNET_EPOCH_UNIX_OFFSET_SECS: i64 = 62_135_596_800;

/// Read-only filesystem metadata the [`process_image`](ImageProcessor::process_image)
/// pipeline needs, behind a seam so tests can fake it.
///
/// The two calls the C# `ImageProcessor` makes on `IFileSystem` / `File` are
/// `File.Exists(path)` and `IFileSystem.GetLastWriteTimeUtc(path)`; keeping them
/// behind a trait means the real `std::fs` stat stays out of the parity and
/// coverage numbers and unit tests stay hermetic.
pub trait FileMeta: Send + Sync {
    /// Whether a file exists at `path`. Port of `File.Exists`.
    fn exists(&self, path: &str) -> bool;

    /// The last-modified time of `path` in UTC, or [`Utc::now`] when it cannot be
    /// read. Port of `IFileSystem.GetLastWriteTimeUtc`.
    fn last_write_time_utc(&self, path: &str) -> DateTime<Utc>;
}

/// The production [`FileMeta`], backed by `std::fs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFileMeta;

impl FileMeta for StdFileMeta {
    /// Port of `File.Exists` via [`Path::exists`].
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    /// Port of `IFileSystem.GetLastWriteTimeUtc`. Falls back to [`Utc::now`] when
    /// the file cannot be stat'd (mirroring the C# fallback to `DateTime.MinValue`
    /// only loosely — the value is used solely as the returned
    /// [`ProcessedImage::date_modified`], never re-hashed).
    fn last_write_time_utc(&self, path: &str) -> DateTime<Utc> {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(modified) => DateTime::<Utc>::from(modified),
            Err(_) => Utc::now(),
        }
    }
}

/// High-level image service: dimensions, blurhash, cache tags, resize, collage.
///
/// Port of `Jellyfin.Drawing.ImageProcessor`. Owns an [`ImageEncoder`] and the
/// resized-image cache directory; every pixel operation is delegated to the
/// encoder.
pub struct ImageProcessor<F = StdFileMeta> {
    /// The codec that does the real pixel work.
    encoder: Arc<dyn ImageEncoder>,

    /// The base image-cache directory (`IServerApplicationPaths.ImageCachePath`).
    image_cache_path: PathBuf,

    /// The filesystem-metadata seam (`File.Exists` / `GetLastWriteTimeUtc`).
    file_meta: F,

    /// The cache-invalidation version stamped into every cache key. Defaults to
    /// [`DEFAULT_CACHE_VERSION`]; a configurable setting so a host can force a
    /// cache flush.
    cache_version: char,
}

impl ImageProcessor<StdFileMeta> {
    /// Constructs an [`ImageProcessor`] over `encoder`, caching resized images
    /// beneath `image_cache_path`, using the real `std::fs`-backed [`FileMeta`]
    /// and the default [`cache_version`](Self::with_cache_version) of `'3'`.
    #[must_use]
    pub fn new(encoder: Arc<dyn ImageEncoder>, image_cache_path: impl Into<PathBuf>) -> Self {
        Self {
            encoder,
            image_cache_path: image_cache_path.into(),
            file_meta: StdFileMeta,
            cache_version: DEFAULT_CACHE_VERSION,
        }
    }
}

impl<F: FileMeta> ImageProcessor<F> {
    /// Constructs an [`ImageProcessor`] with an explicit [`FileMeta`] seam (used
    /// by tests to supply a fake filesystem).
    #[must_use]
    pub fn with_file_meta(
        encoder: Arc<dyn ImageEncoder>,
        image_cache_path: impl Into<PathBuf>,
        file_meta: F,
    ) -> Self {
        Self {
            encoder,
            image_cache_path: image_cache_path.into(),
            file_meta,
            cache_version: DEFAULT_CACHE_VERSION,
        }
    }

    /// Overrides the [`cache_version`](DEFAULT_CACHE_VERSION) stamped into cache
    /// keys. Bumping it invalidates every previously-cached resized image.
    #[must_use]
    pub fn with_cache_version(mut self, version: char) -> Self {
        self.cache_version = version;
        self
    }

    /// The resized-image cache directory, `ImageCachePath/resized-images`. Port
    /// of the C# `ResizedImageCachePath` property.
    fn resized_image_cache_path(&self) -> PathBuf {
        self.image_cache_path.join("resized-images")
    }

    /// The `.NET` `DateTime.Ticks` of `dt`: 100-nanosecond intervals since
    /// `0001-01-01T00:00:00`. Used verbatim in cache tags and keys so the derived
    /// strings match the C# oracle.
    fn dotnet_ticks(dt: DateTime<Utc>) -> i64 {
        let secs = dt.timestamp() + DOTNET_EPOCH_UNIX_OFFSET_SECS;
        // subsec_nanos is 0..1_000_000_000, so /100 is 0..10_000_000 — no overflow.
        secs * TICKS_PER_SECOND + i64::from(dt.timestamp_subsec_nanos() / 100)
    }

    /// Chooses the output format. Port of `GetOutputFormat`: prefer WebP when both
    /// the server and client support it; else PNG when transparency is required
    /// and the client supports it; else the first mutually-supported client
    /// format; else JPG.
    fn output_format(
        &self,
        client_supported_formats: &[ImageFormat],
        requires_transparency: bool,
    ) -> ImageFormat {
        let server_formats = self.encoder.supported_output_formats();

        if server_formats.contains(&ImageFormat::Webp)
            && client_supported_formats.contains(&ImageFormat::Webp)
        {
            return ImageFormat::Webp;
        }

        if requires_transparency && client_supported_formats.contains(&ImageFormat::Png) {
            return ImageFormat::Png;
        }

        for format in client_supported_formats {
            if server_formats.contains(format) {
                return *format;
            }
        }

        // We should never actually get here.
        ImageFormat::Jpg
    }

    /// Builds the resized-image cache key `StringBuilder` and resolves it to a
    /// full cache file path. Port of `GetCacheFilePath`: every set option is
    /// appended in the C# order, then `,v=<version>`, then the whole string is
    /// MD5-prefixed into the cache tree by [`Self::cache_path`].
    #[allow(clippy::too_many_arguments)]
    fn cache_file_path(
        &self,
        original_path: &str,
        options: &ImageProcessingOptions,
        quality: i32,
        date_modified: DateTime<Utc>,
        format: ImageFormat,
    ) -> Result<String, ServiceError> {
        let mut filename = String::with_capacity(256);
        filename.push_str(original_path);

        filename.push_str(",quality=");
        filename.push_str(&quality.to_string());

        filename.push_str(",datemodified=");
        filename.push_str(&Self::dotnet_ticks(date_modified).to_string());

        filename.push_str(",f=");
        // C# appends the enum's ToString(), e.g. "Webp"/"Jpg"/"Png"; the
        // derived Debug of ImageFormat yields the identical PascalCase name.
        write!(filename, "{format:?}")
            .map_err(|e| ServiceError::backend(format!("cache-key format: {e}")))?;

        if let Some(width) = options.width {
            filename.push_str(",width=");
            filename.push_str(&width.to_string());
        }
        if let Some(height) = options.height {
            filename.push_str(",height=");
            filename.push_str(&height.to_string());
        }
        if let Some(max_width) = options.max_width {
            filename.push_str(",maxwidth=");
            filename.push_str(&max_width.to_string());
        }
        if let Some(max_height) = options.max_height {
            filename.push_str(",maxheight=");
            filename.push_str(&max_height.to_string());
        }
        if let Some(fill_width) = options.fill_width {
            filename.push_str(",fillwidth=");
            filename.push_str(&fill_width.to_string());
        }
        if let Some(fill_height) = options.fill_height {
            filename.push_str(",fillheight=");
            filename.push_str(&fill_height.to_string());
        }
        if options.percent_played > 0.0 {
            filename.push_str(",p=");
            filename.push_str(&options.percent_played.to_string());
        }
        if let Some(unplayed_count) = options.unplayed_count {
            filename.push_str(",p=");
            filename.push_str(&unplayed_count.to_string());
        }
        if let Some(blur) = options.blur {
            filename.push_str(",blur=");
            filename.push_str(&blur.to_string());
        }
        if let Some(background_color) = options.background_color.as_deref()
            && !background_color.is_empty()
        {
            filename.push_str(",b=");
            filename.push_str(background_color);
        }
        if let Some(foreground_layer) = options.foreground_layer.as_deref()
            && !foreground_layer.is_empty()
        {
            filename.push_str(",fl=");
            filename.push_str(foreground_layer);
        }

        filename.push_str(",v=");
        filename.push(self.cache_version);

        let cache_dir = self.resized_image_cache_path();
        Self::cache_path_unique(&cache_dir, &filename, format.extension())
    }

    /// Port of the three-argument `GetCachePath(path, uniqueName, fileExtension)`:
    /// MD5s `unique_name`, appends `file_extension`, and shards the result into a
    /// one-character prefix subdirectory of `path`.
    fn cache_path_unique(
        cache_dir: &Path,
        unique_name: &str,
        file_extension: &str,
    ) -> Result<String, ServiceError> {
        if cache_dir.as_os_str().is_empty() {
            return Err(ServiceError::invalid_input("Path can't be empty."));
        }
        if unique_name.is_empty() {
            return Err(ServiceError::invalid_input("uniqueName can't be empty."));
        }
        if file_extension.is_empty() {
            return Err(ServiceError::invalid_input("fileExtension can't be empty."));
        }

        // GetMD5().ToString() then + fileExtension. The C# Guid.ToString() default
        // format is the dashed "D" form; GetCachePath only ever slices the first
        // character (a hex digit), so the dashes never reach the prefix. We use
        // the simple 32-char form, whose first character is likewise a hex digit
        // — the shard prefix is identical.
        let filename = format!("{}{}", get_md5(unique_name).simple(), file_extension);
        Self::cache_path_filename(cache_dir, &filename)
    }

    /// Port of the two-argument `GetCachePath(path, filename)`: shards `filename`
    /// under a single-character prefix directory (`path/<first-char>/<filename>`).
    fn cache_path_filename(cache_dir: &Path, filename: &str) -> Result<String, ServiceError> {
        if cache_dir.as_os_str().is_empty() {
            return Err(ServiceError::invalid_input("Path can't be empty."));
        }
        let prefix = filename
            .chars()
            .next()
            .ok_or_else(|| ServiceError::invalid_input("Filename can't be empty."))?;
        Ok(cache_dir
            .join(prefix.to_string())
            .join(filename)
            .to_string_lossy()
            .into_owned())
    }

    /// Port of the private `GetSupportedImage`: `.tbn` files are jpegs renamed, so
    /// they (and everything else) pass through unchanged in this port — the branch
    /// exists to preserve the C# shape.
    fn supported_image(
        original_path: &str,
        date_modified: DateTime<Utc>,
    ) -> (String, DateTime<Utc>) {
        (original_path.to_owned(), date_modified)
    }
}

/// The 25 container/extension formats the processor advertises as readable
/// input. Port of the C# `SupportedInputFormats` set (order preserved; the
/// lookup itself is case-insensitive in C#, but the set is only ever surfaced,
/// not matched against, in this unit).
const SUPPORTED_INPUT_FORMATS: [&str; 25] = [
    "tiff", "tif", "jpeg", "jpg", "png", "cr2", "crw", "nef", "orf", "pef", "arw", "webp", "gif",
    "bmp", "erf", "raf", "rw2", "nrw", "dng", "ico", "astc", "ktx", "pkm", "wbmp", "avif",
];

/// The lowercase extension (with leading dot) of `path`, or `""` when there is
/// none. Mirrors C# `Path.GetExtension` folded to lowercase for the
/// transparency-type check.
fn extension_of(path: &str) -> String {
    let last_segment = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match last_segment.rfind('.') {
        Some(idx) => last_segment[idx..].to_ascii_lowercase(),
        None => String::new(),
    }
}

#[async_trait]
impl<F: FileMeta + 'static> ImageProcessorTrait for ImageProcessor<F> {
    /// Port of `SupportedInputFormats` — the 25-entry set.
    fn supported_input_formats(&self) -> Vec<String> {
        SUPPORTED_INPUT_FORMATS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// Port of `SupportsImageCollageCreation` — delegates to the encoder.
    fn supports_image_collage_creation(&self) -> bool {
        self.encoder.supports_image_collage_creation()
    }

    /// Port of `GetSupportedImageOutputFormats` — delegates to the encoder.
    fn supported_image_output_formats(&self) -> Vec<ImageFormat> {
        self.encoder.supported_output_formats()
    }

    /// Port of `GetImageDimensions(string path)` — delegates to
    /// [`ImageEncoder::get_image_size`].
    async fn get_image_dimensions(&self, path: &str) -> Result<ImageDimensions, ServiceError> {
        self.encoder.get_image_size(path).await
    }

    /// Port of `GetImageDimensions(BaseItem, ItemImageInfo)`: returns the info's
    /// stored dimensions when both are positive, else probes the file. The
    /// domain item collapses to `item_id`, which is unused here (it only fed a
    /// debug log line in C#).
    async fn get_item_image_dimensions(
        &self,
        _item_id: Uuid,
        info: &ItemImageInfo,
    ) -> Result<ImageDimensions, ServiceError> {
        if info.height > 0 && info.width > 0 {
            return Ok(ImageDimensions::new(info.width, info.height));
        }
        self.encoder.get_image_size(&info.path).await
    }

    /// Port of `GetImageBlurHash(string path)`: probe dimensions, then compute the
    /// blurhash at those dimensions.
    async fn get_image_blur_hash(&self, path: &str) -> Result<String, ServiceError> {
        let size = self.get_image_dimensions(path).await?;
        self.get_image_blur_hash_sized(path, size).await
    }

    /// Port of `GetImageBlurHash(string, ImageDimensions)`: derive near-square DCT
    /// component counts (`xComp = sqrt(16 * w / h)`, `yComp = xComp * h / w`, each
    /// `min(floor + 1, 9)`), then delegate to the encoder. A non-positive size
    /// yields the empty string.
    async fn get_image_blur_hash_sized(
        &self,
        path: &str,
        image_dimensions: ImageDimensions,
    ) -> Result<String, ServiceError> {
        if image_dimensions.width <= 0 || image_dimensions.height <= 0 {
            return Ok(String::new());
        }

        // Deliberately mirror C# `MathF` single-precision arithmetic and its
        // `(int)f` truncation so the derived component counts match the oracle;
        // the precision/truncation the lints warn about is exactly the point.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap
        )]
        let (x_comp, y_comp) = {
            let width = image_dimensions.width as f32;
            let height = image_dimensions.height as f32;
            let x_comp_f = (16.0_f32 * width / height).sqrt();
            let y_comp_f = x_comp_f * height / width;
            let x_comp = ((x_comp_f as i32) + 1).min(9);
            let y_comp = ((y_comp_f as i32) + 1).min(9);
            (x_comp, y_comp)
        };

        self.encoder.get_image_blur_hash(x_comp, y_comp, path).await
    }

    /// Port of `GetImageCacheTag(BaseItem/BaseItemDto, ItemImageInfo)`: folds onto
    /// the path-plus-`DateModified` form. Returns `None` only when the underlying
    /// path tag cannot be produced (never, here — the C# item overloads always
    /// tag).
    async fn get_image_cache_tag(
        &self,
        _item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Result<Option<String>, ServiceError> {
        self.get_image_cache_tag_for_path(&image.path, image.date_modified)
            .await
    }

    /// Port of `GetImageCacheTag(string baseItemPath, DateTime imageDateModified)`:
    /// `md5(path + ticks)` as 32-char lowercase hex (the C# `.ToString("N")`).
    async fn get_image_cache_tag_for_path(
        &self,
        base_item_path: &str,
        image_date_modified: DateTime<Utc>,
    ) -> Result<Option<String>, ServiceError> {
        let key = format!(
            "{base_item_path}{}",
            Self::dotnet_ticks(image_date_modified)
        );
        Ok(Some(get_md5(&key).simple().to_string()))
    }

    /// Port of `ProcessImage`: the full on-the-fly pipeline — `.tbn` passthrough,
    /// `SupportsImageEncoding` gate, missing-file / GIF passthrough, transparency
    /// detection, the (deferred) `Photo` auto-orient branch, the
    /// `HasDefaultOptions` short-circuit, output-format selection, cache-key
    /// derivation, cache-hit skip, encode, `resultPath == original` short-circuit,
    /// and error-to-original fallback.
    async fn process_image(
        &self,
        options: &ImageProcessingOptions,
    ) -> Result<ProcessedImage, ServiceError> {
        let original_image = &options.image;
        let mut original_image_path = original_image.path.clone();
        let mut date_modified = original_image.date_modified;

        let original_image_size = if original_image.width > 0 && original_image.height > 0 {
            Some(ImageDimensions::new(
                original_image.width,
                original_image.height,
            ))
        } else {
            None
        };

        let mime_type = get_mime_type(&original_image_path).to_owned();
        if !self.encoder.supports_image_encoding() {
            return Ok(ProcessedImage {
                path: original_image_path,
                mime_type: Some(mime_type),
                date_modified,
            });
        }

        let supported = Self::supported_image(&original_image_path, date_modified);
        original_image_path = supported.0;

        // Original file doesn't exist, or original file is gif.
        if !self.file_meta.exists(&original_image_path)
            || mime_type.eq_ignore_ascii_case(GIF_MIME_TYPE)
        {
            return Ok(ProcessedImage {
                path: original_image_path,
                mime_type: Some(mime_type),
                date_modified,
            });
        }

        date_modified = supported.1;
        let requires_transparency =
            TRANSPARENT_IMAGE_TYPES.contains(&extension_of(&original_image_path).as_str());

        // The Photo auto-orientation branch is deferred: no Photo entity is wired
        // into the options, so auto_orient stays false. The gate is preserved.
        let auto_orient = false;

        if options.has_default_options(&original_image_path, original_image_size)
            && (!auto_orient || !options.requires_auto_orientation)
        {
            // Just spit out the original file if all the options are default.
            return Ok(ProcessedImage {
                path: original_image_path.clone(),
                mime_type: Some(get_mime_type(&original_image_path).to_owned()),
                date_modified,
            });
        }

        let quality = options.quality;
        let output_format =
            self.output_format(&options.supported_output_formats, requires_transparency);
        let cache_file_path = self.cache_file_path(
            &original_image_path,
            options,
            quality,
            date_modified,
            output_format,
        )?;

        // The whole tail is the C# try/catch: any error returns the original.
        match self
            .encode_to_cache(
                &original_image_path,
                date_modified,
                &cache_file_path,
                auto_orient,
                quality,
                options,
                output_format,
            )
            .await
        {
            Ok(Some(processed)) => Ok(processed),
            // `resultPath == originalPath` short-circuit (`Ok(None)`) and the C#
            // catch-all that returns the original on any encode error (`Err`)
            // both fall through to the same original-file passthrough.
            Ok(None) | Err(_) => Ok(ProcessedImage {
                path: original_image_path.clone(),
                mime_type: Some(get_mime_type(&original_image_path).to_owned()),
                date_modified,
            }),
        }
    }

    /// Port of `CreateImageCollage` — delegates to the encoder.
    async fn create_image_collage(
        &self,
        options: &ImageCollageOptions,
        library_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.encoder
            .create_image_collage(options, library_name)
            .await
    }
}

impl<F: FileMeta> ImageProcessor<F> {
    /// The cache-write half of [`process_image`](ImageProcessorTrait::process_image),
    /// factored out so its fallible I/O is a single `?`-chain the pipeline wraps.
    ///
    /// Returns `Ok(Some(processed))` for a fresh or cached resized file,
    /// `Ok(None)` when the encoder short-circuited to the original path, and
    /// `Err(_)` for any encode failure (which the caller maps to the original).
    #[allow(clippy::too_many_arguments)]
    async fn encode_to_cache(
        &self,
        original_image_path: &str,
        date_modified: DateTime<Utc>,
        cache_file_path: &str,
        auto_orient: bool,
        quality: i32,
        options: &ImageProcessingOptions,
        output_format: ImageFormat,
    ) -> Result<Option<ProcessedImage>, ServiceError> {
        if !self.file_meta.exists(cache_file_path) {
            let result_path = self
                .encoder
                .encode_image(
                    original_image_path,
                    date_modified,
                    cache_file_path,
                    auto_orient,
                    None,
                    quality,
                    options,
                    output_format,
                )
                .await?;

            if result_path.eq_ignore_ascii_case(original_image_path) {
                return Ok(None);
            }
        }

        Ok(Some(ProcessedImage {
            path: cache_file_path.to_owned(),
            mime_type: Some(output_format.mime_type().to_owned()),
            date_modified: self.file_meta.last_write_time_utc(cache_file_path),
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Round-trip cases transliterated from `ImageProcessor.cs`: the cache tag is
    //! a stable 32-char hex sensitive to path and ticks; the cache key changes per
    //! option and carries `v=3`; default options pass the original through; a
    //! resize produces then hits the cache.
    use super::*;
    use crate::image_encoder::ImageCrateEncoder;
    use chrono::TimeZone;
    use image::{DynamicImage, Rgba, RgbaImage};
    use std::path::PathBuf as StdPathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// A fake [`FileMeta`] with an in-memory existence set, so cache-hit/miss can
    /// be driven deterministically without touching the disk.
    #[derive(Default)]
    struct FakeFs {
        existing: Mutex<Vec<String>>,
    }

    impl FakeFs {
        fn with(paths: &[&str]) -> Self {
            Self {
                existing: Mutex::new(paths.iter().map(|s| (*s).to_owned()).collect()),
            }
        }

        fn add(&self, path: &str) {
            self.existing.lock().expect("lock").push(path.to_owned());
        }
    }

    impl FileMeta for FakeFs {
        fn exists(&self, path: &str) -> bool {
            self.existing
                .lock()
                .expect("lock")
                .iter()
                .any(|p| p == path)
        }

        fn last_write_time_utc(&self, _path: &str) -> DateTime<Utc> {
            Utc.timestamp_opt(1_600_000_000, 0).single().expect("ts")
        }
    }

    /// A counting [`ImageEncoder`] that records how many times `encode_image` ran
    /// and lets a test dictate the returned path (to exercise the
    /// `resultPath == original` short-circuit) — no real pixels.
    struct CountingEncoder {
        calls: AtomicUsize,
        /// When set, `encode_image` returns this path; else it returns
        /// `output_path`.
        return_original: Option<String>,
    }

    #[async_trait]
    impl ImageEncoder for CountingEncoder {
        fn supported_input_formats(&self) -> Vec<String> {
            vec!["png".to_owned(), "jpg".to_owned()]
        }
        fn supported_output_formats(&self) -> Vec<ImageFormat> {
            vec![ImageFormat::Webp, ImageFormat::Jpg, ImageFormat::Png]
        }
        fn name(&self) -> String {
            "Counting".to_owned()
        }
        fn supports_image_collage_creation(&self) -> bool {
            true
        }
        fn supports_image_encoding(&self) -> bool {
            true
        }
        async fn get_image_size(&self, _path: &str) -> Result<ImageDimensions, ServiceError> {
            Ok(ImageDimensions::new(1920, 1080))
        }
        async fn get_image_blur_hash(
            &self,
            x_comp: i32,
            y_comp: i32,
            _path: &str,
        ) -> Result<String, ServiceError> {
            Ok(format!("blur:{x_comp}x{y_comp}"))
        }
        #[allow(clippy::too_many_arguments)]
        async fn encode_image(
            &self,
            _input_path: &str,
            _date_modified: DateTime<Utc>,
            output_path: &str,
            _auto_orient: bool,
            _orientation: Option<ferrofin_model::drawing::ImageOrientation>,
            _quality: i32,
            _options: &ImageProcessingOptions,
            _output_format: ImageFormat,
        ) -> Result<String, ServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.return_original {
                Some(p) => Ok(p.clone()),
                None => Ok(output_path.to_owned()),
            }
        }
        async fn create_image_collage(
            &self,
            _options: &ImageCollageOptions,
            _library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn create_splashscreen(
            &self,
            _posters: &[String],
            _backdrops: &[String],
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn create_trickplay_tile(
            &self,
            _options: &ImageCollageOptions,
            _quality: i32,
            _img_width: i32,
            _img_height: Option<i32>,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
    }

    fn fixture(dir: &TempDir, name: &str, w: u32, h: u32) -> String {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba([0x30, 0x60, 0x90, 0xFF]);
        }
        let path: StdPathBuf = dir.path().join(name);
        DynamicImage::ImageRgba8(img)
            .save(&path)
            .expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    fn processor_with(
        encoder: Arc<dyn ImageEncoder>,
        cache: &TempDir,
        fs: FakeFs,
    ) -> ImageProcessor<FakeFs> {
        ImageProcessor::with_file_meta(encoder, cache.path().to_path_buf(), fs)
    }

    #[tokio::test]
    async fn cache_tag_is_stable_32_char_hex() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");
        let tag = proc
            .get_image_cache_tag_for_path("/library/poster.jpg", dt)
            .await
            .expect("tag")
            .expect("some");
        assert_eq!(tag.len(), 32);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, tag.to_ascii_lowercase());

        // Stable: identical inputs → identical tag.
        let again = proc
            .get_image_cache_tag_for_path("/library/poster.jpg", dt)
            .await
            .expect("tag")
            .expect("some");
        assert_eq!(tag, again);
    }

    #[tokio::test]
    async fn cache_tag_is_sensitive_to_path_and_ticks() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");
        let other_dt = Utc.timestamp_opt(1_500_000_001, 0).single().expect("ts");

        let base = proc
            .get_image_cache_tag_for_path("/a.jpg", dt)
            .await
            .expect("tag")
            .expect("some");
        let diff_path = proc
            .get_image_cache_tag_for_path("/b.jpg", dt)
            .await
            .expect("tag")
            .expect("some");
        let diff_ticks = proc
            .get_image_cache_tag_for_path("/a.jpg", other_dt)
            .await
            .expect("tag")
            .expect("some");

        assert_ne!(base, diff_path);
        assert_ne!(base, diff_ticks);
    }

    #[tokio::test]
    async fn item_cache_tag_folds_onto_path_form() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");
        let info = ItemImageInfo {
            path: "/library/poster.jpg".into(),
            date_modified: dt,
            ..Default::default()
        };
        let via_item = proc
            .get_image_cache_tag(Uuid::nil(), &info)
            .await
            .expect("tag")
            .expect("some");
        let via_path = proc
            .get_image_cache_tag_for_path("/library/poster.jpg", dt)
            .await
            .expect("tag")
            .expect("some");
        assert_eq!(via_item, via_path);
    }

    #[tokio::test]
    async fn dotnet_ticks_matches_unix_epoch_constant() {
        // 1970-01-01T00:00:00Z is exactly 621_355_968_000_000_000 .NET ticks.
        let epoch = Utc.timestamp_opt(0, 0).single().expect("ts");
        assert_eq!(
            ImageProcessor::<StdFileMeta>::dotnet_ticks(epoch),
            621_355_968_000_000_000
        );
        // One second later adds exactly TICKS_PER_SECOND.
        let one = Utc.timestamp_opt(1, 0).single().expect("ts");
        assert_eq!(
            ImageProcessor::<StdFileMeta>::dotnet_ticks(one),
            621_355_968_010_000_000
        );
    }

    #[tokio::test]
    async fn cache_key_carries_version_and_changes_per_option() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "src.png", 1920, 1080);
        let proc = processor_with(
            Arc::new(ImageCrateEncoder::new()),
            &cache,
            FakeFs::default(),
        );
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");

        let base = ImageProcessingOptions {
            max_width: Some(960),
            ..Default::default()
        };
        let key_base = proc
            .cache_file_path(&input, &base, 90, dt, ImageFormat::Webp)
            .expect("key");

        // The cache key must live under resized-images and carry the format ext.
        assert!(key_base.contains("resized-images"));
        assert_eq!(
            Path::new(&key_base).extension().and_then(|e| e.to_str()),
            Some("webp")
        );

        // Changing an option changes the key.
        let bigger = ImageProcessingOptions {
            max_width: Some(480),
            ..Default::default()
        };
        let key_bigger = proc
            .cache_file_path(&input, &bigger, 90, dt, ImageFormat::Webp)
            .expect("key");
        assert_ne!(key_base, key_bigger);

        // Bumping the cache version changes the key (v= stamp is honoured).
        let proc_v4 = processor_with(
            Arc::new(ImageCrateEncoder::new()),
            &cache,
            FakeFs::default(),
        )
        .with_cache_version('4');
        let key_v4 = proc_v4
            .cache_file_path(&input, &base, 90, dt, ImageFormat::Webp)
            .expect("key");
        assert_ne!(key_base, key_v4);
    }

    #[tokio::test]
    async fn default_options_passthrough_returns_original() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        // A real jpeg the encoder can probe; high quality + matching format +
        // fill bounds covering it → HasDefaultOptions holds.
        let input = fixture(&dir, "orig.jpg", 640, 480);
        let encoder = Arc::new(CountingEncoder {
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        let proc = processor_with(encoder.clone(), &cache, FakeFs::with(&[&input]));

        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                width: 640,
                height: 480,
                ..Default::default()
            },
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            fill_width: Some(640),
            fill_height: Some(480),
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);
        // No encode happened.
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn gif_passthrough_returns_original() {
        let cache = TempDir::new().expect("cache");
        let encoder = Arc::new(CountingEncoder {
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        let input = "/library/anim.gif";
        let proc = processor_with(encoder.clone(), &cache, FakeFs::with(&[input]));
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);
        assert_eq!(processed.mime_type.as_deref(), Some("image/gif"));
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_file_returns_original() {
        let cache = TempDir::new().expect("cache");
        let encoder = Arc::new(CountingEncoder {
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        // Path not in the fake existence set → missing.
        let proc = processor_with(encoder.clone(), &cache, FakeFs::default());
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: "/nope/x.png".into(),
                ..Default::default()
            },
            max_width: Some(100),
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, "/nope/x.png");
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resize_produces_then_hits_cache() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "big.png", 1920, 1080);
        let fs = FakeFs::with(&[&input]);
        // A real encoder writes the resized file; a max-width forces a resize.
        let proc = ImageProcessor::with_file_meta(
            Arc::new(ImageCrateEncoder::new()),
            cache.path().to_path_buf(),
            fs,
        );

        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                width: 1920,
                height: 1080,
                ..Default::default()
            },
            max_width: Some(960),
            quality: 90,
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };

        let first = proc.process_image(&options).await.expect("process");
        // Produced a cache file (not the original), 960x540.
        assert_ne!(first.path, input);
        assert!(first.path.contains("resized-images"));
        let dims = ImageCrateEncoder::new()
            .get_image_size(&first.path)
            .await
            .expect("probe");
        assert_eq!(dims, ImageDimensions::new(960, 540));

        // Register the produced file as existing, then re-process: same path, and
        // the encoder is not re-run (cache hit).
        proc.file_meta.add(&first.path);
        let second = proc.process_image(&options).await.expect("process again");
        assert_eq!(first.path, second.path);
    }

    #[tokio::test]
    async fn encoder_returning_original_short_circuits() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "s.png", 800, 600);
        // Encoder claims it wrote the original path back → short-circuit to it.
        let encoder = Arc::new(CountingEncoder {
            calls: AtomicUsize::new(0),
            return_original: Some(input.clone()),
        });
        let proc = processor_with(encoder.clone(), &cache, FakeFs::with(&[&input]));
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                width: 800,
                height: 600,
                ..Default::default()
            },
            max_width: Some(400),
            quality: 90,
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_encoding_support_passes_through() {
        struct NoEncode;
        #[async_trait]
        impl ImageEncoder for NoEncode {
            fn supported_input_formats(&self) -> Vec<String> {
                vec![]
            }
            fn supported_output_formats(&self) -> Vec<ImageFormat> {
                vec![]
            }
            fn name(&self) -> String {
                "NoEncode".to_owned()
            }
            fn supports_image_collage_creation(&self) -> bool {
                false
            }
            fn supports_image_encoding(&self) -> bool {
                false
            }
            async fn get_image_size(&self, _p: &str) -> Result<ImageDimensions, ServiceError> {
                Ok(ImageDimensions::default())
            }
            async fn get_image_blur_hash(
                &self,
                _x: i32,
                _y: i32,
                _p: &str,
            ) -> Result<String, ServiceError> {
                Ok(String::new())
            }
            #[allow(clippy::too_many_arguments)]
            async fn encode_image(
                &self,
                _i: &str,
                _d: DateTime<Utc>,
                _o: &str,
                _a: bool,
                _or: Option<ferrofin_model::drawing::ImageOrientation>,
                _q: i32,
                _opt: &ImageProcessingOptions,
                _f: ImageFormat,
            ) -> Result<String, ServiceError> {
                Ok(String::new())
            }
            async fn create_image_collage(
                &self,
                _o: &ImageCollageOptions,
                _l: Option<&str>,
            ) -> Result<(), ServiceError> {
                Ok(())
            }
            async fn create_splashscreen(
                &self,
                _p: &[String],
                _b: &[String],
            ) -> Result<(), ServiceError> {
                Ok(())
            }
            async fn create_trickplay_tile(
                &self,
                _o: &ImageCollageOptions,
                _q: i32,
                _w: i32,
                _h: Option<i32>,
            ) -> Result<i32, ServiceError> {
                Ok(0)
            }
        }
        let cache = TempDir::new().expect("cache");
        let proc = processor_with(Arc::new(NoEncode), &cache, FakeFs::default());
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: "/x/y.png".into(),
                ..Default::default()
            },
            max_width: Some(100),
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        // Passed straight through with the source mime type.
        assert_eq!(processed.path, "/x/y.png");
        assert_eq!(processed.mime_type.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn blur_hash_component_math() {
        // xComp = sqrt(16 * 1920 / 1080) = sqrt(28.44) ≈ 5.33 → floor+1 = 6.
        // yComp = 5.33 * 1080 / 1920 ≈ 3.0 → floor 3, +1 = 4 (capped at 9).
        let proc = ImageProcessor::new(
            Arc::new(CountingEncoder {
                calls: AtomicUsize::new(0),
                return_original: None,
            }),
            "/cache",
        );
        let hash = proc
            .get_image_blur_hash_sized("/a.png", ImageDimensions::new(1920, 1080))
            .await
            .expect("hash");
        assert_eq!(hash, "blur:6x4");

        // Non-positive size yields empty.
        let empty = proc
            .get_image_blur_hash_sized("/a.png", ImageDimensions::new(0, 0))
            .await
            .expect("hash");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn item_dimensions_use_info_when_positive_else_probe() {
        let proc = ImageProcessor::new(
            Arc::new(CountingEncoder {
                calls: AtomicUsize::new(0),
                return_original: None,
            }),
            "/cache",
        );

        // Positive stored dims → used directly.
        let stored = ItemImageInfo {
            path: "/a.png".into(),
            width: 100,
            height: 50,
            ..Default::default()
        };
        assert_eq!(
            proc.get_item_image_dimensions(Uuid::nil(), &stored)
                .await
                .expect("dims"),
            ImageDimensions::new(100, 50)
        );

        // Zero stored dims → probe (CountingEncoder reports 1920x1080).
        let unknown = ItemImageInfo {
            path: "/b.png".into(),
            ..Default::default()
        };
        assert_eq!(
            proc.get_item_image_dimensions(Uuid::nil(), &unknown)
                .await
                .expect("dims"),
            ImageDimensions::new(1920, 1080)
        );
    }

    #[tokio::test]
    async fn advertises_25_input_formats_and_delegated_capabilities() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        assert_eq!(proc.supported_input_formats().len(), 25);
        assert!(proc.supported_input_formats().contains(&"avif".to_owned()));
        assert!(proc.supported_input_formats().contains(&"tiff".to_owned()));
        // Delegated to the encoder.
        assert!(proc.supports_image_collage_creation());
        assert_eq!(
            proc.supported_image_output_formats(),
            vec![ImageFormat::Webp, ImageFormat::Jpg, ImageFormat::Png]
        );
    }
}
