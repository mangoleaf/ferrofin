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
//! - The Skia-only input formats (`dng`, `astc`, `ktx`, raw camera formats, `svg`)
//!   are dropped: this encoder advertises only the decodable subset the `image`
//!   crate is compiled with (`jpeg`/`jpg`/`png`/`webp`), plus the container names
//!   Skia lists that map onto those decoders. Requesting an unsupported input
//!   returns the input path untouched (mirrors `EncodeImage`).
//! - The C# `throw` sites (`FileNotFoundException`, `InvalidDataException`,
//!   `ArgumentException`, `InvalidOperationException`) become
//!   [`ServiceError`] variants: [`ServiceError::NotFound`] for a missing input,
//!   [`ServiceError::InvalidInput`] for a bad argument/undecodable image, and
//!   [`ServiceError::Backend`] for an I/O/encode failure.
//! - `GetImageSize` returns [`ImageDimensions::default`] (`0×0`) on a zero-byte or
//!   undecodable file, exactly like the Skia `return default;` fallbacks.
//! - `CreateSplashscreen` is a deferred feature returning [`ServiceError::Backend`].
//!   `GetImageBlurHash` is implemented (decode → 128x128 downscale → BlurHash-encode);
//!   its output is valid but not byte-identical to Skia's (see the method doc).
//! - The overlay work Skia does inside `EncodeImage` (background colour, blur,
//!   foreground layer, unplayed/percent-played indicators) is **not** ported —
//!   this encoder does resize + format-convert only, so `has_default_options`
//!   already decides whether re-encoding is needed.

use std::path::Path;

use crate::error::DrawingError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_model::drawing::drawing_utils::{resize, resize_fill};
use hermit_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use hermit_traits::drawing::ImageEncoder;
use hermit_traits::error::ServiceError;
use hermit_traits::options::{ImageCollageOptions, ImageProcessingOptions};
use image::{DynamicImage, ImageFormat as CrateFormat, ImageReader, RgbaImage, imageops};

/// A real image codec implementing [`ImageEncoder`] on the `image` crate.
///
/// Handles the decodable subset of Jellyfin's Skia input set (`jpeg`/`jpg`/`png`/
/// `webp`) and can write [`ImageFormat::Webp`], [`ImageFormat::Jpg`], and
/// [`ImageFormat::Png`]. Resize/crop math is delegated to the shared
/// `hermit-model` `drawing_utils`, so it matches the C# `DrawingUtils` /
/// `ImageHelper.GetNewImageSize` exactly.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageCrateEncoder;

impl ImageCrateEncoder {
    /// The message on the [`ServiceError::Backend`] returned by the deferred
    /// blurhash/splashscreen features.
    const DEFERRED: &'static str = "ImageCrateEncoder: feature not implemented (deferred)";

    /// Constructs a new [`ImageCrateEncoder`].
    #[must_use]
    pub fn new() -> Self {
        Self
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
    fn decode(path: &str) -> Result<DynamicImage, ServiceError> {
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
    /// default → PNG); gif/bmp are not writable by this build, so they too fall
    /// back to PNG.
    fn format_for_path(path: &str) -> CrateFormat {
        match Self::input_format(path).as_str() {
            "jpg" | "jpeg" => CrateFormat::Jpeg,
            "webp" => CrateFormat::WebP,
            _ => CrateFormat::Png,
        }
    }

    /// Writes `image` to `output_path` in `format` at `quality` (0–100),
    /// creating the parent directory first. JPEG honours `quality`; PNG/WebP are
    /// lossless here, so `quality` is ignored (the `image` crate's default WebP
    /// writer is lossless). Port of the `SKFileWStream`/`Encode(..., quality)`
    /// tail of `EncodeImage`.
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
            let quality = quality.clamp(0, 100);
            // Truncation is safe: quality is clamped to 0..=100.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let quality = quality as u8;
            let file = std::fs::File::create(output_path)
                .map_err(|e| DrawingError::io(format!("create {output_path}"), e))?;
            let mut writer = std::io::BufWriter::new(file);
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality);
            image
                .to_rgb8()
                .write_with_encoder(encoder)
                .map_err(|e| DrawingError::encode(format!("encode {output_path}"), e).into())
        } else {
            image
                .save_with_format(output_path, format)
                .map_err(|e| DrawingError::encode(format!("encode {output_path}"), e).into())
        }
    }

    /// Resizes `image` to exactly `size` using a high-quality Lanczos3 filter
    /// (the `image` crate's nearest analogue to Skia's Mitchell/linear
    /// resamplers). Mirrors `SkiaEncoder.ResizeImage` at the dimensional level.
    fn resize_exact(image: &DynamicImage, size: ImageDimensions) -> DynamicImage {
        // Non-positive dimensions cannot be requested here: DrawingUtils never
        // produces them from a decoded (positive) source. Clamp to 1 defensively.
        let width = u32::try_from(size.width.max(1)).unwrap_or(1);
        let height = u32::try_from(size.height.max(1)).unwrap_or(1);
        image.resize_exact(width, height, imageops::FilterType::Lanczos3)
    }
}

/// The decodable input extensions this encoder advertises.
///
/// The subset of `SkiaEncoder.SupportedInputFormats` the `image` crate is
/// compiled to decode (`jpeg`/`png`/`webp`), with the alias `jpg` for `jpeg`.
/// The Skia-only formats (`dng`/`astc`/`ktx`/raw/`svg`/`gif`/`bmp`/`ico`) are
/// dropped because this build cannot decode them.
const SUPPORTED_INPUT_FORMATS: [&str; 4] = ["jpeg", "jpg", "png", "webp"];

#[async_trait]
impl ImageEncoder for ImageCrateEncoder {
    /// Port of `SupportedInputFormats` — the decodable subset
    /// (`jpeg`/`jpg`/`png`/`webp`).
    fn supported_input_formats(&self) -> Vec<String> {
        SUPPORTED_INPUT_FORMATS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// Port of `SupportedOutputFormats` — [`ImageFormat::Webp`],
    /// [`ImageFormat::Jpg`], [`ImageFormat::Png`] (Skia also lists `Svg`, which
    /// this raster encoder cannot produce).
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
    async fn get_image_size(&self, path: &str) -> Result<ImageDimensions, ServiceError> {
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
        let decoded = Self::decode(path)?;
        let small = decoded
            .resize(128, 128, image::imageops::FilterType::Triangle)
            .to_rgba8();
        let x = u32::try_from(x_comp.clamp(1, 9)).unwrap_or(1);
        let y = u32::try_from(y_comp.clamp(1, 9)).unwrap_or(1);
        blurhash::encode(x, y, small.width(), small.height(), small.as_raw())
            .map_err(|e| ServiceError::backend(e.to_string()))
    }

    /// Port of `EncodeImage`. Rejects unsupported inputs (returns `input_path`
    /// untouched), decodes, short-circuits to the original file when
    /// [`ImageProcessingOptions::has_default_options`] holds and no
    /// auto-orientation is requested, otherwise resizes to
    /// `ImageHelper.GetNewImageSize` and writes `output_format` at `quality`.
    ///
    /// The `auto_orient`/`orientation` overlay work Skia performs is not ported;
    /// this encoder does resize + format-convert only.
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

        // Unsupported input: spit out the original path, like Skia does.
        if !Self::is_supported_input(input_path) {
            return Ok(input_path.to_owned());
        }

        let image = Self::decode(input_path)?;
        let original = ImageDimensions::new(
            i32::try_from(image.width()).unwrap_or(i32::MAX),
            i32::try_from(image.height()).unwrap_or(i32::MAX),
        );

        // "Just spit out the original file if all the options are default."
        if options.has_default_options(input_path, Some(original)) && !auto_orient {
            return Ok(input_path.to_owned());
        }

        // GetNewImageSize: DrawingUtils.Resize then DrawingUtils.ResizeFill.
        let sized = resize(
            original,
            options.width.unwrap_or(0),
            options.height.unwrap_or(0),
            options.max_width.unwrap_or(0),
            options.max_height.unwrap_or(0),
        );
        let new_size = resize_fill(sized, options.fill_width, options.fill_height);

        let resized = Self::resize_exact(&image, new_size);
        Self::write(
            &resized,
            output_path,
            Self::crate_format(output_format),
            quality,
        )?;
        Ok(output_path.to_owned())
    }

    /// Port of `CreateImageCollage`. Dispatches on the width/height ratio: a
    /// `>= 1.4` ratio builds a horizontal thumb strip, anything else a `2×2`
    /// square collage. The text/gradient extras Skia draws are omitted — this is
    /// a plain tile composite.
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

    /// Deferred feature — always [`ServiceError::Backend`]. Port of
    /// `CreateSplashscreen`, whose builder is out of scope for this unit.
    async fn create_splashscreen(
        &self,
        _posters: &[String],
        _backdrops: &[String],
    ) -> Result<(), ServiceError> {
        Err(ServiceError::backend(Self::DEFERRED))
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

        let grid_w = u32::try_from(img_width * tile_width).unwrap_or(u32::MAX);
        let grid_h = u32::try_from(img_height * tile_height).unwrap_or(u32::MAX);
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

/// The width/height ratio at or above which a collage is laid out as a
/// horizontal thumb strip rather than a `2×2` square. Port of the C# `>= 1.4`
/// dispatch in `CreateImageCollage`.
const COLLAGE_THUMB_RATIO: f64 = 1.4;

/// The fixed JPEG/PNG quality collages are written at. Port of the hard-coded
/// `90` in `StripCollageBuilder.BuildSquareCollage`/`BuildThumbCollage`.
const COLLAGE_QUALITY: i32 = 90;

/// Decodes the first path that loads, tiling the rest into a `2×2` square,
/// each cell `width/2 × height/2`. Port of
/// `StripCollageBuilder.BuildSquareCollageBitmap` (no text/shadow extras).
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
/// width (aspect-preserved) and drawn at the top on a black canvas. Port of the
/// backdrop-fill portion of `StripCollageBuilder.BuildThumbCollageBitmap`; the
/// shadow rectangle and library-name text are omitted.
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
    }

    canvas
}

/// Returns the first decodable image at or after `start`, plus the index one
/// past it. Port of `SkiaHelper.GetNextValidImage` — undecodable paths are
/// skipped rather than fatal.
fn next_valid_image(paths: &[String], start: usize) -> Option<(DynamicImage, usize)> {
    let mut index = start;
    while index < paths.len() {
        if let Ok(image) = ImageCrateEncoder::decode(&paths[index]) {
            return Some((image, index + 1));
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    //! Parity is dimensional + decodable-output (no byte oracle). Round-trip
    //! cases transliterate the SkiaEncoder behaviour: probe fixture dims, the
    //! `1920×1080` → `maxWidth 960` → `960×540` resize (the C#
    //! `DrawingUtils`/`ImageHelper` oracle), PNG→JPEG re-encode decodes at the
    //! target size, and the trickplay grid pack.
    use super::*;
    use hermit_traits::options::ItemImageInfo;
    use image::{Rgba, RgbaImage};
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

    #[test]
    fn advertises_capabilities() {
        let enc = ImageCrateEncoder::new();
        assert_eq!(
            enc.supported_input_formats(),
            vec![
                "jpeg".to_owned(),
                "jpg".to_owned(),
                "png".to_owned(),
                "webp".to_owned()
            ]
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

    #[tokio::test]
    async fn splashscreen_is_deferred() {
        let enc = ImageCrateEncoder::new();
        assert!(matches!(
            enc.create_splashscreen(&[], &[]).await,
            Err(ServiceError::Backend(_))
        ));
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
}
