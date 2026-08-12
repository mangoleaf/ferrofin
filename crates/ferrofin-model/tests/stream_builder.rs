//! Transliteration of `Jellyfin.Model.Tests.Dlna.StreamBuilderTests`.
//!
//! The `[Theory]`/`[InlineData]` matrix is ported to `#[rstest]`/`#[case]`,
//! loading the verbatim `Test Data/*.json` fixtures and asserting the same
//! `PlayMethod` + `TranscodeReason` outcomes as upstream. The expected values
//! are the parity oracle and must not be weakened.

use std::path::Path;

use ferrofin_model::data::MediaStreamProtocol;
use ferrofin_model::dlna::stream_builder::StreamBuilder;
use ferrofin_model::dlna::stream_info::StreamInfo;
use ferrofin_model::dlna::transcoder_support::TranscoderSupport;
use ferrofin_model::dlna::{DeviceProfile, MediaOptions};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::session::{PlayMethod, TranscodeReasons};
use rstest::rstest;
use uuid::Uuid;

/// A no-op transcoder-support probe, mirroring the unconfigured Moq used
/// upstream (every method returns `false`).
struct MockTranscoderSupport;

impl TranscoderSupport for MockTranscoderSupport {
    fn can_encode_to_audio_codec(&self, _codec: &str) -> bool {
        false
    }
    fn can_encode_to_subtitle_codec(&self, _codec: &str) -> bool {
        false
    }
    fn can_extract_subtitles(&self, _codec: &str) -> bool {
        false
    }
}

fn data_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data"))
}

fn load<T: serde::de::DeserializeOwned>(prefix: &str, name: &str) -> T {
    let path = data_dir().join(format!("{prefix}-{name}.json"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn get_media_options(device_profile: &str, source: &str) -> MediaOptions {
    let media_source: MediaSourceInfo = load("MediaSourceInfo", source);
    let media_source_id = media_source.id.clone();
    let dp: DeviceProfile = load("DeviceProfile", device_profile);

    let mut options = MediaOptions::new(dp);
    options.item_id = Uuid::parse_str("11D229B7-2D48-4B95-9F9B-49F6AB75E613").unwrap();
    options.media_source_id = media_source_id;
    options.media_sources = vec![media_source];
    options.device_id = Some("test-deviceId".to_owned());
    options.allow_audio_stream_copy = true;
    options.allow_video_stream_copy = true;
    options.enable_direct_stream = false; // Disabled in server.
    options
}

struct ParsedUri {
    filename: String,
    extension: String,
}

fn parse_uri(val: &StreamInfo) -> ParsedUri {
    let href = val.to_url(Some("media:"), Some("ACCESSTOKEN"), None);
    let path = href.split('?').next().unwrap_or("").to_owned();
    let last = path.rsplit('/').next().unwrap_or("");
    let (filename, extension) = match last.rsplit_once('.') {
        Some((f, e)) => (f.to_owned(), e.to_owned()),
        None => (last.to_owned(), String::new()),
    };
    ParsedUri {
        filename,
        extension,
    }
}

/// Mirrors `BuildVideoItemSimpleTest`.
#[allow(clippy::too_many_lines)]
fn build_video_item_simple_test(
    options: &MediaOptions,
    play_method: Option<PlayMethod>,
    why: TranscodeReasons,
    transcode_mode: &str,
    transcode_protocol_in: &str,
) -> StreamInfo {
    let transcode_protocol = if transcode_protocol_in.is_empty() {
        "HLS.ts"
    } else {
        transcode_protocol_in
    };

    let support = MockTranscoderSupport;
    let builder = StreamBuilder::new(&support);

    let stream_info = builder
        .get_optimal_video_stream(options)
        .expect("stream info");

    if let Some(pm) = play_method {
        assert_eq!(pm, stream_info.play_method, "PlayMethod");
    }

    assert_eq!(why, stream_info.transcode_reasons, "TranscodeReasons");

    let target_video_stream = stream_info.target_video_stream().cloned();
    let target_audio_stream = stream_info.target_audio_stream().cloned();

    let media_source = options
        .media_sources
        .iter()
        .find(|s| s.id.as_deref() == stream_info.media_source_id())
        .expect("media source");

    let uri = parse_uri(&stream_info);

    if play_method == Some(PlayMethod::DirectPlay) {
        let containers = media_source.container.clone().unwrap_or_default();
        assert!(
            containers.split(',').any(|c| c == uri.extension),
            "expected container '{}' in '{}'",
            uri.extension,
            containers
        );

        if let Some(vc) = target_video_stream
            .as_ref()
            .and_then(|s| s.codec.as_deref())
        {
            assert!(stream_info.target_video_codec().iter().any(|c| c == vc));
            assert_eq!(stream_info.target_video_codec().len(), 1);
        }
        if let Some(ac) = target_audio_stream
            .as_ref()
            .and_then(|s| s.codec.as_deref())
        {
            assert!(stream_info.target_audio_codec().iter().any(|c| c == ac));
            assert_eq!(stream_info.target_audio_codec().len(), 1);
        }

        if transcode_mode == "DirectStream" {
            assert_eq!(
                stream_info.container.as_deref(),
                Some(uri.extension.as_str())
            );
        }
    } else if play_method == Some(PlayMethod::Transcode) {
        assert!(stream_info.container.is_some());
        assert!(!stream_info.video_codecs.is_empty());
        assert!(!stream_info.audio_codecs.is_empty());

        if transcode_protocol == "http" {
            assert_eq!(
                stream_info.container.as_deref(),
                Some(uri.extension.as_str())
            );
            assert_eq!(uri.filename, "stream");
            assert_eq!(stream_info.sub_protocol, MediaStreamProtocol::http);
        } else if transcode_protocol == "HLS.mp4" {
            assert_eq!(stream_info.container.as_deref(), Some("mp4"));
            assert_eq!(uri.extension, "m3u8");
            assert_eq!(uri.filename, "master");
            assert_eq!(stream_info.sub_protocol, MediaStreamProtocol::hls);
        } else {
            assert_eq!(stream_info.container.as_deref(), Some("ts"));
            assert_eq!(uri.extension, "m3u8");
            assert_eq!(uri.filename, "master");
            assert_eq!(stream_info.sub_protocol, MediaStreamProtocol::hls);
        }

        if transcode_mode == "Transcode" {
            if !stream_info.transcode_reasons.intersects(
                StreamBuilder::container_reasons()
                    | TranscodeReasons::DIRECT_PLAY_ERROR
                    | TranscodeReasons::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
            ) {
                for stream in media_source
                    .media_streams
                    .iter()
                    .filter(|s| s.stream_type == MediaStreamType::Video)
                {
                    if let Some(codec) = stream.codec.as_deref() {
                        assert!(
                            !stream_info.video_codecs.iter().any(|c| c == codec),
                            "video codec {codec} should not be present in full transcode"
                        );
                    }
                }
            }
        } else {
            assert!(
                target_video_stream
                    .as_ref()
                    .and_then(|s| s.codec.as_deref())
                    .is_none_or(|vc| stream_info.target_video_codec().iter().any(|c| c == vc))
            );
            assert_eq!(stream_info.target_video_codec().len(), 1);

            assert!(!stream_info.estimate_content_length);
        }
    } else if play_method.is_none() {
        assert_eq!(stream_info.sub_protocol, MediaStreamProtocol::http);
        assert_eq!(uri.filename, "stream");
        assert!(!stream_info.estimate_content_length);
    }

    stream_info
}

fn run_simple(
    device: &str,
    source: &str,
    play_method: Option<PlayMethod>,
    why: TranscodeReasons,
    transcode_mode: &str,
    transcode_protocol: &str,
) {
    let options = get_media_options(device, source);
    build_video_item_simple_test(
        &options,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
}

// TranscodeReason aliases for terse case rows.
use TranscodeReasons as R;

const NONE: TranscodeReasons = TranscodeReasons::empty();
const DP: Option<PlayMethod> = Some(PlayMethod::DirectPlay);
const TC: Option<PlayMethod> = Some(PlayMethod::Transcode);

#[rstest]
// Chrome
#[case("Chrome", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aacExt-srt-2600k",
    TC,
    R::AUDIO_IS_EXTERNAL,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case("Chrome", "mkv-h264-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("Chrome", "mp4-hevc-aac-srt-15200k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-hevc-ac3-aacDef-srt-15200k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mkv-vp9-aac-srt-2600k",
    TC,
    R::CONTAINER_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Chrome", "mkv-vp9-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("Chrome", "mkv-vp9-vorbis-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case("Chrome", "mp4-h264-hi10p-aac-5000k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome",
    "mkv-h264-hi10p-aac-5000k-brokenfps",
    TC,
    R::CONTAINER_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Chrome", "mp4-dvh1.05-eac3-15200k", TC, R::VIDEO_RANGE_TYPE_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Chrome", "mkv-dvhe.05-eac3-28000k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_RANGE_TYPE_NOT_SUPPORTED).union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Chrome", "mkv-dvhe.08-eac3-15200k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_RANGE_TYPE_NOT_SUPPORTED).union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Chrome", "mp4-dvhe.08-eac3-15200k", TC, R::VIDEO_RANGE_TYPE_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Chrome", "numstreams-32", DP, NONE, "DirectStream", "")]
#[case("Chrome", "numstreams-33", DP, NONE, "DirectStream", "")]
// Firefox
#[case("Firefox", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Firefox",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-aacExt-srt-2600k",
    TC,
    R::AUDIO_IS_EXTERNAL,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case("Firefox", "mkv-h264-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case(
    "Firefox",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::VIDEO_CODEC_NOT_SUPPORTED,
    "Transcode",
    "HLS.mp4"
)]
#[case("Firefox", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mp4-hevc-ac3-aacDef-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::SECONDARY_AUDIO_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case(
    "Firefox",
    "mkv-vp9-aac-srt-2600k",
    TC,
    R::CONTAINER_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Firefox", "mkv-vp9-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("Firefox", "mkv-vp9-vorbis-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case("Firefox", "mp4-h264-hi10p-aac-5000k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_PROFILE_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mkv-h264-hi10p-aac-5000k-brokenfps", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_PROFILE_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mp4-dvh1.05-eac3-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mkv-dvhe.05-eac3-28000k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_CODEC_NOT_SUPPORTED).union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mkv-dvhe.08-eac3-15200k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_CODEC_NOT_SUPPORTED).union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case("Firefox", "mp4-dvhe.08-eac3-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
// Safari
#[case("SafariNext", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aacDef-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aacExt-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("SafariNext", "mp4-h264-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("SafariNext", "mkv-h264-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case(
    "SafariNext",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::VIDEO_CODEC_TAG_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("SafariNext", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_TAG_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("SafariNext", "mp4-hevc-ac3-aacExt-srt-15200k", TC, R::VIDEO_CODEC_TAG_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case(
    "SafariNext",
    "mp4-h264-hi10p-aac-5000k",
    TC,
    R::VIDEO_PROFILE_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("SafariNext", "mkv-h264-hi10p-aac-5000k-brokenfps", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_PROFILE_NOT_SUPPORTED), "Remux", "HLS.mp4")]
#[case("SafariNext", "mp4-dvh1.05-eac3-15200k", DP, NONE, "DirectStream", "")]
#[case("SafariNext", "mkv-dvhe.05-eac3-28000k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_CODEC_TAG_NOT_SUPPORTED).union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("SafariNext", "mkv-dvhe.08-eac3-15200k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::VIDEO_CODEC_TAG_NOT_SUPPORTED).union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("SafariNext", "mp4-dvhe.08-eac3-15200k", TC, R::VIDEO_CODEC_TAG_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
// AndroidPixel
#[case("AndroidPixel", "mp4-h264-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "AndroidPixel",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidPixel",
    "mp4-h264-ac3-aacDef-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("AndroidPixel", "mp4-h264-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("AndroidPixel", "mp4-hevc-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
#[case("AndroidPixel", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
// Yatse
#[case("Yatse", "mp4-h264-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "Yatse",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "Yatse",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Yatse",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Yatse", "mp4-hevc-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("Yatse", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case("Yatse", "mp4-hevc-ac3-aacDef-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::SECONDARY_AUDIO_NOT_SUPPORTED), "Transcode", "")]
// RokuSSPlus
#[case("RokuSSPlus", "mp4-h264-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "RokuSSPlus",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("RokuSSPlus", "mp4-h264-ac3-aacDef-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "RokuSSPlus",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("RokuSSPlus", "mp4-hevc-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-aacDef-srt-15200k", DP, NONE, "Remux", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
// JellyfinMediaPlayer
#[case("JellyfinMediaPlayer", "mp4-h264-aac-vtt-2600k", DP, NONE, "Remux", "")]
#[case(
    "JellyfinMediaPlayer",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "Remux",
    ""
)]
#[case("JellyfinMediaPlayer", "mp4-h264-ac3-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "JellyfinMediaPlayer",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
// Non-HLS Progressive transcoding
#[case("Chrome-NoHLS", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome-NoHLS",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "http"
)]
#[case(
    "Chrome-NoHLS",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "http"
)]
#[case(
    "Chrome-NoHLS",
    "mp4-h264-ac3-aacExt-srt-2600k",
    TC,
    R::AUDIO_IS_EXTERNAL,
    "Remux",
    "http"
)]
#[case(
    "Chrome-NoHLS",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "http"
)]
#[case(
    "Chrome-NoHLS",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::VIDEO_CODEC_NOT_SUPPORTED,
    "Transcode",
    "http"
)]
#[case("Chrome-NoHLS", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "http")]
#[case("Chrome-NoHLS", "mp4-hevc-ac3-aacDef-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::SECONDARY_AUDIO_NOT_SUPPORTED), "Transcode", "http")]
#[case("Chrome-NoHLS", "mkv-vp9-aac-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "http")]
#[case("Chrome-NoHLS", "mkv-vp9-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "http")]
#[case("Chrome-NoHLS", "mkv-vp9-vorbis-vtt-2600k", DP, NONE, "Remux", "http")]
// TranscodeMedia
#[case(
    "TranscodeMedia",
    "mp4-h264-aac-vtt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.mp4"
)]
#[case("TranscodeMedia", "mp4-h264-ac3-aac-srt-2600k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "HLS.mp4")]
#[case(
    "TranscodeMedia",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.mp4"
)]
#[case("TranscodeMedia", "mp4-h264-ac3-srt-2600k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "HLS.mp4")]
#[case(
    "TranscodeMedia",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.mp4"
)]
#[case("TranscodeMedia", "mp4-hevc-ac3-aac-srt-15200k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "HLS.mp4")]
#[case(
    "TranscodeMedia",
    "mp4-hevc-ac3-aacDef-srt-15200k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.mp4"
)]
#[case("TranscodeMedia", "mkv-av1-aac-srt-2600k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "http")]
#[case(
    "TranscodeMedia",
    "mkv-av1-vorbis-srt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "http"
)]
#[case("TranscodeMedia", "mkv-vp9-aac-srt-2600k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "http")]
#[case("TranscodeMedia", "mkv-vp9-ac3-srt-2600k", TC, R::AUDIO_CODEC_NOT_SUPPORTED.union(R::DIRECT_PLAY_ERROR), "DirectStream", "http")]
#[case(
    "TranscodeMedia",
    "mkv-vp9-vorbis-vtt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "http"
)]
// DirectMedia
#[case("DirectMedia", "mp4-h264-aac-vtt-2600k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mp4-h264-ac3-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mp4-h264-ac3-aacDef-srt-2600k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mp4-h264-ac3-srt-2600k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mp4-hevc-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mp4-hevc-ac3-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("DirectMedia", "mkv-vp9-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("DirectMedia", "mkv-vp9-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "DirectMedia",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
// LowBandwidth
#[case(
    "LowBandwidth",
    "mp4-h264-aac-vtt-2600k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "LowBandwidth",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "LowBandwidth",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "LowBandwidth",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "LowBandwidth",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case("LowBandwidth", "mkv-vp9-aac-srt-2600k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
#[case("LowBandwidth", "mkv-vp9-ac3-srt-2600k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
#[case("LowBandwidth", "mkv-vp9-vorbis-vtt-2600k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED).union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
// Null
#[case(
    "Null",
    "mp4-h264-aac-vtt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mp4-h264-ac3-aac-srt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mp4-h264-ac3-srt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mp4-hevc-aac-srt-15200k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mp4-hevc-ac3-aac-srt-15200k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mkv-vp9-aac-srt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mkv-vp9-ac3-srt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
#[case(
    "Null",
    "mkv-vp9-vorbis-vtt-2600k",
    None,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "DirectStream",
    ""
)]
// AndroidTV
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mkv-vp9-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mkv-vp9-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("AndroidTVExoPlayer", "mkv-vp9-vorbis-vtt-2600k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-aac-4000k-r180",
    DP,
    NONE,
    "DirectStream",
    ""
)]
// AndroidTV NoHevcRotation
#[case("AndroidTVExoPlayer-NoHevcRotation", "mp4-hevc-aac-4000k-r180", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::VIDEO_ROTATION_NOT_SUPPORTED), "Transcode", "")]
// Tizen 3 Stereo
#[case(
    "Tizen3-stereo",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-dts-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-truehd-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Tizen3-stereo", "mkv-vp9-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("Tizen3-stereo", "mkv-vp9-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen3-stereo",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mkv-dvhe.08-eac3-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("Tizen3-stereo", "mp4-dvh1.05-eac3-15200k", TC, R::VIDEO_RANGE_TYPE_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "Transcode", "")]
#[case(
    "Tizen3-stereo",
    "mp4-dvhe.08-eac3-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("Tizen3-stereo", "mkv-dvhe.05-eac3-28000k", TC, R::VIDEO_BITRATE_NOT_SUPPORTED.union(R::VIDEO_RANGE_TYPE_NOT_SUPPORTED).union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "Transcode", "")]
#[case("Tizen3-stereo", "numstreams-32", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen3-stereo",
    "numstreams-33",
    TC,
    R::STREAM_COUNT_EXCEEDS_LIMIT,
    "Remux",
    ""
)]
// Tizen 4 4K 5.1
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-dts-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-truehd-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Tizen4-4K-5.1", "mkv-vp9-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("Tizen4-4K-5.1", "mkv-vp9-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen4-4K-5.1",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mkv-dvhe.08-eac3-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-dvh1.05-eac3-15200k",
    TC,
    R::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
    "Transcode",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-dvhe.08-eac3-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mkv-dvhe.05-eac3-28000k",
    TC,
    R::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
    "Transcode",
    ""
)]
#[case("Tizen4-4K-5.1", "numstreams-32", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen4-4K-5.1",
    "numstreams-33",
    TC,
    R::STREAM_COUNT_EXCEEDS_LIMIT,
    "Remux",
    ""
)]
// WebOS 23
#[case(
    "WebOS-23",
    "mkv-dvhe.08-eac3-15200k",
    TC,
    R::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case("WebOS-23", "mp4-dvh1.05-eac3-15200k", DP, NONE, "DirectStream", "")]
#[case("WebOS-23", "mp4-dvhe.08-eac3-15200k", DP, NONE, "DirectStream", "")]
#[case(
    "WebOS-23",
    "mkv-dvhe.05-eac3-28000k",
    TC,
    R::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
    "Remux",
    ""
)]
fn build_video_item_simple(
    #[case] device: &str,
    #[case] source: &str,
    #[case] play_method: Option<PlayMethod>,
    #[case] why: TranscodeReasons,
    #[case] transcode_mode: &str,
    #[case] transcode_protocol: &str,
) {
    run_simple(
        device,
        source,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
}

fn run_first_explicit(
    device: &str,
    source: &str,
    play_method: Option<PlayMethod>,
    why: TranscodeReasons,
    transcode_mode: &str,
    transcode_protocol: &str,
) {
    let mut options = get_media_options(device, source);
    options.audio_stream_index = Some(1);
    let stream_count = options.media_sources[0].media_streams.len();
    options.subtitle_stream_index = Some(i32::try_from(stream_count).unwrap() - 1);

    let expected_audio = options.audio_stream_index;
    let expected_sub = options.subtitle_stream_index;
    let stream_info = build_video_item_simple_test(
        &options,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
    assert_eq!(stream_info.audio_stream_index, expected_audio);
    assert_eq!(stream_info.subtitle_stream_index, expected_sub);
}

#[rstest]
#[case("Chrome", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aacExt-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case("Chrome", "mp4-hevc-aac-srt-15200k", DP, NONE, "DirectStream", "")]
#[case(
    "Chrome",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mkv-vp9-aac-srt-2600k",
    TC,
    R::CONTAINER_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Chrome", "mkv-vp9-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("Chrome", "mkv-vp9-vorbis-vtt-2600k", DP, NONE, "Remux", "HLS.mp4")]
#[case("Firefox", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Firefox",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::VIDEO_CODEC_NOT_SUPPORTED,
    "Transcode",
    "HLS.mp4"
)]
#[case("Firefox", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case(
    "Firefox",
    "mkv-vp9-aac-srt-2600k",
    TC,
    R::CONTAINER_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Firefox", "mkv-vp9-ac3-srt-2600k", TC, R::CONTAINER_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("Firefox", "mkv-vp9-vorbis-vtt-2600k", DP, NONE, "Remux", "")]
#[case("SafariNext", "mp4-h264-aac-vtt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aacDef-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "SafariNext",
    "mp4-h264-ac3-aacExt-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("SafariNext", "mp4-h264-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "SafariNext",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::VIDEO_CODEC_TAG_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("SafariNext", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_TAG_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("SafariNext", "mp4-hevc-ac3-aacExt-srt-15200k", TC, R::VIDEO_CODEC_TAG_NOT_SUPPORTED.union(R::AUDIO_CHANNELS_NOT_SUPPORTED), "DirectStream", "HLS.mp4")]
#[case("AndroidPixel", "mp4-h264-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "AndroidPixel",
    "mp4-h264-ac3-aacDef-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("AndroidPixel", "mp4-h264-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("AndroidPixel", "mp4-hevc-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
#[case("AndroidPixel", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::CONTAINER_BITRATE_EXCEEDS_LIMIT), "Transcode", "")]
#[case("Yatse", "mp4-h264-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "Yatse",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "Yatse",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "Yatse",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Yatse", "mp4-hevc-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("Yatse", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case("RokuSSPlus", "mp4-h264-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "RokuSSPlus",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "RokuSSPlus",
    "mp4-h264-ac3-aacDef-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "RokuSSPlus",
    "mp4-h264-ac3-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("RokuSSPlus", "mp4-hevc-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case("JellyfinMediaPlayer", "mp4-h264-aac-vtt-2600k", DP, NONE, "Remux", "")]
#[case(
    "JellyfinMediaPlayer",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "Remux",
    ""
)]
#[case("JellyfinMediaPlayer", "mp4-h264-ac3-srt-2600k", DP, NONE, "Remux", "")]
#[case(
    "JellyfinMediaPlayer",
    "mp4-hevc-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::CONTAINER_BITRATE_EXCEEDS_LIMIT,
    "Transcode",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "JellyfinMediaPlayer",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mkv-vp9-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mkv-vp9-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case("AndroidTVExoPlayer", "mkv-vp9-vorbis-vtt-2600k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::AUDIO_CODEC_NOT_SUPPORTED), "Transcode", "")]
#[case(
    "Tizen3-stereo",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-dts-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-truehd-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Tizen3-stereo", "mkv-vp9-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("Tizen3-stereo", "mkv-vp9-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen3-stereo",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-aac-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-srt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-dts-srt-2600k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-truehd-srt-15200k",
    TC,
    R::AUDIO_CODEC_NOT_SUPPORTED,
    "DirectStream",
    ""
)]
#[case("Tizen4-4K-5.1", "mkv-vp9-aac-srt-2600k", DP, NONE, "DirectStream", "")]
#[case("Tizen4-4K-5.1", "mkv-vp9-ac3-srt-2600k", DP, NONE, "DirectStream", "")]
#[case(
    "Tizen4-4K-5.1",
    "mkv-vp9-vorbis-vtt-2600k",
    DP,
    NONE,
    "DirectStream",
    ""
)]
fn build_video_item_with_first_explicit_stream(
    #[case] device: &str,
    #[case] source: &str,
    #[case] play_method: Option<PlayMethod>,
    #[case] why: TranscodeReasons,
    #[case] transcode_mode: &str,
    #[case] transcode_protocol: &str,
) {
    run_first_explicit(
        device,
        source,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
}

fn run_direct_play_explicit(
    device: &str,
    source: &str,
    play_method: Option<PlayMethod>,
    why: TranscodeReasons,
    transcode_mode: &str,
    transcode_protocol: &str,
) {
    let mut options = get_media_options(device, source);
    let stream_count = options.media_sources[0].media_streams.len();
    if stream_count > 0 {
        options.audio_stream_index = Some(i32::try_from(stream_count).unwrap() - 2);
        options.subtitle_stream_index = Some(i32::try_from(stream_count).unwrap() - 1);
    }

    let expected_audio = options.audio_stream_index;
    let expected_sub = options.subtitle_stream_index;
    let stream_info = build_video_item_simple_test(
        &options,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
    assert_eq!(stream_info.audio_stream_index, expected_audio);
    assert_eq!(stream_info.subtitle_stream_index, expected_sub);
}

#[rstest]
#[case(
    "Chrome",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aac-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-h264-ac3-aacExt-srt-2600k",
    TC,
    R::AUDIO_IS_EXTERNAL,
    "DirectStream",
    "HLS.mp4"
)]
#[case(
    "Chrome",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "Firefox",
    "mp4-h264-ac3-aac-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    "HLS.mp4"
)]
#[case("Firefox", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::SECONDARY_AUDIO_NOT_SUPPORTED), "Transcode", "HLS.mp4")]
#[case(
    "Yatse",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Yatse",
    "mp4-h264-ac3-aac-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case("Yatse", "mp4-hevc-ac3-aac-srt-15200k", TC, R::VIDEO_CODEC_NOT_SUPPORTED.union(R::SECONDARY_AUDIO_NOT_SUPPORTED), "Transcode", "")]
#[case("RokuSSPlus", "mp4-h264-ac3-aac-srt-2600k", DP, NONE, "Remux", "")]
#[case("RokuSSPlus", "mp4-hevc-ac3-aac-srt-15200k", DP, NONE, "Remux", "")]
#[case(
    "Chrome",
    "no-streams",
    TC,
    R::VIDEO_CODEC_NOT_SUPPORTED,
    "Transcode",
    "HLS.mp4"
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-h264-ac3-aac-srt-2600k",
    DP,
    NONE,
    "Remux",
    ""
)]
#[case(
    "AndroidTVExoPlayer",
    "mp4-hevc-ac3-aac-srt-15200k",
    DP,
    NONE,
    "Remux",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-h264-ac3-aac-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Tizen3-stereo",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-h264-ac3-aac-aac-srt-2600k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "Tizen4-4K-5.1",
    "mp4-hevc-ac3-aac-srt-15200k",
    TC,
    R::SECONDARY_AUDIO_NOT_SUPPORTED,
    "Remux",
    ""
)]
#[case(
    "TranscodeMedia",
    "mp4-h264-ac3-aac-srt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.mp4"
)]
#[case(
    "TranscodeMedia",
    "mp4-h264-ac3-aac-mp3-srt-2600k",
    TC,
    R::DIRECT_PLAY_ERROR,
    "Remux",
    "HLS.ts"
)]
fn build_video_item_with_direct_play_explicit_streams(
    #[case] device: &str,
    #[case] source: &str,
    #[case] play_method: Option<PlayMethod>,
    #[case] why: TranscodeReasons,
    #[case] transcode_mode: &str,
    #[case] transcode_protocol: &str,
) {
    run_direct_play_explicit(
        device,
        source,
        play_method,
        why,
        transcode_mode,
        transcode_protocol,
    );
}

// --- GetSubtitleProfile tests ---

use ferrofin_model::dlna::{SubtitleDeliveryMethod, SubtitleProfile};
use ferrofin_model::entities_media::MediaStream;

struct ConfigurableTranscoderSupport {
    can_extract: bool,
}

impl TranscoderSupport for ConfigurableTranscoderSupport {
    fn can_encode_to_audio_codec(&self, _codec: &str) -> bool {
        false
    }
    fn can_encode_to_subtitle_codec(&self, _codec: &str) -> bool {
        false
    }
    fn can_extract_subtitles(&self, _codec: &str) -> bool {
        self.can_extract
    }
}

#[rstest]
// EnableSubtitleExtraction = false, internal subtitles
#[case(
    "srt",
    "srt",
    false,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::Encode
)]
#[case(
    "srt",
    "srt",
    false,
    false,
    PlayMethod::DirectPlay,
    SubtitleDeliveryMethod::External
)]
#[case(
    "pgssub",
    "pgssub",
    false,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::Encode
)]
#[case(
    "pgssub",
    "pgssub",
    false,
    false,
    PlayMethod::DirectPlay,
    SubtitleDeliveryMethod::External
)]
#[case(
    "pgssub",
    "srt",
    false,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::Encode
)]
// EnableSubtitleExtraction = false, external subtitles
#[case(
    "srt",
    "srt",
    false,
    true,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::External
)]
// EnableSubtitleExtraction = true, internal subtitles
#[case(
    "srt",
    "srt",
    true,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::External
)]
#[case(
    "pgssub",
    "pgssub",
    true,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::External
)]
#[case(
    "pgssub",
    "pgssub",
    true,
    false,
    PlayMethod::DirectPlay,
    SubtitleDeliveryMethod::External
)]
#[case(
    "pgssub",
    "srt",
    true,
    false,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::Encode
)]
// EnableSubtitleExtraction = true, external subtitles
#[case(
    "srt",
    "srt",
    true,
    true,
    PlayMethod::Transcode,
    SubtitleDeliveryMethod::External
)]
fn get_subtitle_profile_respects_extraction_setting(
    #[case] codec: &str,
    #[case] profile_format: &str,
    #[case] enable_subtitle_extraction: bool,
    #[case] is_external: bool,
    #[case] play_method: PlayMethod,
    #[case] expected_method: SubtitleDeliveryMethod,
) {
    let media_source = MediaSourceInfo::default();
    let mut subtitle_stream = MediaStream {
        stream_type: MediaStreamType::Subtitle,
        index: 0,
        is_external,
        codec: Some(codec.to_owned()),
        supports_external_stream: MediaStream::is_text_format(Some(codec)),
        ..MediaStream::default()
    };
    if is_external {
        subtitle_stream.path = Some(format!("/media/sub.{codec}"));
    }

    let subtitle_profiles = vec![SubtitleProfile {
        format: Some(profile_format.to_owned()),
        method: SubtitleDeliveryMethod::External,
        ..SubtitleProfile::default()
    }];

    let support = ConfigurableTranscoderSupport {
        can_extract: enable_subtitle_extraction,
    };

    let result = StreamBuilder::get_subtitle_profile(
        &media_source,
        &subtitle_stream,
        &subtitle_profiles,
        play_method,
        &support,
        None,
        None,
    );

    assert_eq!(expected_method, result.method);
}

#[rstest]
#[case(false, None, true, SubtitleDeliveryMethod::External)]
#[case(false, None, false, SubtitleDeliveryMethod::Encode)]
#[case(true, Some("/media/sub.mks"), true, SubtitleDeliveryMethod::External)]
#[case(true, Some("/media/sub.idx"), true, SubtitleDeliveryMethod::Encode)]
#[case(true, Some("/media/sub.sub"), true, SubtitleDeliveryMethod::Encode)]
fn get_subtitle_profile_matches_vobsub_mks_only_when_delivered_as_mks(
    #[case] is_external: bool,
    #[case] path: Option<&str>,
    #[case] enable_subtitle_extraction: bool,
    #[case] expected_method: SubtitleDeliveryMethod,
) {
    let media_source = MediaSourceInfo::default();
    let subtitle_stream = MediaStream {
        stream_type: MediaStreamType::Subtitle,
        index: 0,
        is_external,
        path: path.map(str::to_owned),
        codec: Some("vobsub".to_owned()),
        ..MediaStream::default()
    };

    let subtitle_profiles = vec![SubtitleProfile {
        format: Some("vobsub".to_owned()),
        container: Some("mks".to_owned()),
        method: SubtitleDeliveryMethod::External,
        ..SubtitleProfile::default()
    }];

    let support = ConfigurableTranscoderSupport {
        can_extract: enable_subtitle_extraction,
    };

    let result = StreamBuilder::get_subtitle_profile(
        &media_source,
        &subtitle_stream,
        &subtitle_profiles,
        PlayMethod::Transcode,
        &support,
        None,
        None,
    );

    assert_eq!(expected_method, result.method);
}

#[rstest]
// External text subs embedded into MKV when transcoding (#16403)
#[case(
    "srt",
    true,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
#[case(
    "ass",
    true,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
// External graphical subs embedded into MKV when transcoding
#[case(
    "pgssub",
    true,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
#[case(
    "dvdsub",
    true,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
// External subs remain external when transcoding to non-MKV containers
#[case(
    "srt",
    true,
    PlayMethod::Transcode,
    "mp4",
    Some(MediaStreamProtocol::hls),
    SubtitleDeliveryMethod::External
)]
#[case(
    "srt",
    true,
    PlayMethod::Transcode,
    "ts",
    Some(MediaStreamProtocol::hls),
    SubtitleDeliveryMethod::External
)]
// External subs remain external during DirectPlay even with MKV
#[case(
    "srt",
    true,
    PlayMethod::DirectPlay,
    "mkv",
    None,
    SubtitleDeliveryMethod::External
)]
// Internal subs still embedded into MKV when transcoding (existing behavior)
#[case(
    "srt",
    false,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
#[case(
    "pgssub",
    false,
    PlayMethod::Transcode,
    "mkv",
    Some(MediaStreamProtocol::http),
    SubtitleDeliveryMethod::Embed
)]
fn get_subtitle_profile_returns_expected_delivery_method(
    #[case] codec: &str,
    #[case] is_external: bool,
    #[case] play_method: PlayMethod,
    #[case] output_container: &str,
    #[case] transcoding_sub_protocol: Option<MediaStreamProtocol>,
    #[case] expected_method: SubtitleDeliveryMethod,
) {
    let media_source = MediaSourceInfo::default();
    let subtitle_stream = MediaStream {
        codec: Some(codec.to_owned()),
        language: Some("eng".to_owned()),
        is_external,
        stream_type: MediaStreamType::Subtitle,
        supports_external_stream: true,
        ..MediaStream::default()
    };

    let subtitle_profiles = vec![
        SubtitleProfile {
            format: Some(codec.to_owned()),
            method: SubtitleDeliveryMethod::Embed,
            ..SubtitleProfile::default()
        },
        SubtitleProfile {
            format: Some(codec.to_owned()),
            method: SubtitleDeliveryMethod::External,
            ..SubtitleProfile::default()
        },
    ];

    let support = ConfigurableTranscoderSupport { can_extract: true };

    let result = StreamBuilder::get_subtitle_profile(
        &media_source,
        &subtitle_stream,
        &subtitle_profiles,
        play_method,
        &support,
        Some(output_container),
        transcoding_sub_protocol,
    );

    assert_eq!(expected_method, result.method);
}
