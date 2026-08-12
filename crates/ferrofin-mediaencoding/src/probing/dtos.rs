//! ffprobe JSON DTOs — port of `MediaBrowser.MediaEncoding.Probing.*`.
//!
//! These map the raw `ffprobe -print_format json` output onto strongly typed
//! structs. Field names / `serde(rename)` keys match the upstream
//! `[JsonPropertyName]` attributes byte-for-byte, including the notable
//! `codec_tag_string?` typo (see [`MediaStreamInfo::codec_tag_string`]).

use std::collections::HashMap;

use serde::Deserialize;

/// FFmpeg codec type — port of `Probing.CodecType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecType {
    /// Video.
    Video,
    /// Audio.
    Audio,
    /// Opaque data information, usually continuous.
    Data,
    /// Subtitles.
    Subtitle,
    /// Opaque data information, usually sparse.
    Attachment,
}

/// Top-level ffprobe result — port of `InternalMediaInfoResult`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InternalMediaInfoResult {
    /// The streams.
    #[serde(default)]
    pub streams: Option<Vec<MediaStreamInfo>>,
    /// The format.
    #[serde(default)]
    pub format: Option<MediaFormatInfo>,
    /// The chapters.
    #[serde(default)]
    pub chapters: Option<Vec<MediaChapter>>,
    /// The frames.
    #[serde(default)]
    pub frames: Option<Vec<MediaFrameInfo>>,
}

/// A single stream within the ffprobe output — port of `MediaStreamInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaStreamInfo {
    /// The stream index.
    #[serde(default)]
    pub index: i32,
    /// The profile.
    #[serde(default)]
    pub profile: Option<String>,
    /// The codec name.
    #[serde(default, rename = "codec_name")]
    pub codec_name: Option<String>,
    /// The codec type.
    #[serde(default, rename = "codec_type")]
    pub codec_type: Option<CodecType>,
    /// The sample rate.
    #[serde(default, rename = "sample_rate")]
    pub sample_rate: Option<String>,
    /// The channel count.
    #[serde(default)]
    pub channels: Option<i32>,
    /// The channel layout.
    #[serde(default, rename = "channel_layout")]
    pub channel_layout: Option<String>,
    /// The average frame rate.
    #[serde(default, rename = "avg_frame_rate")]
    pub average_frame_rate: Option<String>,
    /// The duration.
    #[serde(default)]
    pub duration: Option<String>,
    /// The bit rate.
    #[serde(default, rename = "bit_rate")]
    pub bit_rate: Option<String>,
    /// The width.
    #[serde(default)]
    pub width: Option<i32>,
    /// The reference frame count.
    #[serde(default)]
    pub refs: i32,
    /// The height.
    #[serde(default)]
    pub height: Option<i32>,
    /// The display aspect ratio.
    #[serde(default, rename = "display_aspect_ratio")]
    pub display_aspect_ratio: Option<String>,
    /// The tags.
    #[serde(default)]
    pub tags: Option<HashMap<String, Option<String>>>,
    /// The bits per sample.
    #[serde(default, rename = "bits_per_sample")]
    pub bits_per_sample: i32,
    /// The bits per raw sample.
    #[serde(
        default,
        rename = "bits_per_raw_sample",
        deserialize_with = "de_int_flexible"
    )]
    pub bits_per_raw_sample: i32,
    /// The real (r) frame rate.
    #[serde(default, rename = "r_frame_rate")]
    pub r_frame_rate: Option<String>,
    /// The sample aspect ratio.
    #[serde(default, rename = "sample_aspect_ratio")]
    pub sample_aspect_ratio: Option<String>,
    /// The pixel format.
    #[serde(default, rename = "pix_fmt")]
    pub pixel_format: Option<String>,
    /// The level.
    #[serde(default)]
    pub level: Option<i32>,
    /// The time base.
    #[serde(default, rename = "time_base")]
    pub time_base: Option<String>,
    /// The codec time base.
    #[serde(default, rename = "codec_time_base")]
    pub codec_time_base: Option<String>,
    /// The codec tag string.
    ///
    /// NOTE: upstream binds this to the JSON key `"codec_tag_string?"` (with a
    /// trailing `?`), which never matches real ffprobe output. The typo is
    /// preserved verbatim so the derived behaviour (e.g. `mjpeg` streams always
    /// classified as embedded images because `codec_tag` is never populated)
    /// matches the oracle exactly.
    #[serde(default, rename = "codec_tag_string?")]
    pub codec_tag_string: Option<String>,
    /// Whether the stream is AVC (`is_avc`, may be a JSON string).
    #[serde(default, rename = "is_avc", deserialize_with = "de_bool_flexible")]
    pub is_avc: Option<bool>,
    /// The NAL length size.
    #[serde(default, rename = "nal_length_size")]
    pub nal_length_size: Option<String>,
    /// The field order.
    #[serde(default, rename = "field_order")]
    pub field_order: Option<String>,
    /// The disposition flags.
    #[serde(default)]
    pub disposition: Option<HashMap<String, i32>>,
    /// The color range.
    #[serde(default, rename = "color_range")]
    pub color_range: Option<String>,
    /// The color space.
    #[serde(default, rename = "color_space")]
    pub color_space: Option<String>,
    /// The color transfer.
    #[serde(default, rename = "color_transfer")]
    pub color_transfer: Option<String>,
    /// The color primaries.
    #[serde(default, rename = "color_primaries")]
    pub color_primaries: Option<String>,
    /// The side-data list.
    #[serde(default, rename = "side_data_list")]
    pub side_data_list: Option<Vec<MediaStreamInfoSideData>>,
}

/// Container-level format info — port of `MediaFormatInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaFormatInfo {
    /// The filename.
    #[serde(default, rename = "filename")]
    pub file_name: Option<String>,
    /// The format name.
    #[serde(default, rename = "format_name")]
    pub format_name: Option<String>,
    /// The duration.
    #[serde(default)]
    pub duration: Option<String>,
    /// The size.
    #[serde(default)]
    pub size: Option<String>,
    /// The bit rate.
    #[serde(default, rename = "bit_rate")]
    pub bit_rate: Option<String>,
    /// The tags.
    #[serde(default)]
    pub tags: Option<HashMap<String, Option<String>>>,
}

/// A chapter — port of `MediaChapter`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaChapter {
    /// The start time (seconds, as a string).
    #[serde(default, rename = "start_time")]
    pub start_time: Option<String>,
    /// The tags.
    #[serde(default)]
    pub tags: Option<HashMap<String, Option<String>>>,
}

/// A decoded frame — port of `MediaFrameInfo` (only the fields we consume).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaFrameInfo {
    /// The owning stream index.
    #[serde(default, rename = "stream_index")]
    pub stream_index: Option<i32>,
    /// The frame side-data list.
    #[serde(default, rename = "side_data_list")]
    pub side_data_list: Option<Vec<MediaFrameSideDataInfo>>,
}

/// Frame side-data — port of `MediaFrameSideDataInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaFrameSideDataInfo {
    /// The side-data type.
    #[serde(default, rename = "side_data_type")]
    pub side_data_type: Option<String>,
}

/// Stream side-data — port of `MediaStreamInfoSideData`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaStreamInfoSideData {
    /// The side-data type.
    #[serde(default, rename = "side_data_type")]
    pub side_data_type: Option<String>,
    /// The Dolby Vision version major.
    #[serde(default, rename = "dv_version_major")]
    pub dv_version_major: Option<i32>,
    /// The Dolby Vision version minor.
    #[serde(default, rename = "dv_version_minor")]
    pub dv_version_minor: Option<i32>,
    /// The Dolby Vision profile.
    #[serde(default, rename = "dv_profile")]
    pub dv_profile: Option<i32>,
    /// The Dolby Vision level.
    #[serde(default, rename = "dv_level")]
    pub dv_level: Option<i32>,
    /// The Dolby Vision RPU present flag.
    #[serde(default, rename = "rpu_present_flag")]
    pub rpu_present_flag: Option<i32>,
    /// The Dolby Vision EL present flag.
    #[serde(default, rename = "el_present_flag")]
    pub el_present_flag: Option<i32>,
    /// The Dolby Vision BL present flag.
    #[serde(default, rename = "bl_present_flag")]
    pub bl_present_flag: Option<i32>,
    /// The Dolby Vision BL signal compatibility id.
    #[serde(default, rename = "dv_bl_signal_compatibility_id")]
    pub dv_bl_signal_compatibility_id: Option<i32>,
    /// The rotation in degrees.
    #[serde(default)]
    pub rotation: Option<i32>,
}

/// Deserializes a bool that ffprobe may encode as a JSON string
/// (`"true"`/`"false"`), matching Jellyfin's `JsonBoolStringConverter`.
fn de_bool_flexible<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        Str(String),
    }

    let opt = Option::<BoolOrString>::deserialize(deserializer)?;
    Ok(match opt {
        None => None,
        Some(BoolOrString::Bool(b)) => Some(b),
        Some(BoolOrString::Str(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
    })
}

/// Deserializes an int that ffprobe may encode as a JSON string (e.g.
/// `"bits_per_raw_sample": "8"`), defaulting to `0` when absent or unparseable.
fn de_int_flexible<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        Int(i32),
        Str(String),
    }

    let opt = Option::<IntOrString>::deserialize(deserializer)?;
    Ok(match opt {
        None => 0,
        Some(IntOrString::Int(i)) => i,
        Some(IntOrString::Str(s)) => s.trim().parse().unwrap_or(0),
    })
}
