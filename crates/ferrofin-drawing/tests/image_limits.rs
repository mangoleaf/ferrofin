//! gl-1097: every decode and every resize runs under the server's image limits.
//!
//! Each path that turns a file into pixels is checked here with limits sized to
//! small fixtures: past a limit the path does what it does with any image it
//! cannot decode. `image_limits_memory.rs` checks the allocation itself, and
//! `apps/ferrofin-server/tests/image_limits_cgroup.rs` a real server under a
//! memory cap.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use ferrofin_drawing::processor::ImageProcessor;
use ferrofin_drawing::{ImageCrateEncoder, ImageLimits};
use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
use ferrofin_traits::drawing::{ImageEncoder, ImageProcessor as _};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions, ItemImageInfo};
use image::{ImageFormat as CrateFormat, Rgb, RgbImage};
use rstest::rstest;
use tempfile::TempDir;

/// What the fixtures fit in: nothing a test does not name is the limit.
const ROOMY: ImageLimits = ImageLimits {
    max_pixels: 100_000_000,
    max_alloc: 1 << 30,
    resize_budget: 1 << 32,
};

/// The colour of [`Fixture::color`]'s images, which a collage can be read back for.
const SMALL_COLOR: [u8; 3] = [0x30, 0x60, 0x90];

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        Self(TempDir::new().expect("tempdir"))
    }

    fn path(&self, name: &str) -> String {
        self.0.path().join(name).to_string_lossy().into_owned()
    }

    /// Writes a `width`×`height` solid image, in the format `name`'s
    /// extension says, and returns its path.
    fn color(&self, name: &str, width: u32, height: u32) -> String {
        let path = self.path(name);
        RgbImage::from_pixel(width, height, Rgb(SMALL_COLOR))
            .save(&path)
            .expect("write fixture");
        path
    }
}

/// The options of a resize to `width`×`height`.
fn resize_to(width: i32, height: i32) -> ImageProcessingOptions {
    ImageProcessingOptions {
        width: Some(width),
        height: Some(height),
        ..Default::default()
    }
}

async fn encode(
    encoder: &ImageCrateEncoder,
    input: &str,
    output: &str,
    auto_orient: bool,
    options: &ImageProcessingOptions,
) -> Result<String, ServiceError> {
    encoder
        .encode_image(
            input,
            Utc::now(),
            output,
            auto_orient,
            None,
            90,
            options,
            ImageFormat::Jpg,
        )
        .await
}

/// `result` is a refusal naming `setting`.
fn assert_refused<T: std::fmt::Debug>(result: Result<T, ServiceError>, setting: &str) {
    let error = result.expect_err("over the limit");
    assert!(
        matches!(&error, ServiceError::InvalidInput(message) if message.contains(setting)),
        "{error:?}"
    );
}

/// 200×200 pixels, 120 KB as RGB. The plain decode and the auto-orienting
/// one read the file differently, and only one of them made the `image` crate
/// count the output against its allocation limit.
#[rstest]
#[tokio::test]
async fn an_image_over_a_decode_limit_is_refused_by_both_decodes(
    #[values("in.png", "in.jpg", "in.webp", "in.bmp", "in.gif", "in.ico")] name: &str,
    #[values(false, true)] auto_orient: bool,
    #[values(
        (ImageLimits { max_pixels: 39_999, ..ROOMY }, "image_decode_max_megapixels"),
        (ImageLimits { max_alloc: 100_000, ..ROOMY }, "image_decode_max_memory_mb"),
    )]
    limit: (ImageLimits, &str),
) {
    let fixture = Fixture::new();
    let input = fixture.color(name, 200, 200);
    let output = fixture.path("out.jpg");
    let encoder = ImageCrateEncoder::new().with_limits(limit.0);

    let encoded = encode(&encoder, &input, &output, auto_orient, &resize_to(50, 50)).await;

    assert_refused(encoded, limit.1);
    assert!(!Path::new(&output).exists());
}

#[rstest]
#[tokio::test]
async fn an_image_on_the_decode_limits_is_decoded(
    #[values("in.png", "in.jpg", "in.webp", "in.bmp")] name: &str,
    #[values(false, true)] auto_orient: bool,
) {
    let fixture = Fixture::new();
    let input = fixture.color(name, 200, 200);
    let output = fixture.path("out.jpg");
    let encoder = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_pixels: 40_000,
        max_alloc: 120_000,
        ..ROOMY
    });

    let encoded = encode(&encoder, &input, &output, auto_orient, &resize_to(50, 50)).await;

    assert_eq!(encoded.expect("on the limits"), output);
    let image = image::open(&output).expect("a JPEG");
    assert_eq!((image.width(), image.height()), (50, 50));
}

/// A JPEG takes the reduced-scale path first: it opens from the header alone
/// and never reaches the full decode when a smaller scale covers the target.
#[tokio::test]
async fn a_jpeg_over_a_limit_takes_no_reduced_scale_either() {
    let fixture = Fixture::new();
    let input = fixture.color("in.jpg", 800, 800);
    let output = fixture.path("out.jpg");
    let encoder = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_pixels: 639_999,
        ..ROOMY
    });

    assert_refused(
        encode(&encoder, &input, &output, false, &resize_to(50, 50)).await,
        "image_decode_max_megapixels",
    );
    assert_refused(
        encoder.get_image_blur_hash(4, 3, &input).await,
        "image_decode_max_megapixels",
    );
}

#[rstest]
#[tokio::test]
async fn a_blur_hash_runs_under_the_limits(
    #[values("in.png", "in.webp")] name: &str,
    #[values(
        (ImageLimits { max_pixels: 39_999, ..ROOMY }, "image_decode_max_megapixels"),
        (ImageLimits { max_alloc: 100_000, ..ROOMY }, "image_decode_max_memory_mb"),
    )]
    limit: (ImageLimits, &str),
) {
    let fixture = Fixture::new();
    let input = fixture.color(name, 200, 200);
    let encoder = ImageCrateEncoder::new().with_limits(limit.0);

    assert_refused(encoder.get_image_blur_hash(4, 3, &input).await, limit.1);
}

/// A blur hash resizes to a fit of 128 px, and the source's width is what its
/// scratch buffer depends on.
#[tokio::test]
async fn a_blur_hash_of_a_long_image_runs_under_the_budget() {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 20_000, 1);
    let scratch = 20_000 * 16;
    let refused = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: scratch - 1,
        ..ROOMY
    });
    assert_refused(
        refused.get_image_blur_hash(4, 3, &input).await,
        "image_resize_budget_mb",
    );

    let allowed = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: scratch,
        ..ROOMY
    });
    allowed
        .get_image_blur_hash(4, 3, &input)
        .await
        .expect("the budget holds it");
}

/// A resize needs 16 bytes for each pixel of the source's width by the
/// output's height, whatever the file's format.
#[rstest]
#[tokio::test]
async fn a_resize_past_the_budget_is_refused_and_one_on_it_runs(
    #[values(false, true)] auto_orient: bool,
) {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 200, 200);
    let output = fixture.path("out.jpg");
    let scratch = 200 * 199 * 16;
    let options = resize_to(200, 199);

    let past = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: scratch - 1,
        ..ROOMY
    });
    assert_refused(
        encode(&past, &input, &output, auto_orient, &options).await,
        "image_resize_budget_mb",
    );
    assert!(!Path::new(&output).exists());

    let on = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: scratch,
        ..ROOMY
    });
    assert_eq!(
        encode(&on, &input, &output, auto_orient, &options)
            .await
            .expect("the budget holds it"),
        output
    );
}

/// A format change at the same size is a copy, not a resize, so an image that
/// could never be resized can still change format.
#[tokio::test]
async fn the_same_size_needs_no_budget() {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 200, 200);
    let output = fixture.path("out.jpg");
    let encoder = ImageCrateEncoder::new().with_limits(ImageLimits {
        resize_budget: 0,
        ..ROOMY
    });

    encode(&encoder, &input, &output, false, &resize_to(200, 200))
        .await
        .expect("a copy");
}

/// The owner's rule: past a limit the processor serves the original, and
/// caches nothing.
#[rstest]
#[case::decode(ImageLimits { max_pixels: 39_999, ..ROOMY })]
#[case::resize(ImageLimits { resize_budget: 1000, ..ROOMY })]
#[tokio::test]
async fn the_processor_serves_the_original(#[case] limits: ImageLimits) {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 200, 200);
    let cache = fixture.0.path().join("cache");
    let encoder = ImageCrateEncoder::new().with_limits(limits);
    let processor = ImageProcessor::new(Arc::new(encoder), cache.as_path());

    let processed = processor
        .process_image(&ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                ..Default::default()
            },
            supported_output_formats: vec![ImageFormat::Jpg],
            quality: 90,
            ..resize_to(150, 149)
        })
        .await
        .expect("an image");

    assert_eq!(processed.path, input);
    assert_eq!(processed.mime_type.as_deref(), Some("image/png"));
    let cached = std::fs::read_dir(&cache).map_or(0, Iterator::count);
    assert_eq!(cached, 0, "nothing cached");
}

/// The processor still encodes what the limits hold.
#[tokio::test]
async fn the_processor_resizes_under_the_limits() {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 200, 200);
    let cache = fixture.0.path().join("cache");
    let processor = ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), cache.as_path());

    let processed = processor
        .process_image(&ImageProcessingOptions {
            image: ItemImageInfo {
                path: input.clone(),
                ..Default::default()
            },
            supported_output_formats: vec![ImageFormat::Jpg],
            quality: 90,
            ..resize_to(150, 149)
        })
        .await
        .expect("an image");

    assert_ne!(processed.path, input);
    let format = image::ImageReader::open(&processed.path)
        .expect("the encoded file")
        .with_guessed_format()
        .expect("readable")
        .format();
    assert_eq!(format, Some(CrateFormat::Jpeg));
}

/// A probe reads the header and decodes nothing, so `DtoService`'s aspect
/// ratio and the scan's stored dimensions keep working for any image.
#[tokio::test]
async fn an_image_over_the_limits_still_reports_its_size() {
    let fixture = Fixture::new();
    let input = fixture.color("in.png", 200, 200);
    let encoder = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_pixels: 1,
        max_alloc: 100_000,
        resize_budget: 0,
    });

    assert_eq!(
        encoder.get_image_size(&input).await.expect("a size"),
        ImageDimensions::new(200, 200)
    );
}

/// A collage leaves out an image over a decode limit, and one that decodes
/// but is past the budget to resize. A JPEG, which a collage decodes at a
/// reduced scale (gl-1056), is held to the same decode limit.
#[rstest]
#[case::decode("big.png", 600, 600, ImageLimits { max_pixels: 320_000, ..ROOMY })]
// Large enough that a reduced scale covers twice either collage's cell.
#[case::decode_jpeg("big.jpg", 3200, 3200, ImageLimits { max_pixels: 10_000_000, ..ROOMY })]
#[case::resize("wide.png", 2000, 100, ImageLimits { resize_budget: 100_000, ..ROOMY })]
#[tokio::test]
async fn a_collage_skips_an_image_the_limits_refuse(
    #[case] name: &str,
    #[case] width: u32,
    #[case] height: u32,
    #[case] limits: ImageLimits,
    // Square (a 2×2 grid) and thumb strip (the first valid image).
    #[values((400, 400), (800, 400))] collage: (i32, i32),
) {
    let fixture = Fixture::new();
    let refused = fixture.path(name);
    RgbImage::from_pixel(width, height, Rgb([0xFF, 0, 0]))
        .save(&refused)
        .expect("write fixture");
    // The size of a square cell and of the strip, so neither needs a resize.
    let usable = fixture.color("usable.png", 200, 200);
    let strip = fixture.color("strip.png", 800, 400);
    let second = if collage.0 == collage.1 {
        usable
    } else {
        strip
    };
    let output = fixture.path("collage.png");
    let encoder = ImageCrateEncoder::new().with_limits(limits);

    encoder
        .create_image_collage(
            &ImageCollageOptions {
                input_paths: vec![refused, second],
                output_path: output.clone(),
                width: collage.0,
                height: collage.1,
            },
            None,
        )
        .await
        .expect("a collage");

    // The refused image is red and the usable one bluer than it is red, so a
    // JPEG's rounding cannot pass one off as the other.
    let drawn = image::open(&output).expect("a PNG").to_rgba8();
    let pixel = drawn.get_pixel(100, 100).0;
    assert!(
        pixel[2] > pixel[0],
        "the usable image is drawn, not the refused one: {pixel:?}"
    );
}

#[rstest]
#[case::decode("big.png", 2000, 2000, ImageLimits { max_pixels: 1_000_000, ..ROOMY })]
#[case::decode_jpeg("big.jpg", 2000, 2000, ImageLimits { max_pixels: 1_000_000, ..ROOMY })]
// 360 is the wall's row height.
#[case::resize("wide.png", 4000, 100, ImageLimits { resize_budget: 4000 * 360 * 16 - 1, ..ROOMY })]
#[tokio::test]
async fn a_splashscreen_skips_an_image_the_limits_refuse(
    #[case] name: &str,
    #[case] width: u32,
    #[case] height: u32,
    #[case] limits: ImageLimits,
) {
    let fixture = Fixture::new();
    let refused = fixture.path(name);
    RgbImage::from_pixel(width, height, Rgb([0xFF, 0, 0]))
        .save(&refused)
        .expect("write fixture");
    let poster = fixture.color("poster.png", 200, 300);
    let backdrop = fixture.color("backdrop.png", 320, 180);
    let encoder = ImageCrateEncoder::new()
        .with_data_path(fixture.0.path())
        .with_limits(limits);

    encoder
        .create_splashscreen(&[refused.clone(), poster], &[backdrop])
        .await
        .expect("a splashscreen from what is left");

    let drawn = image::open(fixture.path("splashscreen.png"))
        .expect("a PNG")
        .to_rgba8();
    // Red, where everything else drawn is no redder than it is blue.
    assert!(
        drawn.pixels().all(|pixel| pixel.0[0] <= pixel.0[2]),
        "the refused image is not drawn"
    );

    // With nothing else left, it is what a file that does not decode is.
    assert!(matches!(
        encoder
            .create_splashscreen(
                std::slice::from_ref(&refused),
                std::slice::from_ref(&refused)
            )
            .await,
        Err(ServiceError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn a_trickplay_frame_over_a_limit_is_refused() {
    let fixture = Fixture::new();
    let frame = fixture.color("frame.png", 200, 200);
    let encoder = ImageCrateEncoder::new().with_limits(ImageLimits {
        max_pixels: 39_999,
        ..ROOMY
    });

    assert_refused(
        encoder
            .create_trickplay_tile(
                &ImageCollageOptions {
                    input_paths: vec![frame],
                    output_path: fixture.path("tile.jpg"),
                    width: 1,
                    height: 1,
                },
                90,
                200,
                None,
                true,
            )
            .await,
        "image_decode_max_megapixels",
    );
}
