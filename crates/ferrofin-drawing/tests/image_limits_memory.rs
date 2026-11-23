//! gl-1097: a refused image costs no memory, which is what makes it a limit.
//!
//! `image_limits.rs` checks that a path refuses an image; this checks that it
//! does so before allocating, through the heap: a counting global allocator
//! records the peak while each call runs. A limit that only stopped the work
//! afterwards would be no limit. It also checks that a resize the budget holds
//! allocates no more than the budget counts for it (gl-1345).
//!
//! This binary holds one test, because the allocator counts every thread of
//! the process and a second test running beside it would show in the peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use ferrofin_drawing::{ImageCrateEncoder, ImageLimits};
use ferrofin_model::drawing::ImageFormat;
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::ImageProcessingOptions;
use image::{GrayImage, RgbaImage};

/// The system allocator, counting the bytes live now and the most live since
/// the last [`start_peak`].
struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded unchanged to `System`; the counters are
// only arithmetic on the sizes the caller passes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded with the pointer and layout the caller allocated.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded with the caller's pointer, layout and size.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                let grown = new_size - layout.size();
                let live = LIVE.fetch_add(grown, Ordering::SeqCst) + grown;
                PEAK.fetch_max(live, Ordering::SeqCst);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Starts a new peak window and returns the bytes live at its start.
fn start_peak() -> usize {
    let live = LIVE.load(Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
    live
}

/// The most bytes allocated on top of `baseline` since [`start_peak`].
fn peak_since(baseline: usize) -> usize {
    PEAK.load(Ordering::SeqCst).saturating_sub(baseline)
}

const MIB: usize = 1024 * 1024;

/// The fixture's side: one byte a pixel, so 16 MB decoded.
const SIDE: u32 = 4000;

/// Encodes `input` to a `width`×`height` JPEG.
async fn encode(
    encoder: &ImageCrateEncoder,
    input: &str,
    output: &str,
    auto_orient: bool,
    (width, height): (u32, u32),
) -> Result<String, ServiceError> {
    encoder
        .encode_image(
            input,
            Utc::now(),
            output,
            auto_orient,
            None,
            90,
            &ImageProcessingOptions {
                width: Some(i32::try_from(width).expect("fits")),
                height: Some(i32::try_from(height).expect("fits")),
                ..Default::default()
            },
            ImageFormat::Jpg,
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn the_limits_bound_what_an_image_allocates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.png");
    GrayImage::new(SIDE, SIDE).save(&input).expect("fixture");
    let input = input.to_str().expect("utf-8 path").to_owned();
    let output = dir.path().join("out.jpg");
    let output = output.to_str().expect("utf-8 path").to_owned();

    // Over the decode limits, through either decode: refused from the header,
    // so the 16 MB of pixels are never allocated.
    let over_memory = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_alloc: 8 * 1024 * 1024,
        ..ImageLimits::default()
    });
    let over_pixels = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_pixels: 8_000_000,
        ..ImageLimits::default()
    });
    for (limited, name) in [(&over_memory, "memory"), (&over_pixels, "pixels")] {
        for auto_orient in [false, true] {
            let baseline = start_peak();
            let refused = encode(limited, &input, &output, auto_orient, (SIDE, 100)).await;
            let peak = peak_since(baseline);
            assert!(
                refused.is_err(),
                "over {name}, auto_orient {auto_orient}: {refused:?}"
            );
            assert!(
                peak < MIB,
                "over {name}, auto_orient {auto_orient}: {} MiB allocated",
                peak / MIB
            );
        }
    }

    // Past the resize budget: the 16 MB decode happens, and the 16 MB first
    // pass (4000 × 3999, a byte each) and the 16 MB output do not, so the peak
    // stays under 24 MiB (about 15.5 measured), where either would pass 30.
    let past_budget = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: 8 * 1024 * 1024,
        ..ImageLimits::default()
    });
    let baseline = start_peak();
    let refused = encode(&past_budget, &input, &output, false, (SIDE - 1, SIDE - 1)).await;
    let peak = peak_since(baseline);
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        peak < 24 * MIB,
        "past the budget: {} MiB allocated",
        peak / MIB
    );

    // On the budget: an RGBA resize holds the premultiplied copy of the source
    // and the first pass, 4 bytes a pixel and one more pixel each, each side's
    // weights (a window of 9 per output pixel, 12 bytes a weight and 40 more)
    // and a row of accumulators (16 bytes a pixel), and on top of them only
    // the decoded source and the output are live.
    let side = SIDE / 2;
    let rgba = dir.path().join("rgba.png");
    RgbaImage::new(side, side).save(&rgba).expect("fixture");
    let rgba = rgba.to_str().expect("utf-8 path").to_owned();
    let pixels = |width: u32, height: u32| u64::from(width) * u64::from(height) + 1;
    let scratch = (pixels(side, side) + pixels(side, side - 1)) * 4
        + 2 * u64::from(side - 1) * (9 * 12 + 40)
        + u64::from(side) * 16;
    let decoded = u64::from(side).pow(2) * 4;
    let resized = u64::from(side - 1).pow(2) * 4;
    on_budget_peak_is_counted(
        &rgba,
        &output,
        (side - 1, side - 1),
        scratch,
        decoded + resized,
    )
    .await;

    // A long strip shrunk to 128 holds little image but wide weights: a window
    // of 2 * ceil(3 * 400000 / 128) + 1 = 18751 for each of 128 pixels, where
    // the first pass is 400001 bytes and the row of accumulators 1.6 MB.
    let strip = dir.path().join("strip.png");
    GrayImage::new(400_000, 2).save(&strip).expect("fixture");
    let strip = strip.to_str().expect("utf-8 path").to_owned();
    let scratch = 400_000 + 1 + 128 * (18_751 * 12 + 40) + (13 * 12 + 40) + 400_000 * 4;
    on_budget_peak_is_counted(&strip, &output, (128, 1), scratch, 800_000 + 128).await;
}

/// Encodes `input` at `size` under a budget of exactly `scratch`, and checks
/// that it peaks at no more than that, the decoded source and the output
/// (`images`). The decoder's and the encoder's own state is live only before
/// and after the resize, when less is.
async fn on_budget_peak_is_counted(
    input: &str,
    output: &str,
    size: (u32, u32),
    scratch: u64,
    images: u64,
) {
    let on_budget = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: scratch,
        ..ImageLimits::default()
    });
    let baseline = start_peak();
    encode(&on_budget, input, output, false, size)
        .await
        .expect("the budget holds it");
    let peak = u64::try_from(peak_since(baseline)).expect("fits");
    let counted = scratch + images;
    assert!(
        peak <= counted,
        "{input} on the budget: {peak} bytes allocated, {counted} counted"
    );
}
