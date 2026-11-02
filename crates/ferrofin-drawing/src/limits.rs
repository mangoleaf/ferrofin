//! The memory bounds of image decoding and resizing.
//!
//! Every decode and resize in this crate goes through `ImageMemory`, so all
//! of them run under one set of [`ImageLimits`]. The limits are settings
//! (`image_decode_max_megapixels`, `image_decode_max_memory_mb`,
//! `image_resize_budget_mb`): the server resolves them and hands them to
//! [`ImageCrateEncoder::with_limits`](crate::ImageCrateEncoder::with_limits).
//!
//! Upstream has no such bound (`SkiaEncoder.Decode` hands any file to Skia),
//! and its catch-all answers a failed allocation with the original image. In
//! Rust a failed allocation aborts the process, and under a container's memory
//! limit the kernel kills it first, so the size is checked before the memory
//! is asked for. A refusal is a [`ServiceError::InvalidInput`], which
//! the processor answers with the original file like any other failed encode.

use std::fs::File;
use std::io::BufReader;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use ferrofin_traits::error::ServiceError;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageReader};

use crate::error::DrawingError;

/// Bytes in a MiB, the unit of the memory settings.
const MIB: u64 = 1024 * 1024;

/// Pixels in a megapixel, the unit of the pixel setting.
const MEGAPIXEL: u64 = 1_000_000;

/// What the scratch buffer of a resize holds per pixel: `imageops::resize`
/// samples every input, whatever its colour type, into an `Rgba32FImage`.
const RESIZE_BYTES_PER_PIXEL: u64 = 16;

/// The bounds of image work, in the units the code compares against.
///
/// A file's header is enough to check the first two, so an image over them
/// costs no memory. The third covers what a Lanczos3 or Triangle resize
/// allocates on top of the decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// The most pixels (width × height) an image may have to be decoded.
    pub max_pixels: u64,
    /// The most bytes one decode may allocate, the `image` crate's `max_alloc`.
    pub max_alloc: u64,
    /// The most bytes of scratch memory the resizes in flight hold between
    /// them. A resize needs 16 bytes per pixel of the source's width by the
    /// output's height, whatever the image's format.
    pub resize_budget: u64,
}

impl ImageLimits {
    /// `image_decode_max_megapixels`'s default.
    pub const DEFAULT_DECODE_MEGAPIXELS: u32 = 200;
    /// The values `image_decode_max_megapixels` takes.
    pub const DECODE_MEGAPIXELS: RangeInclusive<u32> = 16..=1000;
    /// `image_decode_max_memory_mb`'s default, the `image` crate's `max_alloc`
    /// default.
    pub const DEFAULT_DECODE_MEMORY_MB: u32 = 512;
    /// The values `image_decode_max_memory_mb` takes.
    pub const DECODE_MEMORY_MB: RangeInclusive<u32> = 128..=2048;
    /// `image_resize_budget_mb`'s default.
    pub const DEFAULT_RESIZE_BUDGET_MB: u32 = 2048;
    /// The values `image_resize_budget_mb` takes. The ceiling is what one
    /// resize of a 1000 MP image can need.
    pub const RESIZE_BUDGET_MB: RangeInclusive<u32> = 256..=16_384;

    /// The limits from the settings' units: megapixels and MiB.
    #[must_use]
    pub fn from_settings(
        decode_megapixels: u32,
        decode_memory_mb: u32,
        resize_budget_mb: u32,
    ) -> Self {
        Self {
            max_pixels: u64::from(decode_megapixels) * MEGAPIXEL,
            max_alloc: u64::from(decode_memory_mb) * MIB,
            resize_budget: u64::from(resize_budget_mb) * MIB,
        }
    }

    /// What every reader of an image file runs under. The `image` crate limits
    /// each side but not their product, so [`Self::check_decode`] holds the
    /// pixel limit.
    pub(crate) fn decode_limits(&self) -> image::Limits {
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(self.max_alloc);
        limits
    }

    /// Whether a `width`×`height` image decoding to `bytes` bytes may be
    /// decoded; the error says which limit it is over.
    pub(crate) fn check_decode(&self, width: u32, height: u32, bytes: u64) -> Result<(), String> {
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.max_pixels {
            return Err(format!(
                "{width}x{height} is over the limit of {} pixels (image_decode_max_megapixels)",
                self.max_pixels
            ));
        }
        if bytes > self.max_alloc {
            return Err(format!(
                "{width}x{height} takes {bytes} bytes, over the limit of {} \
                 (image_decode_max_memory_mb)",
                self.max_alloc
            ));
        }
        Ok(())
    }
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self::from_settings(
            Self::DEFAULT_DECODE_MEGAPIXELS,
            Self::DEFAULT_DECODE_MEMORY_MB,
            Self::DEFAULT_RESIZE_BUDGET_MB,
        )
    }
}

/// The decodes and resizes of one encoder, under its [`ImageLimits`].
///
/// A decode is bounded per image; concurrent ones are bounded by the encode
/// permits. A resize's budget is shared, because the permits follow the core
/// count and a budget per image would multiply by it: a resize that fits the
/// budget but not what the others hold waits for them, so what a client gets
/// never depends on the load. Resizes take the budget in the order they came,
/// so a large one is never passed over for ever. One holds an encode permit
/// while it waits, so at most that many wait, and it holds nothing else, so the
/// wait always ends.
#[derive(Debug)]
pub(crate) struct ImageMemory {
    limits: ImageLimits,
    /// Who holds how much of [`ImageLimits::resize_budget`].
    resizes: Mutex<ResizeQueue>,
    /// Signalled when the queue moves or bytes are given back.
    resizes_moved: Condvar,
}

/// The resizes in flight and waiting, in the order they came.
#[derive(Debug, Default)]
struct ResizeQueue {
    /// The bytes the resizes in flight hold.
    held: u64,
    /// The turn the next resize to arrive takes.
    next_turn: u64,
    /// The turn that takes its bytes next.
    serving: u64,
}

impl ImageMemory {
    pub(crate) fn new(limits: ImageLimits) -> Self {
        Self {
            limits,
            resizes: Mutex::default(),
            resizes_moved: Condvar::new(),
        }
    }

    /// The limits this runs under.
    pub(crate) fn limits(&self) -> &ImageLimits {
        &self.limits
    }

    /// A reader of the image at `path`, its format guessed from the content,
    /// under [`ImageLimits::decode_limits`]. Reading the size through it
    /// decodes nothing, so an image over the limits still reports its size.
    pub(crate) fn reader(&self, path: &str) -> Result<ImageReader<BufReader<File>>, ServiceError> {
        let mut reader = ImageReader::open(path)
            .map_err(|e| DrawingError::io(format!("open {path}"), e))?
            .with_guessed_format()
            .map_err(|e| DrawingError::io(format!("probe {path}"), e))?;
        reader.limits(self.limits.decode_limits());
        Ok(reader)
    }

    /// The decoder of the image at `path`, its header read and checked.
    ///
    /// `ImageReader::decode` counts the output against `max_alloc`, and
    /// `ImageReader::into_decoder` does not: only the GIF decoder tracks its
    /// own buffers, so without this a decoder would fill any size the header
    /// names. This checks it for every decode, whichever way it reads the file.
    fn decoder(&self, path: &str) -> Result<impl ImageDecoder + use<>, ServiceError> {
        if !Path::new(path).exists() {
            return Err(ServiceError::not_found(format!(
                "image file not found: {path}"
            )));
        }
        let mut decoder = self
            .reader(path)?
            .into_decoder()
            .map_err(|e| undecodable(path, &e))?;
        let (width, height) = decoder.dimensions();
        let bytes = decoder.total_bytes();
        self.limits
            .check_decode(width, height, bytes)
            .map_err(|limit| ServiceError::invalid_input(format!("decode {path}: {limit}")))?;
        let mut remaining = self.limits.decode_limits();
        remaining
            .reserve(bytes)
            .map_err(|e| undecodable(path, &e))?;
        decoder
            .set_limits(remaining)
            .map_err(|e| undecodable(path, &e))?;
        Ok(decoder)
    }

    /// Decodes the image at `path`, as stored. A missing file is
    /// [`ServiceError::NotFound`]; an undecodable one, or one over the limits,
    /// is [`ServiceError::InvalidInput`] (the port of Skia's
    /// `InvalidDataException`/`return null` fallbacks).
    pub(crate) fn decode(&self, path: &str) -> Result<DynamicImage, ServiceError> {
        let decoder = self.decoder(path)?;
        DynamicImage::from_decoder(decoder).map_err(|e| undecodable(path, &e))
    }

    /// `SkiaEncoder.GetBitmap(path, autoOrient: true, …)`: decodes `path` and
    /// applies its own EXIF origin (`codec.EncodedOrigin` → `OrientImage`), so
    /// the pixels stand as the photo was taken. A file whose orientation
    /// cannot be read is taken as stored.
    pub(crate) fn decode_oriented(&self, path: &str) -> Result<DynamicImage, ServiceError> {
        let mut decoder = self.decoder(path)?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let mut image = DynamicImage::from_decoder(decoder).map_err(|e| undecodable(path, &e))?;
        image.apply_orientation(orientation);
        Ok(image)
    }

    /// `image.resize_exact`, or [`ServiceError::InvalidInput`] when its
    /// scratch buffer alone is over the resize budget. A resize the budget
    /// holds waits here for any others it does not fit beside. A copy has no
    /// scratch buffer, so it waits for nothing.
    pub(crate) fn resize_exact(
        &self,
        image: &DynamicImage,
        width: u32,
        height: u32,
        filter: FilterType,
    ) -> Result<DynamicImage, ServiceError> {
        let scratch = resize_scratch_bytes((image.width(), image.height()), (width, height));
        if scratch > self.limits.resize_budget {
            return Err(ServiceError::invalid_input(format!(
                "cannot resize {}x{} to {width}x{height}: it takes {scratch} bytes, over the \
                 budget of {} (image_resize_budget_mb)",
                image.width(),
                image.height(),
                self.limits.resize_budget
            )));
        }
        let _held = (scratch > 0).then(|| self.hold(scratch));
        Ok(image.resize_exact(width, height, filter))
    }

    /// Takes `bytes` of the resize budget, waiting for this resize's turn and
    /// for them to be free. `bytes` is at most the budget, so that always
    /// happens.
    fn hold(&self, bytes: u64) -> HeldScratch<'_> {
        let mut queue = self.lock_resizes();
        let turn = queue.next_turn;
        queue.next_turn += 1;
        while queue.serving != turn || queue.held + bytes > self.limits.resize_budget {
            queue = self
                .resizes_moved
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
        queue.serving += 1;
        queue.held += bytes;
        // The next in line may fit beside this one.
        self.resizes_moved.notify_all();
        HeldScratch {
            memory: self,
            bytes,
        }
    }

    fn lock_resizes(&self) -> MutexGuard<'_, ResizeQueue> {
        self.resizes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for ImageMemory {
    fn default() -> Self {
        Self::new(ImageLimits::default())
    }
}

/// Bytes of the resize budget a resize holds, given back when it ends.
struct HeldScratch<'a> {
    memory: &'a ImageMemory,
    bytes: u64,
}

impl Drop for HeldScratch<'_> {
    fn drop(&mut self) {
        self.memory.lock_resizes().held -= self.bytes;
        self.memory.resizes_moved.notify_all();
    }
}

/// The scratch buffer of resizing `source` to `target`, both width by height:
/// its vertical pass, the source's width by the output's height. The same size
/// is a plain copy, which builds none.
fn resize_scratch_bytes(source: (u32, u32), target: (u32, u32)) -> u64 {
    if source == target {
        return 0;
    }
    u64::from(source.0)
        .saturating_mul(u64::from(target.1))
        .saturating_mul(RESIZE_BYTES_PER_PIXEL)
}

/// The error of a file the decoder refuses or fails on.
fn undecodable(path: &str, error: &ImageError) -> ServiceError {
    ServiceError::invalid_input(format!("decode {path}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[rstest]
    #[case(500, 400, 800_000, Ok(()))]
    #[case(1000, 1000, 1_000_000, Ok(()))]
    #[case(1000, 1001, 1_000, Err("image_decode_max_megapixels"))]
    #[case(100_000_000, 1, 100, Err("image_decode_max_megapixels"))]
    #[case(1000, 1000, 2_000_001, Err("image_decode_max_memory_mb"))]
    fn a_decode_is_checked_against_both_limits(
        #[case] width: u32,
        #[case] height: u32,
        #[case] bytes: u64,
        #[case] expected: Result<(), &str>,
    ) {
        let limits = ImageLimits {
            max_pixels: 1_000_000,
            max_alloc: 2_000_000,
            resize_budget: 0,
        };
        let checked = limits.check_decode(width, height, bytes);
        match expected {
            Ok(()) => assert_eq!(checked, Ok(())),
            Err(setting) => {
                let message = checked.expect_err("over a limit");
                assert!(message.contains(setting), "{message}");
            }
        }
    }

    /// A panorama is long, so the pixel limit is on the product, never a side.
    #[test]
    fn the_limits_name_no_side() {
        let limits = ImageLimits::default().decode_limits();
        assert_eq!(limits.max_image_width, None);
        assert_eq!(limits.max_image_height, None);
        assert_eq!(limits.max_alloc, Some(512 * MIB));
    }

    #[test]
    fn the_defaults_are_the_settings_defaults() {
        assert_eq!(
            ImageLimits::default(),
            ImageLimits {
                max_pixels: 200_000_000,
                max_alloc: 512 * MIB,
                resize_budget: 2048 * MIB,
            }
        );
        for (default, range) in [
            (
                ImageLimits::DEFAULT_DECODE_MEGAPIXELS,
                ImageLimits::DECODE_MEGAPIXELS,
            ),
            (
                ImageLimits::DEFAULT_DECODE_MEMORY_MB,
                ImageLimits::DECODE_MEMORY_MB,
            ),
            (
                ImageLimits::DEFAULT_RESIZE_BUDGET_MB,
                ImageLimits::RESIZE_BUDGET_MB,
            ),
        ] {
            assert!(range.contains(&default), "{default} outside {range:?}");
        }
    }

    #[rstest]
    // Width of the source by height of the output, 16 bytes each.
    #[case((100, 100), (100, 50), 80_000)]
    #[case((100, 100), (10, 50), 80_000)]
    #[case((100, 100), (50, 10), 16_000)]
    // The output's height, not the source's, even when enlarging.
    #[case((4, 4), (300, 450), 28_800)]
    // The same size is a copy.
    #[case((100, 100), (100, 100), 0)]
    fn a_resize_needs_the_source_width_by_the_output_height(
        #[case] source: (u32, u32),
        #[case] target: (u32, u32),
        #[case] bytes: u64,
    ) {
        assert_eq!(resize_scratch_bytes(source, target), bytes);
    }

    #[test]
    fn the_scratch_size_saturates() {
        assert_eq!(resize_scratch_bytes((u32::MAX, 1), (1, u32::MAX)), u64::MAX);
    }

    fn memory_with_budget(resize_budget: u64) -> Arc<ImageMemory> {
        Arc::new(ImageMemory::new(ImageLimits {
            resize_budget,
            ..ImageLimits::default()
        }))
    }

    #[test]
    fn a_resize_over_the_budget_is_refused_and_one_on_it_runs() {
        let image = DynamicImage::new_luma8(100, 100);
        let on_budget = memory_with_budget(80_000);
        let resized = on_budget
            .resize_exact(&image, 100, 50, FilterType::Triangle)
            .expect("the budget holds it");
        assert_eq!((resized.width(), resized.height()), (100, 50));
        assert_eq!(on_budget.lock_resizes().held, 0, "given back");

        let error = memory_with_budget(79_999)
            .resize_exact(&image, 100, 50, FilterType::Triangle)
            .expect_err("one byte over");
        assert!(
            matches!(&error, ServiceError::InvalidInput(message) if message.contains("image_resize_budget_mb")),
            "{error:?}"
        );
    }

    #[test]
    fn the_same_size_needs_no_budget() {
        let image = DynamicImage::new_luma8(100, 100);
        let resized = memory_with_budget(0)
            .resize_exact(&image, 100, 100, FilterType::Lanczos3)
            .expect("a copy");
        assert_eq!((resized.width(), resized.height()), (100, 100));
    }

    /// Waits until `turns` resizes have arrived, so a test knows their order.
    fn wait_for_turns(memory: &ImageMemory, turns: u64) {
        while memory.lock_resizes().next_turn < turns {
            std::thread::yield_now();
        }
    }

    /// The budget is shared: what one resize holds is not there for another,
    /// which waits rather than failing, and goes on once it is given back.
    #[test]
    fn a_resize_waits_for_the_budget_the_others_hold() {
        let memory = memory_with_budget(100);
        let held = memory.hold(70);
        let finished = Arc::new(AtomicBool::new(false));
        let waiter = std::thread::spawn({
            let (memory, finished) = (Arc::clone(&memory), Arc::clone(&finished));
            move || {
                drop(memory.hold(40));
                finished.store(true, Ordering::SeqCst);
            }
        });

        wait_for_turns(&memory, 2);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!finished.load(Ordering::SeqCst), "70 + 40 is over 100");

        drop(held);
        waiter.join().expect("the waiter ends");
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(memory.lock_resizes().held, 0);
    }

    /// Resizes take the budget in the order they came: a small one that would
    /// fit now waits behind a large one that does not, so a stream of small
    /// resizes can never starve it. The two do not fit together either, so the
    /// small one records itself only after the large one gives its bytes back.
    #[test]
    fn a_large_resize_is_not_passed_over_by_a_small_one() {
        let memory = memory_with_budget(100);
        let held = memory.hold(60);
        let order = Arc::new(Mutex::new(Vec::new()));
        let resize = |bytes| {
            let (memory, order) = (Arc::clone(&memory), Arc::clone(&order));
            std::thread::spawn(move || {
                let _held = memory.hold(bytes);
                order.lock().unwrap().push(bytes);
            })
        };

        let large = resize(70);
        wait_for_turns(&memory, 2);
        let small = resize(40);
        wait_for_turns(&memory, 3);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            order.lock().unwrap().is_empty(),
            "40 would fit, but came second"
        );

        drop(held);
        large.join().expect("the large resize ends");
        small.join().expect("the small resize ends");
        assert_eq!(*order.lock().unwrap(), [70, 40]);
    }

    /// Two resizes that fit together run together: the turn order costs no
    /// concurrency when the budget has room.
    #[test]
    fn resizes_that_fit_together_run_together() {
        let memory = memory_with_budget(100);
        let first = memory.hold(50);
        let second = memory.hold(50);
        assert_eq!(memory.lock_resizes().held, 100);
        drop((first, second));
    }

    /// A copy needs no scratch buffer, so it does not queue behind a resize.
    #[test]
    fn a_copy_does_not_wait_for_the_budget() {
        let memory = memory_with_budget(100);
        let _held = memory.hold(100);
        let image = DynamicImage::new_luma8(10, 10);
        memory
            .resize_exact(&image, 10, 10, FilterType::Lanczos3)
            .expect("a copy");
        assert_eq!(memory.lock_resizes().next_turn, 1, "took no turn");
    }

    /// A panicking resize still gives its bytes back, so it cannot wedge the
    /// resizes after it.
    #[test]
    fn the_budget_survives_a_panicking_resize() {
        let memory = memory_with_budget(100);
        let panicking = std::thread::spawn({
            let memory = Arc::clone(&memory);
            move || {
                let _held = memory.hold(100);
                panic!("resize failed");
            }
        });
        assert!(panicking.join().is_err());
        drop(memory.hold(100));
    }

    /// A PNG of only a signature, an `IHDR` declaring `width`×`height` 8-bit
    /// RGBA, an `IDAT` holding a bare zlib header, and `IEND`: a few dozen
    /// bytes that claim any size, the shape of a decode bomb. The limits are
    /// checked before pixel data is inflated, so no pixels are needed.
    fn header_only_png(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
            out.extend_from_slice(&u32::try_from(data.len()).expect("len").to_be_bytes());
            let start = out.len();
            out.extend_from_slice(&kind);
            out.extend_from_slice(data);
            let crc = crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut out, *b"IHDR", &ihdr);
        // A zlib header and nothing after it: enough for the PNG header read to
        // succeed, after which the limits judge the declared size.
        chunk(&mut out, *b"IDAT", &[0x78, 0x9c]);
        chunk(&mut out, *b"IEND", &[]);
        out
    }

    /// A tiny PNG declaring more than the default limits allow is refused by
    /// both decodes, from its header and not on its missing pixel data.
    /// 15000² is 225 MP, over the 200 MP default; 13000² is 169 MP, under it,
    /// but 676 MB as RGBA, over the 512 MiB default.
    #[rstest]
    #[case::over_the_pixel_limit(15_000, 15_000, "image_decode_max_megapixels")]
    #[case::over_the_memory_limit(13_000, 13_000, "image_decode_max_memory_mb")]
    fn a_header_alone_is_refused_at_the_default_limits(
        #[case] width: u32,
        #[case] height: u32,
        #[case] setting: &str,
    ) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bomb.png");
        std::fs::write(&path, header_only_png(width, height)).expect("write");
        let path = path.to_str().expect("utf8");
        let memory = ImageMemory::new(ImageLimits::default());
        for result in [memory.decode(path), memory.decode_oriented(path)] {
            match result {
                Err(ServiceError::InvalidInput(message)) => {
                    assert!(message.contains(setting), "{message}");
                }
                other => panic!("expected a limit error, got {other:?}"),
            }
        }
    }

    /// The same header-only file at a small size fails on its missing pixel
    /// data, not on a limit: the control for the test above.
    #[test]
    fn a_small_header_alone_is_not_a_limit_error() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("small.png");
        std::fs::write(&path, header_only_png(64, 64)).expect("write");
        let memory = ImageMemory::new(ImageLimits::default());
        match memory.decode(path.to_str().expect("utf8")) {
            Err(ServiceError::InvalidInput(message)) => {
                assert!(!message.contains("image_decode_max"), "{message}");
            }
            other => panic!("expected a decode error, got {other:?}"),
        }
    }
}
