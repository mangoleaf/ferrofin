//! Real-ffmpeg integration test for [`TrickplayFrameExtractorImpl`] — the
//! trickplay frame-extraction run over the concrete [`TokioTranscoder`].
//!
//! Exercises the un-mockable piece (the real process spawn plus the ported
//! `fps`/`scale` filter chain) against a live ffmpeg on a tiny generated clip.
//! Skips itself unless `FERROFIN_FFMPEG_TESTS` is set *and* `ffmpeg` is on
//! `PATH`, so ffmpeg-less CI stays green.
//!
//! Run with:
//! `FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-mediaencoding --test trickplay_frames_ffmpeg`

use std::path::Path;
use std::sync::Arc;

use ferrofin_mediaencoding::{
    EncoderValidator, FfmpegVersion, TokioTranscoder, TrickplayFrameExtractorImpl,
};
use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor as _};

/// The version of the `ffmpeg` on `PATH`, probed the way the composition root
/// probes it.
///
/// Passing `None` here instead would be a lie the test tells itself: the
/// unprobed branch emits the deprecated `-vsync`, which ffmpeg **removed** in
/// 8.0, so on a modern build the run dies with "Unrecognized option 'vsync'"
/// — a failure production never sees, because production always probes.
fn probed_ffmpeg_version() -> Option<FfmpegVersion> {
    let out = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()?;
    EncoderValidator::new("ffmpeg")
        .get_ffmpeg_version_internal(&String::from_utf8_lossy(&out.stdout))
}

/// A 2 s-interval, 64 px-wide software extraction request.
///
/// Hardware is off here deliberately: this test runs on whatever ffmpeg and
/// whatever GPU (or none) the machine has, so the accelerated path is not
/// reproducible enough to assert on. The hardware argument builders are
/// covered by unit goldens; this proves the process spawn and the software
/// filter chain against a live ffmpeg.
fn extraction<'a>(input_path: &'a str, output_dir: &'a str) -> TrickplayExtraction<'a> {
    TrickplayExtraction {
        input_path,
        video_stream: None,
        interval_ms: 2_000,
        max_width: 64,
        qscale: 4,
        threads: 1,
        output_dir,
        allow_hw_accel: false,
        enable_hw_encoding: false,
        keyframe_only: false,
    }
}

/// Whether a program is on `PATH` (via `<prog> -version`).
fn on_path(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the ffmpeg-gated suite should run: `FERROFIN_FFMPEG_TESTS` set AND
/// `ffmpeg` present. Prints a skip line and returns `false` otherwise.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    if !on_path("ffmpeg") {
        eprintln!("skipping: ffmpeg not found on PATH");
        return false;
    }
    true
}

/// Generates a tiny 6-second silent `testsrc` clip at `path` (mirrors the
/// fixture generator in `segment_transcode_ffmpeg.rs`, minus the audio the
/// extractor drops anyway).
fn make_clip(path: &Path) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:size=128x72:rate=10",
            "-c:v",
            "libx264",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for clip");
    assert!(status.success(), "clip generation failed");
    assert!(
        std::fs::metadata(path).is_ok_and(|m| m.len() > 0),
        "generated clip is empty"
    );
}

#[tokio::test]
async fn extracts_interval_frames_from_a_real_clip() {
    if !ffmpeg_gate() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("clip (with spaces).mp4");
    make_clip(&clip);

    let out_dir = tmp.path().join("frames");
    let extractor = TrickplayFrameExtractorImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        probed_ffmpeg_version(),
    );

    // A 6 s clip sampled every 2 s at 64 px max width.
    let frames = extractor
        .extract_trickplay_frames(&extraction(
            &clip.to_string_lossy(),
            &out_dir.to_string_lossy(),
        ))
        .await
        .expect("frame extraction succeeds");

    // fps=1/2 over 6 s yields 3 frames (the muxer may round one extra in).
    assert!(
        (3..=4).contains(&frames.len()),
        "expected 3-4 frames, got {}: {frames:?}",
        frames.len()
    );
    assert!(frames.windows(2).all(|w| w[0] < w[1]), "sorted order");
    for frame in &frames {
        let path = Path::new(frame);
        assert!(path.is_file(), "frame exists: {frame}");
        let bytes = std::fs::read(path).expect("read frame");
        assert!(!bytes.is_empty(), "frame is non-empty: {frame}");
        // JPEG magic — the mjpeg encoder really wrote images.
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG SOI marker in {frame}");
    }
}

#[tokio::test]
async fn missing_input_is_an_error() {
    if !ffmpeg_gate() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("frames");
    let extractor = TrickplayFrameExtractorImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        probed_ffmpeg_version(),
    );

    let err = extractor
        .extract_trickplay_frames(&extraction(
            &tmp.path().join("nope.mp4").to_string_lossy(),
            &out_dir.to_string_lossy(),
        ))
        .await
        .expect_err("missing input must fail");
    assert!(
        err.to_string().contains("no trickplay frames"),
        "diagnostic mentions the empty run: {err}"
    );
}
