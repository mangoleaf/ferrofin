//! gl-1015, gl-1056: a blur hash, a thumbnail, a collage or the splashscreen
//! drawn from a large JPEG never builds its full-resolution bitmap.
//!
//! A photo library uses each photo as its own Primary image, so a scan
//! blur-hashes every photo, draws the library's tile from them, and the
//! client then asks for thumbnails of them. Decoding a 24 MP photo in full
//! costs about 72 MB for a 128 px result. The encoder decodes a JPEG at the
//! smallest DCT scale that still covers its target, which this test sees
//! through the heap: a counting global allocator records the peak while each
//! call runs.
//!
//! This binary holds one test, because the allocator counts every thread of
//! the process and a second test running beside it would show in the peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use ferrofin_drawing::ImageCrateEncoder;
use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions};
use image::{Rgb, RgbImage};

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

/// Writes a `width`×`height` baseline 4:2:0 JPEG with photo-like detail, as a
/// camera writes it.
fn write_photo(path: &std::path::Path, width: u16, height: u16) {
    let pixels = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
        // Truncation is the point: wrap the coordinates into channel values.
        #[allow(clippy::cast_possible_truncation)]
        Rgb([
            (x % 251) as u8,
            (y % 241) as u8,
            ((x / 7 + y / 5) % 256) as u8,
        ])
    });
    let mut encoder = jpeg_encoder::Encoder::new_file(path, 85).expect("create fixture");
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
    encoder
        .encode(pixels.as_raw(), width, height, jpeg_encoder::ColorType::Rgb)
        .expect("write fixture");
}

#[tokio::test(flavor = "current_thread")]
async fn a_large_jpeg_is_never_decoded_at_full_size() {
    const WIDTH: u16 = 2400;
    const HEIGHT: u16 = 1600;
    // The bitmap a full decode builds: 3 bytes a pixel.
    const FULL_BITMAP: usize = WIDTH as usize * HEIGHT as usize * 3;

    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("photo.jpg");
    write_photo(&input, WIDTH, HEIGHT);
    let input = input.to_str().expect("utf-8 path").to_owned();
    let output = dir.path().join("thumb.jpg");
    let output = output.to_str().expect("utf-8 path").to_owned();
    let encoder = ImageCrateEncoder::new().with_data_path(dir.path());

    // The scan's blur hash: a 128×85 target, decoded at 1/8 (300×200).
    let baseline = start_peak();
    let hash = encoder
        .get_image_blur_hash(5, 4, &input)
        .await
        .expect("blur hash");
    let blur_hash_peak = peak_since(baseline);
    assert_eq!(hash.len(), 6 + (5 * 4 - 1) * 2, "blur hash {hash}");

    // A 200 px wide thumbnail: decoded at 1/4 (600x400), the smallest scale
    // covering twice its 200x133.
    let options = ImageProcessingOptions {
        max_width: Some(200),
        supported_output_formats: vec![ImageFormat::Jpg],
        ..Default::default()
    };
    let baseline = start_peak();
    let written = encoder
        .encode_image(
            &input,
            Utc::now(),
            &output,
            false,
            None,
            90,
            &options,
            ImageFormat::Jpg,
        )
        .await
        .expect("encode");
    let thumbnail_peak = peak_since(baseline);
    assert_eq!(written, output);
    assert_eq!(
        encoder.get_image_size(&output).await.expect("probe"),
        ImageDimensions::new(200, 133)
    );

    // A full decode alone would hold FULL_BITMAP; the scaled ones hold 1/64
    // (blur hash) and 1/16 (thumbnail) of it plus the resize and encode
    // buffers.
    for (what, peak) in [("blur hash", blur_hash_peak), ("thumbnail", thumbnail_peak)] {
        assert!(
            peak < FULL_BITMAP / 4,
            "{what} peaked at {peak} bytes, a full-size bitmap is {FULL_BITMAP}"
        );
    }

    collages_and_the_splashscreen_shrink_a_poster_from_a_reduced_decode(&encoder, dir.path()).await;
}

/// gl-1056: the collage cells and the splashscreen's tiles a 2000x3000 poster
/// is shrunk to are drawn from a reduced-scale decode of it.
async fn collages_and_the_splashscreen_shrink_a_poster_from_a_reduced_decode(
    encoder: &ImageCrateEncoder,
    dir: &std::path::Path,
) {
    const WIDTH: u16 = 2000;
    const HEIGHT: u16 = 3000;
    const FULL_BITMAP: usize = WIDTH as usize * HEIGHT as usize * 3;
    // The splashscreen's 5760x2160 RGBA wall, held while it decodes.
    const SPLASH_WALL: usize = 5760 * 2160 * 4;

    let poster = dir.join("poster.jpg");
    write_photo(&poster, WIDTH, HEIGHT);
    let poster = poster.to_str().expect("utf-8 path").to_owned();
    let collage = |name: &str, width: i32, height: i32| ImageCollageOptions {
        input_paths: vec![poster.clone()],
        output_path: dir.join(name).to_str().expect("utf-8 path").to_owned(),
        width,
        height,
    };

    // A 600x600 square collage (a playlist's or a box set's Primary): four
    // 300x300 cells, each decoded at 1/2 (1000x1500), the smallest scale
    // covering twice the cell (1/4, 500x750, is too narrow).
    let square = collage("square.png", 600, 600);
    let baseline = start_peak();
    encoder
        .create_image_collage(&square, None)
        .await
        .expect("square collage");
    let square_peak = peak_since(baseline);

    // A 240x135 thumb strip: the poster at the strip's width, 240x360,
    // decoded at 1/4 (500x750).
    let thumb = collage("thumb.png", 240, 135);
    let baseline = start_peak();
    encoder
        .create_image_collage(&thumb, Some("Movies"))
        .await
        .expect("thumb collage");
    let thumb_peak = peak_since(baseline);

    // Both are their size, and the poster was drawn into them: a source
    // that failed to load would leave the square's cells transparent and the
    // strip black.
    let square_image = image::open(&square.output_path).expect("png").to_rgba8();
    assert_eq!(square_image.dimensions(), (600, 600));
    for (x, y) in [(150, 150), (450, 150), (150, 450), (450, 450)] {
        assert_eq!(square_image.get_pixel(x, y).0[3], 0xFF, "cell at {x},{y}");
    }
    let thumb_image = image::open(&thumb.output_path).expect("png").to_rgba8();
    assert_eq!(thumb_image.dimensions(), (240, 135));
    assert_ne!(thumb_image.get_pixel(120, 67).0, [0, 0, 0, 0xFF]);

    // The splashscreen's 360 px rows: 240x360 tiles, decoded at 1/4.
    let baseline = start_peak();
    encoder
        .create_splashscreen(std::slice::from_ref(&poster), std::slice::from_ref(&poster))
        .await
        .expect("splashscreen");
    // Past its wall, the splashscreen's peak is the 1920x1080 RGBA frame it
    // projects the wall onto, which outweighs the reduced decode.
    let splash_peak = peak_since(baseline).saturating_sub(SPLASH_WALL);
    assert_eq!(
        encoder
            .get_image_size(dir.join("splashscreen.png").to_str().expect("utf-8 path"))
            .await
            .expect("probe"),
        ImageDimensions::new(1920, 1080)
    );

    // A full decode alone would hold FULL_BITMAP on top of the canvas.
    let over: Vec<String> = [
        ("square collage", square_peak),
        ("thumb collage", thumb_peak),
        ("splashscreen, past its wall", splash_peak),
    ]
    .into_iter()
    .filter(|&(_, peak)| peak >= FULL_BITMAP)
    .map(|(what, peak)| format!("{what} peaked at {peak} bytes"))
    .collect();
    assert!(
        over.is_empty(),
        "{over:?}; a full-size bitmap is {FULL_BITMAP}"
    );
}
