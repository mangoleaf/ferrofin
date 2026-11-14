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
//!   [`ServiceError::BackendSource`] for an I/O/encode failure (a
//!   [`DrawingError`](crate::error::DrawingError), its cause kept as the source).
//! - `GetImageSize` returns [`ImageDimensions::default`] (`0×0`) on a zero-byte or
//!   undecodable file, exactly like the Skia `return default;` fallbacks.
//! - `CreateSplashscreen` builds `{DataPath}/splashscreen.png` (see
//!   `crate::splashscreen`). `GetImageBlurHash` is implemented (decode → 128x128 resize → BlurHash-encode);
//!   its output is valid but not byte-identical to Skia's (see the method doc).
//! - A JPEG headed for a blur hash, a smaller `EncodeImage` output, a collage
//!   cell or a splashscreen tile is decoded at the smallest DCT scale (1/8,
//!   1/4, 1/2) that still covers the target (twice the target for everything
//!   but the blur hash, `THUMBNAIL_DECODE_FACTOR`), as Skia's codec decodes it
//!   for upstream's blur hash, so its full-size bitmap is never built (see
//!   `scaled_jpeg`). Other formats, and targets no reduced scale covers,
//!   decode in full.
//! - Every decode and resize runs under [`ImageLimits`] ([`crate::limits`]),
//!   which upstream has no counterpart for. An image they refuse is
//!   [`ServiceError::InvalidInput`], and the processor serves the original.
//! - `EncodeImage`'s background colour, blur and foreground layer are drawn
//!   after the resize (`composite`); its unplayed/percent-played indicators
//!   are not (gl-806), so a request for one alone is resized and re-encoded
//!   undecorated.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

use crate::error::DrawingError;
use crate::limits::{ImageLimits, ImageMemory};
use crate::scaled_jpeg::{ScalableJpeg, swaps_axes};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::drawing::drawing_utils::{resize, resize_fill, scale_down_to_fit};
use ferrofin_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions};
use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat as CrateFormat, RgbImage, RgbaImage, imageops};
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
    /// The limits every decode and resize runs under, and the resize budget
    /// all of them share.
    memory: Arc<ImageMemory>,
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

    /// The encoder running under `limits` instead of [`ImageLimits`]'s defaults.
    #[must_use]
    pub fn with_limits(mut self, limits: ImageLimits) -> Self {
        self.memory = Arc::new(ImageMemory::new(limits));
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
    ///
    /// The image is encoded in memory and lands at `output_path` through
    /// [`Self::write_encoded`].
    fn write(
        image: &DynamicImage,
        output_path: &str,
        format: CrateFormat,
        quality: i32,
    ) -> Result<(), ServiceError> {
        let bytes = Self::encode(image, output_path, format, quality)?;
        Self::write_encoded(&bytes, output_path)
    }

    /// Writes an encoded image to `output_path`, creating the parent directory
    /// first, through a temporary file and a rename, so the path never holds a
    /// partial image. Upstream streams straight into the destination, and the
    /// processor serves whatever file it finds at a cache path with an
    /// immutable year-long cache header: a request racing an encode, or one
    /// after an encode that failed part-way, would serve and pin the truncated
    /// file.
    fn write_encoded(bytes: &[u8], output_path: &str) -> Result<(), ServiceError> {
        if let Some(parent) = Path::new(output_path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| DrawingError::io(format!("mkdir {}", parent.display()), e))?;
        }

        ferrofin_util::file_helper::atomic_write_media(Path::new(output_path), bytes)
            .map_err(|e| DrawingError::io(format!("write {output_path}"), e).into())
    }

    /// Encodes `image` in `format` at `quality`; `output_path` only names the
    /// destination in errors.
    fn encode(
        image: &DynamicImage,
        output_path: &str,
        format: CrateFormat,
        quality: i32,
    ) -> Result<Vec<u8>, ServiceError> {
        if format == CrateFormat::Jpeg {
            Self::encode_jpeg(image, output_path, quality)
        } else if format == CrateFormat::WebP {
            // LOSSY WebP via libwebp (the `webp` crate), mirroring Skia's
            // `bitmap.Encode(stream, Webp, quality)`. The `image` crate's own
            // WebP encoder is lossless-only and ignores `quality`, which made
            // every poster several times larger than Jellyfin's — so this branch
            // routes around it.
            //
            // libwebp cannot write a side over 16383 px. Refused before any
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
            // RGB8 (every colour JPEG) and RGBA8 go in as they are: libwebp
            // imports RGB with alpha 0xFF, the input an RGBA copy of it would
            // give. Any other layout is converted to RGBA first.
            let converted;
            let encoder = match image {
                DynamicImage::ImageRgb8(rgb) => {
                    webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height())
                }
                DynamicImage::ImageRgba8(rgba) => {
                    webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                }
                other => {
                    converted = other.to_rgba8();
                    webp::Encoder::from_rgba(
                        converted.as_raw(),
                        converted.width(),
                        converted.height(),
                    )
                }
            };
            // Precision loss is irrelevant: quality is clamped to 0..=100.
            #[allow(clippy::cast_precision_loss)]
            let quality = quality.clamp(0, 100) as f32;
            // `encode_simple`, not `encode`: the latter unwraps libwebp's error
            // and panics on any other encode failure.
            let bytes = encoder
                .encode_simple(false, quality)
                .map_err(|code| DrawingError::webp_encode(format!("encode {output_path}"), code))?;
            Ok(bytes.to_vec())
        } else {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut bytes, format)
                .map_err(|e| DrawingError::encode(format!("encode {output_path}"), e))?;
            Ok(bytes.into_inner())
        }
    }

    /// Encodes `image` as a baseline JPEG at `quality` (0-100)
    /// with **4:2:0 chroma subsampling and optimized Huffman tables**, mirroring
    /// the C# oracle's `bitmap.Encode(stream, SKEncodedImageFormat.Jpeg, quality)`.
    ///
    /// Every JPEG goes through here except a trickplay tile written with
    /// `TrickplayOptions::full_chroma` on, which is 4:4:4
    /// (`create_trickplay_tile_blocking`).
    ///
    /// This routes around the `image` crate for the same reason the WebP branch
    /// does. `image` 0.25's `JpegEncoder` hardcodes 4:4:4 (every component is
    /// declared `h: 1, v: 1` in `new_with_quality`, at every quality), whereas
    /// Skia's `SkJpegEncoder` defaults to `Downsample::k420` for all three of
    /// Jellyfin's JPEG writes (`SkiaEncoder.cs` v10.11.8 lines 628, 670 and 778 —
    /// resized image, pixmap, and the trickplay tile grid). A tile written with
    /// `full_chroma` on departs from that on purpose:
    /// `TrickplayManager.CreateTiles` derives `TrickplayInfo.Bandwidth` from the
    /// tile's file length
    /// (`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`), so a
    /// 4:4:4 tile reports a larger scrub-prefetch budget than Jellyfin's, a true
    /// one for the file it describes.
    ///
    /// `jpeg-encoder` is configured to match Skia on every knob that changes the
    /// wire format:
    ///
    /// - **4:2:0 chroma subsampling**, as above. That crate's own default is
    ///   quality-dependent (4:4:4 at `quality >= 90`), so it is set explicitly
    ///   rather than inherited.
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
    /// serves, so Ferrofin pays it too. With it, `jpeg-encoder` codes each
    /// component in a scan of its own (Skia interleaves them in one), a layout
    /// every decoder must read but the `image` crate's misreads, so this crate
    /// reads such files back through `jpeg-decoder` ([`crate::split_scans`]).
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
    fn encode_jpeg(
        image: &DynamicImage,
        output_path: &str,
        quality: i32,
    ) -> Result<Vec<u8>, ServiceError> {
        Self::encode_jpeg_sampled(image, output_path, quality, SamplingFactor::F_2_2)
    }

    /// [`Self::encode_jpeg`] at the given chroma `sampling`.
    fn encode_jpeg_sampled(
        image: &DynamicImage,
        output_path: &str,
        quality: i32,
        sampling: SamplingFactor,
    ) -> Result<Vec<u8>, ServiceError> {
        let quality = quality.clamp(0, 100);
        // Truncation is safe: quality is clamped to 0..=100.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let quality = quality as u8;
        let rgb = match image {
            DynamicImage::ImageRgb8(rgb) => Cow::Borrowed(rgb),
            // Skia's JPEG encoder ignores alpha and writes the premultiplied
            // colour, so a clear pixel is black and a blur's faded edge dark
            // (measured on Jellyfin 12.4).
            DynamicImage::ImageRgba8(rgba) => Cow::Owned(premultiplied_rgb(rgba)),
            other if other.color().has_alpha() => Cow::Owned(premultiplied_rgb(&other.to_rgba8())),
            other => Cow::Owned(other.to_rgb8()),
        };
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
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new(&mut bytes, quality);
        encoder.set_sampling_factor(sampling);
        encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
        encoder.set_optimized_huffman_tables(true);
        encoder
            .encode(rgb.as_raw(), width, height, JpegColorType::Rgb)
            .map_err(|e| DrawingError::jpeg_encode(format!("encode {output_path}"), e))?;
        Ok(bytes)
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
    /// original), never a 1-pixel axis beside an unclamped one. So is a resize
    /// past the budget ([`ImageMemory::resize_exact`]).
    fn resize_exact(
        memory: &ImageMemory,
        image: &DynamicImage,
        size: ImageDimensions,
    ) -> Result<DynamicImage, ServiceError> {
        match (u32::try_from(size.width), u32::try_from(size.height)) {
            (Ok(width @ 1..), Ok(height @ 1..)) => {
                memory.resize_exact(image, width, height, imageops::FilterType::Lanczos3)
            }
            _ => Err(ServiceError::invalid_input(format!(
                "cannot resize to {}x{}",
                size.width, size.height
            ))),
        }
    }

    /// The synchronous body of [`get_image_size`](ImageEncoder::get_image_size),
    /// split out so it can run on the blocking pool via `spawn_blocking`.
    fn image_size_blocking(
        memory: &ImageMemory,
        path: &str,
    ) -> Result<ImageDimensions, ServiceError> {
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
        let dims = memory
            .reader(path)
            .ok()
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
        memory: &ImageMemory,
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

        // A JPEG is only opened here: its header gives the size, and the
        // decode waits for the target so it can run at a reduced scale.
        let source = EncodeSource::open(memory, input_path, auto_orient)?;
        let original = source.dimensions();

        // "Just spit out the original file if all the options are default."
        if options.has_default_options(input_path, Some(original)) && !auto_orient {
            return Ok(input_path.to_owned());
        }

        let resized =
            source.resize_to(memory, input_path, |size| get_new_image_size(options, size))?;
        let format = Self::crate_format(output_format);
        composite(memory, resized, options, format, |image| {
            Self::write(image, output_path, format, quality)
        })?;
        Ok(output_path.to_owned())
    }

    /// The synchronous body of [`ImageEncoder::get_image_blur_hash`]: decode,
    /// resize to fit 128×128, then BlurHash-encode. CPU-bound (plus the
    /// decode's file read), so it runs on the blocking pool, never inline.
    fn blur_hash_blocking(
        memory: &ImageMemory,
        x_comp: i32,
        y_comp: i32,
        path: &str,
    ) -> Result<String, ServiceError> {
        let small = Self::blur_hash_pixels(memory, path)?;
        let x = u32::try_from(x_comp.clamp(1, 9)).unwrap_or(1);
        let y = u32::try_from(y_comp.clamp(1, 9)).unwrap_or(1);
        blurhash::encode(x, y, small.width(), small.height(), small.as_raw())
            .map_err(|e| ServiceError::backend(e.to_string()))
    }

    /// The pixels a blur hash is computed from: the image as stored (no EXIF
    /// orientation, like upstream's codec decode) fitted inside
    /// [`BLUR_HASH_MAX_SIDE`] squared with a Triangle filter, in one resize.
    ///
    /// An image inside the limit is enlarged to that fit, where upstream hashes
    /// it as it is. The encoder samples each cosine from a pixel's edge, so a
    /// few pixels read as detail the image does not have: hashed as it is, a
    /// solid 16×16 renders about 7 levels off on average and a solid 1×1 about
    /// 85, against about 1 once enlarged (gl-1058).
    ///
    /// A JPEG is decoded at the smallest DCT scale that still covers the fitted
    /// size, as Skia's codec decodes it for upstream (BlurHashSharp asks it for
    /// `GetScaledDimensions`), so a photo never builds its full-size bitmap. Any
    /// other input, or a JPEG too small to reduce, decodes in full as before.
    fn blur_hash_pixels(memory: &ImageMemory, path: &str) -> Result<RgbaImage, ServiceError> {
        let filter = imageops::FilterType::Triangle;
        if let Some(jpeg) = ScalableJpeg::open(path, memory.limits()) {
            let (width, height) = jpeg.dimensions();
            let (fit_width, fit_height) = fit_within(width, height, BLUR_HASH_MAX_SIDE);
            if let Some(scaled) = jpeg.decode_at_least(fit_width, fit_height, None) {
                return Ok(memory
                    .resize_exact(&scaled, fit_width, fit_height, filter)?
                    .to_rgba8());
            }
        }
        let image = memory.decode(path)?;
        let (fit_width, fit_height) = fit_within(image.width(), image.height(), BLUR_HASH_MAX_SIDE);
        Ok(memory
            .resize_exact(&image, fit_width, fit_height, filter)?
            .to_rgba8())
    }

    /// The build + write body of [`ImageEncoder::create_image_collage`]: several
    /// decodes, Lanczos3 resizes and an encode. CPU-bound, so it runs on the
    /// blocking pool. Assumes the caller has validated `width`/`height`.
    fn create_image_collage_blocking(
        memory: &ImageMemory,
        options: &ImageCollageOptions,
    ) -> Result<(), ServiceError> {
        let ratio = f64::from(options.width) / f64::from(options.height);
        let canvas = if ratio >= COLLAGE_THUMB_RATIO {
            build_thumb_collage(memory, &options.input_paths, options.width, options.height)
        } else {
            build_square_collage(memory, &options.input_paths, options.width, options.height)
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

    /// The decode + copy + JPEG-encode body of
    /// [`ImageEncoder::create_trickplay_tile`]: up to `tile_width`×`tile_height`
    /// (default 100) JPEG decodes packed into one large grid. CPU- and
    /// file-I/O-bound with no yield points, so it runs on the blocking pool.
    /// Assumes the caller has validated the O(1) arguments (non-empty inputs,
    /// positive tile dimensions and `img_width`, and that the inputs fit the
    /// grid); the per-image dimension checks stay here with the decode.
    ///
    /// `full_chroma` writes the grid at 4:4:4 where Skia writes 4:2:0, when an
    /// admin opts in (`TrickplayOptions::full_chroma`, gl-1128): a thumbnail is
    /// small enough that chroma blocks are a large share of it. The tile's own
    /// bytes still set `TrickplayInfo.Bandwidth`, so that stays true of the
    /// file, and it is above Jellyfin's for the same frames.
    fn create_trickplay_tile_blocking(
        memory: &ImageMemory,
        options: &ImageCollageOptions,
        quality: i32,
        img_width: i32,
        img_height: Option<i32>,
        full_chroma: bool,
    ) -> Result<i32, ServiceError> {
        let paths = &options.input_paths;
        let tile_width = options.width;
        let tile_height = options.height;

        // If no height provided, use the first image's height (after validating
        // its width matches img_width), matching the Skia default path.
        let first = memory.decode(&paths[0])?;
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

        // Checked: a wrapped i32 product would ask RgbImage::new for a huge buffer.
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
        // RGB, as the JPEG it becomes: the frames are JPEGs with no alpha to
        // composite, and an empty cell stays black.
        let mut grid = RgbImage::new(grid_w, grid_h);

        let mut first = Some(first);
        let mut img_index = 0usize;
        'outer: for y in 0..tile_height {
            for x in 0..tile_width {
                if img_index >= paths.len() {
                    break 'outer;
                }

                let img = match first.take() {
                    Some(first) => first,
                    None => memory.decode(&paths[img_index])?,
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
                imageops::replace(&mut grid, &img.into_rgb8(), i64::from(px), i64::from(py));
            }
        }

        let sampling = if full_chroma {
            SamplingFactor::F_1_1
        } else {
            SamplingFactor::F_2_2
        };
        let bytes = Self::encode_jpeg_sampled(
            &DynamicImage::ImageRgb8(grid),
            &options.output_path,
            quality,
            sampling,
        )?;
        Self::write_encoded(&bytes, &options.output_path)?;

        Ok(img_height)
    }
}

/// The input of [`ImageCrateEncoder::encode_image_blocking`] and
/// [`decode_resized`]: a JPEG whose header alone has been read, so it can
/// still decode at a reduced scale once the target size is known, or an image
/// already decoded in full.
enum EncodeSource {
    /// A JPEG `jpeg-decoder` handles (boxed: its decoder state is large).
    /// `orientation` is the EXIF orientation to apply, `None` when the caller
    /// did not ask for auto-orientation.
    Jpeg {
        jpeg: Box<ScalableJpeg>,
        orientation: Option<Orientation>,
    },
    /// Any other input, decoded (and oriented, when asked) in full.
    Decoded(DynamicImage),
}

impl EncodeSource {
    /// Opens `path`: reads a JPEG's header only, decodes anything else.
    fn open(memory: &ImageMemory, path: &str, auto_orient: bool) -> Result<Self, ServiceError> {
        if let Some(jpeg) = ScalableJpeg::open(path, memory.limits()) {
            let orientation = auto_orient.then(|| jpeg.orientation());
            return Ok(Self::Jpeg {
                jpeg: Box::new(jpeg),
                orientation,
            });
        }
        Ok(Self::Decoded(full_decode(memory, path, auto_orient)?))
    }

    /// The size of the image as it will be drawn (after any orientation).
    fn dimensions(&self) -> ImageDimensions {
        let (width, height) = match self {
            Self::Jpeg { jpeg, orientation } => {
                let (width, height) = jpeg.dimensions();
                if orientation.is_some_and(swaps_axes) {
                    (height, width)
                } else {
                    (width, height)
                }
            }
            Self::Decoded(image) => return image_dimensions(image),
        };
        ImageDimensions::new(
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
        )
    }

    /// The upright image, decoded at the smallest DCT scale that still covers
    /// [`THUMBNAIL_DECODE_FACTOR`] times `target` on both axes, so the Lanczos3
    /// resize after it always shrinks by at least that much. A JPEG that no
    /// reduced scale covers, or that the scaled decoder fails on, decodes in
    /// full through the `image` crate as before.
    fn decode_covering(
        self,
        memory: &ImageMemory,
        path: &str,
        target: ImageDimensions,
    ) -> Result<CoveringImage, ServiceError> {
        let full_size = |image| CoveringImage {
            image,
            full_size: true,
        };
        let (jpeg, orientation) = match self {
            Self::Decoded(image) => return Ok(full_size(image)),
            Self::Jpeg { jpeg, orientation } => (jpeg, orientation),
        };
        if let Some((min_width, min_height)) = thumbnail_decode_minimum(target, orientation)
            && let Some(mut image) = jpeg.decode_at_least(min_width, min_height, orientation)
        {
            if let Some(orientation) = orientation {
                image.apply_orientation(orientation);
            }
            return Ok(CoveringImage {
                image,
                full_size: false,
            });
        }
        full_decode(memory, path, orientation.is_some()).map(full_size)
    }

    /// The image resized with Lanczos3 to the size `target_of` gives for its
    /// upright size, decoded through [`Self::decode_covering`]; the decoded
    /// image itself when `target_of` gives its own size.
    ///
    /// A full decode measures the upright image itself. When it differs from
    /// what the header read planned with (an EXIF orientation stored after the
    /// frame header, which the header read cannot see), the size is planned
    /// again from the decoded image.
    fn resize_to(
        self,
        memory: &ImageMemory,
        path: &str,
        target_of: impl Fn(ImageDimensions) -> ImageDimensions,
    ) -> Result<DynamicImage, ServiceError> {
        let planned = self.dimensions();
        let target = target_of(planned);
        let decoded = self.decode_covering(memory, path, target)?;
        let upright = image_dimensions(&decoded.image);
        let target = if decoded.full_size && upright != planned {
            target_of(upright)
        } else {
            target
        };
        // At its own size the decoded image is the result: a resize to it
        // would only copy it whole.
        if upright == target {
            return Ok(decoded.image);
        }
        ImageCrateEncoder::resize_exact(memory, &decoded.image, target)
    }
}

/// The image at `path` as stored (no EXIF orientation, as upstream's
/// `SkiaEncoder.Decode(path, false, null, out _)` decodes collage and
/// splashscreen sources), resized with Lanczos3 to the target `target_of`
/// gives for its size.
///
/// A JPEG is sized from its header and decoded at the smallest DCT scale that
/// still covers [`THUMBNAIL_DECODE_FACTOR`] times the target, as
/// [`ImageEncoder::encode_image`] decodes one; anything else decodes in full.
/// Upstream decodes in full and resizes; the size drawn is the same either way.
pub(crate) fn decode_resized(
    memory: &ImageMemory,
    path: &str,
    target_of: impl Fn(ImageDimensions) -> ImageDimensions,
) -> Result<DynamicImage, ServiceError> {
    EncodeSource::open(memory, path, false)?.resize_to(memory, path, target_of)
}

/// `EncodeImage`'s drawing after the resize, then `write`: a canvas cleared
/// to `backgroundColor` (`SKColor.Parse`), the image drawn over it through a
/// `blur` filter of that sigma ([`blur_decal`]), then a black layer of alpha
/// `(1 - foregroundLayer) * 255` (`0.4` when the value is not a number). With
/// none of the three the resized image is written as it is, as upstream's
/// "all we're doing is resizing" branch encodes it. A colour `SKColor.Parse`
/// throws on is [`ServiceError::InvalidInput`], so the processor serves the
/// original, as `ProcessImage`'s catch does.
///
/// Every step works in place on one RGBA copy of the image. That copy, the
/// blur's line of sums and the JPEG writer's premultiplied copy
/// ([`premultiplied_rgb`]) are held against the resize budget until `write`
/// returns; the encoder's own working memory is not counted, as for every
/// write. Blends round (Skia's integer `SrcOver`); the encoder promises
/// decodable output of the right size, not Skia's bytes.
fn composite(
    memory: &ImageMemory,
    image: DynamicImage,
    options: &ImageProcessingOptions,
    format: CrateFormat,
    write: impl FnOnce(&DynamicImage) -> Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    let background = options
        .background_color
        .as_deref()
        .filter(|color| !color.trim().is_empty());
    let foreground = options
        .foreground_layer
        .as_deref()
        .filter(|layer| !layer.trim().is_empty());
    let blur = options.blur.unwrap_or(0);
    if background.is_none() && foreground.is_none() && blur == 0 {
        return write(&image);
    }
    let background = background
        .map(|color| {
            parse_sk_color(color).ok_or_else(|| {
                ServiceError::invalid_input(format!("backgroundColor {color:?} is not a colour"))
            })
        })
        .transpose()?;

    let pixels = u64::from(image.width()) * u64::from(image.height());
    let sums_bytes = if blur > 0 {
        u64::from(image.width().max(image.height())) * BLUR_SUMS_BYTES
    } else {
        0
    };
    let jpeg_bytes = if format == CrateFormat::Jpeg {
        pixels * 3
    } else {
        0
    };
    memory.draw(
        pixels * 4 + sums_bytes + jpeg_bytes,
        "composite the image",
        || {
            let mut canvas = image.into_rgba8();
            // A negative blur draws through no filter, as `blur > 0 ? … : null`.
            if blur > 0 {
                blur_decal(&mut canvas, f64::from(blur));
            }
            if let Some(color) = background {
                for pixel in canvas.pixels_mut() {
                    *pixel = src_over(*pixel, color);
                }
            }
            if let Some(layer) = foreground {
                // ponytail: Rust's float syntax, not .NET's double.TryParse (no
                // thousands separators); any other value is upstream's 0.4.
                let opacity = layer.trim().parse::<f64>().unwrap_or(0.4);
                // `(byte)((1 - opacity) * 0xFF)`: truncating, and saturating as
                // .NET 9+ converts, which `as` matches.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let shade = image::Rgba([0, 0, 0, ((1.0 - opacity) * 255.0) as u8]);
                for pixel in canvas.pixels_mut() {
                    *pixel = src_over(shade, *pixel);
                }
            }
            write(&DynamicImage::ImageRgba8(canvas))
        },
    )?
}

/// The third running sum of each channel, [`blur_decal`]'s scratch per pixel
/// of the image's longest side.
type ChannelSums = [i128; 4];

/// The bytes of a [`ChannelSums`], held against the resize budget.
const BLUR_SUMS_BYTES: u64 = std::mem::size_of::<ChannelSums>() as u64;

/// `top` drawn over `bottom` (`SrcOver`), both unpremultiplied, rounded.
fn src_over(top: image::Rgba<u8>, bottom: image::Rgba<u8>) -> image::Rgba<u8> {
    let top_alpha = f32::from(top[3]) / 255.0;
    let bottom_alpha = f32::from(bottom[3]) / 255.0 * (1.0 - top_alpha);
    let alpha = top_alpha + bottom_alpha;
    if alpha <= 0.0 {
        return image::Rgba([0, 0, 0, 0]);
    }
    let channel = |i: usize| {
        let mixed = (f32::from(top[i]) * top_alpha + f32::from(bottom[i]) * bottom_alpha) / alpha;
        // In 0..=255: a weighted mean of two channels.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mixed = mixed.round() as u8;
        mixed
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let alpha = (alpha * 255.0).round() as u8;
    image::Rgba([channel(0), channel(1), channel(2), alpha])
}

/// `SKImageFilter.CreateBlur(sigma, sigma)` as Skia draws it: a Gaussian,
/// here approximated by three box blurs (Kovesi's sizes), over premultiplied
/// pixels, with Skia's default `Decal` edges, so outside the image is
/// transparent and the edges fade (measured on Jellyfin 12.4: a white
/// image's corner alpha is 113 at `blur=1` and 80 at `blur=3`, and a sigma far
/// past the image's size leaves it almost clear).
///
/// Each axis is one pass of [`blur_line`], exact and linear in the pixels
/// whatever the sigma, as Skia answers `blur=2147483647` at once; the only
/// scratch is one row or column of [`ChannelSums`].
fn blur_decal(image: &mut RgbaImage, sigma: f64) {
    for pixel in image.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        for c in 0..3 {
            pixel[c] = mul_div_255(u32::from(pixel[c]), alpha);
        }
    }
    let (width, height) = image.dimensions();
    let (width, height) = (width as usize, height as usize);
    let raw: &mut [u8] = image;
    let mut sums: Vec<ChannelSums> = vec![[0; 4]; width.max(height)];
    let radii = box_radii(sigma);
    for row in 0..height {
        blur_line(raw, &mut sums, row * width * 4, 4, width, radii);
    }
    for column in 0..width {
        blur_line(raw, &mut sums, column * 4, width * 4, height, radii);
    }
    for pixel in image.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        for c in 0..3 {
            // Premultiplied, so the channel is at most the alpha; a clear
            // pixel has no colour.
            #[allow(clippy::cast_possible_truncation)]
            let channel = (u32::from(pixel[c]) * 255 + alpha / 2)
                .checked_div(alpha)
                .map_or(0, |channel| channel.min(255) as u8);
            pixel[c] = channel;
        }
    }
}

/// `image`'s colour premultiplied by its alpha, the alpha dropped: what
/// Skia's JPEG encoder writes for a pixel that is not opaque.
fn premultiplied_rgb(image: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b, a] = image.get_pixel(x, y).0;
        let a = u32::from(a);
        image::Rgb([r, g, b].map(|c| mul_div_255(u32::from(c), a)))
    })
}

/// `value * alpha / 255`, rounded.
fn mul_div_255(value: u32, alpha: u32) -> u8 {
    // At most 255 * 255 / 255.
    #[allow(clippy::cast_possible_truncation)]
    let product = ((value * alpha + 127) / 255) as u8;
    product
}

/// The three box radii whose successive blurs approximate a Gaussian of
/// `sigma` (Kovesi, "Fast Almost-Gaussian Filtering", 2010): `m` boxes of the
/// odd width just under the ideal one, the rest two wider.
fn box_radii(sigma: f64) -> [usize; 3] {
    const PASSES: f64 = 3.0;
    let ideal = (12.0 * sigma * sigma / PASSES + 1.0).sqrt();
    let mut lower = ideal.floor();
    if lower % 2.0 == 0.0 {
        lower -= 1.0;
    }
    let narrow =
        (12.0 * sigma * sigma - PASSES * lower * lower - 4.0 * PASSES * lower - 3.0 * PASSES)
            / (-4.0 * lower - 4.0);
    let narrow = narrow.round();
    // Widths are odd and at least 1; `as` saturates a huge sigma's width.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let radius = |width: f64| (width as usize).saturating_sub(1) / 2;
    std::array::from_fn(|i| {
        #[allow(clippy::cast_precision_loss)]
        let wide = (i as f64) >= narrow;
        radius(if wide { lower + 2.0 } else { lower })
    })
}

/// The three box blurs of `radii` over `len` pixels `stride` bytes apart from
/// `start`, in place, pixels outside the line counting as transparent
/// (`Decal`).
///
/// A box blur of radius `r` is `Q(x + r) - Q(x - r - 1)` for `Q` the line's
/// running sum, so three are an alternating sum of eight values of the third
/// running sum `Q³`, offset by `±r` per box. Past the line's end `Q³` is the
/// polynomial the constant total makes it, so the values far outside a huge
/// box cost nothing, and nothing beyond the line is stored or cut off between
/// boxes, which would lose the light a box spreads past an edge before the
/// next brings it back. The sums are exact (`i128`), and each pixel rounds once.
fn blur_line(
    raw: &mut [u8],
    cubes: &mut [ChannelSums],
    start: usize,
    stride: usize,
    len: usize,
    radii: [usize; 3],
) {
    // Q¹, Q² and Q³ at the line's last pixel, per channel.
    let (mut q1, mut q2, mut q3) = ([0i128; 4], [0i128; 4], [0i128; 4]);
    for (i, cube) in cubes.iter_mut().take(len).enumerate() {
        let at = start + i * stride;
        for c in 0..4 {
            q1[c] += i128::from(raw[at + c]);
            q2[c] += q1[c];
            q3[c] += q2[c];
        }
        *cube = q3;
    }
    let len_i = len as i128;
    let cube = |x: i128, c: usize| -> i128 {
        if x < 0 {
            0
        } else if x < len_i {
            // In 0..len.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let index = x as usize;
            cubes[index][c]
        } else {
            let past = x - len_i + 1;
            q3[c] + past * q2[c] + q1[c] * past * (past + 1) / 2
        }
    };
    let radii = radii.map(|r| r as i128);
    let window: i128 = radii.iter().map(|r| 2 * r + 1).product();
    // The eight corners: each box adds `+r` (sign +) or `-r - 1` (sign -).
    let corners: [(i128, i128); 8] = std::array::from_fn(|corner| {
        (0..3).fold((0, 1), |(offset, sign), i| {
            if (corner >> i) & 1 == 0 {
                (offset + radii[i], sign)
            } else {
                (offset - radii[i] - 1, -sign)
            }
        })
    });
    let mut out = [0u8; 4];
    for i in 0..len {
        let x = i as i128;
        for (c, value) in out.iter_mut().enumerate() {
            let total: i128 = corners
                .iter()
                .map(|&(offset, sign)| sign * cube(x + offset, c))
                .sum();
            // The kernel is non-negative, so the total is a weighted sum of
            // bytes: in 0..=255 · window, and the rounded mean in 0..=255.
            debug_assert!((0..=255 * window).contains(&total), "{total} / {window}");
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let mean = ((total + window / 2) / window).clamp(0, 255) as u8;
            *value = mean;
        }
        // Every read above is of `cubes`, filled before the first write.
        let at = start + i * stride;
        raw[at..at + 4].copy_from_slice(&out);
    }
}

/// SkiaSharp's `SKColor.TryParse`: `[#]RGB`, `[#]ARGB`, `[#]RRGGBB` or
/// `[#]AARRGGBB` in hex, alpha first, surrounding whitespace ignored. `None` for
/// anything else, where `SKColor.Parse` throws.
fn parse_sk_color(text: &str) -> Option<image::Rgba<u8>> {
    let hex = text.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // A single digit stands for itself twice (`F` is `FF`).
    let nibble = |i: usize| u8::from_str_radix(&hex[i..=i], 16).ok().map(|d| d * 17);
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let [a, r, g, b] = match hex.len() {
        3 => [Some(u8::MAX), nibble(0), nibble(1), nibble(2)],
        4 => [nibble(0), nibble(1), nibble(2), nibble(3)],
        6 => [Some(u8::MAX), byte(0), byte(2), byte(4)],
        8 => [byte(0), byte(2), byte(4), byte(6)],
        _ => return None,
    };
    Some(image::Rgba([r?, g?, b?, a?]))
}

/// An image [`EncodeSource::decode_covering`] produced.
struct CoveringImage {
    /// The upright pixels.
    image: DynamicImage,
    /// Whether `image` is the full-size decode, whose own size is the image's
    /// upright size; a reduced-scale decode is smaller by design.
    full_size: bool,
}

/// The smallest stored-orientation size a JPEG thumbnail of `target` (upright)
/// may be decoded at: [`THUMBNAIL_DECODE_FACTOR`] times the target, with the
/// sides swapped when `orientation` turns the image a quarter turn. `None` for
/// an empty target, which the resize rejects anyway.
fn thumbnail_decode_minimum(
    target: ImageDimensions,
    orientation: Option<Orientation>,
) -> Option<(u32, u32)> {
    let (Ok(width @ 1..), Ok(height @ 1..)) =
        (u32::try_from(target.width), u32::try_from(target.height))
    else {
        return None;
    };
    let (width, height) = if orientation.is_some_and(swaps_axes) {
        (height, width)
    } else {
        (width, height)
    };
    Some((
        width.saturating_mul(THUMBNAIL_DECODE_FACTOR),
        height.saturating_mul(THUMBNAIL_DECODE_FACTOR),
    ))
}

/// The size of a decoded image, as the encoder's dimension type.
fn image_dimensions(image: &DynamicImage) -> ImageDimensions {
    ImageDimensions::new(
        i32::try_from(image.width()).unwrap_or(i32::MAX),
        i32::try_from(image.height()).unwrap_or(i32::MAX),
    )
}

/// The full-size decode through the `image` crate, oriented when asked.
fn full_decode(
    memory: &ImageMemory,
    path: &str,
    auto_orient: bool,
) -> Result<DynamicImage, ServiceError> {
    if auto_orient {
        memory.decode_oriented(path)
    } else {
        memory.decode(path)
    }
}

/// How many times the target size a JPEG thumbnail is decoded at, at least,
/// before its Lanczos3 resize: the smallest DCT scale (1/8, 1/4, 1/2) whose
/// output still covers this multiple is chosen.
///
/// It trades thumbnail sharpness against decode memory. A 1/8 decode averages
/// each 8x8 block, so decoding at just the target size (a factor of 1) and
/// then resizing almost 1:1 comes out visibly softer than a Lanczos3 shrink of
/// the full image: about 15 dB further from a full-resolution Lanczos
/// reference on lossless PNG output, measured on 41 photos and artworks. At 2
/// the resize always shrinks at least twofold and the JPEG and WebP outputs
/// match the full decode's closeness to that reference (median within 0.1 dB),
/// while the decode still holds a quarter or less of the full bitmap's pixels
/// for any thumbnail at most a quarter of the source's width and height.
/// Lossless PNG output stays softer than the full decode's, about 9 dB further
/// from that reference (median), where 1 cost about 15 dB.
///
/// Collage cells and splashscreen tiles, the same Lanczos3 shrink, decode at
/// this factor too ([`decode_resized`], gl-1056).
///
/// Not a setting (owner decision): the blur hash, whose 128 px input is
/// blurred anyway, decodes at a factor of 1.
const THUMBNAIL_DECODE_FACTOR: u32 = 2;

/// The longest side of the image a blur hash is computed from. Upstream's:
/// `SkiaEncoder.GetImageBlurHash` passes 128×128 ("Any larger than 128x128 is
/// too slow and there's no visually discernible difference").
const BLUR_HASH_MAX_SIDE: u32 = 128;

/// The size `width`×`height` takes when fitted inside `side`×`side` with its
/// aspect ratio kept, rounded as the `image` crate's `DynamicImage::resize`
/// rounds it (each side `round(len * ratio)`, at least 1).
fn fit_within(width: u32, height: u32, side: u32) -> (u32, u32) {
    let ratio = (f64::from(side) / f64::from(width)).min(f64::from(side) / f64::from(height));
    // Both results are at most `side`, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fit = |len: u32| ((f64::from(len) * ratio).round() as u32).max(1);
    (fit(width), fit(height))
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
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || Self::image_size_blocking(&memory, &path))
            .await
            .map_err(|e| ServiceError::backend(format!("image probe task failed: {e}")))?
    }

    /// Port of `SkiaEncoder.GetImageBlurHash`: decode, resize to fit 128x128
    /// ("larger is too slow, no visible difference"), then BlurHash-encode with the
    /// caller's component counts. A JPEG decodes at the smallest DCT scale that
    /// still covers the 128x128 fit, as upstream's BlurHashSharp has Skia's codec
    /// decode it (`GetScaledDimensions`).
    ///
    /// The hash is functionally valid but **not byte-identical** to Jellyfin's — its
    /// pixels come from Skia's decode+resample, which the `image` crate can't reproduce
    /// exactly. Upstream also shrinks a bitmap its codec has already scaled (JPEG, WebP)
    /// by the original-relative factor a second time, so it hashes a large image from
    /// about 16 px and throws on a wide WebP logo (jellyfin#12106). That bug is not ported
    /// (gl-1058): every format here is hashed from the one 128x128 fit.
    async fn get_image_blur_hash(
        &self,
        x_comp: i32,
        y_comp: i32,
        path: &str,
    ) -> Result<String, ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_input("path is empty"));
        }
        // A decode (a JPEG at a reduced DCT scale, anything else in full) plus
        // a Triangle resize is CPU-bound synchronous work, and the decode's
        // file read can be a cold NFS/SMB read; offload it to the blocking
        // pool under the shared encode permit so it never pins an async worker
        // (see `encode_image`).
        let _permit = encode_permits()
            .acquire()
            .await
            .map_err(|e| ServiceError::backend(format!("image encode semaphore closed: {e}")))?;
        let path = path.to_owned();
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || {
            Self::blur_hash_blocking(&memory, x_comp, y_comp, &path)
        })
        .await
        .map_err(|e| ServiceError::backend(format!("blur hash task failed: {e}")))?
    }

    /// Port of `EncodeImage`. Rejects unsupported inputs (returns `input_path`
    /// untouched), reads the image's size (a JPEG's from its header, anything
    /// else by decoding it), short-circuits to the original file when
    /// [`ImageProcessingOptions::has_default_options`] holds and no
    /// auto-orientation is requested, otherwise resizes to
    /// `ImageHelper.GetNewImageSize` and writes `output_format` at `quality`.
    ///
    /// With `auto_orient` the file's own EXIF origin is applied first
    /// (`SkiaEncoder.GetBitmap` → `OrientImage`), so the resize sees the
    /// upright size. `orientation` is unused: Skia falls back to it only when
    /// the codec cannot open the file, and then has no bitmap to draw either.
    /// The background colour, blur and foreground layer are drawn after the
    /// resize (`composite`); the played/unplayed indicators are not (gl-806).
    ///
    /// A JPEG is sized from its header and decoded only once the target is
    /// known, at the smallest DCT scale still covering twice it
    /// (`THUMBNAIL_DECODE_FACTOR`); the resize after it is the same Lanczos3
    /// shrink. Upstream decodes in full here
    /// (`SkiaEncoder.Decode`); the output size is the same either way.
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
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || {
            Self::encode_image_blocking(
                &memory,
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
    /// (see `build_thumb_collage`).
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
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || Self::create_image_collage_blocking(&memory, &options))
            .await
            .map_err(|e| ServiceError::backend(format!("image collage task failed: {e}")))?
    }

    /// Port of `CreateSplashscreen`: builds `{DataPath}/splashscreen.png`
    /// (`crate::splashscreen`) when there is at least one poster and one
    /// backdrop, and does nothing otherwise. `Self::write` lands it whole, so
    /// a reader never sees half a PNG.
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
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || {
            // Each write goes through its own temporary file, so two
            // validations finishing together never touch each other's file.
            let splash = crate::splashscreen::generate_splash(&memory, &posters, &backdrops)?;
            let output = data_path.join("splashscreen.png");
            Self::write(
                &DynamicImage::ImageRgba8(splash),
                &output.to_string_lossy(),
                CrateFormat::Png,
                COLLAGE_QUALITY,
            )
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
        full_chroma: bool,
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
        let memory = Arc::clone(&self.memory);
        tokio::task::spawn_blocking(move || {
            Self::create_trickplay_tile_blocking(
                &memory,
                &options,
                quality,
                img_width,
                img_height,
                full_chroma,
            )
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
/// fewer than four images repeat, and an image the limits refuse is skipped
/// like an undecodable one. A JPEG is decoded at a reduced scale covering
/// twice its cell ([`decode_resized`]).
fn build_square_collage(
    memory: &ImageMemory,
    paths: &[String],
    width: i32,
    height: i32,
) -> RgbaImage {
    let cell_w = (width / 2).max(1);
    let cell_h = (height / 2).max(1);
    let cell_size = ImageDimensions::new(cell_w, cell_h);
    let mut canvas = RgbaImage::new(
        u32::try_from(width).unwrap_or(1),
        u32::try_from(height).unwrap_or(1),
    );

    let mut index = 0usize;
    for x in 0..2 {
        for y in 0..2 {
            let scaled = next_valid(paths, index, &mut |path| {
                decode_resized(memory, path, |_| cell_size).ok()
            });
            let Some((cell, next)) = scaled else {
                index = paths.len();
                continue;
            };
            index = next;

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
/// (TODO(gl-804): the text needs a font rasterizer and a bundled font). An image
/// the limits refuse is skipped like an undecodable one. A JPEG is decoded at a
/// reduced scale covering twice the size it is drawn at ([`decode_resized`]).
fn build_thumb_collage(
    memory: &ImageMemory,
    paths: &[String],
    width: i32,
    height: i32,
) -> RgbaImage {
    let mut canvas = RgbaImage::from_pixel(
        u32::try_from(width).unwrap_or(1),
        u32::try_from(height).unwrap_or(1),
        image::Rgba([0, 0, 0, 0xFF]),
    );

    let backdrop = next_valid(paths, 0, &mut |path| {
        decode_resized(memory, path, |source| {
            // resize to the same aspect as the original (C# width * h / w), in
            // i64: the product overflows an i32 for an image far taller than
            // it is wide.
            let backdrop_height =
                i64::from(width) * i64::from(source.height) / i64::from(source.width.max(1));
            ImageDimensions::new(
                width,
                i32::try_from(backdrop_height.max(1)).unwrap_or(i32::MAX),
            )
        })
        .ok()
    });
    if let Some((resized, _)) = backdrop {
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

    /// gl-143: an encode lands at its path only complete. The processor serves
    /// any file it finds at a cache path (with a year-long immutable cache
    /// header), so a request racing an encode, or one after an encode that
    /// failed part-way, must never find a truncated image there.
    #[rstest]
    #[case::jpeg(CrateFormat::Jpeg, "out.jpg")]
    #[case::png(CrateFormat::Png, "out.png")]
    #[case::webp(CrateFormat::WebP, "out.webp")]
    fn an_encode_appears_at_its_path_only_complete(
        #[case] format: CrateFormat,
        #[case] name: &str,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let source = gradient_fixture(&dir, "source.png", 2000, 2000);
        let image = image::open(&source).expect("fixture decodes");
        let output = out_path(&dir, name);
        let writer = {
            let output = output.clone();
            std::thread::spawn(move || ImageCrateEncoder::write(&image, &output, format, 90))
        };
        let mut seen = 0;
        while !writer.is_finished() {
            if let Ok(bytes) = std::fs::read(&output) {
                let decoded = image::load_from_memory(&bytes)
                    .unwrap_or_else(|e| panic!("partial {name} of {} bytes: {e}", bytes.len()));
                assert_eq!((decoded.width(), decoded.height()), (2000, 2000));
                seen += 1;
            }
            std::thread::yield_now();
        }
        writer.join().expect("writer thread").expect("encode");
        let decoded = image::open(&output).expect("final image decodes");
        assert_eq!((decoded.width(), decoded.height()), (2000, 2000));
        // Only the finished image is left in the directory: no temporary.
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [name, "source.png"], "seen complete {seen} times");
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
        assert_ne!(
            enc.get_image_blur_hash(4, 3, &input)
                .await
                .expect("blurhash"),
            ""
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

    /// Encodes `pixels` (a PNG at `name`) with `options` to a PNG and decodes
    /// the result.
    async fn composited(
        name: &str,
        pixels: RgbaImage,
        options: ImageProcessingOptions,
    ) -> RgbaImage {
        let dir = TempDir::new().expect("tempdir");
        let input = dir.path().join(name).to_string_lossy().into_owned();
        DynamicImage::ImageRgba8(pixels)
            .save(&input)
            .expect("write fixture");
        let output = out_path(&dir, "out.png");
        let written = ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &output,
                false,
                None,
                100,
                &ImageProcessingOptions {
                    supported_output_formats: vec![ImageFormat::Png],
                    ..options
                },
                ImageFormat::Png,
            )
            .await
            .expect("encode");
        assert_eq!(written, output, "re-encoded, not passed through");
        image::open(&output).expect("decode").into_rgba8()
    }

    /// gl-805: `EncodeImage` clears the canvas to `backgroundColor` and draws
    /// the image over it, so a transparent image takes the colour, and a
    /// half-transparent one blends with it.
    #[tokio::test]
    async fn a_background_colour_shows_through_transparency() {
        let mut pixels = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        pixels.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
        let out = composited(
            "clear.png",
            pixels,
            ImageProcessingOptions {
                background_color: Some("#FF0000".to_owned()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(*out.get_pixel(7, 7), Rgba([255, 0, 0, 255]));
        assert_eq!(*out.get_pixel(0, 0), Rgba([255, 255, 255, 255]));
    }

    /// A blank `backgroundColor` or `foregroundLayer` draws nothing
    /// (`IsNullOrWhiteSpace`), and is not parsed as a colour.
    #[rstest]
    #[case::background(Some("  "), None)]
    #[case::foreground(None, Some("\t"))]
    #[tokio::test]
    async fn a_blank_compositing_value_draws_nothing(
        #[case] background: Option<&str>,
        #[case] foreground: Option<&str>,
    ) {
        let out = composited(
            "clear.png",
            RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0])),
            ImageProcessingOptions {
                background_color: background.map(str::to_owned),
                foreground_layer: foreground.map(str::to_owned),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.get_pixel(1, 1).0, [0, 0, 0, 0]);
    }

    /// gl-805: `foregroundLayer` draws black at alpha `(1 - value) * 255`
    /// over the image; a value that is no number is upstream's 0.4.
    #[rstest]
    #[case::half("0.5", 128)]
    #[case::not_a_number("dark", 102)]
    #[case::opaque_layer("0", 0)]
    #[tokio::test]
    async fn a_foreground_layer_darkens_the_image(#[case] layer: &str, #[case] grey: u8) {
        let out = composited(
            "white.png",
            RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 255])),
            ImageProcessingOptions {
                foreground_layer: Some(layer.to_owned()),
                ..Default::default()
            },
        )
        .await;
        let [r, g, b, a] = out.get_pixel(2, 2).0;
        assert_eq!(a, 255);
        assert!(r == g && g == b, "{:?}", out.get_pixel(2, 2));
        assert!(r.abs_diff(grey) <= 1, "{layer}: {r} vs {grey}");
    }

    /// gl-805: `blur` filters the image, so a hard black/white edge comes
    /// back with greys across it; a blur of 0 (or below) leaves it sharp.
    #[rstest]
    #[case::blurred(Some(3), true)]
    #[case::zero(Some(0), false)]
    #[case::negative(Some(-2), false)]
    #[tokio::test]
    async fn blur_softens_an_edge(#[case] blur: Option<i32>, #[case] softened: bool) {
        let pixels = RgbaImage::from_fn(16, 4, |x, _| {
            if x < 8 {
                Rgba([0, 0, 0, 255])
            } else {
                Rgba([255, 255, 255, 255])
            }
        });
        let out = composited(
            "edge.png",
            pixels,
            ImageProcessingOptions {
                blur,
                ..Default::default()
            },
        )
        .await;
        let edge = out.get_pixel(7, 2).0[0];
        assert_eq!(edge != 0, softened, "pixel at the edge: {edge}");
    }

    /// Skia blurs premultiplied pixels with `Decal` edges, so an opaque
    /// image's corners fade to transparent while keeping their colour (no dark
    /// fringe), and its middle stays opaque.
    #[tokio::test]
    async fn a_blur_fades_the_edges_without_darkening_them() {
        let out = composited(
            "white.png",
            RgbaImage::from_pixel(32, 32, Rgba([255, 255, 255, 255])),
            ImageProcessingOptions {
                blur: Some(2),
                ..Default::default()
            },
        )
        .await;
        let corner = out.get_pixel(0, 0).0;
        assert!(corner[3] < 200, "corner {corner:?}");
        // Radii 1, 1, 2: an edge keeps (1 + 9/45) / 2 of the kernel, 0.6.
        for (x, y) in [(0, 16), (31, 16), (16, 0), (16, 31)] {
            assert_eq!(out.get_pixel(x, y).0[3], 153, "the edge at {x},{y}");
        }
        assert_eq!(&corner[..3], &[255, 255, 255], "corner {corner:?}");
        assert_eq!(out.get_pixel(16, 16).0, [255, 255, 255, 255]);
    }

    /// Skia writes a JPEG from premultiplied pixels, so a blur's faded corner
    /// comes back dark in a JPEG (Jellyfin 12.4: a white image's corner is 80
    /// grey at `blur=3`), and an opaque pixel keeps its colour.
    #[test]
    fn a_jpeg_takes_the_premultiplied_colour() {
        let mut image = RgbaImage::from_pixel(2, 1, Rgba([255, 255, 255, 255]));
        image.put_pixel(0, 0, Rgba([255, 200, 0, 80]));
        let rgb = premultiplied_rgb(&image);
        assert_eq!(rgb.get_pixel(0, 0).0, [80, 63, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
    }

    /// The blur is linear in the pixels whatever the sigma, so the largest `blur` a
    /// query can carry returns at once, as Skia's does, with an image all but
    /// clear.
    #[tokio::test]
    async fn the_largest_blur_returns_at_once() {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            composited(
                "big-blur.png",
                RgbaImage::from_pixel(64, 64, Rgba([10, 200, 30, 255])),
                ImageProcessingOptions {
                    blur: Some(i32::MAX),
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("returns at once");
        assert!(
            out.pixels().all(|p| p[3] <= 1),
            "{:?}",
            out.get_pixel(32, 32)
        );
    }

    /// `blur_line` is the three boxes over the line padded with transparency,
    /// nothing cut off between them: checked against that convolution done
    /// the long way, with boxes shorter and longer than the line.
    #[rstest]
    #[case(12, [2, 2, 3])]
    #[case(5, [7, 9, 12])]
    #[case(1, [0, 0, 1])]
    #[case(8, [0, 0, 0])]
    #[case(30, [1, 4, 40])]
    fn blur_line_is_the_three_boxes_over_transparency(
        #[case] len: usize,
        #[case] radii: [usize; 3],
    ) {
        // Truncation is the point: varied bytes from the index.
        #[allow(clippy::cast_possible_truncation)]
        let line: Vec<u8> = (0..len * 4).map(|i| (i * 97 % 256) as u8).collect();
        let mut raw = line.clone();
        let mut sums = vec![[0; 4]; len];
        blur_line(&mut raw, &mut sums, 0, 4, len, radii);

        let pad: usize = radii.iter().sum();
        let window: i128 = radii.iter().map(|&r| 2 * r as i128 + 1).product();
        for c in 0..4 {
            let mut signal = vec![0i128; len + 2 * pad];
            for i in 0..len {
                signal[i + pad] = i128::from(line[i * 4 + c]);
            }
            for r in radii {
                signal = (0..signal.len())
                    .map(|x| {
                        signal[x.saturating_sub(r)..=(x + r).min(signal.len() - 1)]
                            .iter()
                            .sum()
                    })
                    .collect();
            }
            for i in 0..len {
                let expected = (signal[i + pad] + window / 2) / window;
                assert_eq!(
                    i128::from(raw[i * 4 + c]),
                    expected,
                    "pixel {i}, channel {c}"
                );
            }
        }
    }

    /// Kovesi's box widths for sigma 3: 5, 5 and 7 (radii 2, 2 and 3); a
    /// sigma of 1 is two boxes of width 1 and one of 3.
    #[rstest]
    #[case(3.0, [2, 2, 3])]
    #[case(1.0, [0, 0, 1])]
    fn box_radii_follow_kovesi(#[case] sigma: f64, #[case] radii: [usize; 3]) {
        assert_eq!(box_radii(sigma), radii);
    }

    /// A colour `SKColor.Parse` throws on fails the encode, which the
    /// processor answers with the original file.
    #[tokio::test]
    async fn an_unparsable_background_colour_fails_the_encode() {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "in.png", 4, 4);
        let err = ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &out_path(&dir, "out.png"),
                false,
                None,
                100,
                &ImageProcessingOptions {
                    background_color: Some("red".to_owned()),
                    supported_output_formats: vec![ImageFormat::Png],
                    ..Default::default()
                },
                ImageFormat::Png,
            )
            .await
            .expect_err("red is not hex");
        assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
    }

    /// SkiaSharp's `SKColor.TryParse` cases: three, four, six or eight hex
    /// digits, alpha first, `#` and surrounding whitespace optional.
    #[rstest]
    #[case("#F00", Some([255, 0, 0, 255]))]
    #[case("8F00", Some([255, 0, 0, 0x88]))]
    #[case("#123456", Some([0x12, 0x34, 0x56, 255]))]
    #[case(" #80ff0000 ", Some([255, 0, 0, 0x80]))]
    #[case("", None)]
    #[case("#", None)]
    #[case("#12", None)]
    #[case("#12345", None)]
    #[case("#GGG", None)]
    #[case("red", None)]
    fn sk_color_parse(#[case] text: &str, #[case] rgba: Option<[u8; 4]>) {
        assert_eq!(parse_sk_color(text).map(|c| c.0), rgba, "{text:?}");
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

    /// Writes `pixels` to `dir/name` as a quality-95 JPEG, with `exif` when
    /// given, and returns its path.
    fn jpeg_fixture(
        dir: &TempDir,
        name: &str,
        pixels: &image::RgbImage,
        exif: Option<&[u8]>,
    ) -> String {
        use image::ImageEncoder as _;
        let path = out_path(dir, name);
        let file = std::fs::File::create(&path).expect("fixture");
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 95);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif.to_vec()).expect("exif");
        }
        encoder
            .write_image(
                pixels,
                pixels.width(),
                pixels.height(),
                image::ExtendedColorType::Rgb8,
            )
            .expect("write fixture");
        path
    }

    /// A smooth photo-like `width`×`height` image: two gradients and a soft
    /// diagonal ripple, nothing finer than the 1/8 scale can carry.
    fn smooth_photo(width: u32, height: u32) -> image::RgbImage {
        image::RgbImage::from_fn(width, height, |x, y| {
            let (fx, fy) = (
                f64::from(x) / f64::from(width),
                f64::from(y) / f64::from(height),
            );
            let ripple = ((fx + fy) * 12.0).sin() * 40.0;
            // The values are clamped to 0..=255 before the cast.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let channel = |v: f64| v.clamp(0.0, 255.0) as u8;
            image::Rgb([
                channel(40.0 + fx * 180.0 + ripple),
                channel(200.0 - fy * 150.0),
                channel(90.0 + ripple),
            ])
        })
    }

    /// The mean absolute difference between two same-sized images, per
    /// channel, in 0-255 levels.
    fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let total: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        // Pixel buffers stay far below 2^52 bytes.
        #[allow(clippy::cast_precision_loss)]
        let mean = total as f64 / a.len() as f64;
        mean
    }

    /// gl-1015: a JPEG thumbnail decodes at a reduced DCT scale that still
    /// covers twice the target (here 1/4, 400x300 for a 200x150 thumbnail)
    /// and comes out the size and look of a full decode.
    #[tokio::test]
    async fn a_scaled_jpeg_thumbnail_matches_the_full_decode() {
        let dir = TempDir::new().expect("tempdir");
        let input = jpeg_fixture(&dir, "photo.jpg", &smooth_photo(1600, 1200), None);
        let output = out_path(&dir, "thumb.png");
        let options = ImageProcessingOptions {
            max_width: Some(200),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let written = ImageCrateEncoder::new()
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
        let thumb = image::open(&output).expect("decodes").to_rgb8();
        let full = image::open(&input)
            .expect("decodes")
            .resize_exact(200, 150, imageops::FilterType::Lanczos3)
            .to_rgb8();
        assert_eq!(thumb.dimensions(), (200, 150));
        let diff = mean_abs_diff(thumb.as_raw(), full.as_raw());
        assert!(
            diff < 2.0,
            "mean difference {diff} levels from the full decode"
        );
    }

    /// gl-1213: a JPEG this encoder wrote (4:2:0, one scan per component)
    /// keeps its colours when an image request reads it back. A thumbnail at
    /// half its size decodes it in full; the `image` crate's decoder came back
    /// green there.
    #[tokio::test]
    async fn a_jpeg_this_encoder_wrote_reads_back_with_its_colours() {
        let dir = TempDir::new().expect("tempdir");
        let photo = smooth_photo(160, 120);
        let input = out_path(&dir, "written.jpg");
        let bytes =
            ImageCrateEncoder::encode_jpeg(&DynamicImage::ImageRgb8(photo.clone()), &input, 90)
                .expect("encode");
        std::fs::write(&input, bytes).expect("write");
        let output = out_path(&dir, "thumb.png");
        let options = ImageProcessingOptions {
            max_width: Some(80),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        ImageCrateEncoder::new()
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
        let thumb = image::open(&output).expect("decodes").to_rgb8();
        let expected = imageops::resize(&photo, 80, 60, imageops::FilterType::Lanczos3);
        assert_eq!(thumb.dimensions(), (80, 60));
        let diff = mean_abs_diff(thumb.as_raw(), expected.as_raw());
        assert!(diff < 2.0, "mean difference {diff} levels from the photo");
    }

    /// gl-1015: the scaled decode plans its size in stored orientation and
    /// draws the result upright: a landscape-stored portrait (EXIF 6, red on
    /// the left, blue on the right) comes out red on top.
    #[tokio::test]
    async fn auto_orient_applies_to_a_scaled_jpeg_decode() {
        let dir = TempDir::new().expect("tempdir");
        let stored = image::RgbImage::from_fn(800, 400, |x, _| {
            if x < 400 {
                image::Rgb([220, 20, 20])
            } else {
                image::Rgb([20, 20, 220])
            }
        });
        let input = jpeg_fixture(&dir, "phone.jpg", &stored, Some(&ORIENTATION_6_EXIF));
        let output = out_path(&dir, "upright.png");
        let options = ImageProcessingOptions {
            max_height: Some(100),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &output,
                true,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await
            .expect("encode");
        let upright = image::open(&output).expect("decodes").to_rgb8();
        assert_eq!(upright.dimensions(), (50, 100));
        let [top_red, _, top_blue] = upright.get_pixel(25, 10).0;
        let [bottom_red, _, bottom_blue] = upright.get_pixel(25, 90).0;
        assert!(top_red > 180 && top_blue < 60, "top {top_red}/{top_blue}");
        assert!(
            bottom_blue > 180 && bottom_red < 60,
            "bottom {bottom_red}/{bottom_blue}"
        );
    }

    /// gl-1056: a collage cell or splashscreen tile of a JPEG is sized from
    /// the image as stored (its EXIF orientation is not applied, as upstream's
    /// `Decode(path, false, null, …)` does not) and comes out the size and
    /// look of a full decode. It is decoded at a reduced DCT scale covering
    /// twice it (here 1/4, 400x300 for a 200x150 cell), which
    /// `tests/image_decode_memory.rs` sees through the heap.
    #[test]
    fn a_jpeg_collage_cell_is_drawn_as_stored_like_the_full_decode() {
        let dir = TempDir::new().expect("tempdir");
        let input = jpeg_fixture(
            &dir,
            "photo.jpg",
            &smooth_photo(1600, 1200),
            Some(&ORIENTATION_6_EXIF),
        );
        let cell = decode_resized(&ImageMemory::default(), &input, |source| {
            ImageDimensions::new(source.width / 8, source.height / 8)
        })
        .expect("decodes")
        .to_rgb8();
        let full = image::open(&input)
            .expect("decodes")
            .resize_exact(200, 150, imageops::FilterType::Lanczos3)
            .to_rgb8();
        assert_eq!(cell.dimensions(), (200, 150));
        let diff = mean_abs_diff(cell.as_raw(), full.as_raw());
        assert!(
            diff < 2.0,
            "mean difference {diff} levels from the full decode"
        );
    }

    /// gl-1015: a thumbnail decodes at no less than twice its size, in the
    /// orientation the file is stored in (owner decision: the factor is 2).
    #[rstest]
    #[case::landscape(200, 150, None, Some((400, 300)))]
    #[case::quarter_turn_swaps(200, 150, Some(Orientation::Rotate90), Some((300, 400)))]
    #[case::half_turn_keeps(200, 150, Some(Orientation::Rotate180), Some((400, 300)))]
    #[case::empty_width(0, 150, None, None)]
    #[case::negative_height(200, -1, None, None)]
    fn a_thumbnail_decodes_at_twice_its_size(
        #[case] width: i32,
        #[case] height: i32,
        #[case] orientation: Option<Orientation>,
        #[case] expected: Option<(u32, u32)>,
    ) {
        assert_eq!(
            thumbnail_decode_minimum(ImageDimensions::new(width, height), orientation),
            expected
        );
    }

    /// The blur hash's input size is what `DynamicImage::resize` would give,
    /// so the hash keeps its input dimensions with the scaled decode.
    #[rstest]
    #[case(1999, 1333)]
    #[case(1333, 1999)]
    #[case(1001, 7)]
    #[case(7, 1001)]
    #[case(129, 128)]
    #[case(300, 200)]
    #[case(2000, 123)]
    fn fit_within_rounds_as_the_image_crate_resizes(#[case] width: u32, #[case] height: u32) {
        let resized = DynamicImage::new_rgb8(width, height).resize(
            BLUR_HASH_MAX_SIDE,
            BLUR_HASH_MAX_SIDE,
            imageops::FilterType::Nearest,
        );
        assert_eq!(
            fit_within(width, height, BLUR_HASH_MAX_SIDE),
            (resized.width(), resized.height())
        );
    }

    /// `jpeg_bytes` with an APP1 EXIF segment holding `exif` inserted right
    /// after the frame header (SOF0), where the header read cannot see it.
    fn with_exif_after_frame_header(jpeg_bytes: &[u8], exif: &[u8]) -> Vec<u8> {
        let mut at = 2; // past SOI
        loop {
            assert_eq!(jpeg_bytes[at], 0xFF, "a marker at {at}");
            let marker = jpeg_bytes[at + 1];
            let length = usize::from(u16::from_be_bytes([jpeg_bytes[at + 2], jpeg_bytes[at + 3]]));
            at += 2 + length;
            if marker == 0xC0 {
                break;
            }
        }
        let segment_length = u16::try_from(2 + 6 + exif.len()).expect("small segment");
        let mut out = jpeg_bytes[..at].to_vec();
        out.extend([0xFF, 0xE1]);
        out.extend(segment_length.to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(exif);
        out.extend(&jpeg_bytes[at..]);
        out
    }

    /// gl-1015: an EXIF orientation stored after the frame header is missed by
    /// the header read, so the scaled decode is discarded; the full decode then
    /// sizes the thumbnail from the upright image, as it always did, instead of
    /// stretching it to the size planned for the stored one.
    #[tokio::test]
    async fn an_orientation_after_the_frame_header_still_sizes_the_upright_image() {
        let dir = TempDir::new().expect("tempdir");
        let stored = image::RgbImage::from_fn(800, 400, |x, _| {
            if x < 400 {
                image::Rgb([220, 20, 20])
            } else {
                image::Rgb([20, 20, 220])
            }
        });
        let plain = jpeg_fixture(&dir, "plain.jpg", &stored, None);
        let input = out_path(&dir, "late-exif.jpg");
        std::fs::write(
            &input,
            with_exif_after_frame_header(
                &std::fs::read(&plain).expect("read"),
                &ORIENTATION_6_EXIF,
            ),
        )
        .expect("write fixture");
        let output = out_path(&dir, "upright.png");
        let options = ImageProcessingOptions {
            max_height: Some(100),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        ImageCrateEncoder::new()
            .encode_image(
                &input,
                Utc::now(),
                &output,
                true,
                None,
                90,
                &options,
                ImageFormat::Png,
            )
            .await
            .expect("encode");
        let upright = image::open(&output).expect("decodes").to_rgb8();
        assert_eq!(upright.dimensions(), (50, 100));
        let [top_red, _, top_blue] = upright.get_pixel(25, 10).0;
        assert!(top_red > 180 && top_blue < 60, "top {top_red}/{top_blue}");
    }

    /// gl-1015: a JPEG whose header reads but whose data is cut short ends as
    /// the full decode ends it: the scaled decode's failure falls back to the
    /// `image` crate, so the outcome is the one this path always had.
    #[tokio::test]
    async fn a_truncated_jpeg_ends_as_the_full_decode_ends_it() {
        let dir = TempDir::new().expect("tempdir");
        let whole = jpeg_fixture(&dir, "whole.jpg", &smooth_photo(1600, 1200), None);
        let bytes = std::fs::read(&whole).expect("read");
        let input = out_path(&dir, "cut.jpg");
        std::fs::write(&input, &bytes[..bytes.len() / 2]).expect("write fixture");
        let output = out_path(&dir, "thumb.png");
        let options = ImageProcessingOptions {
            max_width: Some(200),
            supported_output_formats: vec![ImageFormat::Png],
            ..Default::default()
        };
        let encoded = ImageCrateEncoder::new()
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
        match image::open(&input) {
            Ok(_) => {
                assert_eq!(encoded.expect("the full decode reads it"), output);
                let thumb = image::open(&output).expect("decodes");
                assert_eq!((thumb.width(), thumb.height()), (200, 150));
            }
            Err(error) => assert!(encoded.is_err(), "the full decode fails ({error})"),
        }
    }

    /// gl-1015: a JPEG's blur hash comes from a reduced DCT scale, as Skia's
    /// codec decodes it for upstream. Its colours stay those of the hash a full
    /// decode gives: both hashes rendered back differ by under a level.
    #[tokio::test]
    async fn a_scaled_jpeg_blur_hash_matches_the_full_decode() {
        let dir = TempDir::new().expect("tempdir");
        let input = jpeg_fixture(&dir, "photo.jpg", &smooth_photo(2400, 1600), None);
        let scaled = ImageCrateEncoder::new()
            .get_image_blur_hash(5, 4, &input)
            .await
            .expect("blur hash");
        let small = image::open(&input)
            .expect("decodes")
            .resize(
                BLUR_HASH_MAX_SIDE,
                BLUR_HASH_MAX_SIDE,
                imageops::FilterType::Triangle,
            )
            .to_rgba8();
        let full =
            blurhash::encode(5, 4, small.width(), small.height(), small.as_raw()).expect("encode");
        let render = |hash: &str| blurhash::decode(hash, 32, 32, 1.0).expect("decodes");
        let diff = mean_abs_diff(&render(&scaled), &render(&full));
        assert!(
            diff < 1.0,
            "{scaled} vs {full}: mean difference {diff} levels"
        );
    }

    /// gl-1058: a blur hash comes from one resize of the image to fit 128×128,
    /// whether or not the format's codec scales while decoding. Upstream's JPEG
    /// and WebP shrink a second time (jellyfin#12106); the large sizes are those
    /// of BlurHashSharp PR 164, its proposed fix.
    #[rstest]
    #[case::wide_logo(4319, 481, 128, 14)]
    #[case::tall_logo(481, 4319, 14, 128)]
    #[case::height_rounds_to_zero(10000, 1, 128, 1)]
    #[case::width_rounds_to_zero(1, 10000, 1, 128)]
    #[case::one_side_at_the_limit(128, 200, 82, 128)]
    #[case::at_the_limit(128, 128, 128, 128)]
    #[case::inside_the_limit(100, 50, 128, 64)]
    #[case::one_pixel(1, 1, 128, 128)]
    fn a_blur_hash_comes_from_one_fit_of_its_image(
        #[values(("jpg", CrateFormat::Jpeg), ("webp", CrateFormat::WebP), ("png", CrateFormat::Png))]
        format: (&str, CrateFormat),
        #[case] width: u32,
        #[case] height: u32,
        #[case] expected_width: u32,
        #[case] expected_height: u32,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = out_path(&dir, &format!("logo.{}", format.0));
        let image = DynamicImage::ImageRgb8(smooth_photo(width, height));
        ImageCrateEncoder::write(&image, &path, format.1, 90).expect("fixture");

        let memory = ImageMemory::default();
        let pixels = ImageCrateEncoder::blur_hash_pixels(&memory, &path).expect("pixels");
        assert_eq!(pixels.dimensions(), (expected_width, expected_height));
        // Well-formed length: 6 + (xComp*yComp - 1)*2 base83 chars.
        let hash = ImageCrateEncoder::blur_hash_blocking(&memory, 4, 3, &path).expect("hash");
        assert_eq!(
            hash.len(),
            6 + (4 * 3 - 1) * 2,
            "unexpected blur hash: {hash}"
        );
    }

    /// gl-1058: an image inside the limit is enlarged to the fit before it is
    /// hashed, so a solid one stays flat. Upstream hashes it as it is, and the
    /// encoder then reads its few pixels as detail the image does not have: a
    /// solid 1×1 renders about 85 levels off and a solid 16×16 about 7.
    #[rstest]
    #[case(1)]
    #[case(8)]
    #[case(16)]
    #[tokio::test]
    async fn a_small_solid_image_keeps_a_flat_blur_hash(#[case] side: u32) {
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "solid.png", side, side);
        let hash = ImageCrateEncoder::new()
            .get_image_blur_hash(5, 5, &input)
            .await
            .expect("blur hash");
        let rendered = blurhash::decode(&hash, 32, 32, 1.0).expect("decodes");
        let solid = [0x30, 0x60, 0x90, 0xFF].repeat(32 * 32);
        // Alpha counts here too: enlarged is about 0.7 off, as it is at least 5.
        let diff = mean_abs_diff(&rendered, &solid);
        assert!(diff < 2.0, "{hash}: mean difference {diff} levels");
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

    /// gl-1481: an opaque RGB8 image goes into libwebp as it is, and encodes
    /// to the same bytes as its RGBA copy did, at low, usual and full quality.
    #[rstest]
    #[case(0)]
    #[case(80)]
    #[case(100)]
    fn an_rgb_webp_is_byte_identical_to_its_rgba_copy(#[case] quality: i32) {
        // Truncation is the point: wrap the coordinates into channel values.
        #[allow(clippy::cast_possible_truncation)]
        let rgb = image::RgbImage::from_fn(97, 61, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 5) as u8, ((x + y) * 7) as u8])
        });
        let rgba = DynamicImage::ImageRgb8(rgb.clone()).to_rgba8();
        let encode = |image: DynamicImage| {
            ImageCrateEncoder::encode(&image, "out.webp", CrateFormat::WebP, quality)
                .expect("encode")
        };

        assert_eq!(
            encode(DynamicImage::ImageRgb8(rgb)),
            encode(DynamicImage::ImageRgba8(rgba))
        );
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
    /// (`SkiaEncoder.cs` v10.11.8 lines 628, 670, 778). Ferrofin departs from it
    /// only for a trickplay tile written with `full_chroma` on
    /// (`a_trickplay_tile_is_written_at_the_chroma_asked_for`); everything else
    /// is pinned here.
    ///
    /// The high-quality case is the one that regresses silently: `jpeg-encoder`'s
    /// own default sampling factor flips to 4:4:4 at `quality >= 90`.
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
        // HasDefaultOptions holds (an unset fill would pass too: the C# fill
        // fields are `int?`).
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
    async fn bounds_the_source_meets_with_no_fill_spit_out_original() {
        // EncodeImage's own HasDefaultOptions check, which decides for an image
        // whose stored size was unknown to the processor: a `maxWidth` above
        // the source and no fill leave it as it is (gl-1117).
        let dir = TempDir::new().expect("tempdir");
        let input = fixture(&dir, "orig.jpg", 640, 480);
        let output = out_path(&dir, "should_not_exist.jpg");
        let options = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            max_width: Some(2000),
            ..Default::default()
        };
        let written = ImageCrateEncoder::new()
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
            .create_trickplay_tile(&options, 90, 320, None, true)
            .await
            .expect("trickplay");
        assert_eq!(returned_height, 180);

        // Grid is imgW*tileW x imgH*tileH = 640 x 360.
        let dims = enc.get_image_size(&output).await.expect("probe tile");
        assert_eq!(dims, ImageDimensions::new(640, 360));
    }

    /// A tile is written at the chroma asked for, at every quality. Jellyfin's
    /// 4:2:0 stays the default (`TrickplayOptions::full_chroma` off), and
    /// neither case moves the 4:2:0 every other JPEG is pinned to above.
    #[rstest]
    #[case::full_at_80(true, 80, vec![(1, 1), (1, 1), (1, 1)])]
    #[case::full_at_90(true, 90, vec![(1, 1), (1, 1), (1, 1)])]
    #[case::full_at_100(true, 100, vec![(1, 1), (1, 1), (1, 1)])]
    #[case::jellyfin_at_80(false, 80, vec![(2, 2), (1, 1), (1, 1)])]
    #[case::jellyfin_at_90(false, 90, vec![(2, 2), (1, 1), (1, 1)])]
    #[case::jellyfin_at_100(false, 100, vec![(2, 2), (1, 1), (1, 1)])]
    #[tokio::test]
    async fn a_trickplay_tile_is_written_at_the_chroma_asked_for(
        #[case] full_chroma: bool,
        #[case] quality: i32,
        #[case] sampling: Vec<(u8, u8)>,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let output = out_path(&dir, "tile.jpg");
        let options = ImageCollageOptions {
            input_paths: vec![fixture(&dir, "frame.png", 64, 36)],
            output_path: output.clone(),
            width: 1,
            height: 1,
        };

        ImageCrateEncoder::new()
            .create_trickplay_tile(&options, quality, 64, None, full_chroma)
            .await
            .expect("tile");

        let bytes = std::fs::read(&output).expect("read tile");
        assert_eq!(jpeg_sampling_factors(&bytes), sampling);
    }

    /// A tile is its frames in reading order on black, with the bytes an RGBA
    /// canvas flattened for JPEG gives, whichever format a frame decodes to.
    /// With `full_chroma` off they are `encode_jpeg`'s, the 4:2:0 every other
    /// JPEG is written at, so the default tile is what it was before the option.
    #[rstest]
    #[case::jellyfin_chroma(false)]
    #[case::full_chroma(true)]
    #[tokio::test]
    async fn a_trickplay_tile_is_its_frames_on_black(#[case] full_chroma: bool) {
        let dir = TempDir::new().expect("tempdir");
        let (width, height) = (32_u32, 18_u32);
        let colour = image::RgbImage::from_fn(width, height, |x, y| {
            // Truncation is the point: wrap coordinates into channel space.
            #[allow(clippy::cast_possible_truncation)]
            image::Rgb([(x * 7) as u8, (y * 13) as u8, ((x + y) * 3) as u8])
        });
        let gray = DynamicImage::ImageRgb8(colour.clone()).to_luma8();
        let frames = ["colour.jpg", "gray.jpg", "rgba.png"].map(|name| out_path(&dir, name));
        DynamicImage::ImageRgb8(colour.clone())
            .save(&frames[0])
            .expect("write the colour frame");
        DynamicImage::ImageLuma8(gray)
            .save(&frames[1])
            .expect("write the gray frame");
        DynamicImage::ImageRgb8(colour)
            .to_rgba8()
            .save(&frames[2])
            .expect("write the RGBA frame");
        let output = out_path(&dir, "tile.jpg");

        let options = ImageCollageOptions {
            input_paths: frames.to_vec(),
            output_path: output.clone(),
            width: 2,
            height: 2,
        };
        ImageCrateEncoder::new()
            .create_trickplay_tile(&options, 90, 32, None, full_chroma)
            .await
            .expect("trickplay");

        let mut canvas = RgbaImage::new(width * 2, height * 2);
        for (index, cell) in [(0, 0), (1, 0), (0, 1)].into_iter().enumerate() {
            let frame = ImageMemory::default()
                .decode(&frames[index])
                .expect("decodes");
            let (x, y) = (i64::from(cell.0 * width), i64::from(cell.1 * height));
            imageops::overlay(&mut canvas, &frame.to_rgba8(), x, y);
        }
        let canvas = DynamicImage::ImageRgba8(canvas);
        let expected = if full_chroma {
            ImageCrateEncoder::encode_jpeg_sampled(&canvas, &output, 90, SamplingFactor::F_1_1)
        } else {
            ImageCrateEncoder::encode_jpeg(&canvas, &output, 90)
        }
        .expect("encodes");
        assert_eq!(std::fs::read(&output).expect("reads the tile"), expected);
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
            enc.create_trickplay_tile(&options, 90, 320, Some(180), true)
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
            enc.create_trickplay_tile(&options, 90, 100, Some(100), true)
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
            enc.create_trickplay_tile(&options, 90, 999, None, true)
                .await,
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
            enc.create_trickplay_tile(&options, 90, 16, None, true)
                .await,
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
                encoder.create_trickplay_tile(&tile_options, 90, 100, Some(100), true),
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
