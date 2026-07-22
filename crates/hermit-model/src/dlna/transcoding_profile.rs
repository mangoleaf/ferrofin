//! Port of `MediaBrowser.Model.Dlna.TranscodingProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::DlnaProfileType;
use super::enums::{EncodingContext, TranscodeSeekInfo};
use super::profile_condition::ProfileCondition;
use crate::data::MediaStreamProtocol;

/// Describes a container/codec combination to transcode to when direct play is
/// not possible.
///
/// Note: conditions defined on a [`super::codec_profile::CodecProfile`] have
/// higher priority and can override values defined here.
// This is a faithful DTO port; the boolean flags mirror the wire contract.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct TranscodingProfile {
    /// The container.
    pub container: String,
    /// The DLNA profile type.
    #[serde(rename = "Type")]
    pub profile_type: DlnaProfileType,
    /// The video codec.
    pub video_codec: String,
    /// The audio codec.
    pub audio_codec: String,
    /// The delivery protocol.
    #[serde(default, deserialize_with = "deserialize_protocol")]
    pub protocol: MediaStreamProtocol,
    /// Whether the content length should be estimated.
    pub estimate_content_length: bool,
    /// Whether M2TS mode is enabled.
    #[serde(rename = "EnableMpegtsM2TsMode")]
    pub enable_mpegts_m2ts_mode: bool,
    /// The transcoding seek info mode.
    pub transcode_seek_info: TranscodeSeekInfo,
    /// Whether timestamps should be copied.
    pub copy_timestamps: bool,
    /// The encoding context.
    pub context: EncodingContext,
    /// Whether subtitles are allowed in the manifest.
    pub enable_subtitles_in_manifest: bool,
    /// The maximum audio channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_audio_channels: Option<String>,
    /// The minimum amount of segments.
    #[serde(default, deserialize_with = "deserialize_int_or_string")]
    pub min_segments: i32,
    /// The segment length.
    #[serde(default, deserialize_with = "deserialize_int_or_string")]
    pub segment_length: i32,
    /// Whether breaking the video stream on non-keyframes is supported.
    ///
    /// Obsolete upstream — this is always `false`.
    #[serde(default)]
    pub break_on_non_key_frames: bool,
    /// The profile conditions.
    pub conditions: Vec<ProfileCondition>,
    /// Whether variable bitrate encoding is supported.
    pub enable_audio_vbr_encoding: bool,
}

impl Default for TranscodingProfile {
    fn default() -> Self {
        Self {
            container: String::new(),
            profile_type: DlnaProfileType::default(),
            video_codec: String::new(),
            audio_codec: String::new(),
            protocol: MediaStreamProtocol::default(),
            estimate_content_length: false,
            enable_mpegts_m2ts_mode: false,
            transcode_seek_info: TranscodeSeekInfo::default(),
            copy_timestamps: false,
            context: EncodingContext::default(),
            enable_subtitles_in_manifest: false,
            max_audio_channels: None,
            min_segments: 0,
            segment_length: 0,
            break_on_non_key_frames: false,
            conditions: Vec::new(),
            // C# initializes this to true.
            enable_audio_vbr_encoding: true,
        }
    }
}

/// Deserializes a [`MediaStreamProtocol`], treating an empty string (or a
/// missing value) as the default `http` — as Jellyfin's device profiles encode
/// an unspecified transcoding protocol.
fn deserialize_protocol<'de, D>(deserializer: D) -> Result<MediaStreamProtocol, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref() {
        None | Some("" | "http") => Ok(MediaStreamProtocol::http),
        Some("hls") => Ok(MediaStreamProtocol::hls),
        Some(other) => Err(serde::de::Error::custom(format!(
            "invalid MediaStreamProtocol: {other}"
        ))),
    }
}

/// Deserializes an `i32` from either a JSON number or a JSON string.
///
/// Jellyfin device profiles encode `MinSegments` / `SegmentLength` as strings
/// in some profiles and as numbers in others.
fn deserialize_int_or_string<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        Int(i32),
        Str(String),
    }

    match IntOrString::deserialize(deserializer)? {
        IntOrString::Int(n) => Ok(n),
        IntOrString::Str(s) if s.is_empty() => Ok(0),
        IntOrString::Str(s) => s.parse().map_err(serde::de::Error::custom),
    }
}
