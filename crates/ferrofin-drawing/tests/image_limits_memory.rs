//! gl-1097: a refused image costs no memory, which is what makes it a limit.
//!
//! `image_limits.rs` checks that a path refuses an image; this checks that it
//! does so before allocating, through the heap: a counting global allocator
//! records the peak while each call runs. A limit that only stopped the work
//! afterwards would be no limit.
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
use image::GrayImage;

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

async fn encode(
    encoder: &ImageCrateEncoder,
    input: &str,
    output: &str,
    auto_orient: bool,
    height: i32,
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
                width: Some(i32::try_from(SIDE).expect("fits")),
                height: Some(height),
                ..Default::default()
            },
            ImageFormat::Jpg,
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_image_never_allocates_what_it_would_have_cost() {
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
            let refused = encode(limited, &input, &output, auto_orient, 100).await;
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

    // Past the resize budget: the 16 MB decode happens, and the 256 MB
    // scratch buffer (4000 × 3999 × 16 bytes) does not.
    let past_budget = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: 64 * 1024 * 1024,
        ..ImageLimits::default()
    });
    let baseline = start_peak();
    let refused = encode(&past_budget, &input, &output, false, 3999).await;
    let peak = peak_since(baseline);
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        peak < 32 * MIB,
        "past the budget: {} MiB allocated",
        peak / MIB
    );
}
