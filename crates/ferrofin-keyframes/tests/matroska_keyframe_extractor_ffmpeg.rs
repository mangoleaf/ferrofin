//! The Matroska keyframe extractor over a real file, checked against ffprobe.
//!
//! ffmpeg writes a 12 s Matroska file with keyframes only where it is told to,
//! at irregular times. The extractor reads them from the cues; ffprobe decodes
//! the same file's keyframes. The two must agree.
//!
//! Gated like the other ffmpeg tests: it skips itself unless
//! `FERROFIN_FFMPEG_TESTS` is set and both `ffmpeg` and `ffprobe` are on `PATH`.
//!
//! Run with:
//! `FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-keyframes --test matroska_keyframe_extractor_ffmpeg`

use std::path::Path;
use std::process::{Command, Stdio};

use ferrofin_keyframes::{ff_probe, matroska};

fn on_path(binary: &str) -> bool {
    Command::new(binary)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the ffmpeg-gated test should run; prints why it skips otherwise.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    for binary in ["ffmpeg", "ffprobe"] {
        if !on_path(binary) {
            eprintln!("skipping: {binary} not found on PATH");
            return false;
        }
    }
    true
}

#[test]
fn the_cues_of_an_ffmpeg_matroska_file_match_ffprobes_keyframes() {
    if !ffmpeg_gate() {
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("matroska_keyframe_extractor");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let file = dir.join("irregular-gop.mkv");
    let file = file.to_string_lossy();

    // 25 fps, so each forced time lands on the first frame at or after it.
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=25:duration=12",
            "-c:v",
            "mpeg4",
            "-g",
            "1000",
            "-force_key_frames",
            "0,1.3,4.2,6.5,9.84,11",
            "-f",
            "matroska",
            &file,
        ])
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed: {status}");

    let cues = matroska::get_keyframe_data(&file).expect("read the cues");
    let probed = ff_probe::get_keyframe_data("ffprobe", &file).expect("run ffprobe");

    assert_eq!(
        cues.keyframe_ticks,
        [
            0,
            13_200_000,
            42_000_000,
            65_200_000,
            98_400_000,
            110_000_000
        ]
    );
    assert_eq!(cues.keyframe_ticks, probed.keyframe_ticks);
    // The segment's duration against ffprobe's, within a millisecond.
    assert!(
        (cues.total_duration - probed.total_duration).abs() <= 10_000,
        "cues {} vs ffprobe {}",
        cues.total_duration,
        probed.total_duration
    );
}
