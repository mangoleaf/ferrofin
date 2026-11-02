//! Real-ffmpeg integration for `MediaEncoder::extract_image_stream` — gated like
//! every ffmpeg test: `FERROFIN_FFMPEG_TESTS=1`, skipped when ffmpeg is absent.
//!
//! A Matroska file carries a video stream and a PNG attachment; ffmpeg exposes
//! the attachment as a stream, which is extracted by index in the requested
//! format, as `EmbeddedImageProvider` does for a video.

use std::sync::Arc;

use ferrofin_mediaencoding::{
    FfmpegVersion, MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder,
};
use ferrofin_model::drawing::ImageFormat;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_traits::media_encoding::MediaEncoder as _;

fn ffmpeg_available() -> bool {
    std::env::var("FERROFIN_FFMPEG_TESTS").ok().as_deref() == Some("1")
        && std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
}

fn run(args: &[&str]) {
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg {args:?}");
}

#[tokio::test]
async fn extracts_an_attached_picture_by_stream_index() {
    if !ffmpeg_available() {
        eprintln!("skipped: FERROFIN_FFMPEG_TESTS!=1 or no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("cover.png");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=red:size=32x24",
        "-frames:v",
        "1",
        png.to_str().unwrap(),
    ]);
    let media = dir.path().join("movie.mkv");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=blue:size=64x64:duration=1",
        "-c:v",
        "libx264",
        "-attach",
        png.to_str().unwrap(),
        "-metadata:s:t",
        "mimetype=image/png",
        "-metadata:s:t",
        "filename=cover.png",
        media.to_str().unwrap(),
    ]);
    let temp = dir.path().join("temp");
    let encoder = MediaEncoderImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        "ffprobe",
        MediaEncoderConfig {
            temp_dir: temp.clone().into(),
            // The server probes this; ffmpeg 9 has dropped `-vsync`.
            ffmpeg_version: Some(FfmpegVersion::new(9, 0)),
            ..MediaEncoderConfig::default()
        },
    );
    // Stream 0 is the video, stream 1 the attachment.
    let out = encoder
        .extract_image_stream(
            media.to_str().unwrap(),
            "matroska",
            &MediaSourceInfo::default(),
            1,
            ImageFormat::Png,
        )
        .await
        .expect("attachment extracted");
    assert!(
        std::path::Path::new(&out).starts_with(&temp) && out.rsplit('.').next() == Some("png"),
        "{out}"
    );
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG file");
    // 32x24 per the IHDR chunk.
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 32);
    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 24);

    // A stream that is not a picture yields an error and leaves no file.
    let before = std::fs::read_dir(&temp).unwrap().count();
    let err = encoder
        .extract_image_stream(
            media.to_str().unwrap(),
            "matroska",
            &MediaSourceInfo::default(),
            7,
            ImageFormat::Jpg,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("stream 7"), "{err}");
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), before);
}
