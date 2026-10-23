//! `ImageCrateEncoder` — a real codec [`ImageEncoder`] backed by the `image`
//! crate (no native Skia).
//!
//! Port of `Jellyfin.Drawing.Skia.SkiaEncoder`, re-implemented on the pure-Rust
//! [`image`] crate. Parity with the C# oracle is **dimensional and
//! decodable-output**, not byte-exact: the two libraries use different resamplers
//! and JPEG/PNG/WebP writers, so there is no byte oracle. What is ported exactly
//! is the *shape* of each operation — which axis wins during resize, the collage
//! ratio dispatch, the trickplay grid packing, and the "spit out the original
//! file when options are default" short-circuit.
//!
//! Port rules applied:
//! - This encoder advertises the subset of Skia's input formats the `image`
//!   crate is compiled to decode (`jpeg`/`jpg`/`png`/`webp`/`gif`/`bmp`/`ico`).
//!   The rest of Skia's list (raw `dng`/`cr2`/`nef`/`arw`, `wbmp`,
//!   `astc`/`ktx`/`pkm`, `svg`) has no decoder yet (gl-802). Requesting an
//!   unsupported input returns the input path untouched (mirrors `EncodeImage`).
//! - The C# `throw` sites (`FileNotFoundException`, `InvalidDataException`,
//!   `ArgumentException`, `InvalidOperationException`) become
//!   [`ServiceError`] variants: [`ServiceError::NotFound`] for a missing input,
//!   [`ServiceError::InvalidInput`] for a bad argument/undecodable image, and
//!   [`ServiceError::Backend`] for an I/O/encode failure.
//! - `GetImageSize` returns [`ImageDimensions::default`] (`0×0`) on a zero-byte or
//!   undecodable file, exactly like the Skia `return default;` fallbacks.
//! - `CreateSplashscreen` builds `{DataPath}/splashscreen.png` (see
//!   [`crate::splashscreen`]). `GetImageBlurHash` is implemented (decode → 128x128 downscale → BlurHash-encode);
//!   its output is valid but not byte-identical to Skia's (see the method doc).
//! - The overlay work Skia does inside `EncodeImage` (background colour, blur,
//!   foreground layer: gl-805; unplayed/percent-played indicators: gl-806) is
//!   **not** ported yet — this encoder does auto-orientation, resize and
//!   format-convert only, so `has_default_options` and `auto_orient` already
//!   decide whether re-encoding is needed.

use std::path::Path;
use std::sync::OnceLock;

use tokio::sync::Semaphore;

use crate::error::DrawingError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::drawing::drawing_utils::{resize, resize_fill, scale_down_to_fit};
use ferrofin_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions};
use image::{DynamicImage, ImageFormat as CrateFormat, ImageReader, RgbaImage, imageops};
use jpeg_encoder::{
    ChromaSubsamplingMethod, ColorType as JpegColorType, Encoder as JpegEncoder, SamplingFactor,
};

/// Caps how many image encodes run concurrently on the blocking pool.
///
/// [`spawn_blocking`](tokio::task::spawn_blocking) keeps CPU-bound encodes off
/// the async reactor, but its pool is effectively unbounded — a cold-cache storm
/// (e.g. a fresh library grid right after a deploy/cache-flush) would launch one
/// blocking thread per in-flight poster, all fighting the same cores and burning
/// memory. Bound concurrent encodes to the machine's parallelism so the encoders
/// saturate the CPUs without oversubscribing them.
///
/// ponytail: process-global limit — correct because the constraint (physical
/// cores) is global. If a host ever needs to tune image throughput independently
/// of core count, lift this into a config setting / per-encoder field.
fn encode_permits() -> &'static Semaphore {
    static PERMITS: OnceLock<Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Semaphore::new(cores)
    })
}

/// A real image codec implementing [`ImageEncoder`] on the `image` crate.
///
/// Handles the decodable subset of Jellyfin's Skia input set (`jpeg`/`jpg`/`png`/
/// `webp`) and can write [`ImageFormat::Webp`] (lossy, via libwebp),
/// [`ImageFormat::Jpg`], and [`ImageFormat::Png`]. Resize/crop math is
/// delegated to the shared
/// `ferrofin-model` `drawing_utils`, so it matches the C# `DrawingUtils` /
/// `ImageHelper.GetNewImageSize` exactly.
#[derive(Debug, Clone, Default)]
pub struct ImageCrateEncoder {
    /// The server's data directory, where [`ImageEncoder::create_splashscreen`]
    /// writes `splashscreen.png` (upstream's `IApplicationPaths.DataPath`).
    data_path: Option<std::path::PathBuf>,
}

impl ImageCrateEncoder {
    /// Constructs a new [`ImageCrateEncoder`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The encoder writing the generated splashscreen into `data_path`.
    #[must_use]
    pub fn with_data_path(mut self, data_path: impl Into<std::path::PathBuf>) -> Self {
        self.data_path = Some(data_path.into());
        self
    }

    /// The lowercase file extension of `path` without its leading dot, or an
    /// empty string when there is none. Mirrors C#
    /// `Path.GetExtension(...).TrimStart('.')` folded to lowercase for the
    /// case-insensitive `SupportedInputFormats.Contains` check.
    fn input_format(path: &str) -> String {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Whether `path`'s extension is one this encoder can decode. Port of the
    /// case-insensitive `SupportedInputFormats.Contains(inputFormat)` guard.
    fn is_supported_input(path: &str) -> bool {
        SUPPORTED_INPUT_FORMATS.contains(&Self::input_format(path).as_str())
    }

    /// Decodes the image at `path` into memory, mapping the C# `throw` sites onto
    /// [`ServiceError`]. A missing file is [`ServiceError::NotFound`]; an
    /// undecodable one is [`ServiceError::InvalidInput`] (the port of Skia's
    /// `InvalidDataException`/`return null` fallbacks).
    pub(crate) fn decode(path: &str) -> Result<DynamicImage, ServiceError> {
        if !Path::new(path).exists() {
            return Err(ServiceError::not_found(format!(
                "image file not found: {path}"
            )));
        }
        let reader = ImageReader::open(path)
            .map_err(|e| DrawingError::io(format!("open {path}"), e))?
            .with_guessed_format()
            .map_err(|e| DrawingError::io(format!("probe {path}"), e))?;
        reader
            .decode()
            .map_err(|e| ServiceError::invalid_input(format!("decode {path}: {e}")))
    }

    /// `SkiaEncoder.GetBitmap(path, autoOrient: true, …)`: decodes `path` and
    /// applies its own EXIF origin (`codec.EncodedOrigin` → `OrientImage`), so
    /// the pixels stand as the photo was taken. A file whose orientation
    /// cannot be read is taken as stored.
    fn decode_oriented(path: &str) -> Result<DynamicImage, ServiceError> {
        use image::ImageDecoder as _;
        if !Path::new(path).exists() {
            return Err(ServiceError::not_found(format!(
                "image file not found: {path}"
            )));
        }
        let mut decoder = ImageReader::open(path)
            .map_err(|e| DrawingError::io(format!("open {path}"), e))?
            .with_guessed_format()
            .map_err(|e| DrawingError::io(format!("probe {path}"), e))?
            .into_decoder()
            .map_err(|e| ServiceError::invalid_input(format!("decode {path}: {e}")))?;
        let orientation = decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut image = DynamicImage::from_decoder(decoder)
            .map_err(|e| ServiceError::invalid_input(format!("decode {path}: {e}")))?;
        image.apply_orientation(orientation);
        Ok(image)
    }

    /// Maps an output [`ImageFormat`] onto the `image` crate's writer format.
    /// Port of `SkiaEncoder.GetImageFormat`, restricted to the three formats
    /// this encoder advertises (anything else falls back to PNG, as Skia does).
    fn crate_format(format: ImageFormat) -> CrateFormat {
        match format {
            ImageFormat::Jpg => CrateFormat::Jpeg,
            ImageFormat::Webp => CrateFormat::WebP,
            _ => CrateFormat::Png,
        }
    }

    /// Chooses the writer format from an output *path* extension. Port of
    /// `StripCollageBuilder.GetEncodedFormat` (jpg/jpeg → JPEG, webp → WebP,
    /// default → PNG); gif/bmp fall back to PNG as upstream's do, although
    /// this build could write them.
    fn format_for_path(path: &str) -> CrateFormat {
        match Self::input_format(path).as_str() {
            "jpg" | "jpeg" => CrateFormat::Jpeg,
            "webp" => CrateFormat::WebP,
            _ => CrateFormat::Png,
        }
    }

    /// Writes `image` to `output_path` in `format` at `quality` (0–100),
    /// creating the parent directory first. JPEG and WebP (lossy, via libwebp)
    /// honour `quality`; PNG is lossless, so it ignores `quality`. Port of the
    /// `SKFileWStream`/`Encode(..., quality)` tail of `EncodeImage`.
    fn write(
        image: &DynamicImage,
        output_path: &str,
        format: CrateFormat,
        quality: i32,
    ) -> Result<(), ServiceError> {
        if let Some(parent) = Path::new(output_path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| DrawingError::io(format!("mkdir {}", parent.display()), e))?;
        }

        if format == CrateFormat::Jpeg {
            Self::write_jpeg(image, output_path, quality)
        } else if format == CrateFormat::WebP {
            // LOSSY WebP via libwebp (the `webp` crate), mirroring Skia's
            // `bitmap.Encode(stream, Webp, quality)`. The `image` crate's own
            // WebP encoder is lossless-only and ignores `quality`, which made
            // every poster several times larger than Jellyfin's — so this branch
            // routes around it.
            //
            // libwebp cannot write a side over 16383 px. Refused before the RGBA
            // copy, and as invalid input like the JPEG limit: it is decided by the
            // input alone and recurs on every request (nothing is cached).
            if image.width() > WEBP_MAX_DIMENSION || image.height() > WEBP_MAX_DIMENSION {
                return Err(ServiceError::InvalidInput(format!(
                    "cannot write {output_path}: {}x{} exceeds WebP's \
                     {WEBP_MAX_DIMENSION}x{WEBP_MAX_DIMENSION} limit",
                    image.width(),
                    image.height()
                )));
            }
            let rgba = image.to_rgba8();
            // Precision loss is irrelevant: quality is clamped to 0..=100.
            #[allow(clippy::cast_precision_loss)]
            let quality = quality.clamp(0, 100) as f32;
            // `encode_simple`, not `encode`: the latter unwraps libwebp's error
            // and panics on any other encode failure.
            let bytes = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                .encode_simple(false, quality)
                .map_err(|code| DrawingError::webp_encode(format!("encode {output_path}"), code))?;
            std::fs::write(output_path, &*bytes)
                .map_err(|e| DrawingError::io(format!("write {output_path}"), e).into())
        } else {
            image
                .save_with_format(output_path, format)
                .map_err(|e| DrawingError::encode(format!("encode {output_path}"), e).into())
        }
    }

    /// Writes `image` to `output_path` as a baseline JPEG at `quality` (0-100)
    /// with **4:2:0 chroma subsampling and optimized Huffman tables**, mirroring
    /// the C# oracle's `bitmap.Encode(stream, SKEncodedImageFormat.Jpeg, quality)`.
    ///
    /// This routes around the `image` crate for the same reason the WebP branch
    /// does. `image` 0.25's `JpegEncoder` hardcodes 4:4:4 (every component is
    /// declared `h: 1, v: 1` in `new_with_quality`, at every quality), whereas
    /// Skia's `SkJpegEncoder` defaults to `Downsample::k420` for all three of
    /// Jellyfin's JPEG writes (`SkiaEncoder.cs` v10.11.8 lines 628, 670 and 778 —
    /// resized image, pixmap, and the trickplay tile grid). A 4:4:4 tile is
    /// roughly 3x the bytes of the 4:2:0 one, and `TrickplayManager.CreateTiles`
    /// derives `TrickplayInfo.Bandwidth` straight from the tile's file length
    /// (`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`), so the
    /// subsampling was visible to clients as a ~3.4x inflated scrub-prefetch
    /// budget, not just as bigger files on disk.
    ///
    /// `jpeg-encoder` is configured to match Skia on every knob that changes the
    /// wire format:
    ///
    /// - **4:2:0 chroma subsampling**, as above. That crate's own default is
    ///   quality-dependent (4:4:4 at `quality >= 90`, which is exactly the
    ///   quality trickplay tiles are written at), so it is set explicitly rather
    ///   than inherited.
    /// - **Optimized Huffman tables** (libjpeg `optimize_coding = TRUE`), which
    ///   `jpeg-encoder` leaves off. Measured on the wire against Jellyfin
    ///   10.11.8: its tiles carry content-derived tables with 11/93/11/56
    ///   symbols where libjpeg's standard tables have 12/162/12/162. On the flat,
    ///   mostly-DC content a trickplay tile is made of, that difference alone is
    ///   a factor of two in file size — for the same quantization tables (verified
    ///   byte-identical between the two servers' tiles) and the same pixels
    ///   (mean absolute difference 0.005/255).
    /// - **Baseline, not progressive**, and box-averaged chroma downsampling
    ///   (libjpeg's `h2v2_downsample`) — both already the crate's defaults.
    ///
    /// Optimized coding costs a second entropy pass over the image. Skia pays it
    /// on every JPEG Jellyfin writes, and it halves what this server stores and
    /// serves, so Ferrofin pays it too.
    ///
    /// The output is still not byte-identical to Skia's — no two independent
    /// JPEG writers are — but the two now agree on subsampling, quantization and
    /// entropy coding, which is everything a client can observe about the format.
    ///
    /// One recorded, decoder-invisible divergence remains: `jpeg-encoder`
    /// hardcodes the SOF component identifiers to 0/1/2 (`init_components`,
    /// v0.7.1), where libjpeg — and so Skia — writes the JFIF-conventional
    /// 1/2/3. JPEG decoders bind components positionally and match the SOS
    /// selectors against whatever the SOF declared, so no client can observe
    /// this; the crate exposes no knob for it.
    fn write_jpeg(
        image: &DynamicImage,
        output_path: &str,
        quality: i32,
    ) -> Result<(), ServiceError> {
        let quality = quality.clamp(0, 100);
        // Truncation is safe: quality is clamped to 0..=100.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let quality = quality as u8;
        let rgb = image.to_rgb8();
        // JPEG's frame header stores the dimensions in 16 bits, so anything wider
        // or taller than 65535 cannot be written; Skia fails the same encode.
        let (Ok(width), Ok(height)) = (u16::try_from(rgb.width()), u16::try_from(rgb.height()))
        else {
            return Err(ServiceError::InvalidInput(format!(
                "cannot write {output_path}: {}x{} exceeds JPEG's 65535x65535 limit",
                rgb.width(),
                rgb.height()
            )));
        };
        let file = std::fs::File::create(output_path)
            .map_err(|e| DrawingError::io(format!("create {output_path}"), e))?;
        let writer = std::io::BufWriter::new(file);
        let mut encoder = JpegEncoder::new(writer, quality);
        encoder.set_sampling_factor(SamplingFactor::F_2_2);
        encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
        encoder.set_optimized_huffman_tables(true);
        encoder
            .encode(rgb.as_raw(), width, height, JpegColorType::Rgb)
            .map_err(|e| DrawingError::jpeg_encode(format!("encode {output_path}"), e).into())
    }

    /// Resizes `image` to exactly `size` using a high-quality Lanczos3 filter
    /// (the `image` crate's nearest analogue to Skia's Mitchell/linear
    /// resamplers). Mirrors `SkiaEncoder.ResizeImage` at the dimensional level.
    ///
    /// `DrawingUtils.Resize` can round one axis to 0 (an ordinary `width=100` on a
    /// 10000x10 source, or an extreme `width`x`height` shrunk by `maxWidth`/
    /// `maxHeight`), and `ScaleDownToFit` passes that through.
    /// Skia's `SKSurface.Create` returns null for an empty target and the encode
    /// throws, so that is an error here too (the processor then serves the
    /// original), never a 1-pixel axis beside an unclamped one.
    fn resize_exact(
        image: &DynamicImage,
        size: ImageDimensions,
    ) -> Result<DynamicImage, ServiceError> {
        match (u32::try_from(size.width), u32::try_from(size.height)) {
            (Ok(width @ 1..), Ok(height @ 1..)) => {
                Ok(image.resize_exact(width, height, imageops::FilterType::Lanczos3))
            }
            _ => Err(ServiceError::invalid_input(format!(
                "cannot resize to {}x{}",
                size.width, size.height
            ))),
        }
    }

    /// The synchronous body of [`get_image_size`](ImageEncoder::get_image_size),
    /// split out so it can run on the blocking pool via `spawn_blocking`.
    fn image_size_blocking(path: &str) -> Result<ImageDimensions, ServiceError> {
        if !Path::new(path).exists() {
            return Err(ServiceError::not_found(format!(
                "image file not found: {path}"
            )));
        }

        // Zero-byte guard, matching the Skia `FileInfo(...).Length == 0` check.
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() == 0 => return Ok(ImageDimensions::default()),
            Ok(_) => {}
            Err(e) => return Err(DrawingError::io(format!("stat {path}"), e).into()),
        }

        // Probe dimensions from the header only; undecodable → default (0×0).
        let dims = ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.into_dimensions().ok());

        match dims {
            Some((w, h)) => {
                let width = i32::try_from(w).unwrap_or(i32::MAX);
                let height = i32::try_from(h).unwrap_or(i32::MAX);
                Ok(ImageDimensions::new(width, height))
            }
            None => Ok(ImageDimensions::default()),
        }
    }

    /// The synchronous decode → resize → encode body of
    /// [`encode_image`](ImageEncoder::encode_image), split out so it can run on
    /// the blocking pool via `spawn_blocking` (all CPU-bound, no `.await`).
    fn encode_image_blocking(
        input_path: &str,
        output_path: &str,
        auto_orient: bool,
        quality: i32,
        options: &ImageProcessingOptions,
        output_format: ImageFormat,
    ) -> Result<String, ServiceError> {
        // Unsupported input: spit out the original path, like Skia does.
        if !Self::is_supported_input(input_path) {
            return Ok(input_path.to_owned());
        }

        let image = if auto_orient {
            Self::decode_oriented(input_path)?
        } else {
            Self::decode(input_path)?
        };
        let original = ImageDimensions::new(
            i32::try_from(image.width()).unwrap_or(i32::MAX),
            i32::try_from(image.height()).unwrap_or(i32::MAX),
        );

        // "Just spit out the original file if all the options are default."
        if options.has_default_options(input_path, Some(original)) && !auto_orient {
            return Ok(input_path.to_owned());
        }

        let new_size = get_new_image_size(options, original);

        let resized = Self::resize_exact(&image, new_size)?;
        Self::write(
            &resized,
            output_path,
            Self::crate_format(output_format),
            quality,
        )?;
        Ok(output_path.to_owned())
    }

    /// The synchronous body of [`ImageEncoder::get_image_blur_hash`]: decode,
    /// downscale to fit 128×128, then BlurHash-encode. CPU-bound (plus the
    /// decode's file read), so it runs on the blocking pool, never inline.
    fn blur_hash_blocking(x_comp: i32, y_comp: i32, path: &str) -> Result<String, ServiceError> {
        let decoded = Self::decode(path)?;
        let small = decoded
            .resize(128, 128, image::imageops::FilterType::Triangle)
            .to_rgba8();
        let x = u32::try_from(x_comp.clamp(1, 9)).unwrap_or(1);
        let y = u32::try_from(y_comp.clamp(1, 9)).unwrap_or(1);
        blurhash::encode(x, y, small.width(), small.height(), small.as_raw())
            .map_err(|e| ServiceError::backend(e.to_string()))
    }

    /// The build + write body of [`ImageEncoder::create_image_collage`]: several
    /// decodes, Lanczos3 resizes and an encode. CPU-bound, so it runs on the
    /// blocking pool. Assumes the caller has validated `width`/`height`.
    fn create_image_collage_blocking(options: &ImageCollageOptions) -> Result<(), ServiceError> {
        let ratio = f64::from(options.width) / f64::from(options.height);
        let canvas = if ratio >= COLLAGE_THUMB_RATIO {
            build_thumb_collage(&options.input_paths, options.width, options.height)
        } else {
            build_square_collage(&options.input_paths, options.width, options.height)
        };

        let format = Self::format_for_path(&options.output_path);
        // Collages are written at the C# fixed quality of 90.
        Self::write(
            &DynamicImage::ImageRgba8(canvas),
            &options.output_path,
            format,
            COLLAGE_QUALITY,
        )
    }

    /// The decode + overlay + JPEG-encode body of
    /// [`ImageEncoder::create_trickplay_tile`]: up to `tile_width`×`tile_height`
    /// (default 100) JPEG decodes packed into one large grid. CPU- and
    /// file-I/O-bound with no yield points, so it runs on the blocking pool.
    /// Assumes the caller has validated the O(1) arguments (non-empty inputs,
    /// positive tile dimensions and `img_width`, and that the inputs fit the
    /// grid); the per-image dimension checks stay here with the decode.
    fn create_trickplay_tile_blocking(
        options: &ImageCollageOptions,
        quality: i32,
        img_width: i32,
        img_height: Option<i32>,
    ) -> Result<i32, ServiceError> {
        let paths = &options.input_paths;
        let tile_width = options.width;
        let tile_height = options.height;

        // If no height provided, use the first image's height (after validating
        // its width matches img_width), matching the Skia default path.
        let first = Self::decode(&paths[0])?;
        let first_w = i32::try_from(first.width()).unwrap_or(i32::MAX);
        if first_w != img_width {
            return Err(ServiceError::invalid_input(
                "Image width does not match provided width.",
            ));
        }
        let img_height = match img_height {
            Some(h) if h > 0 => h,
            _ => i32::try_from(first.height()).unwrap_or(i32::MAX),
        };

        // Checked: a wrapped i32 product would ask RgbaImage::new for a huge buffer.
        let (Some(grid_w), Some(grid_h)) = (
            img_width
                .checked_mul(tile_width)
                .and_then(|w| u32::try_from(w).ok()),
            img_height
                .checked_mul(tile_height)
                .and_then(|h| u32::try_from(h).ok()),
        ) else {
            return Err(ServiceError::invalid_input(format!(
                "A {tile_width}x{tile_height} grid of {img_width}x{img_height} images is too large.",
            )));
        };
        let mut grid = RgbaImage::new(grid_w, grid_h);

        let mut img_index = 0usize;
        'outer: for y in 0..tile_height {
            for x in 0..tile_width {
                if img_index >= paths.len() {
                    break 'outer;
                }

                let img = if img_index == 0 {
                    first.clone()
                } else {
                    Self::decode(&paths[img_index])?
                };
                img_index += 1;

                if i32::try_from(img.width()).unwrap_or(i32::MAX) != img_width {
                    return Err(ServiceError::invalid_input(
                        "Image width does not match provided width.",
                    ));
                }
                if i32::try_from(img.height()).unwrap_or(i32::MAX) != img_height {
                    return Err(ServiceError::invalid_input(
                        "Image height does not match first image height.",
                    ));
                }

                let px = u32::try_from(x * img_width).unwrap_or(0);
                let py = u32::try_from(y * img_height).unwrap_or(0);
                imageops::overlay(&mut grid, &img.to_rgba8(), i64::from(px), i64::from(py));
            }
        }

        Self::write(
            &DynamicImage::ImageRgba8(grid),
            &options.output_path,
            CrateFormat::Jpeg,
            quality,
        )?;

        Ok(img_height)
    }
}

/// Port of `ImageHelper.GetNewImageSize`: `DrawingUtils.Resize`, then
/// `DrawingUtils.ResizeFill`, then `DrawingUtils.ScaleDownToFit` against the
/// source, so a caller-supplied `width`/`height` never encodes (or allocates)
/// larger than the original image.
fn get_new_image_size(
    options: &ImageProcessingOptions,
    original: ImageDimensions,
) -> ImageDimensions {
    let new_size = resize(
        original,
        options.width.unwrap_or(0),
        options.height.unwrap_or(0),
        options.max_width.unwrap_or(0),
        options.max_height.unwrap_or(0),
    );
    let new_size = resize_fill(new_size, options.fill_width, options.fill_height);

    // Never encode larger than the source.
    scale_down_to_fit(new_size, original)
}

/// The decodable input extensions this encoder advertises.
///
/// The subset of `SkiaEncoder.SupportedInputFormats` the `image` crate is
/// compiled to decode (`jpeg`/`png`/`webp`/`gif`/`bmp`/`ico`), with the alias
/// `jpg` for `jpeg`. A GIF decodes to its first frame, as Skia's does.
///
/// TODO(gl-802): the rest of Skia's list (`dng`/`cr2`/`nef`/`arw`, `wbmp`,
/// `astc`/`ktx`/`pkm`, `svg`) has no decoder in the `image` crate; each needs
/// its own decoder dependency before it can join this list.
const SUPPORTED_INPUT_FORMATS: [&str; 7] = ["jpeg", "jpg", "png", "webp", "gif", "bmp", "ico"];

/// The widest or tallest image libwebp can write, its `WEBP_MAX_DIMENSION`
/// (the bitstream's 14-bit size fields), which the `webp` crate does not
/// re-export.
const WEBP_MAX_DIMENSION: u32 = 16_383;

#[async_trait]
impl ImageEncoder for ImageCrateEncoder {
    /// Port of `SupportedInputFormats` — the decodable subset
    /// (`jpeg`/`jpg`/`png`/`webp`/`gif`/`bmp`/`ico`).
    fn supported_input_formats(&self) -> Vec<String> {
        SUPPORTED_INPUT_FORMATS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// Port of `SupportedOutputFormats` — [`ImageFormat::Webp`],
    /// [`ImageFormat::Jpg`], [`ImageFormat::Png`] (Skia also lists `Svg`, which
    /// this raster encoder cannot produce). WebP output is real *lossy* WebP via
    /// libwebp — the same encoder Skia links — so the prefer-WebP negotiation
    /// yields Jellyfin-sized files, not the `image` crate's lossless-only blobs.
    fn supported_output_formats(&self) -> Vec<ImageFormat> {
        vec![ImageFormat::Webp, ImageFormat::Jpg, ImageFormat::Png]
    }

    /// Port of `Name` — the constant `"Image Crate"`.
    fn name(&self) -> String {
        "Image Crate".to_owned()
    }

    /// Port of `SupportsImageCollageCreation` — always `true`.
    fn supports_image_collage_creation(&self) -> bool {
        true
    }

    /// Port of `SupportsImageEncoding` — always `true`.
    fn supports_image_encoding(&self) -> bool {
        true
    }

    /// Port of `GetImageSize`. Returns [`ImageDimensions::default`] (`0×0`) for a
    /// zero-byte or undecodable file (Skia's `return default;`), and the header
    /// dimensions otherwise. A missing file is [`ServiceError::NotFound`].
    ///
    /// Every step is synchronous filesystem work — two stats plus an open and a
    /// header read — so it runs on the blocking pool, never inline on the async
    /// worker. It is not a background-only call: `DtoService` reaches it once per
    /// item whose stored image dimensions are `0` while building
    /// `PrimaryImageAspectRatio`, i.e. up to a full page of probes per request,
    /// and on network-backed media (NFS/SMB) a cold probe parks the calling
    /// thread for seconds.
    async fn get_image_size(&self, path: &str) -> Result<ImageDimensions, ServiceError> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || Self::image_size_blocking(&path))
            .await
            .map_err(|e| ServiceError::backend(format!("image probe task failed: {e}")))?
    }

    /// Port of `SkiaEncoder.GetImageBlurHash`: decode, downscale to fit 128x128
    /// ("larger is too slow, no visible difference"), then BlurHash-encode with the
    /// caller's component counts.
    ///
    /// The hash is functionally valid but **not byte-identical** to Jellyfin's — its
    /// pixels come from Skia's decode+resample, which the `image` crate can't reproduce
    /// exactly (documented divergence; `ImageBlurHashes` is denylisted in the parity test).
    async fn get_image_blur_hash(
        &self,
        x_comp: i32,
        y_comp: i32,
        path: &str,
    ) -> Result<String, ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path is empty"));
        }
        // A full decode (4K backdrops included) plus a Triangle resize is
        // CPU-bound synchronous work, and the decode's file read can be a cold
        // NFS/SMB read; offload it to the blocking pool under the shared encode
        // permit so it never pins an async worker (see `encode_image`).
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || Self::blur_hash_blocking(x_comp, y_comp, &path))
            .await
            .map_err(|e| ServiceError::backend(format!("blur hash task failed: {e}")))?
    }

    /// Port of `EncodeImage`. Rejects unsupported inputs (returns `input_path`
    /// untouched), decodes, short-circuits to the original file when
    /// [`ImageProcessingOptions::has_default_options`] holds and no
    /// auto-orientation is requested, otherwise resizes to
    /// `ImageHelper.GetNewImageSize` and writes `output_format` at `quality`.
    ///
    /// With `auto_orient` the file's own EXIF origin is applied first
    /// (`SkiaEncoder.GetBitmap` → `OrientImage`), so the resize sees the
    /// upright size. `orientation` is unused: Skia falls back to it only when
    /// the codec cannot open the file, and then has no bitmap to draw either.
    /// The played/unplayed/blur overlays are not drawn.
    async fn encode_image(
        &self,
        input_path: &str,
        _date_modified: DateTime<Utc>,
        output_path: &str,
        auto_orient: bool,
        _orientation: Option<ImageOrientation>,
        quality: i32,
        options: &ImageProcessingOptions,
        output_format: ImageFormat,
    ) -> Result<String, ServiceError> {
        if input_path.is_empty() {
            return Err(ServiceError::invalid_input("input_path is empty"));
        }
        if output_path.is_empty() {
            return Err(ServiceError::invalid_input("output_path is empty"));
        }

        // Decode + Lanczos3 resize + re-encode is CPU-bound synchronous work.
        // Running it directly on the async worker freezes that thread for the
        // whole encode (tens of ms per poster), starving every other request
        // sharing the worker — a cheap 3 ms endpoint's p99 balloons behind an
        // image derivation. Offload to the blocking pool so the reactor stays
        // free (Jellyfin's Skia path runs off the request thread likewise). The
        // permit bounds concurrent encodes to core count so a cold-cache storm
        // can't oversubscribe the CPUs; it is held across the whole encode.
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let input_path = input_path.to_owned();
        let output_path = output_path.to_owned();
        let options = options.clone();
        tokio::task::spawn_blocking(move || {
            Self::encode_image_blocking(
                &input_path,
                &output_path,
                auto_orient,
                quality,
                &options,
                output_format,
            )
        })
        .await
        .map_err(|e| ServiceError::backend(format!("image encode task failed: {e}")))?
    }

    /// Port of `CreateImageCollage`. Dispatches on the width/height ratio: a
    /// `>= 1.4` ratio builds a horizontal thumb strip, anything else a `2×2`
    /// square collage. The thumb strip's library-name text is not drawn yet
    /// (see [`build_thumb_collage`]).
    async fn create_image_collage(
        &self,
        options: &ImageCollageOptions,
        _library_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        if options.width <= 0 || options.height <= 0 {
            return Err(ServiceError::invalid_input(
                "collage width/height must be positive",
            ));
        }

        // Several poster decodes, Lanczos3 resizes and a final encode are
        // CPU-bound synchronous work; offload under the shared encode permit so
        // it never pins an async worker (see `encode_image`).
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let options = options.clone();
        tokio::task::spawn_blocking(move || Self::create_image_collage_blocking(&options))
            .await
            .map_err(|e| ServiceError::backend(format!("image collage task failed: {e}")))?
    }

    /// Port of `CreateSplashscreen`: builds `{DataPath}/splashscreen.png`
    /// ([`crate::splashscreen`]) when there is at least one poster and one
    /// backdrop, and does nothing otherwise. The file is written beside the
    /// final name and renamed over it, so a reader never sees half a PNG.
    async fn create_splashscreen(
        &self,
        posters: &[String],
        backdrops: &[String],
    ) -> Result<(), ServiceError> {
        // Only generate the splash screen if we have at least one poster and at
        // least one backdrop/thumbnail.
        if posters.is_empty() || backdrops.is_empty() {
            return Ok(());
        }
        let Some(data_path) = self.data_path.clone() else {
            return Err(ServiceError::backend(
                "ImageCrateEncoder has no data path to write the splashscreen to",
            ));
        };
        // Dozens of decodes, a 5760x2160 wall and a projection: CPU-bound, so
        // under the shared encode permit on the blocking pool.
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let (posters, backdrops) = (posters.to_vec(), backdrops.to_vec());
        tokio::task::spawn_blocking(move || {
            // A scratch name per call: two validations finishing together must
            // not write, or rename away, each other's file.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let splash = crate::splashscreen::generate_splash(&posters, &backdrops)?;
            let output = data_path.join("splashscreen.png");
            let partial = data_path.join(format!(
                "splashscreen.png.{}.partial",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let written = Self::write(
                &DynamicImage::ImageRgba8(splash),
                &partial.to_string_lossy(),
                CrateFormat::Png,
                COLLAGE_QUALITY,
            )
            .and_then(|()| {
                std::fs::rename(&partial, &output).map_err(|e| {
                    DrawingError::io(format!("rename {}", partial.display()), e).into()
                })
            });
            if written.is_err() {
                // Best effort: the scratch file may not exist at all.
                let _ = std::fs::remove_file(&partial);
            }
            written
        })
        .await
        .map_err(|e| ServiceError::backend(format!("splashscreen task failed: {e}")))?
    }

    /// Port of `CreateTrickplayTile`. Packs `input_paths` into an
    /// `img_width*tile_width × img_height*tile_height` grid (row-major), encodes
    /// it as JPEG at `quality`, and returns the single-thumbnail height.
    ///
    /// Validation mirrors the C# exactly: empty inputs and an over-full grid are
    /// [`ServiceError::InvalidInput`]; every tile's width must equal `img_width`
    /// and its height the first tile's height, else [`ServiceError::InvalidInput`]
    /// (the ports of `ArgumentException`/`InvalidOperationException`). A missing
    /// `img_height` is taken from the first image.
    async fn create_trickplay_tile(
        &self,
        options: &ImageCollageOptions,
        quality: i32,
        img_width: i32,
        img_height: Option<i32>,
    ) -> Result<i32, ServiceError> {
        let paths = &options.input_paths;
        let tile_width = options.width;
        let tile_height = options.height;

        if paths.is_empty() {
            return Err(ServiceError::invalid_input("InputPaths cannot be empty."));
        }
        if tile_width <= 0 || tile_height <= 0 {
            return Err(ServiceError::invalid_input(
                "tile width/height must be positive",
            ));
        }
        // i64 product avoids overflow before the comparison.
        if i64::try_from(paths.len()).unwrap_or(i64::MAX)
            > i64::from(tile_width) * i64::from(tile_height)
        {
            return Err(ServiceError::invalid_input(format!(
                "InputPaths contains more images than would fit on {tile_width}x{tile_height} grid.",
            )));
        }
        if img_width <= 0 {
            return Err(ServiceError::invalid_input("img_width must be positive"));
        }

        // Up to tile_width×tile_height (default 100) JPEG decodes, overlaid onto
        // one large grid and JPEG-encoded — CPU- and file-I/O-bound with no yield
        // points. Offload to the blocking pool under the shared encode permit
        // (one per tile, not per film, so tiling cannot starve request-path
        // poster encodes that share the same core-count semaphore).
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let options = options.clone();
        tokio::task::spawn_blocking(move || {
            Self::create_trickplay_tile_blocking(&options, quality, img_width, img_height)
        })
        .await
        .map_err(|e| ServiceError::backend(format!("trickplay tile task failed: {e}")))?
    }
}

/// The width/height ratio at or above which a collage is laid out as a
/// horizontal thumb strip rather than a `2×2` square. Port of the C# `>= 1.4`
/// dispatch in `CreateImageCollage`.
const COLLAGE_THUMB_RATIO: f64 = 1.4;

/// The fixed JPEG/PNG quality collages are written at. Port of the hard-coded
/// `90` in `StripCollageBuilder.BuildSquareCollage`/`BuildThumbCollage`.
const COLLAGE_QUALITY: i32 = 90;

/// Decodes the first path that loads, tiling the rest into a `2×2` square,
/// each cell `width/2 × height/2`. Port of
/// `StripCollageBuilder.BuildSquareCollageBitmap`; [`next_valid`] wraps, so
/// fewer than four images repeat.
fn build_square_collage(paths: &[String], width: i32, height: i32) -> RgbaImage {
    let cell_w = (width / 2).max(1);
    let cell_h = (height / 2).max(1);
    let mut canvas = RgbaImage::new(
        u32::try_from(width).unwrap_or(1),
        u32::try_from(height).unwrap_or(1),
    );

    let mut index = 0usize;
    for x in 0..2 {
        for y in 0..2 {
            let Some((decoded, next)) = next_valid_image(paths, index) else {
                index = paths.len();
                continue;
            };
            index = next;

            let cell = decoded.resize_exact(
                u32::try_from(cell_w).unwrap_or(1),
                u32::try_from(cell_h).unwrap_or(1),
                imageops::FilterType::Lanczos3,
            );
            let px = i64::from(x * cell_w);
            let py = i64::from(y * cell_h);
            imageops::overlay(&mut canvas, &cell.to_rgba8(), px, py);
        }
    }

    canvas
}

/// Builds a thumb-strip collage: the first valid image scaled to the collage
/// width (aspect-preserved) and drawn at the top on a black canvas, under a
/// black rectangle at alpha `0x78`. Port of
/// `StripCollageBuilder.BuildThumbCollageBitmap` without its library-name text
/// (TODO(gl-804): the text needs a font rasterizer and a bundled font).
fn build_thumb_collage(paths: &[String], width: i32, height: i32) -> RgbaImage {
    let mut canvas = RgbaImage::from_pixel(
        u32::try_from(width).unwrap_or(1),
        u32::try_from(height).unwrap_or(1),
        image::Rgba([0, 0, 0, 0xFF]),
    );

    if let Some((backdrop, _)) = next_valid_image(paths, 0) {
        // resize to the same aspect as the original (C# width * h / w).
        let bw = i32::try_from(backdrop.width()).unwrap_or(1).max(1);
        let bh = i32::try_from(backdrop.height()).unwrap_or(1);
        let backdrop_height = (width * bh / bw).abs().max(1);
        let resized = backdrop.resize_exact(
            u32::try_from(width).unwrap_or(1),
            u32::try_from(backdrop_height).unwrap_or(1),
            imageops::FilterType::Lanczos3,
        );
        imageops::overlay(&mut canvas, &resized.to_rgba8(), 0, 0);
        // The shadow rectangle: black at alpha 0x78 over every pixel.
        for pixel in canvas.pixels_mut() {
            for channel in &mut pixel.0[..3] {
                let dimmed = (u16::from(*channel) * (255 - THUMB_SHADOW_ALPHA) + 127) / 255;
                *channel = u8::try_from(dimmed).unwrap_or(u8::MAX);
            }
        }
    }

    canvas
}

/// The alpha of the black rectangle `BuildThumbCollageBitmap` draws over the
/// backdrop (`SKColors.Black.WithAlpha(0x78)`).
const THUMB_SHADOW_ALPHA: u16 = 0x78;

/// Returns the first decodable image from `start` on, plus the index one past
/// it. See [`next_valid`].
fn next_valid_image(paths: &[String], start: usize) -> Option<(DynamicImage, usize)> {
    next_valid(paths, start, &mut |path| {
        ImageCrateEncoder::decode(path).ok()
    })
}

/// Port of `SkiaHelper.GetNextValidImage`: the first path from `start` that
/// `load` accepts, wrapping to the start of the list, plus the index one past
/// it. Each path is tried at most once; undecodable ones are skipped, and
/// `None` means no path loads.
pub(crate) fn next_valid<T>(
    paths: &[String],
    start: usize,
    load: &mut impl FnMut(&str) -> Option<T>,
) -> Option<(T, usize)> {
    (0..paths.len()).find_map(|offset| {
        let index = (start + offset) % paths.len();
        load(&paths[index]).map(|value| (value, index + 1))
    })
}

#[cfg(test)]
mod tests {
    //! Parity is dimensional + decodable-output (no byte oracle). Round-trip
    //! cases transliterate the SkiaEncoder behaviour: probe fixture dims, the
    //! `1920×1080` → `maxWidth 960` → `960×540` resize (the C#
    //! `DrawingUtils`/`ImageHelper` oracle), PNG→JPEG re-encode decodes at the
    //! target size, and the trickplay grid pack.
    use super::*;
    use ferrofin_traits::options::ItemImageInfo;
    use image::{Rgba, RgbaImage};
    use rstest::rstest;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Writes a solid-colour image of `w×h` to `dir/name` in the format implied
    /// by the extension, returning its path.
    fn fixture(dir: &TempDir, name: &str, w: u32, h: u32) -> String {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba([0x30, 0x60, 0x90, 0xFF]);
        }
        let path: PathBuf = dir.path().join(name);
        DynamicImage::ImageRgba8(img)
            .save(&path)
            .expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    fn out_path(dir: &TempDir, name: &str) -> String {
        dir.path().join(name).to_string_lossy().into_owned()
    }

    /// A gradient fixture: lossy-vs-quality effects vanish on flat colors, so
    /// the WebP quality test needs pixel variation to bite on.
    fn gradient_fixture(dir: &TempDir, name: &str, w: u32, h: u32) -> String {
        let mut img = RgbaImage::new(w, h);
        for (col, row, px) in img.enumerate_pixels_mut() {
            // Truncation is the point: wrap coordinates into channel space.
            #[allow(clippy::cast_possible_truncation)]
            let channels = [
                (col % 256) as u8,
                (row % 256) as u8,
                ((col + row) % 256) as u8,
            ];
            *px = Rgba([channels[0], channels[1], channels[2], 0xFF]);
        }
        let path: PathBuf = dir.path().join(name);
        DynamicImage::ImageRgba8(img)
            .save(&path)
            .expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn advertises_capabilities() {
        let enc = ImageCrateEncoder::new();
        assert_eq!(
            enc.supported_input_formats(),
            ["jpeg", "jpg", "png", "webp", "gif", "bmp", "ico"].map(str::to_owned)
        );
        assert_eq!(
            enc.supported_output_formats(),
            vec![ImageFormat::Webp, ImageFormat::Jpg, ImageFormat::Png]
        );
        assert_eq!(enc.name(), "Image Crate");
        assert!(enc.supports_image_collage_creation());
        assert!(enc.supports_image_encoding());
    }

    #[tokio::test]
    async fn probe_fixture_dims() {
        let dir = TempDir::new().expect("tempdir");
        let path = fixture(&dir, "probe.png", 1920, 1080);
        let enc = ImageCrateEncoder::new();
        let dims = enc.get_image_size(&path).await.expect("probe");
        assert_eq!(dims, ImageDimensions::new(1920, 1080));
    }

    #[tokio::test]
    async fn probe_zero_byte_returns_default() {
        let dir = TempDir::new().expect("tempdir");
        let path = out_path(&dir, "empty.png");
        std::fs::write(&path, []).expect("write empty");
        let enc = ImageCrateEncoder::new();
        let dims = enc.get_image_size(&path).await.expect("probe");
        assert_eq!(dims, ImageDimensions::default());
    }

    #[tokio::test]
    async fn probe_undecodable_returns_default() {
        let dir = TempDir::new().expect("tempdir");
        let path = out_path(&dir, "junk.png");
        std::fs::write(&path, b"not an image").expect("write junk");
        let enc = ImageCrateEncoder::new();
        let dims = enc.get_image_size(&path).await.expect("probe");
        assert_eq!(dims, ImageDimensions::default());
    }

    #[tokio::test]
    async fn probe_missing_is_not_found() {
        let enc = ImageCrateEncoder::new();
        assert!(matches!(
            enc.get_image_size("/no/such/file.png").await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn resize_1920x1080_max_width_960_yields_960x540() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "big.png", 1920, 1080);
        let output = out_path(&dir, "small.png");
        let enc = ImageCrateEncoder::new();

        let options = ImageProcessingOptions {
            max_width: Some(960),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let written = enc
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await
            .expect("encode");
        assert_eq!(written, output);

        let dims = enc.get_image_size(&output).await.expect("probe out");
        assert_eq!(dims, ImageDimensions::new(960, 540));
    }

    /// The formats Skia decodes beyond jpeg/png/webp (`SkiaEncoder.
    /// SupportedInputFormats`) that the `image` crate can: their size and
    /// blurhash are read and a resize is encoded, not served as the original.
    #[rstest]
    #[case::bmp("poster.bmp")]
    #[case::ico("poster.ico")]
    #[case::gif("poster.gif")]
    #[tokio::test]
    async fn bmp_ico_and_gif_inputs_are_decoded_and_resized(#[case] name: &str) {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, name, 200, 100);
        let output = out_path(&dir, "small.png");
        let enc = ImageCrateEncoder::new();
        assert_eq!(
            enc.get_image_size(&input).await.expect("probe in"),
            ImageDimensions::new(200, 100)
        );
        assert!(
            !enc.get_image_blur_hash(4, 3, &input)
                .await
                .expect("blurhash")
                .is_empty()
        );
        let options = ImageProcessingOptions {
            max_width: Some(100),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let written = enc
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await
            .expect("encode");
        assert_eq!(written, output);
        assert_eq!(
            enc.get_image_size(&output).await.expect("probe out"),
            ImageDimensions::new(100, 50)
        );
    }

    /// An EXIF block holding only `Orientation = 6` (rotate 90° clockwise),
    /// as a phone writes a portrait shot stored landscape: a little-endian
    /// TIFF header and one IFD entry.
    const ORIENTATION_6_EXIF: [u8; 26] = [
        0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // "II*", IFD at 8
        0x01, 0x00, // one entry
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
        0x00, // Orientation = 6
        0x00, 0x00, 0x00, 0x00, // no next IFD
    ];

    /// `SkiaEncoder.GetBitmap(path, autoOrient: true, …)`: the file's EXIF
    /// origin is applied before the resize, so a 40×20 JPEG tagged with
    /// orientation 6 comes out 20×40; without auto-orientation it is left as
    /// stored.
    #[tokio::test]
    async fn auto_orient_applies_the_exif_orientation() {
        use image::ImageEncoder as _;
        let dir = TempDir::new().expect("tempdir");
        let input = out_path(&dir, "phone.jpg");
        {
            let pixels = image::RgbImage::from_pixel(40, 20, image::Rgb([200, 30, 30]));
            let file = std::fs::File::create(&input).expect("fixture");
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
            encoder
                .set_exif_metadata(ORIENTATION_6_EXIF.to_vec())
                .expect("exif");
            encoder
                .write_image(&pixels, 40, 20, image::ExtendedColorType::Rgb8)
                .expect("write fixture");
        }
        let enc = ImageCrateEncoder::new();
        // The resize measures the upright image: a 10-pixel height bound
        // halves the rotated 20×40 to 5×10.
        for (auto_orient, max_height, expected) in [
            (true, 100, (20, 40)),
            (false, 100, (40, 20)),
            (true, 10, (5, 10)),
        ] {
            let output = out_path(&dir, &format!("out-{auto_orient}-{max_height}.jpg"));
            let options = ImageProcessingOptions {
                max_height: Some(max_height),
                supported_output_formats: vec![ImageFormat::Jpg],
                ..Default::default()
            };
            let written = enc
                .encode_image(
                    &input,
                    Utc::now(),
                    &output,
                    auto_orient,
                    None,
                    90,
                    &options,
                    ImageFormat::Jpg,
                )
                .await
                .expect("encode");
            let dims = enc.get_image_size(&written).await.expect("probe out");
            assert_eq!(
                (dims.width, dims.height),
                expected,
                "auto_orient {auto_orient}"
            );
        }
    }

    /// Transliterated from upstream `ImageHelperTests` (jellyfin#17056): an
    /// explicit size larger than a 600x336 source is clamped to it.
    #[rstest]
    #[case::explicit_size_larger_than_source(Some(23100), Some(23100), None, None, None, 336, 336)]
    #[case::width_larger_than_source(Some(10000), None, None, None, None, 600, 336)]
    #[case::fill_larger_than_source(None, None, None, Some(23100), Some(23100), 600, 336)]
    #[case::smaller_than_source_still_downscales(None, None, Some(300), None, None, 300, 168)]
    #[case::no_size_requested_returns_source(None, None, None, None, None, 600, 336)]
    fn get_new_image_size_never_exceeds_source(
        #[case] width: Option<i32>,
        #[case] height: Option<i32>,
        #[case] max_width: Option<i32>,
        #[case] fill_width: Option<i32>,
        #[case] fill_height: Option<i32>,
        #[case] expected_width: i32,
        #[case] expected_height: i32,
    ) {
        let options = ImageProcessingOptions {
            width,
            height,
            max_width,
            fill_width,
            fill_height,
            ..Default::default()
        };
        assert_eq!(
            get_new_image_size(&options, ImageDimensions::new(600, 336)),
            ImageDimensions::new(expected_width, expected_height)
        );
    }

    #[tokio::test]
    async fn oversized_request_encodes_at_source_size() {
        // Regression: width/height went straight to resize_exact, so one
        // request could ask for an unbounded allocation.
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 60, 40);
        let output = out_path(&dir, "out.png");
        let enc = ImageCrateEncoder::new();

        let options = ImageProcessingOptions {
            width: Some(i32::MAX),
            height: Some(i32::MAX),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        enc.encode_image(
            &input,
            Utc::now(),
            &output,
            false,
            None,
            90,
            &options,
            ImageFormat::Png,
        )
        .await
        .expect("encode");

        let dims = enc.get_image_size(&output).await.expect("probe out");
        assert_eq!(dims, ImageDimensions::new(40, 40));
    }

    /// `Resize` rounds one axis to 0 and `ScaleDownToFit` passes it through.
    /// Skia fails on the empty surface (so the original is served); this must
    /// fail too, never resize to 1 x (unclamped other axis).
    #[rstest]
    #[case::zero_width(Some(1), Some(i32::MAX), None, Some(1_000_000_000))]
    #[case::zero_height(Some(i32::MAX), Some(1), Some(1_000_000_000), None)]
    #[tokio::test]
    async fn zero_axis_size_is_an_error(
        #[case] width: Option<i32>,
        #[case] height: Option<i32>,
        #[case] max_width: Option<i32>,
        #[case] max_height: Option<i32>,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 60, 40);
        let output = out_path(&dir, "out.png");
        let options = ImageProcessingOptions {
            width,
            height,
            max_width,
            max_height,
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let result = ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await;
        assert!(matches!(result, Err(ServiceError::InvalidInput(_))));
        assert!(!Path::new(&output).exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_encodes_all_complete_under_the_permit_bound() {
        // More concurrent encodes than the semaphore allows must still all finish
        // (the excess wait for a permit rather than deadlocking or being dropped).
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 400, 300);
        let enc = ImageCrateEncoder::new();
        let options = ImageProcessingOptions {
            width: Some(100),
            height: Some(75),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };

        let mut handles = Vec::new();
        for i in 0..32 {
            let (e, inp, op) = (enc.clone(), input.clone(), options.clone());
            let out = out_path(&dir, &format!("o{i}.png"));
            handles.push(tokio::spawn(async move {
                let written = e
                    .encode_image(
                        &inp,
                        Utc::now(),
                        &out,
                        false,
                        None,
                        90,
                        &op,
                        ImageFormat::Png,
                    )
                    .await
                    .expect("encode");
                assert_eq!(written, out);
            }));
        }
        for h in handles {
            h.await.expect("task");
        }
    }

    /// libwebp cannot write a side over 16383 px, and its `encode` panicked on
    /// one (`VP8_ENC_ERROR_BAD_DIMENSION`). Such an output is now refused as
    /// invalid input before encoding, like the JPEG limit, so `process_image`
    /// logs it at debug and serves the original; a side of exactly 16383 px
    /// still encodes.
    #[rstest]
    #[case(16_384, 1)]
    #[case(1, 16_384)]
    fn a_webp_output_over_libwebps_limit_is_invalid_input_not_a_panic(
        #[case] width: u32,
        #[case] height: u32,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let out = out_path(&dir, "huge.webp");

        let err = ImageCrateEncoder::write(
            &DynamicImage::new_rgba8(width, height),
            &out,
            CrateFormat::WebP,
            90,
        )
        .expect_err("libwebp's dimension limit");

        assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
        assert!(err.to_string().contains("16383x16383 limit"), "{err}");
        assert!(!Path::new(&out).exists(), "nothing is written");

        let edge = out_path(&dir, "edge.webp");
        ImageCrateEncoder::write(
            &DynamicImage::new_rgba8(width.min(16_383), height.min(16_383)),
            &edge,
            CrateFormat::WebP,
            90,
        )
        .expect("16383 px is within the limit");
        assert!(Path::new(&edge).exists());
    }

    /// Any other libwebp failure is an ordinary encode error too, not the
    /// `unwrap` panic of `Encoder::encode`: a zero-width image, which libwebp
    /// rejects itself.
    #[test]
    fn a_libwebp_failure_is_an_encode_error_not_a_panic() {
        let dir = TempDir::new().expect("tempdir");
        let out = out_path(&dir, "empty.webp");

        let err =
            ImageCrateEncoder::write(&DynamicImage::new_rgba8(0, 1), &out, CrateFormat::WebP, 90)
                .expect_err("libwebp refuses an empty picture");

        assert!(matches!(err, ServiceError::BackendSource(_)), "{err:?}");
        assert!(err.to_string().contains("libwebp VP8_ENC_ERROR"), "{err}");
        assert!(!Path::new(&out).exists(), "nothing is written");
    }

    #[tokio::test]
    async fn webp_output_is_lossy_and_honors_quality() {
        // Regression: the `image` crate's WebP encoder is lossless-only and
        // ignores quality; the libwebp branch must produce LOSSY output whose
        // size tracks the quality parameter (Skia parity: Encode(..., quality)).
        let dir = TempDir::new().expect("tempdir");
        let input = gradient_fixture(&dir, "src.png", 400, 300);
        let enc = ImageCrateEncoder::new();
        let options = ImageProcessingOptions {
            max_width: Some(200),
            supported_output_formats: vec![ImageFormat::Webp],
            ..Default::default()
        };

        let mut sizes = Vec::new();
        for quality in [10, 90] {
            let out = out_path(&dir, &format!("q{quality}.webp"));
            let written = enc
                .encode_image(
                    &input,
                    Utc::now(),
                    &out,
                    false,
                    None,
                    quality,
                    &options,
                    ImageFormat::Webp,
                )
                .await
                .expect("encode");
            assert_eq!(written, out);
            // The output really is a decodable WebP at the resized dimensions.
            assert_eq!(
                enc.get_image_size(&out).await.expect("probe"),
                ImageDimensions::new(200, 150)
            );
            sizes.push(std::fs::metadata(&out).expect("stat").len());
        }
        assert!(
            sizes[0] < sizes[1],
            "q10 ({}) must be smaller than q90 ({}): quality is ignored (lossless?)",
            sizes[0],
            sizes[1]
        );
    }

    #[tokio::test]
    async fn png_to_jpeg_decodes_at_target_size() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 1000, 500);
        let output = out_path(&dir, "out.jpg");
        let enc = ImageCrateEncoder::new();

        let options = ImageProcessingOptions {
            width: Some(200),
            height: Some(100),
            ..Default::default()
        };
        let written = enc
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                80,
                &options,
                ImageFormat::Jpg,
            )
            .await
            .expect("encode");
        assert_eq!(written, output);

        // The JPEG must decode back at the requested target size.
        let dims = enc.get_image_size(&output).await.expect("probe jpeg");
        assert_eq!(dims, ImageDimensions::new(200, 100));
    }

    /// The (horizontal, vertical) sampling factors of every component in a
    /// JPEG's SOF marker, in declaration order (Y, Cb, Cr).
    ///
    /// `[(2, 2), (1, 1), (1, 1)]` is 4:2:0 — luma sampled twice per chroma
    /// sample on both axes; `[(1, 1), (1, 1), (1, 1)]` is 4:4:4. The component
    /// *identifiers* are deliberately not returned: `jpeg-encoder` numbers them
    /// 0/1/2 where libjpeg (and therefore Skia) uses the JFIF-conventional
    /// 1/2/3. Decoders bind components positionally, so that difference is
    /// invisible to every client; the sampling factors are not.
    fn jpeg_sampling_factors(bytes: &[u8]) -> Vec<(u8, u8)> {
        let mut i = 2; // skip SOI
        while i + 4 <= bytes.len() {
            assert_eq!(bytes[i], 0xFF, "not a JPEG marker at offset {i}");
            let marker = bytes[i + 1];
            let length = (usize::from(bytes[i + 2]) << 8) | usize::from(bytes[i + 3]);
            // SOF0 (baseline), SOF1 (extended sequential), SOF2 (progressive).
            if (0xC0..=0xC2).contains(&marker) {
                let count = usize::from(bytes[i + 9]);
                return (0..count)
                    .map(|k| {
                        let sampling = bytes[i + 11 + 3 * k];
                        (sampling >> 4, sampling & 0x0F)
                    })
                    .collect();
            }
            i += 2 + length;
        }
        panic!("no SOF marker found");
    }

    /// The number of symbols in each of a JPEG's Huffman tables (DHT segments),
    /// in file order.
    ///
    /// libjpeg's *standard* tables always have 12/162 symbols per DC/AC table;
    /// tables derived from the image's own statistics (`optimize_coding = TRUE`)
    /// are shorter. Jellyfin 10.11.8's trickplay tile, measured on the wire,
    /// carries 11/93/11/56 — so Skia optimizes, and a Ferrofin tile with the
    /// standard 12/162/12/162 is twice the size for identical pixels.
    fn jpeg_huffman_symbol_counts(bytes: &[u8]) -> Vec<usize> {
        let mut counts = Vec::new();
        let mut i = 2;
        while i + 4 <= bytes.len() {
            assert_eq!(bytes[i], 0xFF, "not a JPEG marker at offset {i}");
            let marker = bytes[i + 1];
            let length = (usize::from(bytes[i + 2]) << 8) | usize::from(bytes[i + 3]);
            if marker == 0xDA {
                break; // start of scan: no more tables
            }
            if marker == 0xC4 {
                let segment = &bytes[i + 4..i + 2 + length];
                let mut j = 0;
                while j + 17 <= segment.len() {
                    let symbols: usize =
                        segment[j + 1..j + 17].iter().map(|c| usize::from(*c)).sum();
                    counts.push(symbols);
                    j += 17 + symbols;
                }
            }
            i += 2 + length;
        }
        counts
    }

    /// Skia sets libjpeg's `optimize_coding`, so its Huffman tables are derived
    /// from the image rather than being the standard 12/162-symbol pair. On the
    /// flat content a trickplay tile is made of this is a 2x difference in bytes
    /// — and `TrickplayManager` turns bytes straight into the `Bandwidth` a
    /// client budgets its scrub prefetch from, so it is not a disk-space nicety.
    #[tokio::test]
    async fn jpeg_uses_optimized_huffman_tables() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 64, 64);
        let output = out_path(&dir, "out.jpg");

        ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                90,
                &ImageProcessingOptions {
                    width: Some(32),
                    height: Some(32),
                    ..Default::default()
                },
                ImageFormat::Jpg,
            )
            .await
            .expect("encode");

        let counts = jpeg_huffman_symbol_counts(&std::fs::read(&output).expect("read jpeg"));
        assert!(!counts.is_empty(), "no DHT segment found");
        assert!(
            counts.iter().all(|n| *n < 162),
            "standard (unoptimized) libjpeg tables leaked through: {counts:?}"
        );
    }

    /// Skia writes every JPEG with `SkJpegEncoder`'s default `Downsample::k420`
    /// — at *every* quality, and on all three of Jellyfin's JPEG writes
    /// (`SkiaEncoder.cs` v10.11.8 lines 628, 670, 778). A 4:4:4 JPEG is ~3x the
    /// bytes, which `TrickplayManager` turns straight into an inflated
    /// `TrickplayInfo.Bandwidth`, so the subsampling is user-visible and is
    /// pinned here.
    ///
    /// The high-quality case is the one that regresses silently: `jpeg-encoder`'s
    /// own default sampling factor flips to 4:4:4 at `quality >= 90`, which is
    /// exactly the quality trickplay tiles are written at.
    #[rstest]
    #[case(80)]
    #[case(90)]
    #[case(100)]
    #[tokio::test]
    async fn jpeg_is_written_with_420_chroma_subsampling_at_every_quality(#[case] quality: i32) {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "src.png", 64, 64);
        let output = out_path(&dir, "out.jpg");
        let enc = ImageCrateEncoder::new();

        enc.encode_image(
            &input,
            Utc::now(),
            &output,
            false,
            None,
            quality,
            &ImageProcessingOptions {
                width: Some(32),
                height: Some(32),
                ..Default::default()
            },
            ImageFormat::Jpg,
        )
        .await
        .expect("encode");

        let bytes = std::fs::read(&output).expect("read jpeg");
        assert_eq!(
            jpeg_sampling_factors(&bytes),
            vec![(2, 2), (1, 1), (1, 1)],
            "quality {quality} must still be 4:2:0"
        );
    }

    #[tokio::test]
    async fn unsupported_input_returns_input_path() {
        let enc = ImageCrateEncoder::new();
        let options = ImageProcessingOptions::default();
        let written = enc
            .encode_image(
                "/some/movie.mkv",
                Utc::now(),
                "/out.png",
                false,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await
            .expect("passthrough");
        assert_eq!(written, "/some/movie.mkv");
    }

    #[tokio::test]
    async fn default_options_spit_out_original() {
        let dir = TempDir::new().expect("tempdir");
        // High quality, matching format, and fill bounds covering the source →
        // HasDefaultOptions holds (the C# guard rejects a source larger than the
        // unset-zero FillWidth/Height, so the fill bounds must cover it).
        let input = fixture(&dir, "orig.jpg", 640, 480);
        let output = out_path(&dir, "should_not_exist.jpg");
        let enc = ImageCrateEncoder::new();

        let options = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            fill_width: Some(640),
            fill_height: Some(480),
            ..Default::default()
        };
        let written = enc
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                95,
                &options,
                ImageFormat::Jpg,
            )
            .await
            .expect("encode");
        // Original path returned untouched; output never written.
        assert_eq!(written, input);
        assert!(!Path::new(&output).exists());
    }

    #[tokio::test]
    async fn trickplay_grid_pack() {
        let dir = TempDir::new().expect("tempdir");
        // 3 tiles of 320x180 packed into a 2x2 grid.
        let inputs: Vec<String> = (0..3)
            .map(|i| fixture(&dir, &format!("t{i}.png"), 320, 180))
            .collect();
        let output = out_path(&dir, "tile.jpg");
        let enc = ImageCrateEncoder::new();

        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: output.clone(),
            width: 2,
            height: 2,
        };
        let returned_height = enc
            .create_trickplay_tile(&options, 90, 320, None)
            .await
            .expect("trickplay");
        assert_eq!(returned_height, 180);

        // Grid is imgW*tileW x imgH*tileH = 640 x 360.
        let dims = enc.get_image_size(&output).await.expect("probe tile");
        assert_eq!(dims, ImageDimensions::new(640, 360));
    }

    #[tokio::test]
    async fn trickplay_empty_inputs_is_invalid() {
        let enc = ImageCrateEncoder::new();
        let options = ImageCollageOptions {
            input_paths: Vec::new(),
            output_path: "/out.jpg".to_owned(),
            width: 2,
            height: 2,
        };
        assert!(matches!(
            enc.create_trickplay_tile(&options, 90, 320, Some(180))
                .await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn trickplay_over_full_grid_is_invalid() {
        let dir = TempDir::new().expect("tempdir");
        let inputs: Vec<String> = (0..5)
            .map(|i| fixture(&dir, &format!("o{i}.png"), 100, 100))
            .collect();
        let enc = ImageCrateEncoder::new();
        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: out_path(&dir, "o.jpg"),
            width: 2,
            height: 2,
        };
        // 5 images do not fit a 2x2 grid.
        assert!(matches!(
            enc.create_trickplay_tile(&options, 90, 100, Some(100))
                .await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn trickplay_width_mismatch_is_invalid() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "w.png", 320, 180);
        let enc = ImageCrateEncoder::new();
        let options = ImageCollageOptions {
            input_paths: vec![input],
            output_path: out_path(&dir, "w.jpg"),
            width: 1,
            height: 1,
        };
        // Provided img_width (999) does not match the decoded 320.
        assert!(matches!(
            enc.create_trickplay_tile(&options, 90, 999, None).await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn trickplay_overflowing_grid_is_invalid() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "g.png", 16, 16);
        let enc = ImageCrateEncoder::new();
        let options = ImageCollageOptions {
            input_paths: vec![input],
            output_path: out_path(&dir, "g.jpg"),
            // 16 * (i32::MAX / 8) overflows i32 instead of wrapping into a huge buffer.
            width: i32::MAX / 8,
            height: 1,
        };
        assert!(matches!(
            enc.create_trickplay_tile(&options, 90, 16, None).await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn square_collage_writes_output() {
        let dir = TempDir::new().expect("tempdir");
        let inputs: Vec<String> = (0..4)
            .map(|i| fixture(&dir, &format!("c{i}.png"), 200, 200))
            .collect();
        let output = out_path(&dir, "collage.png");
        let enc = ImageCrateEncoder::new();

        // ratio 1.0 (< 1.4) → square collage of 400x400.
        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: output.clone(),
            width: 400,
            height: 400,
        };
        enc.create_image_collage(&options, None)
            .await
            .expect("collage");
        let dims = enc.get_image_size(&output).await.expect("probe collage");
        assert_eq!(dims, ImageDimensions::new(400, 400));
    }

    #[tokio::test]
    async fn thumb_collage_writes_output() {
        let dir = TempDir::new().expect("tempdir");
        let inputs = vec![fixture(&dir, "b.png", 1920, 1080)];
        let output = out_path(&dir, "thumb.png");
        let enc = ImageCrateEncoder::new();

        // ratio 16/9 ≈ 1.78 (>= 1.4) → thumb strip of 960x540.
        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: output.clone(),
            width: 960,
            height: 540,
        };
        enc.create_image_collage(&options, Some("Movies"))
            .await
            .expect("thumb collage");
        let dims = enc.get_image_size(&output).await.expect("probe thumb");
        assert_eq!(dims, ImageDimensions::new(960, 540));
    }

    /// Port of `SkiaEncoder.CreateSplashscreen` + `SplashscreenBuilder`: a
    /// 1920x1080 PNG at `{DataPath}/splashscreen.png`, built from however few
    /// posters and backdrops there are (they repeat, `GetNextValidImage` wraps).
    #[tokio::test]
    async fn the_splashscreen_is_written_to_the_data_dir() {
        let dir = TempDir::new().expect("tempdir");
        let posters = vec![fixture(&dir, "poster.png", 200, 300)];
        let backdrops = vec![fixture(&dir, "thumb.png", 320, 180)];
        let enc = ImageCrateEncoder::new().with_data_path(dir.path());
        enc.create_splashscreen(&posters, &backdrops)
            .await
            .expect("splashscreen");
        let output = dir.path().join("splashscreen.png");
        let image = image::open(&output).expect("a PNG");
        assert_eq!((image.width(), image.height()), (1920, 1080));
        // The tilted wall fills the middle of the frame with tiles; only the
        // 20 px gaps between them are black.
        let image = image.to_rgba8();
        let middle: Vec<_> = (760..1160)
            .flat_map(|x| (340..740).map(move |y| (x, y)))
            .map(|(x, y)| image.get_pixel(x, y).0)
            .collect();
        let tiled = middle
            .iter()
            .filter(|p| **p == [0x30, 0x60, 0x90, 0xFF])
            .count();
        assert!(
            tiled * 2 > middle.len(),
            "{tiled} of {} tile pixels",
            middle.len()
        );
    }

    /// Upstream generates nothing unless both lists have an entry.
    #[rstest]
    #[case::no_posters(false, true)]
    #[case::no_backdrops(true, false)]
    #[tokio::test]
    async fn the_splashscreen_needs_a_poster_and_a_backdrop(
        #[case] with_poster: bool,
        #[case] with_backdrop: bool,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let posters: Vec<String> = with_poster
            .then(|| fixture(&dir, "poster.png", 200, 300))
            .into_iter()
            .collect();
        let backdrops: Vec<String> = with_backdrop
            .then(|| fixture(&dir, "thumb.png", 320, 180))
            .into_iter()
            .collect();
        let enc = ImageCrateEncoder::new().with_data_path(dir.path());
        enc.create_splashscreen(&posters, &backdrops)
            .await
            .expect("no-op");
        assert!(!dir.path().join("splashscreen.png").exists());
    }

    /// `GenerateCollage` throws when a list has no decodable image left.
    #[tokio::test]
    async fn a_splashscreen_with_no_decodable_poster_is_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let broken = dir.path().join("broken.png");
        std::fs::write(&broken, b"not an image").expect("write");
        let backdrops = vec![fixture(&dir, "thumb.png", 320, 180)];
        let enc = ImageCrateEncoder::new().with_data_path(dir.path());
        assert!(matches!(
            enc.create_splashscreen(&[broken.to_string_lossy().into_owned()], &backdrops)
                .await,
            Err(ServiceError::InvalidInput(_))
        ));
        assert!(!dir.path().join("splashscreen.png").exists());
    }

    /// `GetNextValidImage` wraps to the start of the list, so a square collage
    /// of two images repeats them instead of leaving cells empty.
    #[tokio::test]
    async fn a_square_collage_of_two_images_fills_every_cell() {
        let dir = TempDir::new().expect("tempdir");
        let inputs: Vec<String> = (0..2)
            .map(|i| fixture(&dir, &format!("c{i}.png"), 200, 200))
            .collect();
        let output = out_path(&dir, "collage.png");
        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: output.clone(),
            width: 400,
            height: 400,
        };
        ImageCrateEncoder::new()
            .create_image_collage(&options, None)
            .await
            .expect("collage");
        let collage = image::open(&output).expect("png").to_rgba8();
        for (x, y) in [(100, 100), (300, 100), (100, 300), (300, 300)] {
            assert_eq!(
                collage.get_pixel(x, y).0,
                [0x30, 0x60, 0x90, 0xFF],
                "cell at {x},{y}"
            );
        }
    }

    /// `BuildThumbCollageBitmap` draws a black rectangle at alpha `0x78` over
    /// the backdrop.
    #[tokio::test]
    async fn a_thumb_collage_is_dimmed() {
        let dir = TempDir::new().expect("tempdir");
        let inputs = vec![fixture(&dir, "b.png", 1920, 1080)];
        let output = out_path(&dir, "thumb.png");
        let options = ImageCollageOptions {
            input_paths: inputs,
            output_path: output.clone(),
            width: 960,
            height: 540,
        };
        ImageCrateEncoder::new()
            .create_image_collage(&options, Some("Movies"))
            .await
            .expect("thumb collage");
        let pixel = image::open(&output)
            .expect("png")
            .to_rgba8()
            .get_pixel(480, 270)
            .0;
        // 0x30/0x60/0x90 under black at 0x78/0xFF: c * (255 - 120) / 255.
        assert_eq!(pixel, [25, 51, 76, 0xFF]);
    }

    #[tokio::test]
    async fn blur_hash_encodes_a_well_formed_deterministic_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("swatch.png");
        let mut img = image::RgbaImage::new(16, 16); // two-tone swatch
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x < 8 {
                image::Rgba([200, 40, 40, 255])
            } else {
                image::Rgba([40, 40, 200, 255])
            };
        }
        img.save(&path).unwrap();

        let enc = ImageCrateEncoder::new();
        let hash = enc
            .get_image_blur_hash(4, 3, path.to_str().unwrap())
            .await
            .expect("blurhash");
        // Well-formed length: 6 + (xComp*yComp - 1)*2 base83 chars.
        assert_eq!(
            hash.len(),
            6 + (4 * 3 - 1) * 2,
            "unexpected blurhash: {hash}"
        );
        // Deterministic for a fixed input.
        let again = enc
            .get_image_blur_hash(4, 3, path.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(hash, again, "blurhash must be deterministic");
    }

    /// Guards that `ImageProcessingOptions` with a filled image field still
    /// resolves default-options correctly (exercises the struct import).
    #[tokio::test]
    async fn item_image_info_field_is_usable() {
        let opts = ImageProcessingOptions {
            image: ItemImageInfo::default(),
            ..Default::default()
        };
        assert!(opts.image.is_local_file());
    }

    /// The dimension probe must be dispatched to the blocking pool, not run
    /// inline on the async worker.
    ///
    /// Discriminating by starvation: the runtime gets exactly one blocking
    /// thread and that thread is held busy, so a `spawn_blocking` probe is
    /// *queued* and the timeout elapses — while the previous inline
    /// `Path::exists` + `std::fs::metadata` + `ImageReader::open` version returns
    /// in microseconds no matter how starved the pool is, failing the assertion.
    /// The second half proves the probe still answers correctly once the pool is
    /// free, so the test cannot pass by simply breaking the probe.
    ///
    /// It matters because `DtoService::primary_aspect_ratio` reaches this call
    /// once per item whose stored image dimensions are `0`; on network-backed
    /// media a cold probe parks the calling thread for the length of the
    /// syscall, a whole page of them per request.
    #[test]
    fn image_size_probe_goes_through_the_blocking_pool() {
        // Only bounds the failing direction: an inline probe of a just-written
        // temp file returns in microseconds, so any value well above the noise
        // floor works. Nothing waits on this when the code is correct.
        const STARVED_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

        let dir = TempDir::new().expect("tempdir");
        let path = fixture(&dir, "probe.png", 64, 32);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let encoder = ImageCrateEncoder::new();

            // Occupy the single blocking thread, and wait until it is provably
            // busy so the probe below cannot win the race for it.
            let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                busy_tx.send(()).ok();
                release_rx.recv().ok();
            });
            busy_rx.await.expect("hog started");

            let starved = tokio::time::timeout(STARVED_WAIT, encoder.get_image_size(&path)).await;
            assert!(
                starved.is_err(),
                "the probe completed with the blocking pool starved, so it ran inline on the \
                 async worker thread"
            );

            // Free the pool and confirm the same call still reports real dimensions.
            release_tx.send(()).expect("release hog");
            hog.await.expect("hog joined");
            let dims = encoder.get_image_size(&path).await.expect("probe");
            assert_eq!((dims.width, dims.height), (64, 32));
        });
    }

    #[test]
    fn blur_hash_collage_and_trickplay_go_through_the_blocking_pool() {
        // Like the probe test above: the three CPU/decode methods must offload
        // to the blocking pool, not run inline on the async worker. With the one
        // blocking thread hogged, each call must fail to complete in the window.
        // Inputs are valid so the O(1) validation passes and execution reaches
        // the offloaded work; the decode never runs because the pool is starved.
        const STARVED_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

        let dir = TempDir::new().expect("tempdir");
        let img = fixture(&dir, "work.png", 100, 100);
        let collage_inputs: Vec<String> = (0..4)
            .map(|i| fixture(&dir, &format!("w{i}.png"), 200, 200))
            .collect();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let encoder = ImageCrateEncoder::new();

            // Occupy the single blocking thread, provably, before the calls.
            let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let hog = tokio::task::spawn_blocking(move || {
                busy_tx.send(()).ok();
                release_rx.recv().ok();
            });
            busy_rx.await.expect("hog started");

            let blur =
                tokio::time::timeout(STARVED_WAIT, encoder.get_image_blur_hash(4, 3, &img)).await;
            assert!(
                blur.is_err(),
                "get_image_blur_hash completed with the blocking pool starved, so it ran inline"
            );

            let collage_options = ImageCollageOptions {
                input_paths: collage_inputs.clone(),
                output_path: out_path(&dir, "work-collage.png"),
                width: 400,
                height: 400,
            };
            let collage = tokio::time::timeout(
                STARVED_WAIT,
                encoder.create_image_collage(&collage_options, None),
            )
            .await;
            assert!(
                collage.is_err(),
                "create_image_collage completed with the blocking pool starved, so it ran inline"
            );

            let tile_options = ImageCollageOptions {
                input_paths: vec![img.clone()],
                output_path: out_path(&dir, "work-tile.jpg"),
                width: 1,
                height: 1,
            };
            let tile = tokio::time::timeout(
                STARVED_WAIT,
                encoder.create_trickplay_tile(&tile_options, 90, 100, Some(100)),
            )
            .await;
            assert!(
                tile.is_err(),
                "create_trickplay_tile completed with the blocking pool starved, so it ran inline"
            );

            release_tx.send(()).expect("release hog");
            hog.await.expect("hog joined");
        });
    }
}
