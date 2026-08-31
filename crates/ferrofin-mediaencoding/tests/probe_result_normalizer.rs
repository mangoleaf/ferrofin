//! Transliteration of `Jellyfin.MediaEncoding.Tests.Probing
//! .ProbeResultNormalizerTests`. Expected values are the C# oracle verbatim.

use chrono::{TimeZone, Utc};
use ferrofin_mediaencoding::probing::{
    InternalMediaInfoResult, PassthroughLocalization, ProbeResultNormalizer,
    get_estimated_audio_bitrate, get_frame_rate, is_near_square_pixel_sar,
};
use ferrofin_model::data::PersonKind;
use ferrofin_model::entities::{MediaStreamType, VideoType};
use ferrofin_model::entities_media::AudioSpatialFormat;
use ferrofin_model::media_info::{MediaInfo, MediaProtocol};
use rstest::rstest;

fn normalizer() -> ProbeResultNormalizer<PassthroughLocalization> {
    ProbeResultNormalizer::new(PassthroughLocalization)
}

fn load(name: &str) -> InternalMediaInfoResult {
    let path = format!("{}/tests/data/probing/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn get_media_info(
    name: &str,
    video_type: Option<VideoType>,
    is_audio: bool,
    path: &str,
) -> MediaInfo {
    normalizer().get_media_info(load(name), video_type, is_audio, path, MediaProtocol::File)
}

// GetFrameRate_Success
#[rstest]
#[case("2997/125", Some(23.976_f32))]
#[case("1/50", Some(0.02_f32))]
#[case("25/1", Some(25_f32))]
#[case("120/1", Some(120_f32))]
#[case("1704753000/71073479", Some(23.985_782_f32))]
#[case("0/0", None)]
#[case("1/1000", Some(0.001_f32))]
#[case("1/90000", Some(1.111_111_1E-5_f32))]
#[case("1/48000", Some(2.083_333_3E-5_f32))]
fn get_frame_rate_success(#[case] value: &str, #[case] expected: Option<f32>) {
    assert_eq!(get_frame_rate(Some(value)), expected);
}

// IsNearSquarePixelSar_DetectsCorrectly
#[rstest]
#[case(Some("1:1"), true)]
#[case(Some("3201:3200"), true)]
#[case(Some("1215:1216"), true)]
#[case(Some("1001:1000"), true)]
#[case(Some("16:15"), false)]
#[case(Some("8:9"), false)]
#[case(Some("32:27"), false)]
#[case(Some("10:11"), false)]
#[case(Some("64:45"), false)]
#[case(Some("4:3"), false)]
#[case(Some("0:1"), false)]
#[case(Some(""), false)]
#[case(None, false)]
fn is_near_square_pixel_sar_detects_correctly(#[case] sar: Option<&str>, #[case] expected: bool) {
    assert_eq!(is_near_square_pixel_sar(sar), expected);
}

// GetEstimatedAudioBitrate_ReturnsExpected
#[rstest]
#[case("aac", None, Some(2), Some(192_000))]
#[case("mp3", None, Some(2), Some(192_000))]
#[case("mp2", None, Some(2), Some(192_000))]
#[case("aac", None, Some(6), Some(320_000))]
#[case("ac3", None, Some(2), Some(192_000))]
#[case("eac3", None, Some(6), Some(640_000))]
#[case("opus", None, Some(2), Some(128_000))]
#[case("vorbis", None, Some(6), Some(320_000))]
#[case("wmav2", None, Some(2), Some(192_000))]
#[case("dts", None, Some(2), Some(768_000))]
#[case("dts", Some("DTS"), Some(6), Some(1_509_000))]
#[case("dts", Some("DTS-HD HRA"), Some(8), Some(1_509_000))]
#[case("dts", Some("DTS-HD MA"), Some(6), Some(4_200_000))]
#[case("dts", Some("DTS-HD MA + DTS:X"), Some(8), Some(5_600_000))]
#[case("flac", None, Some(2), Some(960_000))]
#[case("flac", None, Some(6), Some(2_880_000))]
#[case("flac", None, Some(8), Some(3_840_000))]
#[case("alac", None, Some(6), Some(2_880_000))]
#[case("truehd", None, Some(2), Some(1_400_000))]
#[case("truehd", None, Some(6), Some(4_200_000))]
#[case("truehd", Some("Dolby TrueHD + Dolby Atmos"), Some(8), Some(5_600_000))]
#[case("aac", None, Some(3), Some(320_000))]
#[case("ac3", None, Some(4), Some(640_000))]
#[case("AAC", None, Some(2), Some(192_000))]
#[case("pcm_s16le", None, Some(2), None)]
#[case("aac", None, None, None)]
fn get_estimated_audio_bitrate_returns_expected(
    #[case] codec: &str,
    #[case] profile: Option<&str>,
    #[case] channels: Option<i32>,
    #[case] expected: Option<i32>,
) {
    assert_eq!(
        get_estimated_audio_bitrate(Some(codec), profile, channels),
        expected
    );
}

#[test]
fn get_media_info_metadata_success() {
    let res = get_media_info(
        "video_metadata.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_metadata.mkv",
    );

    assert_eq!(res.media_source.container.as_deref(), Some("mkv"));
    assert_eq!(res.media_source.media_streams.len(), 3);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs.aspect_ratio.as_deref(), Some("4:3"));
    assert_eq!(vs.average_frame_rate, Some(25.0));
    assert_eq!(vs.bit_depth, Some(8));
    assert_eq!(vs.bit_rate, None);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.codec_time_base.as_deref(), Some("1/50"));
    assert_eq!(vs.height, Some(240));
    assert_eq!(vs.width, Some(320));
    assert_eq!(vs.index, 0);
    assert_eq!(vs.is_anamorphic, Some(false));
    assert_eq!(vs.is_avc, Some(true));
    assert!(vs.is_default);
    assert!(!vs.is_external);
    assert!(!vs.is_forced);
    assert!(!vs.is_hearing_impaired);
    assert!(!vs.is_interlaced);
    assert_eq!(vs.level, Some(13.0));
    assert_eq!(vs.nal_length_size.as_deref(), Some("4"));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv444p"));
    assert_eq!(vs.profile.as_deref(), Some("High 4:4:4 Predictive"));
    assert_eq!(vs.real_frame_rate, Some(25.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.time_base.as_deref(), Some("1/1000"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.dv_version_major, Some(1));
    assert_eq!(vs.dv_version_minor, Some(0));
    assert_eq!(vs.dv_profile, Some(5));
    assert_eq!(vs.dv_level, Some(6));
    assert_eq!(vs.rpu_present_flag, Some(1));
    assert_eq!(vs.el_present_flag, Some(0));
    assert_eq!(vs.bl_present_flag, Some(1));
    assert_eq!(vs.dv_bl_signal_compatibility_id, Some(0));
    assert_eq!(vs.rotation, Some(-180));

    let audio1 = &res.media_source.media_streams[1];
    assert_eq!(audio1.codec.as_deref(), Some("eac3"));
    assert_eq!(
        audio1.audio_spatial_format(),
        AudioSpatialFormat::DolbyAtmos
    );

    let audio2 = &res.media_source.media_streams[2];
    assert_eq!(audio2.codec.as_deref(), Some("dts"));
    assert_eq!(audio2.audio_spatial_format(), AudioSpatialFormat::Dtsx);

    assert!(res.chapters.is_empty());
    assert_eq!(res.overview.as_deref(), Some("Just color bars"));
}

#[test]
fn get_media_info_mp4_metadata_success() {
    let res = get_media_info(
        "video_mp4_metadata.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_mp4_metadata.mkv",
    );

    assert_eq!(res.media_source.media_streams.len(), 6);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs, &res.media_source.media_streams[0]);
    assert_eq!(vs.index, 0);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.profile.as_deref(), Some("High"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.height, Some(358));
    assert_eq!(vs.width, Some(720));
    assert_eq!(vs.aspect_ratio.as_deref(), Some("2.40:1"));
    assert_eq!(vs.is_anamorphic, Some(true));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(vs.level, Some(31.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.is_avc, Some(true));
    assert_eq!(vs.real_frame_rate, Some(120.0));
    assert_eq!(vs.time_base.as_deref(), Some("1/90000"));
    assert_eq!(vs.bit_rate, Some(1_147_365));
    assert_eq!(vs.bit_depth, Some(8));
    assert!(vs.is_default);
    assert_eq!(vs.language.as_deref(), Some("und"));

    let s1 = &res.media_source.media_streams[1];
    assert_eq!(s1.stream_type, MediaStreamType::Audio);
    assert_eq!(s1.codec.as_deref(), Some("aac"));
    assert_eq!(s1.channels, Some(7));
    assert!(s1.is_default);
    assert_eq!(s1.language.as_deref(), Some("eng"));
    assert_eq!(s1.title.as_deref(), Some("Surround 6.1"));

    let s2 = &res.media_source.media_streams[2];
    assert_eq!(s2.stream_type, MediaStreamType::Audio);
    assert_eq!(s2.codec.as_deref(), Some("aac"));
    assert_eq!(s2.channels, Some(2));
    assert!(!s2.is_default);
    assert_eq!(s2.language.as_deref(), Some("eng"));
    assert_eq!(s2.title.as_deref(), Some("Commentary"));

    let s3 = &res.media_source.media_streams[3];
    assert_eq!(s3.language.as_deref(), Some("spa"));
    assert_eq!(s3.stream_type, MediaStreamType::Subtitle);
    assert_eq!(s3.codec.as_deref(), Some("DVDSUB"));
    assert_eq!(s3.title, None);
    assert!(!s3.is_hearing_impaired);

    let s4 = &res.media_source.media_streams[4];
    assert_eq!(s4.language.as_deref(), Some("eng"));
    assert_eq!(s4.stream_type, MediaStreamType::Subtitle);
    assert_eq!(s4.codec.as_deref(), Some("mov_text"));
    assert_eq!(s4.title, None);
    assert!(s4.is_hearing_impaired);

    let s5 = &res.media_source.media_streams[5];
    assert_eq!(s5.language.as_deref(), Some("eng"));
    assert_eq!(s5.stream_type, MediaStreamType::Subtitle);
    assert_eq!(s5.codec.as_deref(), Some("mov_text"));
    assert_eq!(s5.title.as_deref(), Some("Commentary"));
    assert!(!s5.is_hearing_impaired);
}

#[test]
fn get_media_info_ts_success() {
    let res = get_media_info(
        "video_ts.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_metadata.mkv",
    );
    assert_eq!(res.media_source.media_streams.len(), 2);
    assert_eq!(res.media_source.media_streams[0].is_avc, Some(false));
}

#[test]
fn get_media_info_webm_success() {
    let res = get_media_info(
        "video_webm.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_metadata.webm",
    );
    assert_eq!(res.media_source.container.as_deref(), Some("mkv,webm"));
    assert_eq!(res.media_source.media_streams.len(), 2);
    assert_eq!(res.media_source.media_streams[0].width, Some(540));
    assert_eq!(res.media_source.media_streams[0].height, Some(360));
}

#[test]
fn get_media_info_webm_like_mkv() {
    let res = get_media_info(
        "video_web_like_mkv_with_subtitle.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_metadata.mkv",
    );
    assert_eq!(res.media_source.container.as_deref(), Some("mkv"));
    assert_eq!(res.media_source.media_streams.len(), 3);
}

#[test]
fn get_media_info_progressive_video_no_field_order_success() {
    let res = get_media_info(
        "video_progressive_no_field_order.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_progressive_no_field_order.mp4",
    );
    assert_eq!(res.media_source.media_streams.len(), 2);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs, &res.media_source.media_streams[0]);
    assert_eq!(vs.index, 0);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.profile.as_deref(), Some("Main"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.height, Some(1080));
    assert_eq!(vs.width, Some(1920));
    assert!(!vs.is_interlaced);
    assert_eq!(vs.aspect_ratio.as_deref(), Some("16:9"));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(vs.level, Some(41.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.is_avc, Some(true));
    assert_eq!(vs.real_frame_rate, Some(23.976_025_f32));
    assert_eq!(vs.time_base.as_deref(), Some("1/24000"));
    assert_eq!(vs.bit_rate, Some(3_948_341));
    assert_eq!(vs.bit_depth, Some(8));
    assert!(vs.is_default);
}

#[test]
fn get_media_info_progressive_video_no_field_order2_success() {
    let res = get_media_info(
        "video_progressive_no_field_order2.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_progressive_no_field_order2.mp4",
    );
    assert_eq!(res.media_source.media_streams.len(), 1);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs, &res.media_source.media_streams[0]);
    assert_eq!(vs.index, 0);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.profile.as_deref(), Some("High"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.height, Some(720));
    assert_eq!(vs.width, Some(1280));
    assert!(!vs.is_interlaced);
    assert_eq!(vs.aspect_ratio.as_deref(), Some("16:9"));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(vs.level, Some(31.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.is_avc, Some(true));
    assert_eq!(vs.real_frame_rate, Some(25.0));
    assert_eq!(vs.time_base.as_deref(), Some("1/12800"));
    assert_eq!(vs.bit_rate, Some(53_288));
    assert_eq!(vs.bit_depth, Some(8));
    assert!(vs.is_default);
}

#[test]
fn get_media_info_interlaced_video_success() {
    let res = get_media_info(
        "video_interlaced.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_interlaced.mp4",
    );
    assert_eq!(res.media_source.media_streams.len(), 1);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs, &res.media_source.media_streams[0]);
    assert_eq!(vs.index, 0);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.profile.as_deref(), Some("High"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.height, Some(720));
    assert_eq!(vs.width, Some(1280));
    assert!(vs.is_interlaced);
    assert_eq!(vs.aspect_ratio.as_deref(), Some("16:9"));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(vs.level, Some(40.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.is_avc, Some(true));
    assert_eq!(vs.real_frame_rate, Some(25.0));
    assert_eq!(vs.time_base.as_deref(), Some("1/12800"));
    assert_eq!(vs.bit_rate, Some(56_945));
    assert_eq!(vs.bit_depth, Some(8));
    assert!(vs.is_default);
}

#[test]
fn get_media_info_missing_video_bitrate_estimated_from_container() {
    let res = get_media_info(
        "video_missing_video_bitrate.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_missing_video_bitrate.mp4",
    );

    assert_eq!(res.media_source.media_streams.len(), 2);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs.stream_type, MediaStreamType::Video);

    let audio = res
        .media_source
        .media_streams
        .iter()
        .find(|s| s.stream_type == MediaStreamType::Audio)
        .expect("audio stream");
    assert_eq!(audio.bit_rate, Some(128_000));

    assert_eq!(vs.bit_rate, Some(5_000_000));
    assert_eq!(res.media_source.bitrate, Some(5_128_000));
}

#[test]
fn get_media_info_nanosecond_duration_tag_bitrate_computed_from_bytes() {
    let res = get_media_info(
        "video_nanosecond_duration_bitrate.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_nanosecond_duration_bitrate.mkv",
    );

    let vs = res.media_source.video_stream().expect("video stream");
    // 10000000 bytes * 8 / 100 seconds.
    assert_eq!(vs.bit_rate, Some(800_000));
}

#[test]
fn get_media_info_missing_video_bitrate_unknown_audio_not_estimated() {
    let res = get_media_info(
        "video_missing_video_bitrate_unknown_audio.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_missing_video_bitrate_unknown_audio.mp4",
    );

    assert_eq!(res.media_source.media_streams.len(), 2);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs.bit_rate, None);

    let audio = res
        .media_source
        .media_streams
        .iter()
        .find(|s| s.stream_type == MediaStreamType::Audio)
        .expect("audio stream");
    assert_eq!(audio.bit_rate, None);

    assert_eq!(res.media_source.bitrate, Some(5_128_000));
}

#[test]
fn get_media_info_video_with_single_frame_mjpeg_success() {
    let res = get_media_info(
        "video_single_frame_mjpeg.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/video_interlaced.mp4",
    );

    assert_eq!(res.media_source.media_streams.len(), 3);

    let vs = res.media_source.video_stream().expect("video stream");
    assert_eq!(vs, &res.media_source.media_streams[0]);
    assert_eq!(vs.index, 0);
    assert_eq!(vs.codec.as_deref(), Some("h264"));
    assert_eq!(vs.profile.as_deref(), Some("High"));
    assert_eq!(vs.stream_type, MediaStreamType::Video);
    assert_eq!(vs.height, Some(1080));
    assert_eq!(vs.width, Some(1920));
    assert!(!vs.is_interlaced);
    assert_eq!(vs.aspect_ratio.as_deref(), Some("16:9"));
    assert_eq!(vs.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(vs.level, Some(42.0));
    assert_eq!(vs.ref_frames, Some(1));
    assert_eq!(vs.is_avc, Some(true));
    assert_eq!(vs.real_frame_rate, Some(50.0));
    assert_eq!(vs.time_base.as_deref(), Some("1/1000"));
    assert_eq!(vs.bit_depth, Some(8));
    assert!(vs.is_default);

    let mjpeg = &res.media_source.media_streams[2];
    assert_eq!(mjpeg.codec.as_deref(), Some("mjpeg"));
    // Every stream in this fixture carries the junk tag `[0][0][0][0]`, which
    // `GetMediaStream` filters out (master `ProbeResultNormalizer.cs:716-720`),
    // so the mjpeg stream has no codec tag to discriminate on and stays an
    // embedded image.
    assert_eq!(mjpeg.codec_tag, None);
    assert_eq!(mjpeg.stream_type, MediaStreamType::EmbeddedImage);

    // ffprobe emits no `level`, `is_avc`, `width` or `height` on an audio
    // stream. Upstream master's fields are all nullable
    // (`MediaStreamInfo.cs:93/107/170/233`), so they stay null instead of the
    // fabricated `0`/`false` v10.11.8's non-nullable value types produce, and
    // `IsAVC` is assigned in the video arm only (master `:794`).
    let audio = &res.media_source.media_streams[1];
    assert_eq!(audio.stream_type, MediaStreamType::Audio);
    assert_eq!(audio.level, None);
    assert_eq!(audio.is_avc, None);
    assert_eq!(audio.width, None);
    assert_eq!(audio.height, None);
}

/// An `mjpeg` video stream carrying a REAL codec tag is a video stream, not an
/// embedded image — `GetMediaStream`'s only discriminator between the two
/// (master `ProbeResultNormalizer.cs:809-820`). This is unreachable if the
/// `codec_tag_string` key fails to deserialize.
#[test]
fn get_media_stream_mjpeg_with_a_real_codec_tag_is_video() {
    let json = r#"{"streams":[
        {"index":0,"codec_name":"mjpeg","codec_type":"video","codec_tag_string":"MJPG",
         "width":1920,"height":1080},
        {"index":1,"codec_name":"mjpeg","codec_type":"video","codec_tag_string":"[0][0][0][0]",
         "width":600,"height":400}
    ],"format":{}}"#;
    let parsed: InternalMediaInfoResult = serde_json::from_str(json).expect("probe json parses");
    let res = normalizer().get_media_info(
        parsed,
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/mjpeg.mkv",
        MediaProtocol::File,
    );

    let tagged = &res.media_source.media_streams[0];
    assert_eq!(tagged.codec_tag.as_deref(), Some("MJPG"));
    assert_eq!(tagged.stream_type, MediaStreamType::Video);

    let junk = &res.media_source.media_streams[1];
    assert_eq!(junk.codec_tag, None);
    assert_eq!(junk.stream_type, MediaStreamType::EmbeddedImage);
}

/// A text subtitle has no ffprobe `width`/`height`/`level`/`is_avc`, so on
/// upstream master — where all four fields are nullable
/// (`MediaStreamInfo.cs:93/107/170/233`) — the wire carries null, not the
/// `0`/`false` v10.11.8's non-nullable value types fabricate. A graphical
/// subtitle that *does* carry dimensions still reports them, because master
/// assigns Width/Height in the shared initializer
/// (`ProbeResultNormalizer.cs:706-707`).
#[test]
fn get_media_stream_subtitle_reports_only_the_dimensions_ffprobe_gave() {
    let json = r#"{"streams":[
        {"index":0,"codec_name":"subrip","codec_type":"subtitle"},
        {"index":1,"codec_name":"dvd_subtitle","codec_type":"subtitle",
         "width":720,"height":480}
    ],"format":{}}"#;
    let parsed: InternalMediaInfoResult = serde_json::from_str(json).expect("probe json parses");
    let res = normalizer().get_media_info(
        parsed,
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/subs.mkv",
        MediaProtocol::File,
    );

    let text = &res.media_source.media_streams[0];
    assert_eq!(text.stream_type, MediaStreamType::Subtitle);
    assert_eq!(text.width, None);
    assert_eq!(text.height, None);
    assert_eq!(text.level, None);
    assert_eq!(text.is_avc, None);

    let graphical = &res.media_source.media_streams[1];
    assert_eq!(graphical.stream_type, MediaStreamType::Subtitle);
    assert_eq!(graphical.width, Some(720));
    assert_eq!(graphical.height, Some(480));
}

/// `GetMediaAttachment` has no junk-tag filter (master
/// `ProbeResultNormalizer.cs:671-675`), so an attachment keeps `[0][0][0][0]`.
#[test]
fn get_media_attachment_keeps_the_junk_codec_tag() {
    let json = r#"{"streams":[
        {"index":0,"codec_name":"ttf","codec_type":"attachment",
         "codec_tag_string":"[0][0][0][0]","tags":{"filename":"font.ttf"}}
    ],"format":{}}"#;
    let parsed: InternalMediaInfoResult = serde_json::from_str(json).expect("probe json parses");
    let res = normalizer().get_media_info(
        parsed,
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/attach.mkv",
        MediaProtocol::File,
    );

    let att = &res.media_source.media_attachments[0];
    assert_eq!(att.codec.as_deref(), Some("ttf"));
    assert_eq!(att.codec_tag.as_deref(), Some("[0][0][0][0]"));
}

#[test]
fn get_media_info_music_video_success() {
    let res = get_media_info(
        "music_video_metadata.json",
        Some(VideoType::VideoFile),
        false,
        "Test Data/Probing/music_video.mkv",
    );

    assert_eq!(res.media_source.name.as_deref(), Some("The Title"));
    assert_eq!(res.forced_sort_name.as_deref(), Some("Title, The"));
    assert_eq!(res.artists.len(), 1);
    assert_eq!(res.artists[0], "The Artist");
    assert_eq!(res.album.as_deref(), Some("Album"));
    assert_eq!(res.production_year, Some(2021));
    assert!(res.premiere_date.is_some());
    assert_eq!(
        res.premiere_date,
        Some(Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).unwrap())
    );
}

#[test]
fn get_media_info_given_original_date_contains_only_year_success() {
    let res = get_media_info(
        "music_year_only_metadata.json",
        None,
        true,
        "Test Data/Probing/music.flac",
    );

    assert_eq!(res.media_source.name.as_deref(), Some("Baker Street"));
    assert_eq!(res.artists.len(), 1);
    assert_eq!(res.artists[0], "Gerry Rafferty");
    assert_eq!(res.album.as_deref(), Some("City to City"));
    assert_eq!(res.production_year, Some(1978));
    assert!(res.premiere_date.is_some());
    assert_eq!(
        res.premiere_date,
        Some(Utc.with_ymd_and_hms(1978, 1, 1, 0, 0, 0).unwrap())
    );
    assert!(res.genres.iter().any(|g| g == "Electronic"));
    assert!(res.genres.iter().any(|g| g == "Ambient"));
    assert!(res.genres.iter().any(|g| g == "Pop"));
    assert!(res.genres.iter().any(|g| g == "Jazz"));
}

#[test]
fn get_media_info_music_success() {
    let res = get_media_info(
        "music_metadata.json",
        None,
        true,
        "Test Data/Probing/music.flac",
    );

    assert_eq!(res.media_source.name.as_deref(), Some("UP NO MORE"));
    assert_eq!(res.artists.len(), 1);
    assert_eq!(res.artists[0], "TWICE");
    assert_eq!(res.album.as_deref(), Some("Eyes wide open"));
    assert_eq!(res.production_year, Some(2020));
    assert!(res.premiere_date.is_some());
    assert_eq!(
        res.premiere_date,
        Some(Utc.with_ymd_and_hms(2020, 10, 26, 0, 0, 0).unwrap())
    );

    assert_eq!(res.people.len(), 22);
    assert_eq!(res.people[0].name.as_deref(), Some("Krysta Youngs"));
    assert_eq!(res.people[0].type_, PersonKind::Composer);
    assert_eq!(res.people[1].name.as_deref(), Some("Julia Ross"));
    assert_eq!(res.people[1].type_, PersonKind::Composer);
    assert_eq!(res.people[2].name.as_deref(), Some("Yiwoomin"));
    assert_eq!(res.people[2].type_, PersonKind::Composer);
    assert_eq!(res.people[3].name.as_deref(), Some("Ji-hyo Park"));
    assert_eq!(res.people[3].type_, PersonKind::Lyricist);
    assert_eq!(res.people[4].name.as_deref(), Some("Yiwoomin"));
    assert_eq!(res.people[4].type_, PersonKind::Actor);
    assert_eq!(res.people[4].role.as_deref(), Some("Electric Piano"));

    assert_eq!(res.genres.len(), 4);
    assert!(res.genres.iter().any(|g| g == "Electronic"));
    assert!(res.genres.iter().any(|g| g == "Trance"));
    assert!(res.genres.iter().any(|g| g == "Dance"));
    assert!(res.genres.iter().any(|g| g == "Jazz"));
}
