//! DLNA/profile enums — port of the pure enums in `MediaBrowser.Model.Dlna`.
//!
//! The `StreamBuilder` engine is ported in a later unit; the standalone enums
//! and the device-profile model structs (in sibling modules) live here.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Enum `DlnaProfileType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum DlnaProfileType {
    /// Audio.
    #[default]
    Audio = 0,
    /// Video.
    Video = 1,
    /// Photo.
    Photo = 2,
    /// Subtitle.
    Subtitle = 3,
    /// Lyric.
    Lyric = 4,
}

/// The codec type of a codec profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum CodecType {
    /// The profile applies to a video codec.
    #[default]
    Video = 0,
    /// The profile applies to the audio codec of a video stream.
    VideoAudio = 1,
    /// The profile applies to an audio codec.
    Audio = 2,
}

/// The encoding context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum EncodingContext {
    /// The media is transcoded on the fly and delivered as a stream.
    #[default]
    Streaming = 0,
    /// The media is transcoded to a static file.
    Static = 1,
}

/// The playback error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlaybackErrorCode {
    /// Playback of the item is not allowed.
    NotAllowed = 0,
    /// No stream compatible with the device profile was found.
    NoCompatibleStream = 1,
    /// The rate limit has been exceeded.
    RateLimitExceeded = 2,
}

/// The comparison a profile condition applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ProfileConditionType {
    /// Values must be equal.
    Equals = 0,
    /// Values must not be equal.
    NotEquals = 1,
    /// Value must be less than or equal.
    LessThanEqual = 2,
    /// Value must be greater than or equal.
    GreaterThanEqual = 3,
    /// Value must equal any of the provided values.
    EqualsAny = 4,
}

/// The stream property a profile condition constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ProfileConditionValue {
    /// Audio channel count.
    AudioChannels = 0,
    /// Audio bitrate.
    AudioBitrate = 1,
    /// Audio profile.
    AudioProfile = 2,
    /// Width.
    Width = 3,
    /// Height.
    Height = 4,
    /// Whether 64-bit offsets are used.
    Has64BitOffsets = 5,
    /// Packet length.
    PacketLength = 6,
    /// Video bit depth.
    VideoBitDepth = 7,
    /// Video bitrate.
    VideoBitrate = 8,
    /// Video framerate.
    VideoFramerate = 9,
    /// Video level.
    VideoLevel = 10,
    /// Video profile.
    VideoProfile = 11,
    /// Video timestamp.
    VideoTimestamp = 12,
    /// Whether the video is anamorphic.
    IsAnamorphic = 13,
    /// Reference frame count.
    RefFrames = 14,
    /// Number of audio streams.
    NumAudioStreams = 16,
    /// Number of video streams.
    NumVideoStreams = 17,
    /// Whether the audio is secondary.
    IsSecondaryAudio = 18,
    /// Video codec tag.
    VideoCodecTag = 19,
    /// Whether the video is AVC.
    IsAvc = 20,
    /// Whether the video is interlaced.
    IsInterlaced = 21,
    /// Audio sample rate.
    AudioSampleRate = 22,
    /// Audio bit depth.
    AudioBitDepth = 23,
    /// Video range type.
    VideoRangeType = 24,
    /// Number of streams.
    NumStreams = 25,
    /// Video rotation.
    VideoRotation = 26,
}

/// Delivery method to use during playback of a specific subtitle format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SubtitleDeliveryMethod {
    /// Burn the subtitles into the video track.
    #[default]
    Encode = 0,
    /// Embed the subtitles in the file or stream.
    Embed = 1,
    /// Serve the subtitles as an external file.
    External = 2,
    /// Serve the subtitles as a separate HLS stream.
    Hls = 3,
    /// Drop the subtitle.
    Drop = 4,
}

/// The transcode seek info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TranscodeSeekInfo {
    /// The seek method is chosen automatically.
    #[default]
    Auto = 0,
    /// Seeking is performed by byte position.
    Bytes = 1,
}
