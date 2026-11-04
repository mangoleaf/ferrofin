//! The full-size decode of a sequential JPEG that codes its components in
//! separate scans.
//!
//! A sequential (baseline or extended Huffman) JPEG may code its components in
//! more than one scan: Y in one, Cb and Cr in others. Every DCT decoder must
//! read that ("Interleaved and non-interleaved scans", ITU-T T.81 Table 1).
//! libjpeg's `cjpeg` writes it from a scan script, and `jpeg-encoder` writes it
//! whenever its Huffman tables are optimized, so every JPEG this crate writes
//! has it (`ImageCrateEncoder::encode_jpeg`).
//!
//! zune-jpeg 0.5.15, the `image` crate's JPEG decoder, reads such a file to
//! the wrong colours and reports no error: with subsampled chroma it leaves
//! the chroma planes at 0, so the whole image comes out green, and at 4:4:4 it
//! can drop a scan's last row of blocks. libjpeg, ffmpeg and `jpeg-decoder`
//! read the same files correctly. zune-jpeg's fix (zune-image issues 419, 448
//! and 496) is in 0.5.16-rc3 but in no stable release, and `image` 0.25's
//! `zune-jpeg = "^0.5.5"` never selects a pre-release.
//!
//! So a file whose first scan leaves components out is decoded by
//! `jpeg-decoder`, the decoder [`crate::scaled_jpeg`] already uses, at full
//! size. Every other file still goes through the `image` crate, and so does
//! one `jpeg-decoder` fails on or does not turn into the pixels the `image`
//! crate would (CMYK, 16-bit, lossless: [`ScalableJpeg::open`]), which zune
//! still misreads in this layout.
//!
//! The decode is checked against the same limits as the `image` crate's.
//! Beyond its output, `jpeg-decoder` holds one byte per sample of the
//! component planes while it decodes a sequential frame; zune-jpeg, for this
//! layout, held the whole frame's coefficients at two bytes per sample.
//!
//! Once `Cargo.lock` resolves zune-jpeg 0.5.16 or
//! later, this module and its two calls in [`crate::limits`] can go; the
//! regression tests there stay.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek};

use image::DynamicImage;
use image::metadata::Orientation;

use crate::limits::ImageLimits;
use crate::scaled_jpeg::ScalableJpeg;

/// Start of image.
const SOI: u8 = 0xD8;
/// End of image.
const EOI: u8 = 0xD9;
/// Start of frame, baseline DCT.
const SOF0: u8 = 0xC0;
/// Start of frame, extended sequential DCT, Huffman coding.
const SOF1: u8 = 0xC1;
/// Define Huffman tables: in the `SOFn` range, but not a frame.
const DHT: u8 = 0xC4;
/// Reserved for JPEG extensions: in the `SOFn` range, but not a frame.
const JPG: u8 = 0xC8;
/// Define arithmetic coding conditioning: in the `SOFn` range, but not a frame.
const DAC: u8 = 0xCC;
/// The last `SOFn` marker (lossless, arithmetic coding).
const SOF15: u8 = 0xCF;
/// Start of scan.
const SOS: u8 = 0xDA;
/// Temporary private use: a marker with no length.
const TEM: u8 = 0x01;
/// The restart markers `RST0`-`RST7`: markers with no length.
const RST: std::ops::RangeInclusive<u8> = 0xD0..=0xD7;

/// The image at `path` and its EXIF orientation, decoded by `jpeg-decoder`,
/// when it is a sequential JPEG coded in more than one scan that decoder
/// reads; `None` for any other file, which the `image` crate decodes.
///
/// A file over `limits` is `None` too ([`ScalableJpeg::open`]), so the
/// `image` crate's decode reports the limit as it does for every file.
pub(crate) fn decode(path: &str, limits: &ImageLimits) -> Option<(DynamicImage, Orientation)> {
    if !has_split_sequential_scans(path) {
        return None;
    }
    ScalableJpeg::open(path, limits)?.decode_full()
}

/// Whether the file at `path` is a sequential JPEG whose first scan leaves
/// some of its components out. A file that cannot be read, or that is not a
/// JPEG, is not.
fn has_split_sequential_scans(path: &str) -> bool {
    File::open(path).is_ok_and(|file| split_sequential_scans(BufReader::new(file)).unwrap_or(false))
}

/// Reads the segments before the first scan of the JPEG in `reader`: `true`
/// when the frame is sequential (`SOF0` or `SOF1`) and has more components
/// than its first scan codes. A progressive, lossless or arithmetic-coded
/// frame, a single-component one, or something that is not a JPEG is `false`.
fn split_sequential_scans(mut reader: impl BufRead + Seek) -> io::Result<bool> {
    let mut start = [0; 2];
    reader.read_exact(&mut start)?;
    if start != [0xFF, SOI] {
        return Ok(false);
    }
    // The component count of a sequential frame, once its header is read.
    let mut sequential_components = None;
    loop {
        if read_u8(&mut reader)? != 0xFF {
            return Ok(false);
        }
        // Any number of fill bytes may come before the marker code.
        let mut marker = read_u8(&mut reader)?;
        while marker == 0xFF {
            marker = read_u8(&mut reader)?;
        }
        if marker == TEM || RST.contains(&marker) {
            continue;
        }
        if marker == 0 || marker == SOI || marker == EOI {
            return Ok(false);
        }
        let length = u16::from_be_bytes([read_u8(&mut reader)?, read_u8(&mut reader)?]);
        // The length counts its own two bytes.
        let Some(body) = length.checked_sub(2) else {
            return Ok(false);
        };
        match marker {
            SOF0 | SOF1 => {
                // Sample precision (1), height (2), width (2), then the
                // component count.
                let mut header = [0; 6];
                if usize::from(body) < header.len() {
                    return Ok(false);
                }
                reader.read_exact(&mut header)?;
                sequential_components = Some(header[5]);
                reader.seek_relative(i64::from(body) - 6)?;
            }
            DHT | JPG | DAC => reader.seek_relative(i64::from(body))?,
            SOF0..=SOF15 => return Ok(false),
            SOS => {
                let scan_components = read_u8(&mut reader)?;
                return Ok(sequential_components.is_some_and(|frame| scan_components < frame));
            }
            _ => reader.seek_relative(i64::from(body))?,
        }
    }
}

/// The next byte of `reader`.
fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;
    use rstest::rstest;
    use std::io::Cursor;

    /// The issue's file: a baseline 4:2:0 JPEG in three single-component scans.
    const SPLIT_SCAN_JPEG: &[u8] = include_bytes!("../tests/fixtures/split_scans_420.jpg");

    /// A 32x16 grey JPEG from `jpeg-encoder` at `sampling`, with optimized
    /// Huffman tables or not, progressive or not.
    fn jpeg_encoder_file(
        sampling: jpeg_encoder::SamplingFactor,
        optimized: bool,
        progressive: bool,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut bytes, 90);
        encoder.set_sampling_factor(sampling);
        encoder.set_optimized_huffman_tables(optimized);
        encoder.set_progressive(progressive);
        encoder
            .encode(&[90; 32 * 16 * 3], 32, 16, jpeg_encoder::ColorType::Rgb)
            .expect("encode");
        bytes
    }

    /// A 16x16 JPEG from the `image` crate's encoder (one interleaved scan).
    fn image_crate_file(color: image::ExtendedColorType, channels: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
            .write_image(&vec![90; 16 * 16 * channels], 16, 16, color)
            .expect("encode");
        bytes
    }

    fn split(bytes: &[u8]) -> bool {
        split_sequential_scans(Cursor::new(bytes)).unwrap_or(false)
    }

    /// `jpeg-encoder` writes one scan per component whenever its Huffman
    /// tables are optimized, as `ImageCrateEncoder::encode_jpeg` asks.
    #[rstest]
    #[case::subsampled(jpeg_encoder::SamplingFactor::F_2_2)]
    #[case::full_chroma(jpeg_encoder::SamplingFactor::F_1_1)]
    fn jpeg_encoders_optimized_files_are_split(#[case] sampling: jpeg_encoder::SamplingFactor) {
        assert!(split(&jpeg_encoder_file(sampling, true, false)));
    }

    #[test]
    fn the_reported_file_is_split() {
        assert!(split(SPLIT_SCAN_JPEG));
    }

    #[rstest]
    #[case::standard_tables_are_interleaved(jpeg_encoder_file(
        jpeg_encoder::SamplingFactor::F_2_2,
        false,
        false
    ))]
    #[case::progressive(jpeg_encoder_file(jpeg_encoder::SamplingFactor::F_2_2, true, true))]
    #[case::image_crate_colour(image_crate_file(image::ExtendedColorType::Rgb8, 3))]
    #[case::grey_has_one_component(image_crate_file(image::ExtendedColorType::L8, 1))]
    #[case::a_png(b"\x89PNG\r\n\x1a\n".to_vec())]
    #[case::empty(Vec::new())]
    #[case::cut_before_the_scan(SPLIT_SCAN_JPEG[..100].to_vec())]
    #[case::a_length_shorter_than_itself(vec![0xFF, SOI, 0xFF, 0xE0, 0x00, 0x01])]
    #[case::a_frame_header_too_short(vec![0xFF, SOI, 0xFF, SOF0, 0x00, 0x04, 8, 0])]
    #[case::bytes_where_a_marker_belongs(vec![0xFF, SOI, 0x12, 0x34])]
    fn other_files_are_not_split(#[case] bytes: Vec<u8>) {
        assert!(!split(&bytes));
    }

    /// Fill bytes before a marker (T.81 B.1.1.2) are skipped, and so are the
    /// `TEM` and `RSTn` markers, which have no segment.
    #[test]
    fn fill_bytes_and_restart_markers_are_skipped() {
        let mut bytes = vec![0xFF, SOI, 0xFF, 0xFF, 0xFF, TEM, 0xFF, 0xD3];
        bytes.extend_from_slice(&SPLIT_SCAN_JPEG[2..]);
        assert!(split(&bytes));
    }

    #[test]
    fn a_split_file_decodes_through_jpeg_decoder() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("split.jpg");
        std::fs::write(&path, SPLIT_SCAN_JPEG).expect("write");
        let (image, orientation) =
            decode(path.to_str().expect("utf8"), &ImageLimits::default()).expect("routed");
        assert_eq!((image.width(), image.height()), (64, 64));
        assert_eq!(orientation, Orientation::NoTransforms);
    }

    /// A file the `image` crate reads correctly, a missing one and one over
    /// the limits are left to it.
    #[test]
    fn other_files_are_left_to_the_image_crate() {
        let dir = tempfile::tempdir().expect("tmp");
        let interleaved = dir.path().join("interleaved.jpg");
        std::fs::write(
            &interleaved,
            image_crate_file(image::ExtendedColorType::Rgb8, 3),
        )
        .expect("write");
        let split = dir.path().join("split.jpg");
        std::fs::write(&split, SPLIT_SCAN_JPEG).expect("write");
        let tiny = ImageLimits {
            max_pixels: 64 * 64 - 1,
            ..ImageLimits::default()
        };
        assert!(decode(interleaved.to_str().expect("utf8"), &ImageLimits::default()).is_none());
        assert!(decode("/nonexistent/split.jpg", &ImageLimits::default()).is_none());
        assert!(decode(split.to_str().expect("utf8"), &tiny).is_none());
    }
}
