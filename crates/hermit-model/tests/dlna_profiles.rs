//! Behavioral + wire-contract tests for the DLNA device-profile model.
//!
//! This unit has no dedicated xUnit tests upstream — the profile-matching
//! methods are exercised through `StreamBuilderTests` (ported in a later unit).
//! These tests lock the container/codec matching semantics that engine relies
//! on, plus the PascalCase/lowercase wire casing against the OpenAPI contract.

use hermit_model::data::MediaStreamProtocol;
use hermit_model::dlna::{
    CodecProfile, CodecType, ContainerProfile, DeviceProfile, DirectPlayProfile, DlnaProfileType,
    ProfileCondition, ProfileConditionType, ProfileConditionValue, ResolutionNormalizer,
    SubtitleDeliveryMethod, SubtitleProfile, TranscodingProfile,
};

// --- DirectPlayProfile matching ---------------------------------------------

#[test]
fn direct_play_supports_container() {
    let profile = DirectPlayProfile {
        container: "mp4,mkv".to_owned(),
        profile_type: DlnaProfileType::Video,
        ..Default::default()
    };
    assert!(profile.supports_container(Some("mkv")));
    assert!(profile.supports_container(Some("mp4")));
    assert!(!profile.supports_container(Some("avi")));
    // Empty profile container accepts everything (ContainerHelper semantics).
    let open = DirectPlayProfile::default();
    assert!(open.supports_container(Some("anything")));
}

#[test]
fn direct_play_video_codec_requires_video_type() {
    let video = DirectPlayProfile {
        container: "mp4".to_owned(),
        video_codec: Some("h264,hevc".to_owned()),
        profile_type: DlnaProfileType::Video,
        ..Default::default()
    };
    assert!(video.supports_video_codec(Some("h264")));
    assert!(!video.supports_video_codec(Some("vp9")));

    // An Audio-typed profile never supports video codecs.
    let audio = DirectPlayProfile {
        profile_type: DlnaProfileType::Audio,
        video_codec: Some("h264".to_owned()),
        ..Default::default()
    };
    assert!(!audio.supports_video_codec(Some("h264")));
}

#[test]
fn direct_play_audio_codec_allows_audio_and_video_types() {
    let video = DirectPlayProfile {
        profile_type: DlnaProfileType::Video,
        audio_codec: Some("aac,ac3".to_owned()),
        ..Default::default()
    };
    assert!(video.supports_audio_codec(Some("aac")));

    let audio = DirectPlayProfile {
        profile_type: DlnaProfileType::Audio,
        audio_codec: Some("mp3".to_owned()),
        ..Default::default()
    };
    assert!(audio.supports_audio_codec(Some("mp3")));
    assert!(!audio.supports_audio_codec(Some("flac")));

    // A Photo-typed profile supports neither.
    let photo = DirectPlayProfile {
        profile_type: DlnaProfileType::Photo,
        audio_codec: Some("mp3".to_owned()),
        ..Default::default()
    };
    assert!(!photo.supports_audio_codec(Some("mp3")));
}

// --- ContainerProfile matching ----------------------------------------------

#[test]
fn container_profile_matches_and_uses_hls_subcontainer() {
    let profile = ContainerProfile {
        profile_type: DlnaProfileType::Video,
        container: Some("mp4".to_owned()),
        ..Default::default()
    };
    assert!(profile.contains_container(Some("mp4"), false));
    assert!(!profile.contains_container(Some("mkv"), false));

    let hls = ContainerProfile {
        profile_type: DlnaProfileType::Video,
        container: Some("hls".to_owned()),
        sub_container: Some("ts".to_owned()),
        ..Default::default()
    };
    // With use_sub_container, an "hls" container matches against the sub.
    assert!(hls.contains_container(Some("ts"), true));
    assert!(!hls.contains_container(Some("ts"), false));
}

// --- CodecProfile matching --------------------------------------------------

#[test]
fn codec_profile_contains_any_codec() {
    let profile = CodecProfile {
        codec_type: CodecType::Video,
        codec: Some("h264,hevc".to_owned()),
        container: Some("mp4".to_owned()),
        ..Default::default()
    };
    assert!(profile.contains_any_codec(&["hevc"], Some("mp4"), false));
    assert!(profile.contains_any_codec(&["vp9", "h264"], Some("mp4"), false));
    assert!(!profile.contains_any_codec(&["vp9"], Some("mp4"), false));
    // Container mismatch fails even with a matching codec.
    assert!(!profile.contains_any_codec(&["h264"], Some("mkv"), false));
}

#[test]
fn codec_profile_contains_codec_single() {
    let profile = CodecProfile {
        codec_type: CodecType::Audio,
        codec: Some("aac".to_owned()),
        ..Default::default()
    };
    // Empty container accepts everything.
    assert!(profile.contains_codec(Some("aac"), Some("mp4"), false));
    assert!(!profile.contains_codec(Some("mp3"), Some("mp4"), false));
}

// --- SubtitleProfile language -----------------------------------------------

#[test]
fn subtitle_supports_language() {
    // No language restriction => supports everything.
    let any = SubtitleProfile {
        format: Some("srt".to_owned()),
        method: SubtitleDeliveryMethod::External,
        ..Default::default()
    };
    assert!(any.supports_language(Some("eng")));
    assert!(any.supports_language(None));

    let eng = SubtitleProfile {
        language: Some("eng,und".to_owned()),
        ..Default::default()
    };
    assert!(eng.supports_language(Some("eng")));
    // Missing language is treated as "und".
    assert!(eng.supports_language(None));
    assert!(!eng.supports_language(Some("fre")));
}

// --- ResolutionNormalizer ---------------------------------------------------

#[test]
fn resolution_keeps_dimensions_when_bitrate_not_reduced() {
    // output >= input and a dimension is present => untouched.
    let opts = ResolutionNormalizer::normalize(
        Some(1_000_000),
        2_000_000,
        2_000_000,
        Some(1920),
        Some(1080),
        None,
        false,
    );
    assert_eq!(opts.max_width, Some(1920));
    assert_eq!(opts.max_height, Some(1080));
}

#[test]
fn resolution_downscales_for_low_bitrate() {
    // A low output bitrate picks the first configuration (max_width 416).
    let opts = ResolutionNormalizer::normalize(
        Some(20_000_000),
        300_000,
        300_000,
        Some(1920),
        Some(1080),
        Some(30.0),
        false,
    );
    assert_eq!(opts.max_width, Some(416));
    // Width changed, so height is cleared.
    assert_eq!(opts.max_height, None);
}

// --- Wire contract ----------------------------------------------------------

#[test]
fn media_stream_protocol_is_lowercase() {
    assert_eq!(
        serde_json::to_string(&MediaStreamProtocol::http).unwrap(),
        "\"http\""
    );
    assert_eq!(
        serde_json::to_string(&MediaStreamProtocol::hls).unwrap(),
        "\"hls\""
    );
}

#[test]
fn device_profile_serializes_pascal_case_and_skips_none() {
    let profile = DeviceProfile {
        name: Some("Test".to_owned()),
        max_streaming_bitrate: Some(8_000_000),
        max_static_bitrate: None,
        music_streaming_transcoding_bitrate: None,
        max_static_music_bitrate: None,
        ..Default::default()
    };
    let json = serde_json::to_value(&profile).unwrap();
    assert_eq!(json["Name"], "Test");
    assert_eq!(json["MaxStreamingBitrate"], 8_000_000);
    // None bitrates are omitted, not serialized as null.
    assert!(json.get("MaxStaticBitrate").is_none());
    assert!(json.get("Id").is_none());
    // Array fields are always present.
    assert!(json["DirectPlayProfiles"].is_array());
}

#[test]
fn transcoding_profile_pascal_case_wire_shape() {
    let profile = TranscodingProfile {
        container: "ts".to_owned(),
        profile_type: DlnaProfileType::Video,
        video_codec: "h264".to_owned(),
        audio_codec: "aac".to_owned(),
        protocol: MediaStreamProtocol::hls,
        ..Default::default()
    };
    let json = serde_json::to_value(&profile).unwrap();
    assert_eq!(json["Container"], "ts");
    assert_eq!(json["Type"], "Video");
    assert_eq!(json["Protocol"], "hls");
    assert_eq!(json["TranscodeSeekInfo"], "Auto");
    assert_eq!(json["Context"], "Streaming");
    // C# initializes EnableAudioVbrEncoding to true.
    assert_eq!(json["EnableAudioVbrEncoding"], true);
}

#[test]
fn profile_condition_round_trips() {
    let cond = ProfileCondition::with_required(
        ProfileConditionType::LessThanEqual,
        ProfileConditionValue::VideoBitrate,
        "10000000".to_owned(),
        true,
    );
    let json = serde_json::to_string(&cond).unwrap();
    let back: ProfileCondition = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cond);

    let value = serde_json::to_value(&cond).unwrap();
    assert_eq!(value["Condition"], "LessThanEqual");
    assert_eq!(value["Property"], "VideoBitrate");
    assert_eq!(value["Value"], "10000000");
    assert_eq!(value["IsRequired"], true);
}

#[test]
fn profile_condition_default_is_required() {
    // The parameterless C# constructor sets IsRequired = true.
    assert!(ProfileCondition::default().is_required);
    // The three-arg constructor defaults IsRequired to false.
    let cond = ProfileCondition::new(
        ProfileConditionType::Equals,
        ProfileConditionValue::Width,
        "1920".to_owned(),
    );
    assert!(!cond.is_required);
}
