//! Real-ffprobe integration for the **external sidecar** probes the scan's
//! `MediaInfoResolver` (ferrofin-core) issues: a `.srt` next to a movie must
//! probe as exactly one `subrip` subtitle stream, and an audio sidecar as
//! exactly one audio stream — the `MediaStreams.Count == 1` branch upstream's
//! `GetExternalStreamsAsync` takes for a plain sidecar. The requests here are
//! built exactly as the resolver builds them (`MediaProtocol::File`, no
//! chapters, `MediaType = Audio` only for the audio resolver).
//!
//! Gated like every ffmpeg test: `FERROFIN_FFMPEG_TESTS=1`, skipped when
//! ffmpeg/ffprobe are absent.
//!
//! ```text
//! FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-mediaencoding --test external_sidecar_probe_ffmpeg
//! ```

use std::path::Path;
use std::sync::Arc;

use ferrofin_mediaencoding::{MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

fn on_path(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the ffmpeg-gated suite should run: `FERROFIN_FFMPEG_TESTS` set AND
/// both binaries present. Prints a skip line and returns `false` otherwise.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    if !on_path("ffmpeg") || !on_path("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
        return false;
    }
    true
}

fn encoder() -> MediaEncoderImpl<TokioTranscoder> {
    MediaEncoderImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        "ffprobe",
        MediaEncoderConfig::default(),
    )
}

/// The request `MediaInfoResolver::get_media_info` builds for a sidecar.
fn sidecar_request(path: &Path, media_is_audio: bool) -> MediaInfoRequest {
    MediaInfoRequest {
        media_source: MediaSourceInfo {
            path: Some(path.to_string_lossy().into_owned()),
            protocol: MediaProtocol::File,
            ..MediaSourceInfo::default()
        },
        extract_chapters: false,
        media_is_audio,
    }
}

#[tokio::test]
async fn a_srt_sidecar_probes_as_one_subrip_subtitle_stream() {
    if !ffmpeg_gate() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let srt = dir.path().join("Heat (1995).eng.forced.srt");
    std::fs::write(
        &srt,
        "1\n00:00:00,000 --> 00:00:02,000\nhello ferrofin\n\n2\n00:00:03,000 --> 00:00:04,000\nbye\n\n",
    )
    .expect("srt fixture");

    let info = encoder()
        .get_media_info(&sidecar_request(&srt, false))
        .await
        .expect("ffprobe reads a bare .srt");

    assert_eq!(info.media_streams.len(), 1, "{:?}", info.media_streams);
    let stream = &info.media_streams[0];
    assert_eq!(stream.stream_type, MediaStreamType::Subtitle);
    assert_eq!(stream.codec.as_deref(), Some("subrip"));
    // A bare .srt carries no language/title of its own — those come from the
    // filename in the resolver's `MergeMetadata`.
    assert!(stream.language.is_none(), "{stream:?}");
    assert!(stream.title.is_none(), "{stream:?}");
}

#[tokio::test]
async fn an_audio_sidecar_probes_as_one_audio_stream() {
    if !ffmpeg_gate() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let mka = dir.path().join("Heat (1995).commentary.mka");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "flac",
        ])
        .arg(&mka)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "audio sidecar generated");

    let info = encoder()
        .get_media_info(&sidecar_request(&mka, true))
        .await
        .expect("ffprobe reads the audio sidecar");

    assert_eq!(info.media_streams.len(), 1, "{:?}", info.media_streams);
    let stream = &info.media_streams[0];
    assert_eq!(stream.stream_type, MediaStreamType::Audio);
    assert_eq!(stream.codec.as_deref(), Some("flac"));
    assert!(
        info.run_time_ticks.is_some_and(|t| t > 0),
        "the sidecar's duration is probed: {info:?}"
    );
}
