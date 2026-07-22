//! Port of `MediaBrowser.Model.Dlna.DeviceProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::codec_profile::CodecProfile;
use super::container_profile::ContainerProfile;
use super::direct_play_profile::DirectPlayProfile;
use super::subtitle_profile::SubtitleProfile;
use super::transcoding_profile::TranscodingProfile;

/// Default maximum streaming bitrate (`8 Mbps`), from the C# field initializer.
pub const DEFAULT_MAX_STREAMING_BITRATE: i32 = 8_000_000;
/// Default maximum static (direct-play) bitrate (`8 Mbps`).
pub const DEFAULT_MAX_STATIC_BITRATE: i32 = 8_000_000;
/// Default transcoding bitrate for music streams (`128 kbps`).
pub const DEFAULT_MUSIC_STREAMING_TRANSCODING_BITRATE: i32 = 128_000;
/// Default maximum static (direct-play) music bitrate (`8 Mbps`).
pub const DEFAULT_MAX_STATIC_MUSIC_BITRATE: i32 = 8_000_000;

/// A set of metadata determining which content a device can play.
///
/// It defines the supported [containers](ContainerProfile) and
/// [codecs](CodecProfile) the device can direct play, and which
/// [containers/codecs to transcode to](TranscodingProfile) when it cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeviceProfile {
    /// The name of this device profile. User profiles must be uniquely named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The unique internal identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub id: Option<Uuid>,
    /// The maximum allowed bitrate for all streamed content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i32>,
    /// The maximum allowed bitrate for statically streamed (direct-played)
    /// content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_static_bitrate: Option<i32>,
    /// The maximum allowed bitrate for transcoded music streams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_streaming_transcoding_bitrate: Option<i32>,
    /// The maximum allowed bitrate for statically streamed (direct-played)
    /// music files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_static_music_bitrate: Option<i32>,
    /// The direct-play profiles.
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    /// The transcoding profiles.
    pub transcoding_profiles: Vec<TranscodingProfile>,
    /// The container profiles. Failing these optional conditions forces
    /// transcoding.
    pub container_profiles: Vec<ContainerProfile>,
    /// The codec profiles.
    pub codec_profiles: Vec<CodecProfile>,
    /// The subtitle profiles.
    pub subtitle_profiles: Vec<SubtitleProfile>,
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            name: None,
            id: None,
            max_streaming_bitrate: Some(DEFAULT_MAX_STREAMING_BITRATE),
            max_static_bitrate: Some(DEFAULT_MAX_STATIC_BITRATE),
            music_streaming_transcoding_bitrate: Some(DEFAULT_MUSIC_STREAMING_TRANSCODING_BITRATE),
            max_static_music_bitrate: Some(DEFAULT_MAX_STATIC_MUSIC_BITRATE),
            direct_play_profiles: Vec::new(),
            transcoding_profiles: Vec::new(),
            container_profiles: Vec::new(),
            codec_profiles: Vec::new(),
            subtitle_profiles: Vec::new(),
        }
    }
}
