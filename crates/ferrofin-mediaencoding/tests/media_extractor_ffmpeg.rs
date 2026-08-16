//! Real-ffmpeg integration for [`FfmpegMediaExtractor`] — gated like every
//! ffmpeg test: `FERROFIN_FFMPEG_TESTS=1`, skipped when ffmpeg is absent.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use ferrofin_mediaencoding::FfmpegMediaExtractor;
use ferrofin_traits::media_analysis::{AudioSpec, MediaExtractor as _};

fn gated() -> Option<String> {
    if std::env::var("FERROFIN_FFMPEG_TESTS").ok().as_deref() != Some("1") {
        return None;
    }
    which_ffmpeg()
}

fn which_ffmpeg() -> Option<String> {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "ffmpeg".to_owned())
}

#[tokio::test]
async fn extracts_pcm_and_frames_from_generated_media() {
    let Some(ffmpeg) = gated() else {
        eprintln!("skipped: FERROFIN_FFMPEG_TESTS!=1 or no ffmpeg");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("tone.mkv");
    // 3 s of sine + solid color video.
    let status = std::process::Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=64x64:duration=3",
            "-c:a",
            "flac",
            "-c:v",
            "libx264",
            "-shortest",
        ])
        .arg(&media)
        .status()
        .unwrap();
    assert!(status.success(), "test media generated");

    let extractor = FfmpegMediaExtractor::new(ffmpeg);
    let samples = extractor
        .extract_audio(
            media.to_str().unwrap(),
            0.5,
            1.0,
            AudioSpec {
                sample_rate: 11_025,
                channels: 1,
            },
        )
        .await
        .expect("audio extracted");
    // ~1 s of 11.025 kHz mono, and a sine is loud (not silence).
    assert!(
        (10_000..=12_000).contains(&samples.len()),
        "{}",
        samples.len()
    );
    assert!(samples.iter().any(|s| s.abs() > 1000), "audible signal");

    let frames = extractor
        .extract_frames(media.to_str().unwrap(), &[0.5, 1.5], 64, false)
        .await
        .expect("frames extracted");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data.len(), 64 * 64, "raw gray8 at exact dims");
    // Red in grayscale is mid-dark but nonzero everywhere.
    assert!(frames[0].data.iter().all(|&b| b > 10));
}

/// The timeout path needs no ffmpeg: any command that hangs silently will
/// do. Not gated — it must run in every CI pass (unix-only: the hanging
/// stand-in is a shell script).
#[cfg(unix)]
#[tokio::test]
async fn stalled_extraction_times_out_and_kills_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hang.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let extractor = FfmpegMediaExtractor::new(script.to_str().unwrap().to_owned())
        .with_timeout(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    let err = extractor
        .extract_audio(
            "/nonexistent.mkv",
            0.0,
            1.0,
            AudioSpec {
                sample_rate: 11_025,
                channels: 1,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "returned promptly, not after the child's 30 s sleep"
    );
}
