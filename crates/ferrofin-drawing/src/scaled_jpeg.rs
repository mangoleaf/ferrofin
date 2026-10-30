//! DCT-scaled JPEG decoding for the encoder's downscaling paths.
//!
//! A JPEG can be decoded straight to 1/2, 1/4 or 1/8 of its size by running a
//! reduced inverse DCT on each 8×8 block, so the full-resolution bitmap is never
//! built: a 24 MP photo bound for a 128 px blur hash decodes to 750×500 instead
//! of 6000×4000 (about 1 MB of pixels instead of 72 MB). A progressive JPEG
//! still holds its whole frame's DCT coefficients while it decodes (2 bytes a
//! sample, as a full decode does too), so for one the saving is the output
//! bitmap and the full-size resize only.
//!
//! Upstream gets this from Skia. `SkiaEncoder.GetImageBlurHash` calls
//! `BlurHashEncoder.Encode(xComp, yComp, stream, 128, 128)` (BlurHashSharp's
//! SkiaSharp binding), which asks the codec for `GetScaledDimensions(scale)` and
//! decodes at that size (libjpeg-turbo's scaled IDCT) before it resamples. The
//! `image` crate's JPEG decoder (zune-jpeg) has no scaled decode, so this module
//! uses `jpeg-decoder`'s [`Decoder::scale`](jpeg_decoder::Decoder::scale).
//!
//! The scale chosen is the smallest one whose output still covers the caller's
//! minimum on both axes, so the resampler that follows only ever shrinks: the
//! encoder's output keeps the size of a full-resolution decode, and its pixels
//! stay close to it. A file this decoder does not handle (CMYK, 12/16-bit,
//! lossless, arithmetic coding, not a JPEG at all), a target too large for any
//! reduction, or a decode error yields `None`, and the caller decodes through
//! the `image` crate as before.

use std::fs::File;
use std::io::BufReader;

use image::metadata::Orientation;
use image::{DynamicImage, GrayImage, RgbImage};
use jpeg_decoder::{CodingProcess, Decoder, PixelFormat};

/// The numerators of the reduced DCT scales `jpeg-decoder` implements, in
/// eighths, smallest first: 1/8, 1/4 and 1/2. 8/8 is the full decode, which the
/// `image` crate already does.
const REDUCED_SCALES: [u32; 3] = [1, 2, 4];

/// A JPEG whose header has been read and that can be decoded at a reduced scale.
pub(crate) struct ScalableJpeg {
    decoder: Decoder<BufReader<File>>,
    path: String,
    width: u32,
    height: u32,
}

impl ScalableJpeg {
    /// Reads the header of the JPEG at `path`.
    ///
    /// `None` when the file cannot be opened, is not a JPEG, or is one this
    /// decoder does not turn into the same pixels the `image` crate would:
    /// anything but an 8-bit YCbCr/RGB or grayscale DCT image.
    ///
    /// Also `None` for an image the `image` crate refuses to decode in full
    /// (its default allocation limit, the one the full decode runs under): a
    /// progressive JPEG keeps its coefficients for the whole frame whatever
    /// the output scale, so the scaled decode must not take on a size the full
    /// one would turn away. The full decode then reports the limit, as before.
    pub(crate) fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let mut decoder = Decoder::new(BufReader::new(file));
        decoder.read_info().ok()?;
        let info = decoder.info()?;
        let channels = match info.pixel_format {
            PixelFormat::RGB24 => 3,
            PixelFormat::L8 => 1,
            PixelFormat::L16 | PixelFormat::CMYK32 => return None,
        };
        let full_decode_bytes = u64::from(info.width) * u64::from(info.height) * channels;
        let refused = image::Limits::default()
            .max_alloc
            .is_some_and(|max| full_decode_bytes > max);
        if refused || info.coding_process == CodingProcess::Lossless {
            return None;
        }
        Some(Self {
            decoder,
            path: path.to_owned(),
            width: u32::from(info.width),
            height: u32::from(info.height),
        })
    }

    /// The stored size, before any EXIF orientation.
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The EXIF orientation read from the header segments, or
    /// [`Orientation::NoTransforms`] when there is none.
    pub(crate) fn orientation(&self) -> Orientation {
        exif_orientation(&self.decoder)
    }

    /// Decodes at the smallest DCT scale whose output is still at least
    /// `min_width`×`min_height` (stored orientation), or `None` when only the
    /// full size covers it or the decode fails.
    ///
    /// `expected_orientation` is the orientation the caller planned the target
    /// with: when the decode finds a different one (an EXIF segment after the
    /// frame header), the plan is wrong and the result is discarded.
    pub(crate) fn decode_at_least(
        mut self,
        min_width: u32,
        min_height: u32,
        expected_orientation: Option<Orientation>,
    ) -> Option<DynamicImage> {
        let eighths = dct_scale(self.width, self.height, min_width, min_height)?;
        let width = scaled_side(self.width, eighths);
        let height = scaled_side(self.height, eighths);
        let requested = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        // Requesting the exact scaled size selects that scale: the decoder
        // takes the smallest scale reaching the request on either axis, and
        // every smaller one falls short on both unless a side is a few pixels
        // long. Then it picks another size, which is refused below.
        match self.decoder.scale(requested.0, requested.1) {
            Ok(actual) if actual == requested => {}
            Ok(actual) => {
                tracing::debug!(
                    path = %self.path,
                    ?requested,
                    ?actual,
                    "scaled JPEG decode picked another size; decoding at full size"
                );
                return None;
            }
            Err(error) => {
                tracing::debug!(path = %self.path, %error, "scaled JPEG decode unavailable; decoding at full size");
                return None;
            }
        }
        // A panic in the decoder (its worker threads, or a malformed file it
        // does not guard against) falls back like an error: the full decode
        // then answers for the file as it did before. The panic hook still
        // reports it.
        let decoded =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.decoder.decode()));
        let pixels = match decoded {
            Ok(Ok(pixels)) => pixels,
            Ok(Err(error)) => {
                tracing::debug!(path = %self.path, %error, "scaled JPEG decode failed; decoding at full size");
                return None;
            }
            Err(_) => {
                tracing::debug!(path = %self.path, "scaled JPEG decoder panicked; decoding at full size");
                return None;
            }
        };
        if expected_orientation.is_some_and(|planned| planned != exif_orientation(&self.decoder)) {
            return None;
        }
        match self.decoder.info()?.pixel_format {
            PixelFormat::RGB24 => {
                RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
            }
            PixelFormat::L8 => {
                GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
            }
            PixelFormat::L16 | PixelFormat::CMYK32 => None,
        }
    }
}

/// The EXIF orientation of the segments `decoder` has read so far, the same
/// lookup the `image` crate's JPEG decoder does.
fn exif_orientation(decoder: &Decoder<BufReader<File>>) -> Orientation {
    decoder
        .exif_data()
        .and_then(Orientation::from_exif_chunk)
        .unwrap_or(Orientation::NoTransforms)
}

/// The numerator, in eighths, of the smallest reduced DCT scale whose output
/// covers `min_width`×`min_height`, or `None` when only the full size does.
fn dct_scale(width: u32, height: u32, min_width: u32, min_height: u32) -> Option<u32> {
    REDUCED_SCALES.into_iter().find(|&eighths| {
        scaled_side(width, eighths) >= min_width && scaled_side(height, eighths) >= min_height
    })
}

/// One side of the image at `eighths`/8 scale, rounded up as libjpeg and
/// `jpeg-decoder` round it.
fn scaled_side(side: u32, eighths: u32) -> u32 {
    (side * eighths).div_ceil(8)
}

/// Whether `orientation` turns the image a quarter turn, so the stored width
/// becomes the displayed height.
pub(crate) fn swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;
    use rstest::rstest;
    use tempfile::TempDir;

    /// An EXIF block holding only `Orientation = 6` (rotate 90° clockwise):
    /// a little-endian TIFF header and one IFD entry.
    const ORIENTATION_6_EXIF: [u8; 26] = [
        0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // "II*", IFD at 8
        0x01, 0x00, // one entry
        0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
        0x00, // Orientation = 6
        0x00, 0x00, 0x00, 0x00, // no next IFD
    ];

    /// Writes a `width`×`height` JPEG of `color` pixels (`[r, g, b]`, or
    /// `[l]` for grayscale) to `dir/name`, with `exif` when given.
    fn jpeg(
        dir: &TempDir,
        name: &str,
        width: u32,
        height: u32,
        color: &[u8],
        exif: Option<&[u8]>,
    ) -> String {
        let path = dir.path().join(name);
        let (pixels, color_type) = if let [luma] = color {
            (
                vec![*luma; (width * height) as usize],
                image::ExtendedColorType::L8,
            )
        } else {
            (
                color.repeat((width * height) as usize),
                image::ExtendedColorType::Rgb8,
            )
        };
        let file = std::fs::File::create(&path).expect("create fixture");
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif.to_vec()).expect("exif");
        }
        encoder
            .write_image(&pixels, width, height, color_type)
            .expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_large_jpeg_decodes_at_the_smallest_covering_scale() {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "photo.jpg", 1600, 1200, &[200, 120, 40], None);
        let jpeg = ScalableJpeg::open(&path).expect("a baseline JPEG opens");
        assert_eq!(jpeg.dimensions(), (1600, 1200));
        let image = jpeg
            .decode_at_least(128, 96, None)
            .expect("an eighth covers 128x96");
        assert_eq!((image.width(), image.height()), (200, 150));
        let DynamicImage::ImageRgb8(pixels) = image else {
            panic!("a colour JPEG decodes to RGB, as the image crate decodes it");
        };
        let [r, g, b] = pixels.get_pixel(100, 75).0;
        assert!(
            r.abs_diff(200) <= 2 && g.abs_diff(120) <= 2 && b.abs_diff(40) <= 2,
            "{r} {g} {b}"
        );
    }

    /// Writes a `width`×`height` JPEG of `color` the way cameras and most
    /// encoders do, with subsampled chroma, optionally progressive.
    fn camera_jpeg(
        dir: &TempDir,
        name: &str,
        (width, height): (u16, u16),
        color: [u8; 3],
        sampling: jpeg_encoder::SamplingFactor,
        progressive: bool,
    ) -> String {
        let path = dir.path().join(name);
        let mut encoder = jpeg_encoder::Encoder::new_file(&path, 90).expect("create fixture");
        encoder.set_sampling_factor(sampling);
        encoder.set_progressive(progressive);
        encoder
            .encode(
                &color.repeat(usize::from(width) * usize::from(height)),
                width,
                height,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    /// Chroma-subsampled and progressive JPEGs, what cameras and phones write,
    /// decode at the covering scale with their colour intact.
    #[rstest]
    #[case::baseline_420(jpeg_encoder::SamplingFactor::F_2_2, false)]
    #[case::progressive_420(jpeg_encoder::SamplingFactor::F_2_2, true)]
    #[case::baseline_422(jpeg_encoder::SamplingFactor::F_2_1, false)]
    fn camera_jpegs_decode_at_the_covering_scale(
        #[case] sampling: jpeg_encoder::SamplingFactor,
        #[case] progressive: bool,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let color = [200, 120, 40];
        let path = camera_jpeg(
            &dir,
            "camera.jpg",
            (1601, 1203),
            color,
            sampling,
            progressive,
        );
        let image = ScalableJpeg::open(&path)
            .expect("opens")
            .decode_at_least(300, 200, None)
            .expect("a quarter covers 300x200");
        // ceil(1601 / 4) x ceil(1203 / 4), as libjpeg rounds a scaled side.
        assert_eq!((image.width(), image.height()), (401, 301));
        let pixel = image.to_rgb8().get_pixel(200, 150).0;
        for (got, want) in pixel.into_iter().zip(color) {
            assert!(got.abs_diff(want) <= 3, "{pixel:?} for {color:?}");
        }
    }

    /// A JPEG whose header reads but whose data is cut short is left to the
    /// full decode, which handles it as it always did.
    #[test]
    fn a_truncated_jpeg_is_left_to_the_full_decode() {
        let dir = TempDir::new().expect("tempdir");
        let path = camera_jpeg(
            &dir,
            "whole.jpg",
            (1600, 1200),
            [10, 200, 90],
            jpeg_encoder::SamplingFactor::F_2_2,
            false,
        );
        let bytes = std::fs::read(&path).expect("read");
        std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("truncate");
        let jpeg = ScalableJpeg::open(&path).expect("the header is intact");
        assert!(jpeg.decode_at_least(200, 150, None).is_none());
    }

    #[test]
    fn a_grayscale_jpeg_decodes_to_luma() {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "gray.jpg", 1024, 512, &[90], None);
        let image = ScalableJpeg::open(&path)
            .expect("a grayscale JPEG opens")
            .decode_at_least(256, 128, None)
            .expect("a quarter covers 256x128");
        assert!(matches!(image, DynamicImage::ImageLuma8(_)));
        assert_eq!((image.width(), image.height()), (256, 128));
    }

    /// No reduced scale reaches past half the size: the caller decodes in full.
    #[test]
    fn a_target_past_half_size_is_left_to_the_full_decode() {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "photo.jpg", 1600, 1200, &[1, 2, 3], None);
        let jpeg = ScalableJpeg::open(&path).expect("opens");
        assert!(jpeg.decode_at_least(801, 600, None).is_none());
    }

    #[test]
    fn the_exif_orientation_comes_from_the_header() {
        let dir = TempDir::new().expect("tempdir");
        let tagged = jpeg(
            &dir,
            "tagged.jpg",
            64,
            32,
            &[1, 2, 3],
            Some(&ORIENTATION_6_EXIF),
        );
        let plain = jpeg(&dir, "plain.jpg", 64, 32, &[1, 2, 3], None);
        assert_eq!(
            ScalableJpeg::open(&tagged).expect("opens").orientation(),
            Orientation::Rotate90
        );
        assert_eq!(
            ScalableJpeg::open(&plain).expect("opens").orientation(),
            Orientation::NoTransforms
        );
    }

    /// A decode that finds another orientation than the one the target was
    /// planned with is discarded rather than drawn the wrong way up.
    #[test]
    fn a_decode_under_another_orientation_is_discarded() {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(
            &dir,
            "tagged.jpg",
            1600,
            800,
            &[1, 2, 3],
            Some(&ORIENTATION_6_EXIF),
        );
        let open = || ScalableJpeg::open(&path).expect("opens");
        assert!(
            open()
                .decode_at_least(200, 100, Some(Orientation::NoTransforms))
                .is_none()
        );
        assert!(
            open()
                .decode_at_least(200, 100, Some(Orientation::Rotate90))
                .is_some()
        );
        assert!(open().decode_at_least(200, 100, None).is_some());
    }

    /// A frame header alone (no scan data): `width`×`height`, 8-bit, three
    /// components at 4:2:0.
    fn frame_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        bytes.extend(height.to_be_bytes());
        bytes.extend(width.to_be_bytes());
        bytes.extend([
            0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xD9,
        ]);
        bytes
    }

    /// A size the `image` crate's full decode refuses is refused here too, so
    /// the scaled decode never allocates for a frame the full one would not.
    #[test]
    fn a_frame_the_full_decode_refuses_is_not_opened() {
        let dir = TempDir::new().expect("tempdir");
        let open = |name: &str, width: u16, height: u16| {
            let path = dir.path().join(name);
            std::fs::write(&path, frame_header(width, height)).expect("write fixture");
            ScalableJpeg::open(path.to_str().expect("utf-8")).map(|jpeg| jpeg.dimensions())
        };
        // 20000x10000 RGB is 600 MB, over the image crate's 512 MiB default.
        assert_eq!(open("huge.jpg", 20_000, 10_000), None);
        assert_eq!(open("large.jpg", 10_000, 5_000), Some((10_000, 5_000)));
    }

    /// Files the scaled decoder would not turn into the pixels the `image`
    /// crate gives are left to it: CMYK, anything that is not a JPEG, and a
    /// missing file (whose error the full decode reports).
    #[test]
    fn other_inputs_are_left_to_the_image_crate() {
        let dir = TempDir::new().expect("tempdir");
        let cmyk = dir.path().join("cmyk.jpg");
        jpeg_encoder::Encoder::new_file(&cmyk, 90)
            .expect("create fixture")
            .encode(
                &[0, 64, 128, 0].repeat(256 * 256),
                256,
                256,
                jpeg_encoder::ColorType::Cmyk,
            )
            .expect("write fixture");
        let png = dir.path().join("poster.png");
        image::RgbImage::new(256, 256)
            .save(&png)
            .expect("write fixture");
        for path in [cmyk, png, dir.path().join("missing.jpg")] {
            assert!(
                ScalableJpeg::open(path.to_str().expect("utf-8")).is_none(),
                "{}",
                path.display()
            );
        }
    }

    /// The smallest scale still covering the target on both axes; `None` when
    /// a half-size decode would already fall short.
    #[rstest]
    #[case::blur_hash_of_a_24mp_photo(6000, 4000, 128, 85, Some(1))]
    #[case::exactly_an_eighth(1024, 1024, 128, 128, Some(1))]
    #[case::one_pixel_past_an_eighth(1024, 1024, 129, 129, Some(2))]
    #[case::taller_axis_decides(1000, 1500, 85, 128, Some(1))]
    #[case::rounds_up_like_the_decoder(1001, 1001, 126, 126, Some(1))]
    #[case::poster_to_a_quarter(2000, 3000, 400, 600, Some(2))]
    #[case::backdrop_to_a_half(3840, 2160, 1920, 1080, Some(4))]
    #[case::needs_the_full_size(3840, 2160, 1921, 1080, None)]
    #[case::other_axis_needs_more(1600, 1600, 100, 600, Some(4))]
    fn dct_scale_picks_the_smallest_covering_scale(
        #[case] width: u32,
        #[case] height: u32,
        #[case] min_width: u32,
        #[case] min_height: u32,
        #[case] expected: Option<u32>,
    ) {
        assert_eq!(dct_scale(width, height, min_width, min_height), expected);
    }

    #[test]
    fn quarter_turns_swap_axes() {
        let swapped: Vec<_> = [
            Orientation::NoTransforms,
            Orientation::Rotate90,
            Orientation::Rotate180,
            Orientation::Rotate270,
            Orientation::FlipHorizontal,
            Orientation::FlipVertical,
            Orientation::Rotate90FlipH,
            Orientation::Rotate270FlipH,
        ]
        .into_iter()
        .map(swaps_axes)
        .collect();
        assert_eq!(
            swapped,
            [false, true, false, true, false, false, true, true]
        );
    }
}
