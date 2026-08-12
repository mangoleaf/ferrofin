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

use ferrofin_mediaencoding::{TokioTranscoder, TrickplayFrameExtractorImpl};
use ferrofin_traits::media_encoding::TrickplayFrameExtractor as _;

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
    let extractor = TrickplayFrameExtractorImpl::new(Arc::new(TokioTranscoder::new()), "ffmpeg");

    // A 6 s clip sampled every 2 s at 64 px max width.
    let frames = extractor
        .extract_trickplay_frames(
            &clip.to_string_lossy(),
            2_000,
            64,
            4,
            1,
            &out_dir.to_string_lossy(),
        )
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
    let extractor = TrickplayFrameExtractorImpl::new(Arc::new(TokioTranscoder::new()), "ffmpeg");

    let err = extractor
        .extract_trickplay_frames(
            &tmp.path().join("nope.mp4").to_string_lossy(),
            2_000,
            64,
            4,
            1,
            &out_dir.to_string_lossy(),
        )
        .await
        .expect_err("missing input must fail");
    assert!(
        err.to_string().contains("no trickplay frames"),
        "diagnostic mentions the empty run: {err}"
    );
}
