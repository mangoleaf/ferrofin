//! Real-ffmpeg integration test for `MediaEncoder::extract_video_image`, the
//! chapter-image and screen-grab path: the JPEG it writes keeps the source's
//! colours. `#[ignore]`d like every ffmpeg test; asked for, it needs an
//! `ffmpeg` on `PATH`.
//!
//! Run with:
//! `cargo test -p ferrofin-mediaencoding --test image_extraction_ffmpeg -- --include-ignored`

mod colour_bars;

use std::sync::Arc;

use colour_bars::{BARS, COLOUR_TOLERANCE, Matrix, bar_colours, make_bars_clip};
use ferrofin_mediaencoding::encoder::trickplay_extractor::MIN_FFMPEG_JPEG_MATRIX_CONVERSION;
use ferrofin_mediaencoding::{
    EncoderValidator, FfmpegVersion, MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder,
};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::MediaEncoder as _;

/// The version of the `ffmpeg` on `PATH`, probed the way the composition root
/// probes it; `None` when there is no ffmpeg to run.
fn probed_ffmpeg_version() -> Option<FfmpegVersion> {
    let out = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    EncoderValidator::new("ffmpeg")
        .get_ffmpeg_version_internal(&String::from_utf8_lossy(&out.stdout))
}

/// A JPEG decoder reads Y'CbCr as BT.601 (JFIF), so the chapter image of a
/// BT.709 source must be converted to that matrix, or its reds and greens
/// shift (gl-1158). BT.601 and untagged sources are already in it and must
/// not be converted again. Below ffmpeg 5.1 nothing is converted, so the
/// BT.709 case is skipped there.
#[rstest::rstest]
#[case::bt709(
    Matrix::Bt709,
    Some("colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv")
)]
#[case::bt601(
    Matrix::Bt601,
    Some("colorspace=smpte170m:color_primaries=smpte170m:color_trc=smpte170m:range=tv")
)]
#[case::untagged(Matrix::Bt601, None)]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_chapter_image_keeps_the_colours_of_the_source(
    #[case] matrix: Matrix,
    #[case] tags: Option<&str>,
) {
    let version = probed_ffmpeg_version().expect("no ffmpeg on PATH, or its version is unreadable");
    if matches!(matrix, Matrix::Bt709) && version < MIN_FFMPEG_JPEG_MATRIX_CONVERSION {
        eprintln!(
            "skipping: ffmpeg {version} is older than {MIN_FFMPEG_JPEG_MATRIX_CONVERSION}, \
             where image extraction converts no matrix"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("bars.mkv");
    make_bars_clip(&clip, matrix, tags);
    let encoder = MediaEncoderImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        "ffprobe",
        MediaEncoderConfig {
            temp_dir: tmp.path().join("temp").into(),
            ffmpeg_version: Some(version),
            ..MediaEncoderConfig::default()
        },
    );
    let stream = MediaStream {
        stream_type: MediaStreamType::Video,
        index: 0,
        ..MediaStream::default()
    };
    let image = encoder
        .extract_video_image(
            &clip.to_string_lossy(),
            "matroska",
            &MediaSourceInfo::default(),
            &stream,
            None,
            None,
        )
        .await
        .expect("image extracted");

    let got = bar_colours(&image::open(&image).expect("decode image").to_rgb8());
    let off = got
        .iter()
        .zip(&BARS)
        .flat_map(|(got, want)| got.iter().zip(want).map(|(a, b)| a.abs_diff(*b)))
        .max()
        .expect("bars");
    assert!(
        off <= COLOUR_TOLERANCE,
        "decoded {got:?}, source {BARS:?} (off by {off})"
    );
}
