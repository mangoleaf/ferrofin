//! `TranscodingInfo` — port of `MediaBrowser.Model.Session.TranscodingInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::TranscodeReason;
use crate::entities::HardwareAccelerationType;

/// Information on a running transcode.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TranscodingInfo {
    /// Gets or sets the audio codec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,

    /// Gets or sets the video codec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,

    /// Gets or sets the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,

    /// Gets or sets a value indicating whether the video is passed through.
    pub is_video_direct: bool,

    /// Gets or sets a value indicating whether the audio is passed through.
    pub is_audio_direct: bool,

    /// Gets or sets the bitrate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<i32>,

    /// Gets or sets the framerate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framerate: Option<f32>,

    /// Gets or sets the completion percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_percentage: Option<f64>,

    /// Gets or sets the video width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,

    /// Gets or sets the video height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,

    /// Gets or sets the audio channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_channels: Option<i32>,

    /// Gets or sets the hardware acceleration type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware_acceleration_type: Option<HardwareAccelerationType>,

    /// Gets or sets the transcode reasons.
    pub transcode_reasons: Vec<TranscodeReason>,
}
