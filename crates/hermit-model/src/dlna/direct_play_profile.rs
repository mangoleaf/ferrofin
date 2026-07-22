//! Port of `MediaBrowser.Model.Dlna.DirectPlayProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::DlnaProfileType;
use crate::extensions::contains_container;

/// Declares a container/codec combination a device can direct play without
/// transcoding or remuxing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct DirectPlayProfile {
    /// The container(s), comma-delimited.
    pub container: String,
    /// The audio codec(s), comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// The video codec(s), comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    /// The DLNA profile type this profile applies to.
    #[serde(rename = "Type")]
    pub profile_type: DlnaProfileType,
}

impl DirectPlayProfile {
    /// Returns whether [`Self::container`] supports `container`.
    #[must_use]
    pub fn supports_container(&self, container: Option<&str>) -> bool {
        contains_container(Some(&self.container), container)
    }

    /// Returns whether [`Self::video_codec`] supports `codec`.
    #[must_use]
    pub fn supports_video_codec(&self, codec: Option<&str>) -> bool {
        self.profile_type == DlnaProfileType::Video
            && contains_container(self.video_codec.as_deref(), codec)
    }

    /// Returns whether [`Self::audio_codec`] supports `codec`.
    ///
    /// Video profiles can carry audio-codec restrictions too, so `Video` is a
    /// valid type here as well as `Audio`.
    #[must_use]
    pub fn supports_audio_codec(&self, codec: Option<&str>) -> bool {
        (self.profile_type == DlnaProfileType::Audio || self.profile_type == DlnaProfileType::Video)
            && contains_container(self.audio_codec.as_deref(), codec)
    }
}
