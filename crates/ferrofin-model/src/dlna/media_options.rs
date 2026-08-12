//! Port of `MediaBrowser.Model.Dlna.MediaOptions`.

use uuid::Uuid;

use super::device_profile::DeviceProfile;
use super::enums::EncodingContext;
use crate::dto::MediaSourceInfo;

/// The input options describing a playback request for the `StreamBuilder`.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct MediaOptions {
    /// Gets or sets a value indicating whether direct playback is allowed.
    pub enable_direct_play: bool,

    /// Gets or sets a value indicating whether direct streaming is allowed.
    pub enable_direct_stream: bool,

    /// Gets or sets a value indicating whether direct playback is forced.
    pub force_direct_play: bool,

    /// Gets or sets a value indicating whether direct streaming is forced.
    pub force_direct_stream: bool,

    /// Gets or sets a value indicating whether audio stream copy is allowed.
    pub allow_audio_stream_copy: bool,

    /// Gets or sets a value indicating whether video stream copy is allowed.
    pub allow_video_stream_copy: bool,

    /// Gets or sets a value indicating whether to always burn in subtitles when
    /// transcoding.
    pub always_burn_in_subtitle_when_transcoding: bool,

    /// Gets or sets the item id.
    pub item_id: Uuid,

    /// Gets or sets the media sources.
    pub media_sources: Vec<MediaSourceInfo>,

    /// Gets or sets the device profile.
    pub profile: DeviceProfile,

    /// Gets or sets a media source id. Optional. Only needed if a specific
    /// `AudioStreamIndex` or `SubtitleStreamIndex` are requested.
    pub media_source_id: Option<String>,

    /// Gets or sets the device id.
    pub device_id: Option<String>,

    /// Gets or sets an override of supported number of audio channels.
    pub max_audio_channels: Option<i32>,

    /// Gets or sets the application's configured maximum bitrate.
    pub max_bitrate: Option<i32>,

    /// Gets or sets the context.
    pub context: EncodingContext,

    /// Gets or sets the audio transcoding bitrate.
    pub audio_transcoding_bitrate: Option<i32>,

    /// Gets or sets an override for the audio stream index.
    pub audio_stream_index: Option<i32>,

    /// Gets or sets an override for the subtitle stream index.
    pub subtitle_stream_index: Option<i32>,
}

impl MediaOptions {
    /// Creates a new `MediaOptions` for the given device profile, mirroring the
    /// C# constructor defaults (`Streaming` context, direct play/stream on).
    #[must_use]
    pub fn new(profile: DeviceProfile) -> Self {
        Self {
            enable_direct_play: true,
            enable_direct_stream: true,
            force_direct_play: false,
            force_direct_stream: false,
            allow_audio_stream_copy: false,
            allow_video_stream_copy: false,
            always_burn_in_subtitle_when_transcoding: false,
            item_id: Uuid::nil(),
            media_sources: Vec::new(),
            profile,
            media_source_id: None,
            device_id: None,
            max_audio_channels: None,
            max_bitrate: None,
            context: EncodingContext::Streaming,
            audio_transcoding_bitrate: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
        }
    }

    /// Gets the maximum bitrate.
    #[must_use]
    pub fn get_max_bitrate(&self, is_audio: bool) -> Option<i32> {
        if self.max_bitrate.is_some() {
            return self.max_bitrate;
        }

        if self.context == EncodingContext::Static {
            if is_audio && self.profile.max_static_music_bitrate.is_some() {
                return self.profile.max_static_music_bitrate;
            }

            return self.profile.max_static_bitrate;
        }

        self.profile.max_streaming_bitrate
    }
}
