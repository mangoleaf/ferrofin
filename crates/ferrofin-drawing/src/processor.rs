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
//!   [`image_cache_path`](ImageProcessor::new) the processor actually uses.
//!   The host supplies the startup `ParallelImageEncodingLimit` through
//!   [`ImageProcessor::with_encoding_limit`], matching the C# constructor.
//! - The overloaded `GetImageDimensions` / `GetImageCacheTag` methods collapse
//!   onto the id-plus-[`ItemImageInfo`] forms the trait declares.
//! - The `Photo` auto-orientation branch is decided by the caller, which read
//!   the item: [`ImageProcessingOptions`] carries an
//!   [`item_id`](ImageProcessingOptions::item_id), not a domain item, so the
//!   `autoOrient` the C# derives from `item is Photo` arrives as
//!   [`auto_orient`](ImageProcessingOptions::auto_orient).
//! - `MimeTypes.GetMimeType` is reused from [`ferrofin_model::net::mime_types`];
//!   `GetMD5().ToString("N")` is reused from [`ferrofin_common::extensions::get_md5`]
//!   plus [`Uuid::simple`] (32-char lowercase hex, identical to .NET `"N"`).
//! - The filesystem reads the pipeline needs (`File.Exists`, `GetLastWriteTimeUtc`)
//!   sit behind the small [`FileMeta`] seam so tests use a fake and the real
//!   `std::fs` stat stays out of the parity/coverage numbers. Its calls block,
//!   so the processor makes them on the blocking pool, never on the runtime
//!   worker serving the request.

use ferrofin_util::directory_path::DirectoryPath;
use ferrofin_util::keyed_locks::KeyedLocks;
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
/// Port of the C# `private const char Version` (`'4'` at v12.4): bump it to
/// invalidate all previously-cached resized images.
///
/// Ferrofin moved to `'4'` while upstream was still at `'3'`, when its JPEG
/// encoder moved from 4:4:4 to Jellyfin's 4:2:0 chroma subsampling with
/// optimized Huffman tables: every image cached by an earlier Ferrofin was
/// stale, and without the bump an upgraded install would keep serving the old
/// bytes from disk forever. The key is Ferrofin-local (its own cache directory)
/// and never reaches the wire, so it is not part of the HTTP contract.
const DEFAULT_CACHE_VERSION: char = '4';

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
///
/// Both methods may block, for as long as a stat on a cold or hung network
/// share takes. [`ImageProcessor`] only calls them through
/// [`tokio::task::spawn_blocking`], so an implementation never needs to be
/// async.
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
    /// Held until actual pixel work finishes, including after request cancellation.
    encoding_slots: Arc<tokio::sync::Semaphore>,
    /// One lock per cache file being encoded, held as long as an encoding slot,
    /// so concurrent misses for one resized image encode it once (gl-1300).
    cache_file_locks: KeyedLocks,

    /// The base image-cache directory (`IServerApplicationPaths.ImageCachePath`).
    image_cache_path: DirectoryPath,

    /// The filesystem-metadata seam (`File.Exists` / `GetLastWriteTimeUtc`),
    /// shared with the blocking-pool tasks that call it.
    file_meta: Arc<F>,

    /// The cache-invalidation version stamped into every cache key: upstream's
    /// `Version` constant, [`DEFAULT_CACHE_VERSION`]. Only tests override it.
    cache_version: char,
}

impl ImageProcessor<StdFileMeta> {
    /// Constructs an [`ImageProcessor`] over `encoder`, caching resized images
    /// beneath `image_cache_path`, using the real `std::fs`-backed [`FileMeta`]
    /// and the cache version `'4'` (upstream's `Version`).
    #[must_use]
    pub fn new(encoder: Arc<dyn ImageEncoder>, image_cache_path: impl Into<DirectoryPath>) -> Self {
        Self {
            encoder,
            encoding_slots: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            cache_file_locks: KeyedLocks::new(),
            image_cache_path: image_cache_path.into(),
            file_meta: Arc::new(StdFileMeta),
            cache_version: DEFAULT_CACHE_VERSION,
        }
    }
}

impl<F: FileMeta> ImageProcessor<F> {
    /// Constructs an [`ImageProcessor`] with an explicit [`FileMeta`] seam, so a
    /// test can supply a fake filesystem.
    #[cfg(test)]
    #[must_use]
    pub fn with_file_meta(
        encoder: Arc<dyn ImageEncoder>,
        image_cache_path: impl Into<DirectoryPath>,
        file_meta: F,
    ) -> Self {
        Self {
            encoder,
            encoding_slots: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            cache_file_locks: KeyedLocks::new(),
            image_cache_path: image_cache_path.into(),
            file_meta: Arc::new(file_meta),
            cache_version: DEFAULT_CACHE_VERSION,
        }
    }

    /// Sets the startup ceiling for concurrent resized-image encodes.
    /// The host resolves nonpositive dashboard values to its usable CPU count.
    /// Zero at this lower-level seam clamps to one to avoid a deadlock.
    #[must_use]
    pub fn with_encoding_limit(mut self, limit: usize) -> Self {
        self.encoding_slots = Arc::new(tokio::sync::Semaphore::new(limit.max(1)));
        self
    }

    /// Overrides the `cache_version` (default `DEFAULT_CACHE_VERSION`) stamped
    /// into cache keys. Bumping it invalidates every previously-cached resized image.
    #[cfg(test)]
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

        // Ferrofin-local, not in upstream's key: earlier Ferrofin builds cached
        // photos un-oriented under the same key, so an oriented encode needs
        // its own. Only auto-oriented photos are re-encoded.
        if options.auto_orient {
            filename.push_str(",autoorient");
        }
        // Ferrofin-local too: earlier builds re-encoded a `blur` request
        // unblurred under the same key (gl-805), so a drawn blur needs its own.
        if options.blur.is_some_and(|blur| blur > 0) {
            filename.push_str(",blurdrawn");
        }
        // And earlier builds wrote a JPEG of an image with alpha with its
        // straight colour, where Skia's (and now Ferrofin's) is premultiplied:
        // any source but a JPEG may carry alpha.
        if format == ImageFormat::Jpg
            && !matches!(extension_of(original_path).as_str(), ".jpg" | ".jpeg")
        {
            filename.push_str(",premultiplied");
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
/// input, without the dot. Port of `ImageProcessor.SupportedInputFormats`
/// (order preserved), which `PhotoResolver.IsImageFile` tests a file's
/// extension against, case-insensitively, to decide it is a photo. Not the
/// encoder's list, which is `image_encoder`'s own.
pub const SUPPORTED_INPUT_FORMATS: [&str; 25] = [
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

/// Whether an encode failure is logged at `debug` rather than `warn`.
///
/// `InvalidInput` is the requested size or the file's content as the encoder
/// sees it: a size that rounds an axis to 0, a source the decoder rejects
/// (corrupt, truncated, a read error mid-decode), one over the
/// [`ImageLimits`](crate::ImageLimits), a resize past their budget, a JPEG
/// output past 65535 px, or a WebP output past 16383 px. A failed encode writes no
/// cache file, so it recurs on every request for that image and its volume
/// grows with items × requests; LOGGING.md §1 keeps that at `debug`.
/// Everything else (cache-dir I/O, an unreadable or removed source, codec or
/// encoder faults) is a degradation an operator can act on: `warn`, as a
/// fallback was taken.
fn encode_failure_logs_at_debug(err: &ServiceError) -> bool {
    matches!(err, ServiceError::InvalidInput(_))
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

    /// Port of `GetImageCacheTag(BaseItem/BaseItemDto, ItemImageInfo) =>
    /// GetImageCacheTag(item.Path, image.DateModified)`: the owning item's
    /// path (a `null` one concatenates as the empty string) with the image's
    /// modification time. Always tags, as the C# item overloads do.
    async fn get_image_cache_tag(
        &self,
        item_path: Option<&str>,
        image: &ItemImageInfo,
    ) -> Result<Option<String>, ServiceError> {
        self.get_image_cache_tag_for_path(item_path.unwrap_or_default(), image.date_modified)
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
    /// detection, the `Photo` auto-orient branch (decided by the caller), the
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

        let requires_transparency =
            TRANSPARENT_IMAGE_TYPES.contains(&extension_of(&original_image_path).as_str());

        // `autoOrient` for a Photo; the caller, which read the item, sets it.
        let auto_orient = options.auto_orient;
        let default_options = options
            .has_default_options(&original_image_path, original_image_size)
            && (!auto_orient || !options.requires_auto_orientation);
        let output_format =
            self.output_format(&options.supported_output_formats, requires_transparency);
        // The cache key is known before any stat, so `File.Exists(original)`,
        // then `File.Exists(cacheFilePath)` and its time, take one blocking-pool
        // hop (gl-1258). The cache file is stat'd only where upstream does: the
        // original exists and the options are not default.
        let cache_file_path = (!default_options).then(|| {
            self.cache_file_path(
                &original_image_path,
                options,
                options.quality,
                supported.1,
                output_format,
            )
        });
        // Original file is gif, or original file doesn't exist. Either way the
        // original is served, so the GIF test goes first and a GIF costs no stat.
        let (original_exists, cached) = if mime_type.eq_ignore_ascii_case(GIF_MIME_TYPE) {
            (false, None)
        } else {
            self.stat_original_and_cache(
                &original_image_path,
                cache_file_path.as_ref().and_then(|path| path.as_ref().ok()),
            )
            .await?
        };
        if !original_exists {
            return Ok(ProcessedImage {
                path: original_image_path,
                mime_type: Some(mime_type),
                date_modified,
            });
        }

        date_modified = supported.1;
        let Some(cache_file_path) = cache_file_path else {
            // Just spit out the original file if all the options are default.
            return Ok(ProcessedImage {
                path: original_image_path.clone(),
                mime_type: Some(get_mime_type(&original_image_path).to_owned()),
                date_modified,
            });
        };
        let cache_file_path = cache_file_path?;

        // The whole tail is the C# try/catch: any error returns the original.
        match self
            .encode_to_cache(
                &original_image_path,
                date_modified,
                &cache_file_path,
                cached,
                auto_orient,
                options.quality,
                options,
                output_format,
            )
            .await
        {
            Ok(Some(processed)) => Ok(processed),
            // `resultPath == originalPath` short-circuit (`Ok(None)`) and the C#
            // catch-all that returns the original on any encode error (`Err`)
            // both fall through to the same original-file passthrough. Upstream
            // logs the catch at Error; see `encode_failure_logs_at_debug` for
            // why the level depends on the failure here.
            result => {
                if let Err(e) = result {
                    if encode_failure_logs_at_debug(&e) {
                        tracing::debug!(
                            error = %e,
                            path = %original_image_path,
                            "image encode rejected; serving the original"
                        );
                    } else {
                        tracing::warn!(
                            error = %e,
                            path = %original_image_path,
                            "error encoding image; serving the original"
                        );
                    }
                }
                Ok(ProcessedImage {
                    path: original_image_path.clone(),
                    mime_type: Some(get_mime_type(&original_image_path).to_owned()),
                    date_modified,
                })
            }
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

impl<F: FileMeta + 'static> ImageProcessor<F> {
    /// Runs `call` against the [`FileMeta`] seam on the blocking pool, so a stat
    /// that hangs (a cold or unreachable network share) parks a blocking-pool
    /// thread rather than the runtime worker, and every other request scheduled
    /// on that worker. `tokio::fs`, which serves the image afterwards, does the
    /// same. Fails only when `call` panicked or the runtime shutting down
    /// cancelled it.
    async fn stat<T: Send + 'static>(
        &self,
        call: impl FnOnce(&F) -> T + Send + 'static,
    ) -> Result<T, ServiceError> {
        let meta = Arc::clone(&self.file_meta);
        tokio::task::spawn_blocking(move || call(&meta))
            .await
            .map_err(ServiceError::backend_source)
    }

    /// `File.Exists(original)`, and when it holds and `cache` is given,
    /// `File.Exists(cache)` with its `GetLastWriteTimeUtc`: one [`Self::stat`].
    async fn stat_original_and_cache(
        &self,
        original: &str,
        cache: Option<&String>,
    ) -> Result<(bool, Option<DateTime<Utc>>), ServiceError> {
        let original = original.to_owned();
        let cache = cache.cloned();
        self.stat(move |meta| {
            let exists = meta.exists(&original);
            let cached = cache
                .filter(|_| exists)
                .and_then(|path| meta.exists(&path).then(|| meta.last_write_time_utc(&path)));
            (exists, cached)
        })
        .await
    }

    /// The cache-write half of [`process_image`](ImageProcessorTrait::process_image),
    /// factored out so its fallible I/O is a single `?`-chain the pipeline wraps.
    ///
    /// `cached` is the cache file's `GetLastWriteTimeUtc` when
    /// `File.Exists(cacheFilePath)`, which the caller stat'd with the original.
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
        cached: Option<DateTime<Utc>>,
        auto_orient: bool,
        quality: i32,
        options: &ImageProcessingOptions,
        output_format: ImageFormat,
    ) -> Result<Option<ProcessedImage>, ServiceError> {
        // `None` is a cache miss: encode, then read the new file's time.
        let date_written = 'written: {
            if let Some(date_written) = cached {
                break 'written date_written;
            }
            // One encode per cache file at a time (gl-1300). A miss that finds
            // the file's lock held waits for that encode, then serves the file
            // it wrote, or encodes for itself when it wrote none (it failed, or
            // served the original). Upstream has no such lock.
            // shortcut: an uncontended lock trusts the stat made with the
            // original (one hop, gl-1258), so an encode that ended in between
            // runs again, as before this lock; re-stat here too if that shows.
            let lock = self.cache_file_locks.get(cache_file_path);
            let guard = if let Ok(guard) = Arc::clone(&lock).try_lock_owned() {
                guard
            } else {
                let guard = lock.lock_owned().await;
                let path = cache_file_path.to_owned();
                let written = self
                    .stat(move |meta| meta.exists(&path).then(|| meta.last_write_time_utc(&path)))
                    .await?;
                if let Some(date_written) = written {
                    break 'written date_written;
                }
                guard
            };
            let permit = Arc::clone(&self.encoding_slots)
                .acquire_owned()
                .await
                .map_err(ServiceError::backend_source)?;
            let encoder = Arc::clone(&self.encoder);
            let input = original_image_path.to_owned();
            let output = cache_file_path.to_owned();
            let options = options.clone();
            // Dropping a request cannot stop spawn_blocking pixel work. Keep
            // its permit and the file's lock in an independent task until the
            // encoder really ends, so a waiter never starts a second encode.
            let result_path = tokio::spawn(async move {
                let _held = (permit, guard);
                encoder
                    .encode_image(
                        &input,
                        date_modified,
                        &output,
                        auto_orient,
                        None,
                        quality,
                        &options,
                        output_format,
                    )
                    .await
            })
            .await
            .map_err(ServiceError::backend_source)??;

            if result_path.eq_ignore_ascii_case(original_image_path) {
                return Ok(None);
            }
            let path = cache_file_path.to_owned();
            self.stat(move |meta| meta.last_write_time_utc(&path))
                .await?
        };

        Ok(Some(ProcessedImage {
            path: cache_file_path.to_owned(),
            mime_type: Some(output_format.mime_type().to_owned()),
            date_modified: date_written,
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Round-trip cases transliterated from `ImageProcessor.cs`: the cache tag is
    //! a stable 32-char hex sensitive to path and ticks; the cache key changes per
    //! option and carries `v=` and [`DEFAULT_CACHE_VERSION`]; default options pass
    //! the original through; a resize produces then hits the cache.
    use super::*;
    use crate::error::DrawingError;
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
        work: Option<Arc<EncodingWork>>,
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
            if let Some(work) = &self.work {
                work.entered.add_permits(1);
                let gate = Arc::clone(work);
                tokio::task::spawn_blocking(move || {
                    let released = gate.release.0.lock().unwrap();
                    let _released = gate.release.1.wait_while(released, |v| !*v).unwrap();
                })
                .await
                .unwrap();
                if work.fail.load(Ordering::SeqCst) {
                    return Err(ServiceError::invalid_input("corrupt source"));
                }
            }
            if let Some(p) = &self.return_original {
                return Ok(p.clone());
            }
            // Written, so a processor on the real filesystem finds the file.
            let output = Path::new(output_path);
            std::fs::create_dir_all(output.parent().expect("parent")).expect("mkdir");
            std::fs::write(output, b"encoded").expect("write");
            Ok(output_path.to_owned())
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
            _full_chroma: bool,
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

    /// gl-849: `GetImageCacheTag(item, image)` hashes the OWNING item's path
    /// with the image's `DateModified` ticks, never the image file's path.
    /// Oracles recomputed from a Jellyfin 12.2 database and matching the tags
    /// Jellyfin served for it: a movie (its Primary, at
    /// `/media/movies/Burning Dance (1953)/poster.jpg`, and its Backdrop share
    /// a date, so one tag), and the genre "Action", whose stored
    /// `%MetadataPath%/Genre/Action` is hashed expanded, not as the token,
    /// nor as its poster's path.
    #[rstest::rstest]
    #[case::movie(
        Some("/media/movies/Burning Dance (1953)/Burning Dance (1953).mkv"),
        "/media/movies/Burning Dance (1953)/poster.jpg",
        1_792_815_355,
        335_431_200,
        "e79799f797e91bedf7e11ed8fbf41656"
    )]
    #[case::genre(
        Some("/config/metadata/Genre/Action"),
        "/config/metadata/library/ce/ce06903d834d2c3417e0889dd4049f3b/poster.png",
        1_788_387_607,
        559_763_800,
        "0184e30b688e2995b166eda5317c526c"
    )]
    #[tokio::test]
    async fn item_cache_tag_hashes_the_item_path(
        #[case] item_path: Option<&str>,
        #[case] image_path: &str,
        #[case] secs: i64,
        #[case] nanos: u32,
        #[case] expected: &str,
    ) {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let info = ItemImageInfo {
            path: image_path.into(),
            date_modified: Utc.timestamp_opt(secs, nanos).single().expect("ts"),
            ..Default::default()
        };
        let tag = proc
            .get_image_cache_tag(item_path, &info)
            .await
            .expect("tag")
            .expect("some");
        assert_eq!(tag, expected);
        let image_path_tag = proc
            .get_image_cache_tag_for_path(image_path, info.date_modified)
            .await
            .expect("tag")
            .expect("some");
        assert_ne!(tag, image_path_tag, "the image's own path is not hashed");
    }

    /// A path-less item hashes `null + ticks`, which C# concatenates as the
    /// ticks alone.
    #[tokio::test]
    async fn item_cache_tag_of_a_path_less_item_hashes_the_ticks_alone() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");
        let info = ItemImageInfo {
            path: "https://example.org/logo.png".into(),
            date_modified: dt,
            ..Default::default()
        };
        assert_eq!(
            proc.get_image_cache_tag(None, &info).await.expect("tag"),
            proc.get_image_cache_tag_for_path("", dt)
                .await
                .expect("tag"),
        );
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
        // Pick a version that is not the current default, so bumping
        // DEFAULT_CACHE_VERSION can never make this assert compare a key
        // against itself and silently stop testing anything.
        let other_version = if DEFAULT_CACHE_VERSION == '9' {
            '8'
        } else {
            '9'
        };
        let proc_other = processor_with(
            Arc::new(ImageCrateEncoder::new()),
            &cache,
            FakeFs::default(),
        )
        .with_cache_version(other_version);
        let key_other = proc_other
            .cache_file_path(&input, &base, 90, dt, ImageFormat::Webp)
            .expect("key");
        assert_ne!(key_base, key_other);
    }

    #[tokio::test]
    async fn default_options_passthrough_returns_original() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        // A real jpeg the encoder can probe; high quality + matching format +
        // fill bounds covering it → HasDefaultOptions holds (as an unset fill
        // would: see `bounds_the_image_already_meets_serve_the_original`).
        let input = fixture(&dir, "orig.jpg", 640, 480);
        let encoder = Arc::new(CountingEncoder {
            work: None,
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

        // `autoOrient` (a photo) defeats the passthrough, as upstream, and the
        // oriented encode is cached under its own key.
        let oriented = ImageProcessingOptions {
            auto_orient: true,
            ..options.clone()
        };
        let processed = proc.process_image(&oriented).await.expect("process");
        assert_ne!(processed.path, input);
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 1);
        let dt = Utc::now();
        assert_ne!(
            proc.cache_file_path(&input, &options, 95, dt, ImageFormat::Jpg)
                .expect("key"),
            proc.cache_file_path(&input, &oriented, 95, dt, ImageFormat::Jpg)
                .expect("key")
        );
    }

    /// Earlier builds cached a `blur` request unblurred, and a transparent
    /// image's JPEG with its straight colour, under upstream's key: a drawn
    /// blur and a premultiplied JPEG each take a token of their own.
    #[test]
    fn drawn_blurs_and_premultiplied_jpegs_have_their_own_keys() {
        let proc = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), "/cache");
        let dt = Utc.timestamp_opt(1_500_000_000, 0).single().expect("ts");
        let key = |path: &str, blur: Option<i32>| {
            let options = ImageProcessingOptions {
                blur,
                ..Default::default()
            };
            proc.cache_file_path(path, &options, 90, dt, ImageFormat::Jpg)
                .expect("key")
        };
        let upstream = |path: &str, blur: &str| {
            let ticks = ImageProcessor::<StdFileMeta>::dotnet_ticks(dt);
            let filename = format!(
                "{path},quality=90,datemodified={ticks},f=Jpg{blur},v={DEFAULT_CACHE_VERSION}"
            );
            ImageProcessor::<StdFileMeta>::cache_path_unique(
                Path::new("/cache/resized-images"),
                &filename,
                ".jpg",
            )
            .expect("key")
        };
        assert_eq!(key("/a.jpg", None), upstream("/a.jpg", ""));
        assert_eq!(key("/a.jpg", Some(0)), upstream("/a.jpg", ",blur=0"));
        assert_ne!(key("/a.jpg", Some(3)), upstream("/a.jpg", ",blur=3"));
        assert_ne!(key("/a.png", None), upstream("/a.png", ""));
    }

    #[tokio::test]
    async fn bounds_the_image_already_meets_serve_the_original() {
        // gl-1117: `?maxWidth=2000` on a 640-wide poster, no fill requested.
        // Upstream's lifted `int?` fill comparison passes, so ProcessImage
        // serves the stored file without an encode.
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "poster.jpg", 640, 480);
        let encoder = Arc::new(CountingEncoder {
            work: None,
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
            quality: 96,
            supported_output_formats: vec![ImageFormat::Jpg],
            max_width: Some(2000),
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 0);
    }

    #[rstest::rstest]
    #[case::bad_size_or_source(ServiceError::invalid_input("cannot resize to 0x1"), true)]
    #[case::cache_write(DrawingError::io("mkdir", std::io::Error::other("full")).into(), false)]
    #[case::encoder_backend(ServiceError::backend("encoder panicked"), false)]
    #[case::vanished_source(ServiceError::not_found("image file not found: /x.png"), false)]
    fn encode_failure_level_policy(#[case] err: ServiceError, #[case] at_debug: bool) {
        assert_eq!(encode_failure_logs_at_debug(&err), at_debug);
    }

    #[tokio::test]
    async fn cache_write_failure_serves_original() {
        // The warn arm: a file where the resized-images directory belongs makes
        // the encoder's mkdir fail (ENOTDIR, even as root), and the C#
        // catch-all serves the original.
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        std::fs::write(cache.path().join("resized-images"), b"").expect("blocker");
        let input = fixture(&dir, "orig.png", 60, 40);
        let proc = processor_with(
            Arc::new(ImageCrateEncoder::new()),
            &cache,
            FakeFs::with(&[&input]),
        );
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                width: 60,
                height: 40,
                ..Default::default()
            },
            max_width: Some(30),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);

        // The blocker was the only cause: without it the same request encodes.
        std::fs::remove_file(cache.path().join("resized-images")).expect("unblock");
        let processed = proc.process_image(&options).await.expect("process");
        assert!(processed.path.contains("resized-images"));
    }

    #[tokio::test]
    async fn zero_axis_resize_serves_original() {
        // The encoder fails on a size that rounds one axis to 0 (Skia's empty
        // surface), and the C# catch-all serves the original.
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "orig.png", 60, 40);
        let proc = processor_with(
            Arc::new(ImageCrateEncoder::new()),
            &cache,
            FakeFs::with(&[&input]),
        );
        let options = ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                width: 60,
                height: 40,
                ..Default::default()
            },
            width: Some(1),
            height: Some(i32::MAX),
            max_height: Some(1_000_000_000),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let processed = proc.process_image(&options).await.expect("process");
        assert_eq!(processed.path, input);
    }

    #[tokio::test]
    async fn gif_passthrough_returns_original() {
        let cache = TempDir::new().expect("cache");
        let encoder = Arc::new(CountingEncoder {
            work: None,
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
            work: None,
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
        let current = Arc::new(std::sync::RwLock::new(cache.path().to_path_buf()));
        let source = Arc::clone(&current);
        let input = fixture(&dir, "big.png", 1920, 1080);
        let fs = FakeFs::with(&[&input]);
        // A real encoder writes the resized file; a max-width forces a resize.
        let proc = ImageProcessor::with_file_meta(
            Arc::new(ImageCrateEncoder::new()),
            DirectoryPath::live(move || source.read().expect("path").clone()),
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
        let replacement = TempDir::new().expect("replacement");
        *current.write().expect("path") = replacement.path().to_path_buf();
        let third = proc
            .process_image(&options)
            .await
            .expect("after reconfiguration");
        assert!(Path::new(&third.path).starts_with(replacement.path()));
        assert!(Path::new(&third.path).is_file());
        assert!(
            Path::new(&first.path).is_file(),
            "old cache is not relocated or deleted"
        );
    }

    #[tokio::test]
    async fn encoder_returning_original_short_circuits() {
        let dir = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let input = fixture(&dir, "s.png", 800, 600);
        // Encoder claims it wrote the original path back → short-circuit to it.
        let encoder = Arc::new(CountingEncoder {
            work: None,
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
                _c: bool,
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
                work: None,
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
        assert_eq!(empty, "");
    }

    #[tokio::test]
    async fn item_dimensions_use_info_when_positive_else_probe() {
        let proc = ImageProcessor::new(
            Arc::new(CountingEncoder {
                work: None,
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

    struct EncodingWork {
        entered: tokio::sync::Semaphore,
        release: (Mutex<bool>, std::sync::Condvar),
        /// Every encode fails once released, as a corrupt source does.
        fail: std::sync::atomic::AtomicBool,
    }

    impl EncodingWork {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                entered: tokio::sync::Semaphore::new(0),
                release: (Mutex::new(false), std::sync::Condvar::new()),
                fail: fail.into(),
            })
        }
    }

    struct ReleaseWork(Arc<EncodingWork>);
    impl Drop for ReleaseWork {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }

    #[tokio::test]
    async fn encoding_limit_survives_request_cancellation_until_pixel_work_finishes() {
        for limit in [1, 2] {
            let work = EncodingWork::new(false);
            let release = ReleaseWork(Arc::clone(&work));
            let encoder = Arc::new(CountingEncoder {
                work: Some(Arc::clone(&work)),
                calls: AtomicUsize::new(0),
                return_original: None,
            });
            let cache = TempDir::new().unwrap();
            let processor = Arc::new(
                processor_with(encoder.clone(), &cache, FakeFs::with(&["/input.jpg"]))
                    .with_encoding_limit(limit),
            );
            // Each request asks for its own width, so its own cache file: one
            // file's encodes run one at a time (gl-1300).
            let request = |processor: Arc<ImageProcessor<FakeFs>>, width: i32| {
                tokio::spawn(async move {
                    processor
                        .process_image(&ImageProcessingOptions {
                            image: ItemImageInfo {
                                path: "/input.jpg".into(),
                                ..Default::default()
                            },
                            width: Some(width),
                            ..Default::default()
                        })
                        .await
                })
            };
            // Fill every slot before the extra request starts. Each request
            // stats on the blocking pool before it takes a slot, so requests
            // spawned together take the slots in no fixed order, and the one
            // aborted below could be the queued one rather than an encode.
            let mut tasks: Vec<_> = (0..limit)
                .map(|i| request(Arc::clone(&processor), 100 + i32::try_from(i).unwrap()))
                .collect();
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                work.entered.acquire_many(u32::try_from(limit).unwrap()),
            )
            .await
            .unwrap()
            .unwrap()
            .forget();
            tasks.push(request(Arc::clone(&processor), 99));
            let first = tasks.remove(0);
            first.abort();
            let _ = first.await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), work.entered.acquire())
                    .await
                    .is_err(),
                "cancelled caller released a running encode's slot"
            );
            assert_eq!(encoder.calls.load(Ordering::SeqCst), limit);
            drop(release);
            for task in tasks {
                task.await.unwrap().unwrap();
            }
            let _permits = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                processor
                    .encoding_slots
                    .acquire_many(u32::try_from(limit).unwrap()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(encoder.calls.load(Ordering::SeqCst), limit + 1);
        }
    }

    /// gl-1300: concurrent misses for one cache file encode it once. The others
    /// wait for that encode, even after its request is dropped, and serve the
    /// file it wrote. When it wrote none, each encodes for itself, as it would
    /// alone: a failure reaches every request and is not kept for the next.
    #[rstest::rstest]
    #[case::encoded(false, false)]
    #[case::encoded_after_the_first_request_is_dropped(false, true)]
    #[case::failed(true, false)]
    #[case::failed_after_the_first_request_is_dropped(true, true)]
    #[tokio::test]
    async fn concurrent_misses_for_one_cache_file_encode_it_once(
        #[case] fail: bool,
        #[case] drop_first: bool,
    ) {
        const WAITERS: usize = 3;
        let wait = std::time::Duration::from_secs(3);
        let work = EncodingWork::new(fail);
        let release = ReleaseWork(Arc::clone(&work));
        let encoder = Arc::new(CountingEncoder {
            work: Some(Arc::clone(&work)),
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        let dir = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let input = dir.path().join("input.jpg").to_string_lossy().into_owned();
        std::fs::write(&input, b"source").unwrap();
        // A slot for every request: only the cache file's lock holds one back.
        let processor = Arc::new(
            ImageProcessor::new(encoder.clone(), cache.path().to_path_buf())
                .with_encoding_limit(WAITERS + 1),
        );
        let request = |width: i32| {
            let processor = Arc::clone(&processor);
            let input = input.clone();
            tokio::spawn(async move {
                processor
                    .process_image(&ImageProcessingOptions {
                        image: ItemImageInfo {
                            path: input,
                            ..Default::default()
                        },
                        width: Some(width),
                        ..Default::default()
                    })
                    .await
            })
        };

        let first = request(100);
        tokio::time::timeout(wait, work.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let mut waiters: Vec<_> = (0..WAITERS).map(|_| request(100)).collect();
        let mut first = Some(first);
        if drop_first {
            let first = first.take().unwrap();
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), work.entered.acquire())
                .await
                .is_err(),
            "a second encode of the same file started"
        );
        // A request dropped while it waits changes nothing for the others.
        let dropped = waiters.remove(0);
        dropped.abort();
        assert!(dropped.await.unwrap_err().is_cancelled());
        drop(release);

        let mut served = Vec::new();
        for task in first.into_iter().chain(waiters) {
            served.push(
                tokio::time::timeout(wait, task)
                    .await
                    .expect("a waiter hung")
                    .unwrap()
                    .unwrap()
                    .path,
            );
        }
        let encodes = if fail { 1 + WAITERS - 1 } else { 1 };
        assert_eq!(encoder.calls.load(Ordering::SeqCst), encodes);
        if fail {
            assert!(served.iter().all(|path| *path == input), "{served:?}");
        } else {
            assert!(served.iter().all(|path| path.contains("resized-images")));
            assert!(served.windows(2).all(|pair| pair[0] == pair[1]));
        }

        work.fail.store(false, Ordering::SeqCst);
        let again = request(100).await.unwrap().unwrap();
        assert!(again.path.contains("resized-images"));
        assert_eq!(
            encoder.calls.load(Ordering::SeqCst),
            encodes + usize::from(fail),
            "a failure is retried, a written file is served"
        );
        // Every lock taken above was released, so another file's encode sweeps
        // them out: the map holds only the files being encoded.
        request(101).await.unwrap().unwrap();
        assert_eq!(processor.cache_file_locks.len(), 1);
    }

    /// A [`FileMeta`] whose `held`-th call (counting `exists` and
    /// `last_write_time_utc` together) blocks its thread, as a stat on a hung
    /// network share does, until the test releases it. The release comes from a
    /// task on the same current-thread runtime, so it can only arrive in time
    /// when the stat runs off the runtime's one worker.
    struct HeldFs {
        inner: FakeFs,
        held: usize,
        calls: AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        released: std::sync::atomic::AtomicBool,
    }

    impl HeldFs {
        fn new(inner: FakeFs, held: usize, release: std::sync::mpsc::Receiver<()>) -> Self {
            Self {
                inner,
                held,
                calls: AtomicUsize::new(0),
                entered: Arc::new(tokio::sync::Notify::new()),
                release: Mutex::new(release),
                released: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// Blocks the `held`-th call until the release, or for at most 30
        /// seconds: on a runtime worker the release can never come in time, and
        /// the test fails instead of hanging.
        fn maybe_hold(&self) {
            if self.calls.fetch_add(1, Ordering::SeqCst) != self.held {
                return;
            }
            self.entered.notify_one();
            let rx = self.release.lock().expect("lock");
            if rx.recv_timeout(std::time::Duration::from_secs(30)).is_ok() {
                self.released.store(true, Ordering::SeqCst);
            }
        }
    }

    impl FileMeta for HeldFs {
        fn exists(&self, path: &str) -> bool {
            self.maybe_hold();
            self.inner.exists(path)
        }

        fn last_write_time_utc(&self, path: &str) -> DateTime<Utc> {
            self.maybe_hold();
            self.inner.last_write_time_utc(path)
        }
    }

    /// gl-1216: every stat `process_image` makes runs off the runtime worker.
    /// Each case holds one of them on its thread; the request's runtime (a
    /// single worker) must stay free to run the task that releases it.
    #[rstest::rstest]
    #[case::original_exists(0)]
    #[case::cache_exists(1)]
    #[case::cache_written_time(2)]
    #[tokio::test(flavor = "current_thread")]
    async fn file_stats_run_off_the_runtime_worker(#[case] held: usize) {
        let cache = TempDir::new().expect("cache");
        let encoder = Arc::new(CountingEncoder {
            work: None,
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        let (release, rx) = std::sync::mpsc::channel();
        let fs = HeldFs::new(FakeFs::with(&["/input.jpg"]), held, rx);
        let entered = Arc::clone(&fs.entered);
        let processor = Arc::new(ImageProcessor::with_file_meta(
            encoder.clone(),
            cache.path().to_path_buf(),
            fs,
        ));
        let task = tokio::spawn({
            let processor = Arc::clone(&processor);
            async move {
                processor
                    .process_image(&ImageProcessingOptions {
                        image: ItemImageInfo {
                            path: "/input.jpg".into(),
                            ..Default::default()
                        },
                        width: Some(100),
                        ..Default::default()
                    })
                    .await
            }
        });

        entered.notified().await;
        release.send(()).expect("the held stat is waiting");
        let processed = task.await.expect("join").expect("process");

        assert!(
            processor.file_meta.released.load(Ordering::SeqCst),
            "stat {held} ran on the runtime worker and blocked it"
        );
        assert!(processed.path.contains("resized-images"));
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 1);
        assert_eq!(processor.file_meta.calls.load(Ordering::SeqCst), 3);
    }

    /// A [`FakeFs`] that records the blocking-pool task each call ran on, so a
    /// test counts the hops `process_image` makes.
    #[derive(Default)]
    struct HopFs {
        inner: FakeFs,
        hops: Mutex<Vec<Option<tokio::task::Id>>>,
    }

    impl HopFs {
        /// The calls since the last take, and the distinct tasks they ran on.
        fn take(&self) -> (usize, usize) {
            let hops = std::mem::take(&mut *self.hops.lock().expect("lock"));
            let tasks: std::collections::HashSet<_> = hops.iter().collect();
            assert!(hops.iter().all(Option::is_some), "a stat ran on no task");
            (hops.len(), tasks.len())
        }
    }

    impl FileMeta for HopFs {
        fn exists(&self, path: &str) -> bool {
            self.hops.lock().expect("lock").push(tokio::task::try_id());
            self.inner.exists(path)
        }

        fn last_write_time_utc(&self, path: &str) -> DateTime<Utc> {
            self.hops.lock().expect("lock").push(tokio::task::try_id());
            self.inner.last_write_time_utc(path)
        }
    }

    /// gl-1258: the original's existence and the cached file's existence and
    /// time are one blocking-pool hop, so a cached poster costs one hop; a miss
    /// adds only the written file's time. A missing original and a request
    /// with default options stat the original alone.
    #[tokio::test]
    async fn the_original_and_cache_stats_share_one_hop() {
        let cache = TempDir::new().expect("cache");
        let encoder = Arc::new(CountingEncoder {
            work: None,
            calls: AtomicUsize::new(0),
            return_original: None,
        });
        let fs = HopFs {
            inner: FakeFs::with(&["/input.jpg"]),
            ..Default::default()
        };
        let processor =
            ImageProcessor::with_file_meta(encoder.clone(), cache.path().to_path_buf(), fs);
        let resize = |path: &str| ImageProcessingOptions {
            image: ItemImageInfo {
                path: path.into(),
                ..Default::default()
            },
            width: Some(100),
            ..Default::default()
        };

        let miss = processor
            .process_image(&resize("/input.jpg"))
            .await
            .expect("miss");
        assert_eq!(processor.file_meta.take(), (3, 2), "miss");
        processor.file_meta.inner.add(&miss.path);
        let hit = processor
            .process_image(&resize("/input.jpg"))
            .await
            .expect("hit");
        assert_eq!(hit, miss);
        assert_eq!(processor.file_meta.take(), (3, 1), "hit");
        assert_eq!(encoder.calls.load(Ordering::SeqCst), 1);

        let missing = processor
            .process_image(&resize("/missing.jpg"))
            .await
            .expect("missing");
        assert_eq!(missing.path, "/missing.jpg");
        assert_eq!(processor.file_meta.take(), (1, 1), "missing original");
        let original = processor
            .process_image(&ImageProcessingOptions {
                image: ItemImageInfo {
                    path: "/input.jpg".into(),
                    ..Default::default()
                },
                quality: 100,
                supported_output_formats: vec![ImageFormat::Jpg],
                ..Default::default()
            })
            .await
            .expect("default options");
        assert_eq!(original.path, "/input.jpg");
        assert_eq!(processor.file_meta.take(), (1, 1), "default options");
        let gif = processor
            .process_image(&resize("/input.gif"))
            .await
            .expect("gif");
        assert_eq!(gif.path, "/input.gif");
        assert_eq!(processor.file_meta.take(), (0, 0), "gif");
    }
}
