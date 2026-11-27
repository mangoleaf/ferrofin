//! JPEG decoding with libjpeg-turbo, at full size or at a reduced DCT scale.
//!
//! libjpeg-turbo is the JPEG codec inside Skia, which upstream decodes with
//! (`SkiaEncoder.Decode`), and the one this crate encodes with. It can decode
//! straight to 1/2, 1/4 or 1/8 of an image's size by running a reduced
//! inverse DCT on each 8×8 block, so the full-resolution bitmap is never
//! built: a 24 MP photo bound for a 128 px blur hash decodes to 750×500
//! instead of 6000×4000 (about 1 MB of pixels instead of 72 MB).
//!
//! libjpeg-turbo scales by every other number of eighths too, without its
//! SIMD code (its README: "only 1/4 and 1/2 are SIMD-accelerated"; 1/8 keeps
//! one coefficient a block). Measured on a 2000×3000 and a 3840×2160 photo,
//! 3/8 decodes about a tenth slower than 1/2, and 5/8, 3/4 and 7/8 slower
//! than the full size: from a fifth more time at 5/8 to about twice at 7/8.
//! They would save memory at that price, and are not used
//! ([`REDUCED_SCALES`]).
//!
//! Upstream gets the reduced scale from Skia for its blur hash only.
//! `SkiaEncoder.GetImageBlurHash` calls
//! `BlurHashEncoder.Encode(xComp, yComp, stream, 128, 128)` (BlurHashSharp's
//! SkiaSharp binding), which asks the codec for `GetScaledDimensions(scale)`
//! and decodes at that size before it resamples. Everything else it decodes in
//! full; here a thumbnail, a collage cell and a splashscreen tile decode
//! reduced too ([`ScalableJpeg::decode_at_least`]), and every other JPEG in
//! full ([`ScalableJpeg::decode_full`]).
//!
//! The scale chosen is the smallest one whose output still covers the caller's
//! minimum on both axes, so the resampler that follows only ever shrinks: the
//! encoder's output keeps the size of a full-resolution decode, and its pixels
//! stay close to it.
//!
//! [`ScalableJpeg::open`] reads the file as far as its first scan, which is
//! enough for the header, so an image over the limits costs no more than
//! that. TurboJPEG decodes from memory, so a decode then reads the rest of
//! the file. A JPEG coded in more than one scan (a progressive one, or a
//! sequential one with a scan per component, which this crate's own encoder
//! wrote before it moved to libjpeg-turbo) also makes libjpeg-turbo hold the
//! DCT coefficients of the whole frame while it decodes, 2 bytes a sample,
//! whatever the output scale: `open` counts them against the decode limit
//! with the full-size bitmap.
//!
//! A warning from libjpeg (data cut short or corrupt, which it fills in) still
//! gives an image, as it does through Skia, whose codec reports such a file
//! as incomplete and upstream draws it. A fatal error fails the decode.
//!
//! A file libjpeg-turbo does not turn into the pixels the `image` crate would
//! (CMYK, 12- or 16-bit, lossless, not a JPEG at all), or one it fails on,
//! yields `None`, and the caller decodes through the `image` crate as before.
//! Its JPEG decoder, zune-jpeg 0.5.15, reads a sequential JPEG with a scan per
//! component to the wrong colours (gl-1213; fixed in no stable release), which
//! libjpeg-turbo reads correctly: such a file reaches zune-jpeg only when
//! libjpeg-turbo fails on it.
//!
//! This module holds the crate's only `unsafe` code: the calls into TurboJPEG
//! (`Decompressor`). The `turbojpeg` crate's own decompressor cannot tell a
//! warning from a fatal error, nor read a file's sample precision.

use std::ffi::{CStr, c_int};
use std::fs::File;
use std::io::Read;

use image::metadata::Orientation;
use image::{DynamicImage, GrayImage, RgbImage};
use turbojpeg::raw;

use crate::limits::ImageLimits;

/// A JPEG whose header has been read and that libjpeg-turbo decodes, at full
/// size or at a reduced scale.
pub(crate) struct ScalableJpeg {
    /// The file, read as far as `bytes` goes.
    file: File,
    /// The start of the file, past its first scan's header at least.
    bytes: Vec<u8>,
    /// The file's length when it was opened.
    length: u64,
    /// The most bytes of the file a decode holds: the memory limit.
    max_bytes: u64,
    /// The file's path, for the log.
    path: String,
    header: Header,
    orientation: Orientation,
}

impl ScalableJpeg {
    /// Reads the header of the JPEG at `path`.
    ///
    /// `Ok(None)` when the file cannot be opened, is not a JPEG, or is one
    /// libjpeg-turbo does not turn into the same pixels the `image` crate
    /// would: anything but an 8-bit YCbCr/RGB or grayscale DCT image. The
    /// `image` crate then answers for it.
    ///
    /// `Err` names the limit a JPEG is over, of the ones in `limits`. Its
    /// size is the full-size bitmap's, whatever scale it is then decoded at,
    /// so a reduced decode takes on nothing the full one would turn away,
    /// plus the coefficients libjpeg-turbo holds for the whole frame when the
    /// file is coded in more than one scan ([`coefficient_bytes`]). A file
    /// that starts as a JPEG does and is longer than the memory limit is
    /// refused too, before more of it is read: a decode holds it whole.
    pub(crate) fn open(path: &str, limits: &ImageLimits) -> Result<Option<Self>, String> {
        let Ok(mut file) = File::open(path) else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        let read = (&mut file).take(HEADER_READ_BYTES).read_to_end(&mut bytes);
        if read.is_err() || !bytes.starts_with(&[0xFF, SOI]) {
            return Ok(None);
        }
        let length = file.metadata().map_or(0, |metadata| metadata.len());
        if length > limits.max_alloc {
            return Err(format!(
                "the file takes {length} bytes, over the limit of {} \
                 (image_decode_max_memory_mb)",
                limits.max_alloc
            ));
        }
        let Some(segments) = read_to_first_scan(&mut file, &mut bytes, limits.max_alloc) else {
            return Ok(None);
        };
        let Some(header) = Decompressor::new().and_then(|mut turbo| turbo.read_header(&bytes))
        else {
            return Ok(None);
        };
        let bitmap_bytes =
            u64::from(header.width) * u64::from(header.height) * header.channels() as u64;
        let held_bytes = if header.progressive || segments.split_scans {
            coefficient_bytes(&header)
        } else {
            0
        };
        limits.check_decode(header.width, header.height, bitmap_bytes + held_bytes)?;
        Ok(Some(Self {
            file,
            bytes,
            length,
            max_bytes: limits.max_alloc,
            path: path.to_owned(),
            header,
            orientation: segments.orientation,
        }))
    }

    /// The stored size, before any EXIF orientation.
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.header.width, self.header.height)
    }

    /// The EXIF orientation in the segments before the first scan, or
    /// [`Orientation::NoTransforms`] when there is none: the one the `image`
    /// crate's JPEG decoder finds.
    pub(crate) fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Decodes at the smallest DCT scale (1/8, 1/4, 1/2) whose output is still
    /// at least `min_width`×`min_height` (stored orientation), and at full
    /// size when none is. `None` when the decode fails.
    pub(crate) fn decode_at_least(self, min_width: u32, min_height: u32) -> Option<DynamicImage> {
        let eighths = dct_scale(self.header.width, self.header.height, min_width, min_height);
        self.decode(eighths)
    }

    /// Decodes at full size, or `None` when the decode fails.
    pub(crate) fn decode_full(self) -> Option<DynamicImage> {
        self.decode(FULL_SCALE)
    }

    /// Decodes at `eighths`/8 of the size, to the colour type the `image`
    /// crate decodes the file to.
    fn decode(mut self, eighths: u32) -> Option<DynamicImage> {
        let width = scaled_side(self.header.width, eighths);
        let height = scaled_side(self.header.height, eighths);
        let channels = self.header.channels();
        let mut pixels = vec![0; width as usize * height as usize * channels];
        match self.decompress(eighths, &mut pixels) {
            Ok(None) => {}
            Ok(Some(warning)) => {
                tracing::debug!(path = %self.path, %warning, "decoded a JPEG libjpeg warned about");
            }
            Err(error) => {
                tracing::debug!(path = %self.path, %error, "libjpeg-turbo failed on a JPEG; decoding through the image crate");
                return None;
            }
        }
        if self.header.grey {
            GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        } else {
            RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
        }
    }

    /// Reads the rest of the file and decodes it into `pixels`
    /// ([`Decompressor::decompress`]).
    fn decompress(&mut self, eighths: u32, pixels: &mut [u8]) -> Result<Option<String>, String> {
        let read = self.bytes.len() as u64;
        self.bytes
            .reserve_exact(usize::try_from(self.length.saturating_sub(read)).unwrap_or(0));
        // No more than the limit, should the file have grown since `open`.
        (&mut self.file)
            .take(self.max_bytes.saturating_sub(read))
            .read_to_end(&mut self.bytes)
            .map_err(|error| format!("reading the file: {error}"))?;
        Decompressor::new()
            .ok_or_else(|| "TurboJPEG could not start".to_owned())?
            .decompress(&self.bytes, eighths, pixels, self.header.channels())
    }
}

/// Start of image.
const SOI: u8 = 0xD8;
/// End of image.
const EOI: u8 = 0xD9;
/// The first `SOFn` marker: start of frame, baseline DCT.
const SOF0: u8 = 0xC0;
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
/// Application segment 1, where EXIF data is.
const APP1: u8 = 0xE1;
/// Temporary private use: a marker with no length.
const TEM: u8 = 0x01;
/// The restart markers `RST0`-`RST7`: markers with no length.
const RST: std::ops::RangeInclusive<u8> = 0xD0..=0xD7;

/// How much of a file [`ScalableJpeg::open`] reads at first: more than the
/// segments before the scan take in most files (an EXIF block with its
/// thumbnail is under 64 KiB, the most one segment holds).
const HEADER_READ_BYTES: u64 = 64 * 1024;

/// Reads on in `file`, whose start is in `bytes`, as far as its first scan's
/// header, and returns what its segments say. `None` when the file has no
/// scan in its first `max_bytes` bytes, or cannot be read.
fn read_to_first_scan(file: &mut File, bytes: &mut Vec<u8>, max_bytes: u64) -> Option<Segments> {
    loop {
        match Segments::read(bytes) {
            Walk::Scan(segments) => return Some(segments),
            Walk::NoScan => return None,
            Walk::Short => {}
        }
        // As much again as was read, so a long run of segments is not walked
        // over from the start more than a few times.
        let read = bytes.len() as u64;
        let piece = read.min(max_bytes.saturating_sub(read));
        if file.by_ref().take(piece).read_to_end(bytes).ok()? == 0 {
            // The end of the file, or of what may be read of it, and no scan.
            return None;
        }
    }
}

/// How far [`Segments::read`] got in the bytes it was given.
enum Walk {
    /// To the first scan.
    Scan(Segments),
    /// To their end, before any scan: the file may go on.
    Short,
    /// To the end of the image, or to a second start of one: there is no scan
    /// to find.
    NoScan,
}

/// What the segments before a JPEG's first scan say that TurboJPEG's header
/// read does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segments {
    /// The EXIF orientation of the last EXIF segment, as zune-jpeg (the
    /// `image` crate's JPEG decoder) keeps it, so the two decoders stand a
    /// file the same way up.
    orientation: Orientation,
    /// Whether the first scan codes fewer components than the frame has: Y in
    /// one scan, Cb and Cr in others ("Interleaved and non-interleaved scans",
    /// ITU-T T.81). libjpeg's `cjpeg` writes that from a scan script, and
    /// `jpeg-encoder` whenever its Huffman tables are optimized. libjpeg then
    /// keeps the whole frame's coefficients, as for a progressive file
    /// (`jdinput.c`, `has_multiple_scans`).
    split_scans: bool,
}

impl Segments {
    /// Reads the segments of `jpeg`, which starts with the start-of-image
    /// marker, up to its first scan, finding each marker as libjpeg does
    /// (`jdmarker.c`, `next_marker`): bytes that belong to no segment are
    /// passed over, which libjpeg warns about and goes on from.
    fn read(jpeg: &[u8]) -> Walk {
        let mut found = Self {
            orientation: Orientation::NoTransforms,
            split_scans: false,
        };
        // The component count of the frame, once its header is read.
        let mut frame_components = None;
        let mut rest = jpeg.get(2..).unwrap_or_default();
        loop {
            // The next 0xFF, any number of fill bytes, then the marker code.
            let Some(marker_at) = rest
                .iter()
                .position(|&byte| byte == 0xFF)
                .and_then(|fill| Some(fill + rest[fill..].iter().position(|&byte| byte != 0xFF)?))
            else {
                return Walk::Short;
            };
            let marker = rest[marker_at];
            rest = &rest[marker_at + 1..];
            // A stuffed zero is no marker, and these three have no segment.
            if marker == 0 || marker == TEM || RST.contains(&marker) {
                continue;
            }
            if marker == SOI || marker == EOI {
                return Walk::NoScan;
            }
            let [high, low, after_length @ ..] = rest else {
                return Walk::Short;
            };
            // The length counts its own two bytes. One under 2 is an empty
            // segment to libjpeg where it skips one, and an error elsewhere,
            // which the header read then reports.
            let length = usize::from(u16::from_be_bytes([*high, *low])).saturating_sub(2);
            let Some((body, after_segment)) = after_length.split_at_checked(length) else {
                return Walk::Short;
            };
            match marker {
                SOS => {
                    found.split_scans = body
                        .first()
                        .zip(frame_components)
                        .is_some_and(|(scan, frame)| *scan < frame);
                    return Walk::Scan(found);
                }
                DHT | JPG | DAC => {}
                // Sample precision (1), height (2), width (2), then the
                // component count.
                SOF0..=SOF15 => frame_components = body.get(5).copied(),
                APP1 => {
                    if let Some(exif) = body.strip_prefix(b"Exif\0\0")
                        && !exif.is_empty()
                    {
                        found.orientation =
                            Orientation::from_exif_chunk(exif).unwrap_or(Orientation::NoTransforms);
                    }
                }
                _ => {}
            }
            rest = after_segment;
        }
    }
}

/// What TurboJPEG's header read says of a JPEG this module decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    width: u32,
    height: u32,
    /// One grey component; three colour components otherwise.
    grey: bool,
    progressive: bool,
    /// How many luma samples one chroma sample covers, across and down;
    /// `None` when the components are sampled another way than TurboJPEG
    /// names.
    chroma_step: Option<(u32, u32)>,
}

impl Header {
    /// The bytes of a decoded pixel: 1 of grey, or 3 of RGB.
    fn channels(&self) -> usize {
        if self.grey { 1 } else { 3 }
    }
}

/// The bytes of DCT coefficients libjpeg-turbo holds for the whole frame while
/// it decodes a JPEG coded in more than one scan: for each component one
/// 128-byte block (64 coefficients of 2 bytes) per 8×8 samples of its plane,
/// the planes padded to whole MCUs (`jdcoefct.c`, `jinit_d_coef_controller`
/// with a full buffer). With a sampling TurboJPEG does not name, every
/// component is counted at the largest MCU there is (4×4 blocks) and at full
/// resolution, which no sampling exceeds. TurboJPEG also gives its names to
/// files that write the same ratios with larger factors, whose MCUs are
/// larger: for those the count is short by under one MCU row and column.
fn coefficient_bytes(header: &Header) -> u64 {
    let padded = |side: u32, mcu: u32| u64::from(side.div_ceil(mcu)) * u64::from(mcu);
    if header.grey {
        return 2 * padded(header.width, 8) * padded(header.height, 8);
    }
    let Some((across, down)) = header.chroma_step else {
        return 3 * 2 * padded(header.width, 32) * padded(header.height, 32);
    };
    let luma = padded(header.width, 8 * across) * padded(header.height, 8 * down);
    2 * (luma + 2 * luma / (u64::from(across) * u64::from(down)))
}

/// The numerators of the reduced DCT scales used, in eighths, smallest first:
/// 1/8, 1/4 and 1/2, the ones libjpeg-turbo decodes faster than the full size.
const REDUCED_SCALES: [u32; 3] = [1, 2, 4];

/// The full size, in eighths.
const FULL_SCALE: u32 = 8;

/// The numerator, in eighths, of the smallest of [`REDUCED_SCALES`] whose
/// output covers `min_width`×`min_height`: [`FULL_SCALE`] when none does.
fn dct_scale(width: u32, height: u32, min_width: u32, min_height: u32) -> u32 {
    REDUCED_SCALES
        .into_iter()
        .find(|&eighths| {
            scaled_side(width, eighths) >= min_width && scaled_side(height, eighths) >= min_height
        })
        .unwrap_or(FULL_SCALE)
}

/// One side of the image at `eighths`/8 scale, rounded up as libjpeg rounds
/// it (`TJSCALED`). A JPEG's side is at most 65535, so the product fits.
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

/// What the last row of a buffer holds until a decode writes it
/// ([`Decompressor::decompress`]): bytes no row of an image is likely to be.
fn unwritten_row(length: usize) -> impl Iterator<Item = u8> {
    // Truncation is the point: a pattern that repeats every 256 bytes.
    #[allow(clippy::cast_possible_truncation)]
    (0..length).map(|at| (at as u8).wrapping_mul(167).wrapping_add(89))
}

/// A TurboJPEG decompression instance, destroyed when dropped.
struct Decompressor(raw::tjhandle);

impl Decompressor {
    /// A new instance, or `None` when TurboJPEG cannot allocate one.
    fn new() -> Option<Self> {
        // SAFETY: `tj3Init` takes no pointer. It returns null when it fails.
        let handle = unsafe { raw::tj3Init(raw::TJINIT_TJINIT_DECOMPRESS.cast_signed()) };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// The value of `param` for the last header read.
    fn get(&mut self, param: raw::TJPARAM) -> c_int {
        // SAFETY: the handle is the live instance `new` created.
        unsafe { raw::tj3Get(self.0, param.cast_signed()) }
    }

    /// TurboJPEG's message for the last failure or warning.
    fn message(&mut self) -> String {
        // SAFETY: the handle is live, and `tj3GetErrorStr` returns a
        // NUL-terminated string the instance owns, copied before any other
        // call on it.
        unsafe { CStr::from_ptr(raw::tj3GetErrorStr(self.0)) }
            .to_string_lossy()
            .into_owned()
    }

    /// Whether libjpeg warned during the call that just failed. A fatal
    /// error may have followed the warning: TurboJPEG reports the two the
    /// same way.
    fn warned(&mut self) -> bool {
        // SAFETY: the handle is the live instance `new` created.
        unsafe { raw::tj3GetErrorCode(self.0) == raw::TJERR_TJERR_WARNING.cast_signed() }
    }

    /// Reads the header of `jpeg`, which need go no further than its first
    /// scan's header: `None` when TurboJPEG cannot read it, or when it is not
    /// an 8-bit YCbCr/RGB or grayscale DCT image.
    fn read_header(&mut self, jpeg: &[u8]) -> Option<Header> {
        let length = raw::size_t::try_from(jpeg.len()).ok()?;
        // SAFETY: the handle is live, and the pointer and length are those of
        // `jpeg`, which TurboJPEG only reads.
        let failed = unsafe { raw::tj3DecompressHeader(self.0, jpeg.as_ptr(), length) } != 0;
        // After a warning the header was still read; after a fatal error the
        // size below is not a size (-1), with or without a warning before it.
        if failed && !self.warned() {
            return None;
        }
        let grey = match u32::try_from(self.get(raw::TJPARAM_TJPARAM_COLORSPACE)).ok()? {
            raw::TJCS_TJCS_GRAY => true,
            raw::TJCS_TJCS_YCbCr | raw::TJCS_TJCS_RGB => false,
            // CMYK and YCCK, which TurboJPEG does not convert to RGB.
            _ => return None,
        };
        if self.get(raw::TJPARAM_TJPARAM_PRECISION) != 8
            || self.get(raw::TJPARAM_TJPARAM_LOSSLESS) != 0
        {
            return None;
        }
        let chroma_step = match self.get(raw::TJPARAM_TJPARAM_SUBSAMP) {
            raw::TJSAMP_TJSAMP_444 => Some((1, 1)),
            raw::TJSAMP_TJSAMP_422 => Some((2, 1)),
            raw::TJSAMP_TJSAMP_420 => Some((2, 2)),
            raw::TJSAMP_TJSAMP_440 => Some((1, 2)),
            raw::TJSAMP_TJSAMP_411 => Some((4, 1)),
            raw::TJSAMP_TJSAMP_441 => Some((1, 4)),
            _ => None,
        };
        let width = self.get(raw::TJPARAM_TJPARAM_JPEGWIDTH);
        let height = self.get(raw::TJPARAM_TJPARAM_JPEGHEIGHT);
        let side = |side: c_int| u32::try_from(side).ok().filter(|&side| side > 0);
        Some(Header {
            width: side(width)?,
            height: side(height)?,
            grey,
            progressive: self.get(raw::TJPARAM_TJPARAM_PROGRESSIVE) != 0,
            chroma_step,
        })
    }

    /// Decodes `jpeg` at `eighths`/8 of its size into `pixels`, rows of
    /// `channels` bytes a pixel (1 for grey, 3 for RGB) with nothing between
    /// them. `pixels` must be exactly the decoded image's size.
    ///
    /// `Ok` carries libjpeg's warning when it gave one: the data was cut short
    /// or corrupt and libjpeg filled it in, down to the last row. `Err` is a
    /// fatal error, after a warning or not, or a `pixels` of another size.
    fn decompress(
        &mut self,
        jpeg: &[u8],
        eighths: u32,
        pixels: &mut [u8],
        channels: usize,
    ) -> Result<Option<String>, String> {
        let format = match channels {
            1 => raw::TJPF_TJPF_GRAY,
            3 => raw::TJPF_TJPF_RGB,
            _ => return Err(format!("no pixel format of {channels} channels")),
        };
        // TurboJPEG takes a scale as the fraction in its lowest terms.
        let reduced = eighths.trailing_zeros().min(3);
        let factor = raw::tjscalingfactor {
            num: c_int::try_from(eighths >> reduced).map_err(|e| e.to_string())?,
            denom: 8 >> reduced,
        };
        // SAFETY: the handle is live, and the factor is passed by value.
        if unsafe { raw::tj3SetScalingFactor(self.0, factor) } != 0 {
            return Err(self.message());
        }
        // The size TurboJPEG is about to write, from the same bytes.
        let header = self
            .read_header(jpeg)
            .ok_or_else(|| "not a JPEG this decodes".to_owned())?;
        let width = scaled_side(header.width, eighths) as usize;
        let height = scaled_side(header.height, eighths) as usize;
        let row = width * channels;
        if pixels.len() != row * height {
            return Err(format!(
                "a buffer of {} bytes for {width}x{height} pixels of {channels}",
                pixels.len()
            ));
        }
        let (Ok(length), Ok(pitch)) = (raw::size_t::try_from(jpeg.len()), c_int::try_from(row))
        else {
            return Err("too large for TurboJPEG".to_owned());
        };
        let last_row = pixels.len() - row..;
        for (byte, mark) in pixels[last_row.clone()].iter_mut().zip(unwritten_row(row)) {
            *byte = mark;
        }
        // SAFETY: the handle is live. TurboJPEG reads `length` bytes of
        // `jpeg`. It writes `height` rows of `pitch` bytes: the header's size
        // scaled by the factor just set (`TJSCALED`, which `scaled_side` is),
        // in a format of `channels` bytes a pixel, and `pixels` was checked
        // above to be exactly that long.
        let failed = unsafe {
            raw::tj3Decompress8(
                self.0,
                jpeg.as_ptr(),
                length,
                pixels.as_mut_ptr(),
                pitch,
                format,
            )
        } != 0;
        if !failed {
            return Ok(None);
        }
        let warned = self.warned();
        let message = self.message();
        // libjpeg writes the rows in order, so a fatal error after the warning
        // left the last one as it was marked. (A decode that ends on exactly
        // the mark is taken for a failure too, and decoded again by the
        // `image` crate.)
        if warned && !pixels[last_row].iter().copied().eq(unwritten_row(row)) {
            Ok(Some(message))
        } else {
            Err(message)
        }
    }
}

impl Drop for Decompressor {
    fn drop(&mut self) {
        // SAFETY: the handle is the live instance `new` created, and nothing
        // uses it after this.
        unsafe { raw::tj3Destroy(self.0) };
    }
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

    /// The file the split-scan bug was found with (gl-1213): a baseline 4:2:0
    /// JPEG in three single-component scans.
    const SPLIT_SCAN_JPEG: &[u8] = include_bytes!("../tests/fixtures/split_scans_420.jpg");

    /// [`ScalableJpeg::open`] under the default limits, for a file they hold.
    fn open(path: &str) -> Option<ScalableJpeg> {
        ScalableJpeg::open(path, &ImageLimits::default()).expect("under the limits")
    }

    /// What the segments of `jpeg` say, when it has a scan.
    fn segments(jpeg: &[u8]) -> Option<Segments> {
        match Segments::read(jpeg) {
            Walk::Scan(found) => Some(found),
            Walk::Short | Walk::NoScan => None,
        }
    }

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
        let jpeg = open(&path).expect("a baseline JPEG opens");
        assert_eq!(jpeg.dimensions(), (1600, 1200));
        let image = jpeg
            .decode_at_least(128, 96)
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

    /// The bytes of a `width`×`height` JPEG of `color` written the way cameras
    /// and most encoders do, with subsampled chroma: progressive, or with
    /// optimized Huffman tables (which `jpeg-encoder` writes as one scan per
    /// component), or neither.
    fn camera_jpeg_bytes(
        (width, height): (u16, u16),
        color: [u8; 3],
        sampling: jpeg_encoder::SamplingFactor,
        progressive: bool,
        optimized: bool,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut bytes, 90);
        encoder.set_sampling_factor(sampling);
        encoder.set_progressive(progressive);
        encoder.set_optimized_huffman_tables(optimized);
        encoder
            .encode(
                &color.repeat(usize::from(width) * usize::from(height)),
                width,
                height,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("write fixture");
        bytes
    }

    /// [`camera_jpeg_bytes`] with standard Huffman tables, written to
    /// `dir/name`.
    fn camera_jpeg(
        dir: &TempDir,
        name: &str,
        size: (u16, u16),
        color: [u8; 3],
        sampling: jpeg_encoder::SamplingFactor,
        progressive: bool,
    ) -> String {
        let path = dir.path().join(name);
        let bytes = camera_jpeg_bytes(size, color, sampling, progressive, false);
        std::fs::write(&path, bytes).expect("write fixture");
        path.to_string_lossy().into_owned()
    }

    /// Chroma-subsampled and progressive JPEGs, what cameras and phones write,
    /// decode at the covering scale with their colour intact: a quarter for
    /// 300x200, a half for 500x400.
    #[rstest]
    fn camera_jpegs_decode_at_the_covering_scale(
        #[values(
            (jpeg_encoder::SamplingFactor::F_2_2, false),
            (jpeg_encoder::SamplingFactor::F_2_2, true),
            (jpeg_encoder::SamplingFactor::F_2_1, false)
        )]
        (sampling, progressive): (jpeg_encoder::SamplingFactor, bool),
        // ceil(1601 * n / 8) x ceil(1203 * n / 8), as libjpeg rounds a scaled side.
        #[values(((300, 200), (401, 301)), ((500, 400), (801, 602)))] (minimum, decoded): (
            (u32, u32),
            (u32, u32),
        ),
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
        let image = open(&path)
            .expect("opens")
            .decode_at_least(minimum.0, minimum.1)
            .expect("decodes");
        assert_eq!((image.width(), image.height()), decoded);
        let pixel = image.to_rgb8().get_pixel(200, 150).0;
        for (got, want) in pixel.into_iter().zip(color) {
            assert!(got.abs_diff(want) <= 3, "{pixel:?} for {color:?}");
        }
    }

    /// A JPEG whose data is cut short is drawn as far as it goes, as Skia
    /// draws it: its codec reports the file as incomplete and
    /// `SkiaEncoder.Decode` takes the bitmap. libjpeg warns and fills the
    /// rest in.
    #[test]
    fn a_truncated_jpeg_is_drawn_as_far_as_it_goes() {
        let dir = TempDir::new().expect("tempdir");
        let color = [10, 200, 90];
        let path = camera_jpeg(
            &dir,
            "whole.jpg",
            (1600, 1200),
            color,
            jpeg_encoder::SamplingFactor::F_2_2,
            false,
        );
        let bytes = std::fs::read(&path).expect("read");
        std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("truncate");
        let image = open(&path)
            .expect("the header is intact")
            .decode_at_least(200, 150)
            .expect("the half that is there decodes");
        assert_eq!((image.width(), image.height()), (200, 150));
        let top = image.to_rgb8().get_pixel(100, 10).0;
        for (got, want) in top.into_iter().zip(color) {
            assert!(got.abs_diff(want) <= 3, "{top:?} for {color:?}");
        }
    }

    /// A warning that costs no pixels (here two bytes that belong to no
    /// segment, before the scan) gives the same image as the clean file.
    #[test]
    fn a_jpeg_libjpeg_warns_about_decodes_to_the_same_pixels() {
        let dir = TempDir::new().expect("tempdir");
        let clean = jpeg(&dir, "clean.jpg", 64, 48, &[200, 120, 40], None);
        let mut bytes = std::fs::read(&clean).expect("read");
        let scan = segment_offset(&bytes, SOS);
        bytes.splice(scan..scan, [0x12, 0x34]);
        let padded = dir.path().join("padded.jpg");
        std::fs::write(&padded, &bytes).expect("write");

        let mut turbo = Decompressor::new().expect("instance");
        let mut pixels = vec![0; 64 * 48 * 3];
        let warning = turbo
            .decompress(&bytes, FULL_SCALE, &mut pixels, 3)
            .expect("not fatal")
            .expect("a warning");
        assert!(warning.contains("extraneous bytes"), "{warning}");

        let decode = |path: &str| open(path).expect("opens").decode_full().expect("decodes");
        assert_eq!(
            decode(padded.to_str().expect("utf-8")).to_rgb8(),
            decode(&clean).to_rgb8()
        );
    }

    /// A fatal error is a failure, which the caller answers with the `image`
    /// crate's decode: here a scan that names a Huffman table the file never
    /// defines. It is one after a warning too (two bytes that belong to no
    /// segment, before the scan), which TurboJPEG reports as a warning alone.
    #[rstest]
    #[case::fatal(false)]
    #[case::fatal_after_a_warning(true)]
    fn a_jpeg_libjpeg_fails_on_is_left_to_the_image_crate(#[case] warned_first: bool) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("frame.jpg");
        let mut bytes = frame_header(64, 48);
        if warned_first {
            let scan = segment_offset(&bytes, SOS);
            bytes.splice(scan..scan, [0x12, 0x34]);
        }
        std::fs::write(&path, &bytes).expect("write fixture");

        let mut turbo = Decompressor::new().expect("instance");
        let mut pixels = vec![0; 64 * 48 * 3];
        let failed = turbo
            .decompress(&bytes, FULL_SCALE, &mut pixels, 3)
            .expect_err("fatal");
        assert_eq!(turbo.warned(), warned_first, "{failed}");

        let jpeg = open(path.to_str().expect("utf-8")).expect("the header reads");
        assert!(jpeg.decode_full().is_none());
    }

    /// The buffer has to be the decoded image's size exactly: the check that
    /// keeps the write inside it.
    #[rstest]
    #[case::too_short(64 * 48 * 3 - 1, 3)]
    #[case::too_long(64 * 48 * 3 + 1, 3)]
    #[case::of_another_format(64 * 48 * 3, 1)]
    #[case::of_no_format(64 * 48 * 2, 2)]
    fn a_buffer_of_another_size_is_refused(#[case] length: usize, #[case] channels: usize) {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "photo.jpg", 64, 48, &[1, 2, 3], None);
        let bytes = std::fs::read(&path).expect("read");
        let mut pixels = vec![0; length];
        let refused = Decompressor::new().expect("instance").decompress(
            &bytes,
            FULL_SCALE,
            &mut pixels,
            channels,
        );
        assert!(refused.is_err(), "{refused:?}");
        assert!(pixels.iter().all(|&byte| byte == 0), "nothing was written");
    }

    #[test]
    fn a_grayscale_jpeg_decodes_to_luma() {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "gray.jpg", 1024, 512, &[90], None);
        let image = open(&path)
            .expect("a grayscale JPEG opens")
            .decode_at_least(256, 128)
            .expect("a quarter covers 256x128");
        assert!(matches!(image, DynamicImage::ImageLuma8(_)));
        assert_eq!((image.width(), image.height()), (256, 128));
    }

    /// A half is the largest reduced scale: a target it does not cover decodes
    /// in full, and so does one larger than the image.
    #[rstest]
    #[case::a_half(800, 600, (800, 600))]
    #[case::one_pixel_more(801, 600, (1600, 1200))]
    #[case::larger_than_the_image(3200, 2400, (1600, 1200))]
    fn a_target_no_reduced_scale_covers_decodes_in_full(
        #[case] min_width: u32,
        #[case] min_height: u32,
        #[case] decoded: (u32, u32),
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = jpeg(&dir, "photo.jpg", 1600, 1200, &[1, 2, 3], None);
        let image = open(&path)
            .expect("opens")
            .decode_at_least(min_width, min_height)
            .expect("decodes");
        assert_eq!((image.width(), image.height()), decoded);
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
            open(&tagged).expect("opens").orientation(),
            Orientation::Rotate90
        );
        assert_eq!(
            open(&plain).expect("opens").orientation(),
            Orientation::NoTransforms
        );
    }

    /// gl-1213: a JPEG coded in one scan per component decodes to its colours
    /// as stored, with its EXIF orientation read for the caller to apply.
    #[test]
    fn a_split_scan_jpeg_keeps_its_colours_and_orientation() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("phone.jpg");
        // Stored landscape, red on the left and blue on the right, with
        // EXIF 6 (a quarter turn clockwise to stand it upright).
        let stored = RgbImage::from_fn(32, 16, |x, _| {
            image::Rgb(if x < 16 { [220, 30, 30] } else { [30, 30, 220] })
        });
        let mut encoder = jpeg_encoder::Encoder::new_file(&path, 90).expect("create");
        encoder.set_optimized_huffman_tables(true);
        encoder
            .add_exif_metadata(&ORIENTATION_6_EXIF)
            .expect("exif");
        encoder
            .encode(stored.as_raw(), 32, 16, jpeg_encoder::ColorType::Rgb)
            .expect("encode");
        let jpeg = open(path.to_str().expect("utf-8")).expect("opens");
        assert_eq!(jpeg.orientation(), Orientation::Rotate90);
        let stored = jpeg.decode_full().expect("decodes").to_rgb8();
        assert_eq!(stored.dimensions(), (32, 16));
        let [left, right] = [stored.get_pixel(4, 8).0, stored.get_pixel(28, 8).0];
        assert!(left[0] > 180 && left[2] < 80, "left {left:?}");
        assert!(right[2] > 180 && right[0] < 80, "right {right:?}");
    }

    /// A frame header and a scan header with no tables or data after them:
    /// `width`×`height`, 8-bit, three components at 4:2:0. Enough for a header
    /// read, and nothing a decode can use.
    fn frame_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, SOI, 0xFF, SOF0, 0x00, 0x11, 0x08];
        bytes.extend(height.to_be_bytes());
        bytes.extend(width.to_be_bytes());
        bytes.extend([0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        bytes.extend([
            0xFF, SOS, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11,
        ]);
        bytes.extend([0x00, 0x3F, 0x00, 0xFF, EOI]);
        bytes
    }

    /// A size over the limits is refused from the header, with the limit it
    /// is over, whatever scale it would be decoded at.
    #[test]
    fn a_frame_over_the_limits_is_refused_with_the_limit() {
        let dir = TempDir::new().expect("tempdir");
        let open_frame = |name: &str, width: u16, height: u16, limits: &ImageLimits| {
            let path = dir.path().join(name);
            std::fs::write(&path, frame_header(width, height)).expect("write fixture");
            ScalableJpeg::open(path.to_str().expect("utf-8"), limits)
                .map(|jpeg| jpeg.expect("a JPEG").dimensions())
        };
        let refused_by = |opened: Result<(u32, u32), String>, setting: &str| {
            let limit = opened.expect_err("over the limit");
            assert!(limit.contains(setting), "{limit}");
        };
        let defaults = ImageLimits::default();
        // 20000x10000 RGB is 600 MB, over the 512 MiB default.
        refused_by(
            open_frame("huge.jpg", 20_000, 10_000, &defaults),
            "image_decode_max_memory_mb",
        );
        assert_eq!(
            open_frame("large.jpg", 10_000, 5_000, &defaults),
            Ok((10_000, 5_000))
        );
        // Over either limit it was given, whichever the file is under.
        let few_pixels = ImageLimits {
            max_pixels: 49_999_999,
            ..defaults
        };
        refused_by(
            open_frame("large.jpg", 10_000, 5_000, &few_pixels),
            "image_decode_max_megapixels",
        );
        let little_memory = ImageLimits {
            max_alloc: 149_999_999,
            ..defaults
        };
        refused_by(
            open_frame("large.jpg", 10_000, 5_000, &little_memory),
            "image_decode_max_memory_mb",
        );
    }

    /// A decode holds the whole file, so a file longer than the memory limit
    /// is refused, whatever its bitmap takes: here an 8x8 image of 192 bytes
    /// behind 60 KB of application data.
    #[test]
    fn a_file_longer_than_the_memory_limit_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let small = std::fs::read(jpeg(&dir, "small.jpg", 8, 8, &[1, 2, 3], None)).expect("read");
        let path = dir.path().join("padded.jpg");
        std::fs::write(&path, with_app1_before(&small, SOS, &vec![0; 60_000])).expect("write");
        let length = std::fs::metadata(&path).expect("metadata").len();
        let open_with = |max_alloc| {
            let limits = ImageLimits {
                max_alloc,
                ..ImageLimits::default()
            };
            ScalableJpeg::open(path.to_str().expect("utf-8"), &limits).map(|jpeg| jpeg.is_some())
        };
        assert_eq!(open_with(length), Ok(true));
        let refused = open_with(length - 1).expect_err("longer than the limit");
        assert!(
            refused.contains("the file takes") && refused.contains("image_decode_max_memory_mb"),
            "{refused}"
        );
    }

    /// `open` reads a file as far as its first scan, not to its end, and goes
    /// on reading while the segments before the scan do: here 150 KB of
    /// application data, past the first read. The decode then reads the rest.
    #[test]
    fn the_header_is_read_before_the_rest_of_the_file() {
        let dir = TempDir::new().expect("tempdir");
        // Detail, so the file is longer than what `open` reads at first.
        let photo = image::RgbImage::from_fn(1600, 1200, |x, y| {
            let level = |value: u32| u8::try_from(value % 256).expect("under 256");
            image::Rgb([level(x * 7 + y * 13), level(x * 3), level(y * 5)])
        });
        let mut detailed = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut detailed, 95)
            .write_image(&photo, 1600, 1200, image::ExtendedColorType::Rgb8)
            .expect("encode");
        let mut padded = detailed.clone();
        for _ in 0..3 {
            padded = with_app1_before(&padded, SOS, &vec![0; 50_000]);
        }
        for (name, bytes, read_at_most) in [
            ("detailed.jpg", &detailed, HEADER_READ_BYTES),
            ("padded.jpg", &padded, 4 * HEADER_READ_BYTES),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).expect("write");
            let jpeg = open(path.to_str().expect("utf-8")).expect("opens");
            assert_eq!(jpeg.dimensions(), (1600, 1200), "{name}");
            let read = jpeg.bytes.len() as u64;
            assert!(
                read <= read_at_most && read < bytes.len() as u64,
                "{name}: read {read} of {} bytes",
                bytes.len()
            );
            let decoded = jpeg.decode_full().expect("decodes").to_rgb8();
            let whole = turbojpeg_decode(&detailed);
            assert_eq!(decoded.as_raw(), &whole, "{name}");
        }
    }

    /// The RGB pixels of `jpeg`, decoded in full from memory.
    fn turbojpeg_decode(jpeg: &[u8]) -> Vec<u8> {
        let mut turbo = Decompressor::new().expect("instance");
        let header = turbo.read_header(jpeg).expect("a header");
        let mut pixels = vec![0; header.width as usize * header.height as usize * 3];
        assert_eq!(turbo.decompress(jpeg, FULL_SCALE, &mut pixels, 3), Ok(None));
        pixels
    }

    /// A JPEG coded in more than one scan takes the coefficients of its whole
    /// frame beside its bitmap; one coded in a single scan takes its bitmap
    /// alone. 200x200 at 4:2:0 pads to 208x208 luma samples, and half as many
    /// chroma samples again, at 2 bytes each.
    #[rstest]
    #[case::one_scan(false, false, 120_000)]
    #[case::progressive(true, false, 120_000 + 2 * (208 * 208 + 2 * 104 * 104))]
    #[case::a_scan_per_component(false, true, 120_000 + 2 * (208 * 208 + 2 * 104 * 104))]
    fn a_jpeg_in_several_scans_counts_its_coefficients(
        #[case] progressive: bool,
        #[case] optimized: bool,
        #[case] needed: u64,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("photo.jpg");
        let bytes = camera_jpeg_bytes(
            (200, 200),
            [200, 120, 40],
            jpeg_encoder::SamplingFactor::F_2_2,
            progressive,
            optimized,
        );
        std::fs::write(&path, bytes).expect("write fixture");
        let open_with = |max_alloc| {
            let limits = ImageLimits {
                max_alloc,
                ..ImageLimits::default()
            };
            ScalableJpeg::open(path.to_str().expect("utf-8"), &limits).map(|jpeg| jpeg.is_some())
        };
        assert_eq!(open_with(needed), Ok(true));
        let refused = open_with(needed - 1).expect_err("a byte short");
        assert!(refused.contains("image_decode_max_memory_mb"), "{refused}");
    }

    /// The count is what libjpeg-turbo holds: told to hold less
    /// (`TJPARAM_MAXMEMORY`, in whole MiB) it fails, and with between one and
    /// two MiB more, for its other buffers, it decodes. A JPEG in one scan
    /// holds no frame of coefficients, and decodes under 1 MiB.
    #[rstest]
    // 2 * (2048 * 1632 + 2 * 1024 * 816) bytes are 9.56 MiB.
    #[case::progressive_420((2048, 1632), jpeg_encoder::SamplingFactor::F_2_2, true, false, 9, 11)]
    // 2 * 3 * 1024 * 1104 bytes are 6.47 MiB.
    #[case::a_scan_per_component_444((1024, 1104), jpeg_encoder::SamplingFactor::F_1_1, false, true, 6, 8)]
    // 2 * (2048 * 1104 + 2 * 1024 * 1104) bytes are 8.63 MiB.
    #[case::progressive_422((2048, 1104), jpeg_encoder::SamplingFactor::F_2_1, true, false, 8, 10)]
    fn the_coefficient_count_is_what_libjpeg_turbo_holds(
        #[case] size: (u16, u16),
        #[case] sampling: jpeg_encoder::SamplingFactor,
        #[case] progressive: bool,
        #[case] optimized: bool,
        #[case] too_few_mib: c_int,
        #[case] enough_mib: c_int,
    ) {
        let decode_within = |bytes: &[u8], mebibytes: c_int| {
            let mut turbo = Decompressor::new().expect("instance");
            // SAFETY: the handle is live; the parameter takes an integer.
            let set = unsafe {
                raw::tj3Set(
                    turbo.0,
                    raw::TJPARAM_TJPARAM_MAXMEMORY.cast_signed(),
                    mebibytes,
                )
            };
            assert_eq!(set, 0);
            let mut pixels = vec![0; usize::from(size.0) * usize::from(size.1) * 3];
            turbo.decompress(bytes, FULL_SCALE, &mut pixels, 3)
        };
        let several_scans =
            camera_jpeg_bytes(size, [200, 120, 40], sampling, progressive, optimized);
        let header = Decompressor::new()
            .expect("instance")
            .read_header(&several_scans)
            .expect("a header");
        assert_eq!(header.progressive, progressive);
        assert!(header.progressive || segments(&several_scans).expect("a scan").split_scans);
        let counted = coefficient_bytes(&header);
        let mebibyte = 1 << 20;
        assert!(
            counted > u64::from(too_few_mib.unsigned_abs()) * mebibyte
                && counted + mebibyte < u64::from(enough_mib.unsigned_abs()) * mebibyte
                && counted + 2 * mebibyte > u64::from(enough_mib.unsigned_abs()) * mebibyte,
            "{counted} bytes between {too_few_mib} and {enough_mib} MiB"
        );
        let failed = decode_within(&several_scans, too_few_mib).expect_err("too little memory");
        assert!(
            failed.contains("emory") || failed.contains("acking store"),
            "{failed}"
        );
        assert_eq!(decode_within(&several_scans, enough_mib), Ok(None));

        let one_scan = camera_jpeg_bytes(size, [200, 120, 40], sampling, false, false);
        assert_eq!(decode_within(&one_scan, 1), Ok(None));
    }

    /// A grey JPEG holds one plane, padded to whole blocks; a sampling
    /// TurboJPEG has no name for is counted as three full planes padded to
    /// the largest MCU.
    #[test]
    fn coefficients_are_counted_for_every_sampling() {
        let header = |grey, chroma_step| Header {
            width: 100,
            height: 50,
            grey,
            progressive: true,
            chroma_step,
        };
        assert_eq!(coefficient_bytes(&header(true, None)), 2 * 104 * 56);
        assert_eq!(
            coefficient_bytes(&header(false, Some((1, 1)))),
            2 * 3 * 104 * 56
        );
        // 4:1:1: MCUs of 32x8, and a quarter of the luma for each chroma plane.
        assert_eq!(
            coefficient_bytes(&header(false, Some((4, 1)))),
            2 * (128 * 56 + 2 * 32 * 56)
        );
        assert_eq!(coefficient_bytes(&header(false, None)), 2 * 3 * 128 * 64);
    }

    /// Files libjpeg-turbo would not turn into the pixels the `image` crate
    /// gives are left to it: CMYK, anything that is not a JPEG, and a missing
    /// file (whose error the `image` crate's decode reports).
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
        let short = dir.path().join("short.jpg");
        std::fs::write(&short, [0xFF]).expect("write fixture");
        // A JPEG's first two bytes and nothing that leads to a scan, for
        // longer than the first read.
        let no_scan = dir.path().join("no-scan.jpg");
        std::fs::write(&no_scan, [&[0xFF, SOI][..], &vec![0x55; 200_000]].concat())
            .expect("write fixture");
        for path in [cmyk, png, short, no_scan, dir.path().join("missing.jpg")] {
            assert!(
                open(path.to_str().expect("utf-8")).is_none(),
                "{}",
                path.display()
            );
        }
    }

    /// The smallest scale still covering the target on both axes, in eighths;
    /// the full size when a half would fall short.
    #[rstest]
    #[case::blur_hash_of_a_24mp_photo(6000, 4000, 128, 85, 1)]
    #[case::exactly_an_eighth(1024, 1024, 128, 128, 1)]
    #[case::one_pixel_past_an_eighth(1024, 1024, 129, 129, 2)]
    #[case::taller_axis_decides(1000, 1500, 85, 128, 1)]
    #[case::rounds_up_like_the_decoder(1001, 1001, 126, 126, 1)]
    #[case::poster_to_a_quarter(2000, 3000, 400, 600, 2)]
    #[case::twice_a_poster_card(2000, 3000, 600, 900, 4)]
    #[case::twice_a_poster_view(2000, 3000, 1200, 1800, 8)]
    #[case::backdrop_to_a_half(3840, 2160, 1920, 1080, 4)]
    #[case::one_pixel_past_a_half(3840, 2160, 1921, 1080, 8)]
    #[case::larger_than_the_image(100, 100, 200, 200, 8)]
    #[case::other_axis_needs_more(1600, 1600, 100, 600, 4)]
    fn dct_scale_picks_the_smallest_covering_scale(
        #[case] width: u32,
        #[case] height: u32,
        #[case] min_width: u32,
        #[case] min_height: u32,
        #[case] expected: u32,
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

    /// A 32x16 grey JPEG from `jpeg-encoder`.
    fn small_jpeg(
        sampling: jpeg_encoder::SamplingFactor,
        optimized: bool,
        progressive: bool,
    ) -> Vec<u8> {
        camera_jpeg_bytes((32, 16), [90, 90, 90], sampling, progressive, optimized)
    }

    /// A 16x16 JPEG from the `image` crate's encoder (one interleaved scan).
    fn image_crate_file(color: image::ExtendedColorType, channels: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
            .write_image(&vec![90; 16 * 16 * channels], 16, 16, color)
            .expect("encode");
        bytes
    }

    /// `jpeg-encoder` writes one scan per component whenever its Huffman
    /// tables are optimized, as this crate's encoder asked before it moved to
    /// libjpeg-turbo (gl-1346); the reported file is one of those.
    #[rstest]
    #[case::subsampled(small_jpeg(jpeg_encoder::SamplingFactor::F_2_2, true, false))]
    #[case::full_chroma(small_jpeg(jpeg_encoder::SamplingFactor::F_1_1, true, false))]
    #[case::the_reported_file(SPLIT_SCAN_JPEG.to_vec())]
    fn a_first_scan_of_one_component_is_split(#[case] bytes: Vec<u8>) {
        assert!(segments(&bytes).expect("a scan").split_scans);
    }

    /// Files whose first scan codes every component, and files with no scan
    /// to read. (A progressive JPEG may start either way: TurboJPEG's header
    /// read tells it.)
    #[rstest]
    #[case::standard_tables_are_interleaved(small_jpeg(
        jpeg_encoder::SamplingFactor::F_2_2,
        false,
        false
    ))]
    #[case::image_crate_colour(image_crate_file(image::ExtendedColorType::Rgb8, 3))]
    #[case::grey_has_one_component(image_crate_file(image::ExtendedColorType::L8, 1))]
    #[case::a_png(b"\x89PNG\r\n\x1a\n".to_vec())]
    #[case::empty(Vec::new())]
    #[case::cut_before_the_scan(SPLIT_SCAN_JPEG[..100].to_vec())]
    #[case::a_length_shorter_than_itself(vec![0xFF, SOI, 0xFF, 0xE0, 0x00, 0x01])]
    #[case::a_frame_header_too_short(vec![0xFF, SOI, 0xFF, SOF0, 0x00, 0x04, 8, 0])]
    #[case::bytes_where_a_marker_belongs(vec![0xFF, SOI, 0x12, 0x34])]
    fn other_files_are_not_split(#[case] bytes: Vec<u8>) {
        let plain = Segments {
            orientation: Orientation::NoTransforms,
            split_scans: false,
        };
        assert!(segments(&bytes).is_none_or(|found| found == plain));
    }

    /// Fill bytes before a marker (T.81 B.1.1.2) are skipped, and so are the
    /// `TEM` and `RSTn` markers, which have no segment.
    #[test]
    fn fill_bytes_and_restart_markers_are_skipped() {
        let mut bytes = vec![0xFF, SOI, 0xFF, 0xFF, 0xFF, TEM, 0xFF, 0xD3];
        bytes.extend_from_slice(&SPLIT_SCAN_JPEG[2..]);
        assert!(segments(&bytes).expect("a scan").split_scans);
    }

    /// Bytes that belong to no segment are passed over, as libjpeg passes
    /// over them with a warning: a file with a scan per component is still
    /// told, and still counted with its coefficients.
    #[test]
    fn bytes_between_segments_are_passed_over() {
        let scan = segment_offset(SPLIT_SCAN_JPEG, SOS);
        let mut bytes = SPLIT_SCAN_JPEG.to_vec();
        bytes.splice(scan..scan, [0x12, 0x00, 0x34]);
        let frame = segment_offset(SPLIT_SCAN_JPEG, SOF0);
        bytes.splice(frame..frame, [0x56, 0xFF, 0x00, 0x78]);
        assert!(segments(&bytes).expect("a scan").split_scans);

        let mut turbo = Decompressor::new().expect("instance");
        let mut pixels = vec![0; 64 * 64 * 3];
        let warning = turbo
            .decompress(&bytes, FULL_SCALE, &mut pixels, 3)
            .expect("not fatal")
            .expect("a warning");
        assert!(warning.contains("extraneous bytes"), "{warning}");
    }

    /// An image's end, or a second start of one, before any scan: there is
    /// none to read on for. Bytes that run out are another matter.
    #[rstest]
    #[case::end_of_image(vec![0xFF, SOI, 0xFF, EOI], false)]
    #[case::second_start(vec![0xFF, SOI, 0xFF, SOI], false)]
    #[case::a_length_shorter_than_itself(vec![0xFF, SOI, 0xFF, 0xE0, 0x00, 0x01], true)]
    #[case::only_the_start(vec![0xFF, SOI], true)]
    #[case::a_segment_cut_short(vec![0xFF, SOI, 0xFF, 0xE0, 0x00, 0x10, 0x4A], true)]
    #[case::cut_before_the_scan(SPLIT_SCAN_JPEG[..100].to_vec(), true)]
    fn a_walk_tells_no_scan_from_too_few_bytes(#[case] bytes: Vec<u8>, #[case] short: bool) {
        match Segments::read(&bytes) {
            Walk::Short => assert!(short),
            Walk::NoScan => assert!(!short),
            Walk::Scan(found) => panic!("no scan in these bytes: {found:?}"),
        }
    }

    /// Where the first segment of `jpeg` with `marker` starts.
    fn segment_offset(jpeg: &[u8], marker: u8) -> usize {
        let mut at = 2; // past SOI
        while jpeg[at + 1] != marker {
            assert_eq!(jpeg[at], 0xFF, "a marker at {at}");
            at += 2 + usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        }
        at
    }

    /// `jpeg` with an APP1 segment holding `body` put in before the segment
    /// whose marker is `before`.
    fn with_app1_before(jpeg: &[u8], before: u8, body: &[u8]) -> Vec<u8> {
        let at = segment_offset(jpeg, before);
        let length = u16::try_from(2 + body.len()).expect("a small segment");
        let mut out = jpeg[..at].to_vec();
        out.extend([0xFF, APP1]);
        out.extend(length.to_be_bytes());
        out.extend(body);
        out.extend(&jpeg[at..]);
        out
    }

    /// The orientation is read from any EXIF segment before the first scan,
    /// one after the frame header included, and the last one decides, as in
    /// zune-jpeg: an EXIF segment without an orientation after one with it
    /// leaves the image as stored. Another kind of APP1 segment (XMP) and an
    /// EXIF header with nothing after it are passed over.
    #[test]
    fn the_last_exif_segment_before_the_scan_decides_the_orientation() {
        let plain = image_crate_file(image::ExtendedColorType::Rgb8, 3);
        let exif = [b"Exif\0\0".as_slice(), &ORIENTATION_6_EXIF].concat();
        // A TIFF header and an IFD with no entries.
        let no_orientation = b"Exif\0\0II*\0\x08\0\0\0\0\0\0\0\0\0".as_slice();
        let orientation = |bytes: &[u8]| segments(bytes).expect("a scan").orientation;

        let after_frame = with_app1_before(&plain, SOS, &exif);
        assert_eq!(orientation(&after_frame), Orientation::Rotate90);
        let then_xmp = with_app1_before(&after_frame, SOS, b"http://ns.adobe.com/xap/1.0/\0<x/>");
        assert_eq!(orientation(&then_xmp), Orientation::Rotate90);
        let then_bare = with_app1_before(&after_frame, SOS, b"Exif\0\0");
        assert_eq!(orientation(&then_bare), Orientation::Rotate90);
        let then_none = with_app1_before(&after_frame, SOS, no_orientation);
        assert_eq!(orientation(&then_none), Orientation::NoTransforms);
        let none_then_tagged = with_app1_before(&then_none, SOS, &exif);
        assert_eq!(orientation(&none_then_tagged), Orientation::Rotate90);
    }
}
