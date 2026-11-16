//! Real-ffmpeg integration for `MediaEncoder::extract_image_stream` — `#[ignore]`d
//! like every ffmpeg test; asked for, it needs ffmpeg on `PATH`.
//!
//! A Matroska file carries a video stream and a PNG attachment; ffmpeg exposes
//! the attachment as a stream, which is extracted by index in the requested
//! format, as `EmbeddedImageProvider` does for a video. Another stores an
//! interlaced picture stream ahead of its video: the picture is deinterlaced
//! and extracted, and a frame grab handed it still grabs the video.

use std::sync::Arc;

use ferrofin_mediaencoding::{
    FfmpegVersion, MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder,
};
use ferrofin_model::drawing::ImageFormat;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
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
            None,
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
            None,
            7,
            ImageFormat::Jpg,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("stream 7"), "{err}");
    assert_eq!(std::fs::read_dir(&temp).unwrap().count(), before);
}

/// The centre pixel and size of the image at `path`.
fn centre(path: &str) -> ([u8; 3], (u32, u32)) {
    let image = image::open(path).unwrap().to_rgb8();
    let (width, height) = image.dimensions();
    (image.get_pixel(width / 2, height / 2).0, (width, height))
}

/// `ExtractVideoImage`'s two overloads with a picture stream (a red 320x240
/// PNG ffprobe calls interlaced) stored ahead of the video (blue, 640x480):
/// the image-stream one maps the picture and passes it as the video stream,
/// so it is deinterlaced and still comes out whole; the frame-grab one maps
/// no stream, so handed the picture it grabs a frame of the video at the
/// offset, where mapping the one-picture stream left nothing there.
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_picture_stream_ahead_of_the_video_is_deinterlaced_and_never_grabbed() {
    assert!(ffmpeg_available(), "ffmpeg not found on PATH");
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("cover.png");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=red:size=320x240",
        "-frames:v",
        "1",
        png.to_str().unwrap(),
    ]);
    let media = dir.path().join("cover-first.mkv");
    run(&[
        "-i",
        png.to_str().unwrap(),
        "-f",
        "lavfi",
        "-i",
        "color=c=blue:size=640x480:rate=25:duration=4",
        "-map",
        "0",
        "-map",
        "1",
        "-filter:v:0",
        "setfield=tff",
        "-c:v:0",
        "png",
        "-c:v:1",
        "libx264",
        "-disposition:v:0",
        "0",
        "-disposition:v:1",
        "default",
        media.to_str().unwrap(),
    ]);
    let encoder = MediaEncoderImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        "ffprobe",
        MediaEncoderConfig {
            temp_dir: dir.path().join("temp").into(),
            ffmpeg_version: Some(FfmpegVersion::new(9, 0)),
            ..MediaEncoderConfig::default()
        },
    );
    let picture = MediaStream {
        stream_type: MediaStreamType::EmbeddedImage,
        index: 0,
        codec: Some("png".to_owned()),
        is_interlaced: true,
        ..MediaStream::default()
    };

    let image = encoder
        .extract_image_stream(
            media.to_str().unwrap(),
            "matroska",
            &MediaSourceInfo::default(),
            Some(&picture),
            0,
            ImageFormat::Png,
        )
        .await
        .expect("the deinterlaced picture is extracted");
    let ([red, green, blue], size) = centre(&image);
    assert_eq!(size, (320, 240));
    assert!(red >= 250 && green <= 4 && blue <= 4, "the red picture");

    // 10 % into the 4 s runtime, as the screen grab seeks.
    let frame = encoder
        .extract_video_image(
            media.to_str().unwrap(),
            "matroska",
            &MediaSourceInfo::default(),
            &picture,
            None,
            Some(4_000_000),
        )
        .await
        .expect("a frame of the video is grabbed");
    let ([red, green, blue], size) = centre(&frame);
    assert_eq!(size, (640, 480));
    assert!(red <= 8 && green <= 8 && blue >= 240, "the blue video");
}
