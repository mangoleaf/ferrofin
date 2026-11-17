//! Real-ffmpeg integration for `MediaEncoder::extract_image_stream` — `#[ignore]`d
//! like every ffmpeg test; asked for, it needs ffmpeg on `PATH`.
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
    std::process::Command::new("ffmpeg")
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
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn extracts_an_attached_picture_by_stream_index() {
    assert!(ffmpeg_available(), "ffmpeg not found on PATH");
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
    // The extraction ends in a conversion to the JPEG's BT.601 matrix
    // (gl-1158). An RGB picture has no matrix to convert, so this only checks
    // that the conversion leaves a PNG of one as it was.
    let pixel = image::open(&out).unwrap().to_rgb8().get_pixel(16, 12).0;
    assert!(
        pixel[0] >= 250 && pixel[1] <= 4 && pixel[2] <= 4,
        "still red: {pixel:?}"
    );

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
