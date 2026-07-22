//! `MediaSourceInfo` — port of `MediaBrowser.Model.Dto.MediaSourceInfo`.
//!
//! This is the direct input to `StreamBuilder`; the field names and casing
//! match the Jellyfin JSON/OpenAPI contract exactly. `TranscodeReasons` and
//! `DefaultAudioIndexSource` are `[JsonIgnore]` upstream, so they are excluded
//! from serialization here as well.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::MediaSourceType;
use crate::entities::{IsoType, MediaStreamType, Video3DFormat, VideoType};
use crate::entities_media::{MediaAttachment, MediaStream};
use crate::media_info::{AudioIndexSource, MediaProtocol, TransportStreamTimestamp};
use crate::session::TranscodeReason;

/// Description of a single media source (file, disc, live stream, …) for an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
#[serde(default)]
pub struct MediaSourceInfo {
    /// Gets or sets the protocol.
    pub protocol: MediaProtocol,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Gets or sets the encoder path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_path: Option<String>,

    /// Gets or sets the encoder protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_protocol: Option<MediaProtocol>,

    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: MediaSourceType,

    /// Gets or sets the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,

    /// Gets or sets the size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,

    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets a value indicating whether the media is remote (internet
    /// url vs local network).
    pub is_remote: bool,

    /// Gets or sets the entity tag.
    #[serde(rename = "ETag", skip_serializing_if = "Option::is_none")]
    pub e_tag: Option<String>,

    /// Gets or sets the run time in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,

    /// Gets or sets a value indicating whether to read at the native framerate.
    pub read_at_native_framerate: bool,

    /// Gets or sets a value indicating whether to ignore DTS.
    pub ignore_dts: bool,

    /// Gets or sets a value indicating whether to ignore the index.
    pub ignore_index: bool,

    /// Gets or sets a value indicating whether to generate PTS on input.
    pub gen_pts_input: bool,

    /// Gets or sets a value indicating whether transcoding is supported.
    pub supports_transcoding: bool,

    /// Gets or sets a value indicating whether direct stream is supported.
    pub supports_direct_stream: bool,

    /// Gets or sets a value indicating whether direct play is supported.
    pub supports_direct_play: bool,

    /// Gets or sets a value indicating whether this is an infinite stream.
    pub is_infinite_stream: bool,

    /// Gets or sets a value indicating whether to use the most compatible
    /// transcoding profile.
    pub use_most_compatible_transcoding_profile: bool,

    /// Gets or sets a value indicating whether the source requires opening.
    pub requires_opening: bool,

    /// Gets or sets the open token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_token: Option<String>,

    /// Gets or sets a value indicating whether the source requires closing.
    pub requires_closing: bool,

    /// Gets or sets the live stream identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,

    /// Gets or sets the buffer in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<i32>,

    /// Gets or sets a value indicating whether the source requires looping.
    pub requires_looping: bool,

    /// Gets or sets a value indicating whether probing is supported.
    pub supports_probing: bool,

    /// Gets or sets the video type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_type: Option<VideoType>,

    /// Gets or sets the ISO type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso_type: Option<IsoType>,

    /// Gets or sets the 3D format.
    #[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")]
    pub video3d_format: Option<Video3DFormat>,

    /// Gets or sets the media streams.
    pub media_streams: Vec<MediaStream>,

    /// Gets or sets the media attachments.
    pub media_attachments: Vec<MediaAttachment>,

    /// Gets or sets the formats.
    pub formats: Vec<String>,

    /// Gets or sets the bitrate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<i32>,

    /// Gets or sets the fallback maximum streaming bitrate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_max_streaming_bitrate: Option<i32>,

    /// Gets or sets the timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<TransportStreamTimestamp>,

    /// Gets or sets the required HTTP headers.
    pub required_http_headers: HashMap<String, String>,

    /// Gets or sets the transcoding URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcoding_url: Option<String>,

    /// Gets or sets the transcoding sub-protocol.
    pub transcoding_sub_protocol: crate::data::MediaStreamProtocol,

    /// Gets or sets the transcoding container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcoding_container: Option<String>,

    /// Gets or sets the analyze duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyze_duration_ms: Option<i32>,

    /// Gets or sets the transcode reasons. `[JsonIgnore]` upstream.
    #[serde(skip)]
    pub transcode_reasons: Option<TranscodeReason>,

    /// Gets or sets the default audio index source. `[JsonIgnore]` upstream.
    #[serde(skip)]
    pub default_audio_index_source: AudioIndexSource,

    /// Gets or sets the index of the default audio stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_audio_stream_index: Option<i32>,

    /// Gets or sets the index of the default subtitle stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_subtitle_stream_index: Option<i32>,

    /// Gets or sets a value indicating whether the source has segments.
    pub has_segments: bool,
}

impl Default for MediaSourceInfo {
    fn default() -> Self {
        Self {
            protocol: MediaProtocol::default(),
            id: None,
            path: None,
            encoder_path: None,
            encoder_protocol: None,
            type_: MediaSourceType::Default,
            container: None,
            size: None,
            name: None,
            is_remote: false,
            e_tag: None,
            run_time_ticks: None,
            read_at_native_framerate: false,
            ignore_dts: false,
            ignore_index: false,
            gen_pts_input: false,
            supports_transcoding: true,
            supports_direct_stream: true,
            supports_direct_play: true,
            is_infinite_stream: false,
            use_most_compatible_transcoding_profile: false,
            requires_opening: false,
            open_token: None,
            requires_closing: false,
            live_stream_id: None,
            buffer_ms: None,
            requires_looping: false,
            supports_probing: true,
            video_type: None,
            iso_type: None,
            video3d_format: None,
            media_streams: Vec::new(),
            media_attachments: Vec::new(),
            formats: Vec::new(),
            bitrate: None,
            fallback_max_streaming_bitrate: None,
            timestamp: None,
            required_http_headers: HashMap::new(),
            transcoding_url: None,
            transcoding_sub_protocol: crate::data::MediaStreamProtocol::default(),
            transcoding_container: None,
            analyze_duration_ms: None,
            transcode_reasons: None,
            default_audio_index_source: AudioIndexSource::NONE,
            default_audio_stream_index: None,
            default_subtitle_stream_index: None,
            has_segments: false,
        }
    }
}

impl MediaSourceInfo {
    /// Gets the first video stream, if any.
    #[must_use]
    pub fn video_stream(&self) -> Option<&MediaStream> {
        self.media_streams
            .iter()
            .find(|s| s.stream_type == MediaStreamType::Video)
    }

    /// Infers the total bitrate from the media streams.
    ///
    /// Unless `force` is set, an already-known [`Self::bitrate`] is retained.
    pub fn infer_total_bitrate(&mut self, force: bool) {
        if !force && self.bitrate.is_some() {
            return;
        }

        let bitrate: i32 = self
            .media_streams
            .iter()
            .filter(|s| !s.is_external)
            .map(|s| s.bit_rate.unwrap_or(0))
            .sum();

        if bitrate > 0 {
            self.bitrate = Some(bitrate);
        }
    }

    /// Gets the default audio stream, preferring `default_index`, then the
    /// stream marked default, then the first audio stream.
    #[must_use]
    pub fn get_default_audio_stream(&self, default_index: Option<i32>) -> Option<&MediaStream> {
        let explicit = default_index.filter(|&v| v != -1).and_then(|val| {
            self.media_streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Audio && s.index == val)
        });

        explicit
            .or_else(|| {
                self.media_streams
                    .iter()
                    .find(|s| s.stream_type == MediaStreamType::Audio && s.is_default)
            })
            .or_else(|| {
                self.media_streams
                    .iter()
                    .find(|s| s.stream_type == MediaStreamType::Audio)
            })
    }

    /// Gets the media stream of a given type and index.
    #[must_use]
    pub fn get_media_stream(
        &self,
        stream_type: MediaStreamType,
        index: i32,
    ) -> Option<&MediaStream> {
        self.media_streams
            .iter()
            .find(|s| s.stream_type == stream_type && s.index == index)
    }

    /// Gets the number of streams of a given type, or `None` when there are no
    /// streams at all.
    #[must_use]
    pub fn get_stream_count(&self, stream_type: MediaStreamType) -> Option<i32> {
        if self.media_streams.is_empty() {
            return None;
        }

        let matches = self
            .media_streams
            .iter()
            .filter(|s| s.stream_type == stream_type)
            .count();

        i32::try_from(matches).ok()
    }

    /// Determines whether the given audio stream is a secondary audio track.
    ///
    /// Returns `None` when the source has no internal audio stream to compare
    /// against.
    #[must_use]
    pub fn is_secondary_audio(&self, stream: &MediaStream) -> Option<bool> {
        if stream.is_external {
            return Some(false);
        }

        self.media_streams
            .iter()
            .find(|s| s.stream_type == MediaStreamType::Audio && !s.is_external)
            .map(|current| current.index != stream.index)
    }
}
