//! Wire-casing regression tests: these enums are the client-compat contract,
//! so pin the exact JSON string each variant serializes to against the
//! vendored OpenAPI spec (`contracts/jellyfin-openapi-10.11.8.json`).

use ferrofin_model::data::{CollectionType, MediaType, PersonKind, VideoRange, VideoRangeType};
use ferrofin_model::dlna::{DlnaProfileType, SubtitleDeliveryMethod};
use ferrofin_model::drawing::{ImageFormat, ImageOrientation};
use ferrofin_model::dto::{MediaSourceType, RatingType};
use ferrofin_model::entities::{
    CollectionTypeOptions, DeinterlaceMethod, EncoderPreset, HardwareAccelerationType, ImageType,
    MediaStreamType, Video3DFormat,
};
use ferrofin_model::media_info::{MediaProtocol, audio_codec};
use ferrofin_model::session::{
    GeneralCommandType, PlayMethod, SessionMessageType, TranscodeReason, TranscodeReasons,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Assert a value serializes to exactly `expected` and round-trips back.
fn assert_json<T>(value: &T, expected: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    assert_eq!(json, format!("\"{expected}\""), "serialized string");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, value, "round-trip");
}

#[test]
fn pascal_case_enums() {
    assert_json(&ImageType::BoxRear, "BoxRear");
    assert_json(&MediaStreamType::EmbeddedImage, "EmbeddedImage");
    assert_json(&MediaSourceType::Placeholder, "Placeholder");
    assert_json(&RatingType::Likes, "Likes");
    assert_json(&PlayMethod::DirectStream, "DirectStream");
    assert_json(
        &GeneralCommandType::SetSubtitleStreamIndex,
        "SetSubtitleStreamIndex",
    );
    assert_json(&SessionMessageType::ForceKeepAlive, "ForceKeepAlive");
    assert_json(&DlnaProfileType::Subtitle, "Subtitle");
    assert_json(&SubtitleDeliveryMethod::Hls, "Hls");
    assert_json(&MediaProtocol::Rtsp, "Rtsp");
    assert_json(&MediaType::Photo, "Photo");
    assert_json(&PersonKind::AlbumArtist, "AlbumArtist");
    assert_json(&ImageOrientation::BottomRight, "BottomRight");
    assert_json(&ImageFormat::Webp, "Webp");
}

#[test]
fn lowercase_enums() {
    assert_json(&CollectionType::musicvideos, "musicvideos");
    assert_json(&CollectionTypeOptions::boxsets, "boxsets");
    assert_json(&DeinterlaceMethod::bwdif, "bwdif");
    assert_json(&EncoderPreset::ultrafast, "ultrafast");
    assert_json(&HardwareAccelerationType::videotoolbox, "videotoolbox");
}

#[test]
fn acronym_renamed_variants() {
    assert_json(&Video3DFormat::Mvc, "MVC");
    assert_json(&VideoRange::Sdr, "SDR");
    assert_json(&VideoRange::Hdr, "HDR");
    assert_json(&VideoRangeType::Hdr10, "HDR10");
    assert_json(&VideoRangeType::DoviWithHdr10Plus, "DOVIWithHDR10Plus");
    assert_json(&VideoRangeType::DoviWithElhdr10Plus, "DOVIWithELHDR10Plus");
    assert_json(&VideoRangeType::DoviInvalid, "DOVIInvalid");
    assert_json(
        &TranscodeReason::VideoRangeTypeNotSupported,
        "VideoRangeTypeNotSupported",
    );
}

#[test]
fn transcode_reasons_serialize_as_string_array() {
    // A set of reasons is a JSON array of PascalCase strings on the wire.
    let reasons = vec![
        TranscodeReason::ContainerNotSupported,
        TranscodeReason::AudioIsExternal,
    ];
    let json = serde_json::to_string(&reasons).expect("serialize");
    assert_eq!(json, r#"["ContainerNotSupported","AudioIsExternal"]"#);
}

#[test]
fn transcode_reason_bit_positions_match_csharp() {
    // Bit positions are load-bearing for the ported stream builder.
    assert_eq!(TranscodeReasons::CONTAINER_NOT_SUPPORTED.bits(), 1 << 0);
    assert_eq!(TranscodeReasons::STREAM_COUNT_EXCEEDS_LIMIT.bits(), 1 << 26);
    assert_eq!(
        TranscodeReasons::VIDEO_RANGE_TYPE_NOT_SUPPORTED.bits(),
        1 << 24
    );
    assert_eq!(
        TranscodeReasons::VIDEO_ROTATION_NOT_SUPPORTED.bits(),
        1 << 27
    );

    // Lifting a single reason yields exactly its bit.
    let mask = TranscodeReasons::from(TranscodeReason::DirectPlayError)
        | TranscodeReasons::from(TranscodeReason::VideoCodecNotSupported);
    assert!(mask.contains(TranscodeReasons::DIRECT_PLAY_ERROR));
    assert!(mask.contains(TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED));
    assert!(!mask.contains(TranscodeReasons::AUDIO_IS_EXTERNAL));
}

#[test]
fn audio_codec_friendly_names() {
    // Oracle: MediaBrowser.Model.MediaInfo.AudioCodec.GetFriendlyName.
    assert_eq!(audio_codec::friendly_name(""), "");
    assert_eq!(audio_codec::friendly_name("ac3"), "Dolby Digital");
    assert_eq!(audio_codec::friendly_name("AC3"), "Dolby Digital");
    assert_eq!(audio_codec::friendly_name("eac3"), "Dolby Digital+");
    assert_eq!(audio_codec::friendly_name("dca"), "DTS");
    assert_eq!(audio_codec::friendly_name("flac"), "FLAC");
}

#[test]
fn image_format_conversions() {
    for (i, expected) in [
        (0, ImageFormat::Bmp),
        (1, ImageFormat::Gif),
        (2, ImageFormat::Jpg),
        (3, ImageFormat::Png),
        (4, ImageFormat::Webp),
        (5, ImageFormat::Svg),
    ] {
        assert_eq!(ImageFormat::try_from(i).expect("valid"), expected);
    }
}
