//! Media-info enums and helpers — port of `MediaBrowser.Model.MediaInfo`.
//!
//! `MediaProtocol`, `TransportStreamTimestamp`, the `SubtitleFormat` string
//! constants, the `AudioCodec` friendly-name helper, plus the
//! `BlurayDiscInfo` / `SubtitleTrackInfo` / `SubtitleTrackEvent` /
//! `AudioIndexSource` / `LiveStreamRequest` structs.
//!
//! The `MediaInfo`, `LiveStreamResponse`, and `PlaybackInfoResponse` structs
//! live here too, now that their `MediaSourceInfo` / `BaseItemPerson`
//! dependencies have landed.

use std::collections::HashMap;

use bitflags::bitflags;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::dlna::PlaybackErrorCode;
use crate::dto::{BaseItemPerson, MediaSourceInfo};
use crate::entities_media::{ChapterInfo, MediaStream};

/// Enum `MediaProtocol` — how a media source is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MediaProtocol {
    /// Local file.
    #[default]
    File = 0,
    /// HTTP.
    Http = 1,
    /// RTMP.
    Rtmp = 2,
    /// RTSP.
    Rtsp = 3,
    /// UDP.
    Udp = 4,
    /// RTP.
    Rtp = 5,
    /// FTP.
    Ftp = 6,
}

/// The type of timestamps used in a transport stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TransportStreamTimestamp {
    /// The stream contains no timestamps.
    None,
    /// The stream contains zero-value timestamps.
    Zero,
    /// The stream contains valid timestamps.
    Valid,
}

/// Subtitle format string constants (C# `SubtitleFormat` static class).
pub mod subtitle_format {
    /// SubRip (`srt`).
    pub const SRT: &str = "srt";
    /// SubRip alternate label (`subrip`).
    pub const SUBRIP: &str = "subrip";
    /// SubStation Alpha (`ssa`).
    pub const SSA: &str = "ssa";
    /// Advanced SubStation Alpha (`ass`).
    pub const ASS: &str = "ass";
    /// WebVTT (`vtt`).
    pub const VTT: &str = "vtt";
    /// WebVTT alternate label (`webvtt`).
    pub const WEBVTT: &str = "webvtt";
    /// Timed Text Markup Language (`ttml`).
    pub const TTML: &str = "ttml";
}

/// Audio codec helpers (C# `AudioCodec` static class).
pub mod audio_codec {
    /// Returns a human-friendly name for an audio `codec` identifier.
    ///
    /// Well-known Dolby/DTS codecs get their marketing names; anything else is
    /// upper-cased. An empty input is returned unchanged (matching the C#
    /// behavior).
    #[must_use]
    pub fn friendly_name(codec: &str) -> String {
        if codec.is_empty() {
            return codec.to_owned();
        }

        if codec.eq_ignore_ascii_case("ac3") {
            return "Dolby Digital".to_owned();
        }

        if codec.eq_ignore_ascii_case("eac3") {
            return "Dolby Digital+".to_owned();
        }

        if codec.eq_ignore_ascii_case("dca") {
            return "DTS".to_owned();
        }

        codec.to_uppercase()
    }
}

bitflags! {
    /// How the audio index is determined (`[Flags]` upstream).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct AudioIndexSource: u32 {
        /// The default index when no preference is specified.
        const NONE = 0;
        /// The index is calculated whether the track is marked as default or not.
        const DEFAULT = 1 << 0;
        /// The index is calculated whether the track is in preferred language or not.
        const LANGUAGE = 1 << 1;
        /// The index is specified by the user.
        const USER = 1 << 2;
    }
}

/// Represents the result of BDInfo output.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BlurayDiscInfo {
    /// The media streams.
    pub media_streams: Vec<MediaStream>,
    /// The run time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,
    /// The files.
    pub files: Vec<String>,
    /// The playlist name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_name: Option<String>,
    /// The chapters.
    pub chapters: Vec<f64>,
}

/// A single subtitle track event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleTrackEvent {
    /// The event id.
    pub id: String,
    /// The event text.
    pub text: String,
    /// The start position ticks.
    pub start_position_ticks: i64,
    /// The end position ticks.
    pub end_position_ticks: i64,
}

impl SubtitleTrackEvent {
    /// Initializes a new instance of the [`SubtitleTrackEvent`] struct.
    #[must_use]
    pub fn new(id: String, text: String) -> Self {
        Self {
            id,
            text,
            start_position_ticks: 0,
            end_position_ticks: 0,
        }
    }
}

/// A parsed subtitle track (a list of timed events).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleTrackInfo {
    /// The track events.
    pub track_events: Vec<SubtitleTrackEvent>,
}

/// A request to open a live stream.
///
/// The upstream `DeviceProfile` field is deferred to a later port unit and is
/// intentionally omitted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LiveStreamRequest {
    /// The open token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_token: Option<String>,
    /// The user id.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,
    /// The play session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    /// The maximum streaming bitrate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<i32>,
    /// The start time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<i64>,
    /// The audio stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    /// The maximum audio channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_audio_channels: Option<i32>,
    /// The item id.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,
    /// Whether direct play is enabled.
    pub enable_direct_play: bool,
    /// Whether direct stream is enabled.
    pub enable_direct_stream: bool,
    /// Whether subtitles are always burned in when transcoding.
    pub always_burn_in_subtitle_when_transcoding: bool,
    /// The direct-play protocols.
    pub direct_play_protocols: Vec<MediaProtocol>,
}

impl Default for LiveStreamRequest {
    fn default() -> Self {
        Self {
            open_token: None,
            user_id: Uuid::nil(),
            play_session_id: None,
            max_streaming_bitrate: None,
            start_time_ticks: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            max_audio_channels: None,
            item_id: Uuid::nil(),
            enable_direct_play: true,
            enable_direct_stream: true,
            always_burn_in_subtitle_when_transcoding: false,
            direct_play_protocols: vec![MediaProtocol::Http],
        }
    }
}

/// Class `PlaybackInfoResponse` — the response to a playback-info request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoResponse {
    /// Gets or sets the media sources.
    pub media_sources: Vec<MediaSourceInfo>,
    /// Gets or sets the play session identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    /// Gets or sets the error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<PlaybackErrorCode>,
}

/// Class `LiveStreamResponse` — the response to opening a live stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LiveStreamResponse {
    /// Gets or sets the media source.
    pub media_source: MediaSourceInfo,
}

impl LiveStreamResponse {
    /// Initializes a new instance of the [`LiveStreamResponse`] struct.
    #[must_use]
    pub fn new(media_source: MediaSourceInfo) -> Self {
        Self { media_source }
    }
}

/// Class `MediaInfo` — a [`MediaSourceInfo`] enriched with item metadata.
///
/// Upstream this derives from `MediaSourceInfo`; here the base source is
/// flattened so the wire shape (base fields alongside the extra metadata
/// fields) is preserved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaInfo {
    /// The underlying media source (flattened onto this object).
    #[serde(flatten)]
    pub media_source: MediaSourceInfo,

    /// Gets or sets the chapters.
    pub chapters: Vec<ChapterInfo>,

    /// Gets or sets the album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,

    /// Gets or sets the artists.
    pub artists: Vec<String>,

    /// Gets or sets the album artists.
    pub album_artists: Vec<String>,

    /// Gets or sets the studios.
    pub studios: Vec<String>,

    /// Gets or sets the genres.
    pub genres: Vec<String>,

    /// Gets or sets the show name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_name: Option<String>,

    /// Gets or sets the forced sort name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_sort_name: Option<String>,

    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,

    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<i32>,

    /// Gets or sets the production year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,

    /// Gets or sets the premiere date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub premiere_date: Option<DateTime<Utc>>,

    /// Gets or sets the people.
    pub people: Vec<BaseItemPerson>,

    /// Gets or sets the provider ids.
    pub provider_ids: HashMap<String, String>,

    /// Gets or sets the official rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_rating: Option<String>,

    /// Gets or sets the official rating description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_rating_description: Option<String>,

    /// Gets or sets the overview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_protocol_default_and_round_trip() {
        assert_eq!(MediaProtocol::default(), MediaProtocol::File);
        for variant in [
            MediaProtocol::File,
            MediaProtocol::Http,
            MediaProtocol::Rtmp,
            MediaProtocol::Rtsp,
            MediaProtocol::Udp,
            MediaProtocol::Rtp,
            MediaProtocol::Ftp,
        ] {
            let back: MediaProtocol =
                serde_json::from_str(&serde_json::to_string(&variant).unwrap()).unwrap();
            assert_eq!(variant, back);
        }
        assert_eq!(
            serde_json::to_string(&MediaProtocol::Http).unwrap(),
            "\"Http\""
        );
    }

    #[test]
    fn transport_stream_timestamp_round_trips() {
        for variant in [
            TransportStreamTimestamp::None,
            TransportStreamTimestamp::Zero,
            TransportStreamTimestamp::Valid,
        ] {
            let back: TransportStreamTimestamp =
                serde_json::from_str(&serde_json::to_string(&variant).unwrap()).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn audio_codec_friendly_names() {
        assert_eq!(audio_codec::friendly_name(""), "");
        assert_eq!(audio_codec::friendly_name("ac3"), "Dolby Digital");
        assert_eq!(audio_codec::friendly_name("AC3"), "Dolby Digital");
        assert_eq!(audio_codec::friendly_name("eac3"), "Dolby Digital+");
        assert_eq!(audio_codec::friendly_name("dca"), "DTS");
        assert_eq!(audio_codec::friendly_name("flac"), "FLAC");
    }

    #[test]
    fn subtitle_format_constants() {
        assert_eq!(subtitle_format::SRT, "srt");
        assert_eq!(subtitle_format::VTT, "vtt");
        assert_eq!(subtitle_format::ASS, "ass");
    }

    #[test]
    fn audio_index_source_flags() {
        let combined = AudioIndexSource::DEFAULT | AudioIndexSource::USER;
        assert!(combined.contains(AudioIndexSource::USER));
        assert!(!combined.contains(AudioIndexSource::LANGUAGE));
        assert_eq!(AudioIndexSource::default(), AudioIndexSource::NONE);
    }

    #[test]
    fn subtitle_track_event_new_and_round_trip() {
        let event = SubtitleTrackEvent::new("1".to_owned(), "Hello".to_owned());
        assert_eq!(event.start_position_ticks, 0);
        assert_eq!(event.end_position_ticks, 0);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["Id"], "1");
        assert_eq!(json["Text"], "Hello");
        let back: SubtitleTrackEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn subtitle_track_info_round_trips() {
        let info = SubtitleTrackInfo {
            track_events: vec![SubtitleTrackEvent::new("a".to_owned(), "t".to_owned())],
        };
        let back: SubtitleTrackInfo =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn bluray_disc_info_round_trips() {
        let info = BlurayDiscInfo {
            run_time_ticks: Some(1_000),
            files: vec!["00001.m2ts".to_owned()],
            playlist_name: Some("00000.mpls".to_owned()),
            chapters: vec![0.0, 60.0],
            ..BlurayDiscInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["RunTimeTicks"], 1_000);
        assert_eq!(json["PlaylistName"], "00000.mpls");
        let back: BlurayDiscInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn live_stream_request_default_and_round_trip() {
        let req = LiveStreamRequest::default();
        assert!(req.enable_direct_play);
        assert!(req.enable_direct_stream);
        assert_eq!(req.direct_play_protocols, vec![MediaProtocol::Http]);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["EnableDirectPlay"], true);
        assert_eq!(json["DirectPlayProtocols"], serde_json::json!(["Http"]));
        let back: LiveStreamRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn playback_info_response_field_names_and_round_trip() {
        let resp = PlaybackInfoResponse {
            media_sources: vec![MediaSourceInfo::default()],
            play_session_id: Some("pss".to_owned()),
            error_code: Some(PlaybackErrorCode::NoCompatibleStream),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("MediaSources").is_some());
        assert_eq!(json["PlaySessionId"], "pss");
        assert_eq!(json["ErrorCode"], "NoCompatibleStream");
        let back: PlaybackInfoResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn playback_info_response_omits_optional_when_none() {
        let json = serde_json::to_value(PlaybackInfoResponse::default()).unwrap();
        assert!(json.get("PlaySessionId").is_none());
        assert!(json.get("ErrorCode").is_none());
        assert_eq!(json["MediaSources"], serde_json::json!([]));
    }

    #[test]
    fn live_stream_response_new_and_round_trip() {
        let resp = LiveStreamResponse::new(MediaSourceInfo::default());
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("MediaSource").is_some());
        let back: LiveStreamResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn media_info_field_names_and_round_trip() {
        let info = MediaInfo {
            album: Some("Album".to_owned()),
            artists: vec!["Artist".to_owned()],
            album_artists: vec!["AlbumArtist".to_owned()],
            genres: vec!["Rock".to_owned()],
            index_number: Some(3),
            production_year: Some(1999),
            overview: Some("An overview.".to_owned()),
            ..MediaInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["Album"], "Album");
        assert_eq!(json["Artists"], serde_json::json!(["Artist"]));
        assert_eq!(json["AlbumArtists"], serde_json::json!(["AlbumArtist"]));
        assert_eq!(json["Genres"], serde_json::json!(["Rock"]));
        assert_eq!(json["IndexNumber"], 3);
        assert_eq!(json["ProductionYear"], 1999);
        assert_eq!(json["Overview"], "An overview.");
        // Flattened base-source field is present at the top level.
        assert!(json.get("Protocol").is_some());
        let back: MediaInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }
}
