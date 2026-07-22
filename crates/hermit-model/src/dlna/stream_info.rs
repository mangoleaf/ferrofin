//! Port of `MediaBrowser.Model.Dlna.StreamInfo`.
//!
//! Holds the result of a `StreamBuilder` decision: the chosen play method,
//! codecs, and all derived target-stream properties. Also builds the output
//! streaming URL and the per-subtitle [`SubtitleStreamInfo`] list.

use std::collections::BTreeMap;

use uuid::Uuid;

use super::device_profile::DeviceProfile;
use super::enums::{DlnaProfileType, SubtitleDeliveryMethod, TranscodeSeekInfo};
use super::stream_builder::StreamBuilder;
use super::subtitle_profile::SubtitleProfile;
use super::subtitle_stream_info::SubtitleStreamInfo;
use super::transcoder_support::TranscoderSupport;
use crate::data::{MediaStreamProtocol, VideoRangeType};
use crate::drawing::ImageDimensions;
use crate::drawing::drawing_utils::resize;
use crate::dto::MediaSourceInfo;
use crate::entities::{MediaStreamType, VideoType};
use crate::entities_media::MediaStream;
use crate::session::{PlayMethod, TranscodeReasons, transcode_reasons_unique_names};

/// Information on an output stream produced by the `StreamBuilder`.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct StreamInfo {
    /// The item id.
    pub item_id: Uuid,
    /// The play method.
    pub play_method: PlayMethod,
    /// The encoding context.
    pub context: super::enums::EncodingContext,
    /// The media type.
    pub media_type: DlnaProfileType,
    /// The container.
    pub container: Option<String>,
    /// The sub protocol.
    pub sub_protocol: MediaStreamProtocol,
    /// The start position ticks.
    pub start_position_ticks: i64,
    /// The segment length.
    pub segment_length: Option<i32>,
    /// The minimum segments count.
    pub min_segments: Option<i32>,
    /// Whether the stream requires AVC.
    pub require_avc: bool,
    /// Whether the stream requires a non-anamorphic video.
    pub require_non_anamorphic: bool,
    /// Whether timestamps should be copied.
    pub copy_timestamps: bool,
    /// Whether MPEG-TS M2TS mode is enabled.
    pub enable_mpegts_m2ts_mode: bool,
    /// Whether the subtitle manifest is enabled.
    pub enable_subtitles_in_manifest: bool,
    /// The audio codecs.
    pub audio_codecs: Vec<String>,
    /// The video codecs.
    pub video_codecs: Vec<String>,
    /// The audio stream index.
    pub audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    pub subtitle_stream_index: Option<i32>,
    /// The maximum transcoding audio channels.
    pub transcoding_max_audio_channels: Option<i32>,
    /// The global maximum audio channels.
    pub global_max_audio_channels: Option<i32>,
    /// The audio bitrate.
    pub audio_bitrate: Option<i32>,
    /// The audio sample rate.
    pub audio_sample_rate: Option<i32>,
    /// The video bitrate.
    pub video_bitrate: Option<i32>,
    /// The maximum output width.
    pub max_width: Option<i32>,
    /// The maximum output height.
    pub max_height: Option<i32>,
    /// The maximum framerate.
    pub max_framerate: Option<f32>,
    /// The device profile.
    pub device_profile: DeviceProfile,
    /// The device profile id.
    pub device_profile_id: Option<String>,
    /// The device id.
    pub device_id: Option<String>,
    /// The runtime ticks.
    pub run_time_ticks: Option<i64>,
    /// The transcode seek info.
    pub transcode_seek_info: TranscodeSeekInfo,
    /// Whether content length should be estimated.
    pub estimate_content_length: bool,
    /// The media source info.
    pub media_source: Option<MediaSourceInfo>,
    /// The subtitle codecs.
    pub subtitle_codecs: Vec<String>,
    /// The subtitle delivery method.
    pub subtitle_delivery_method: SubtitleDeliveryMethod,
    /// The subtitle format.
    pub subtitle_format: Option<String>,
    /// The play session id.
    pub play_session_id: Option<String>,
    /// The transcode reasons.
    pub transcode_reasons: TranscodeReasons,
    /// The stream options (case-insensitive keys).
    pub stream_options: BTreeMap<String, String>,
    /// Whether audio VBR encoding is enabled.
    pub enable_audio_vbr_encoding: bool,
    /// Whether to always burn in subtitles when transcoding.
    pub always_burn_in_subtitle_when_transcoding: bool,
}

impl StreamInfo {
    /// Creates a new `StreamInfo` bound to the given device profile.
    #[must_use]
    pub fn new(device_profile: DeviceProfile) -> Self {
        Self {
            item_id: Uuid::nil(),
            play_method: PlayMethod::Transcode,
            context: super::enums::EncodingContext::Streaming,
            media_type: DlnaProfileType::Audio,
            container: None,
            sub_protocol: MediaStreamProtocol::http,
            start_position_ticks: 0,
            segment_length: None,
            min_segments: None,
            require_avc: false,
            require_non_anamorphic: false,
            copy_timestamps: false,
            enable_mpegts_m2ts_mode: false,
            enable_subtitles_in_manifest: false,
            audio_codecs: Vec::new(),
            video_codecs: Vec::new(),
            audio_stream_index: None,
            subtitle_stream_index: None,
            transcoding_max_audio_channels: None,
            global_max_audio_channels: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            video_bitrate: None,
            max_width: None,
            max_height: None,
            max_framerate: None,
            device_profile,
            device_profile_id: None,
            device_id: None,
            run_time_ticks: None,
            transcode_seek_info: TranscodeSeekInfo::Auto,
            estimate_content_length: false,
            media_source: None,
            subtitle_codecs: Vec::new(),
            subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
            subtitle_format: None,
            play_session_id: None,
            transcode_reasons: TranscodeReasons::empty(),
            stream_options: BTreeMap::new(),
            enable_audio_vbr_encoding: false,
            always_burn_in_subtitle_when_transcoding: false,
        }
    }

    /// Gets the media source id.
    #[must_use]
    pub fn media_source_id(&self) -> Option<&str> {
        self.media_source.as_ref().and_then(|m| m.id.as_deref())
    }

    /// Gets a value indicating whether the stream is direct.
    #[must_use]
    pub fn is_direct_stream(&self) -> bool {
        let video_type = self.media_source.as_ref().and_then(|m| m.video_type);
        !matches!(video_type, Some(VideoType::Dvd | VideoType::BluRay))
            && matches!(
                self.play_method,
                PlayMethod::DirectStream | PlayMethod::DirectPlay
            )
    }

    /// Gets the audio stream that will be used in the output stream.
    #[must_use]
    pub fn target_audio_stream(&self) -> Option<&MediaStream> {
        self.media_source
            .as_ref()
            .and_then(|m| m.get_default_audio_stream(self.audio_stream_index))
    }

    /// Gets the video stream that will be used in the output stream.
    #[must_use]
    pub fn target_video_stream(&self) -> Option<&MediaStream> {
        self.media_source
            .as_ref()
            .and_then(MediaSourceInfo::video_stream)
    }

    /// Gets the target video level that will be in the output stream.
    #[must_use]
    pub fn target_video_level(&self) -> Option<f64> {
        if self.is_direct_stream() {
            return self.target_video_stream().and_then(|s| s.level);
        }

        let target = self.target_video_codec();
        if let Some(codec) = target.first()
            && !codec.is_empty()
        {
            return self.get_target_video_level(Some(codec));
        }

        self.target_video_stream().and_then(|s| s.level)
    }

    /// Gets the target video bit depth that will be in the output stream.
    #[must_use]
    pub fn target_video_bit_depth(&self) -> Option<i32> {
        if self.is_direct_stream() {
            return self.target_video_stream().and_then(|s| s.bit_depth);
        }

        let target = self.target_video_codec();
        if let Some(codec) = target.first()
            && !codec.is_empty()
        {
            return self.get_target_video_bit_depth(Some(codec));
        }

        self.target_video_stream().and_then(|s| s.bit_depth)
    }

    /// Gets the target video profile that will be in the output stream.
    #[must_use]
    pub fn target_video_profile(&self) -> Option<String> {
        if self.is_direct_stream() {
            return self.target_video_stream().and_then(|s| s.profile.clone());
        }

        let target = self.target_video_codec();
        if let Some(codec) = target.first()
            && !codec.is_empty()
        {
            return self.get_option(Some(codec), "profile").map(str::to_owned);
        }

        self.target_video_stream().and_then(|s| s.profile.clone())
    }

    /// Gets the target video range type that will be in the output stream.
    #[must_use]
    pub fn target_video_range_type(&self) -> VideoRangeType {
        if self.is_direct_stream() {
            return self
                .target_video_stream()
                .map_or(VideoRangeType::Unknown, MediaStream::video_range_type);
        }

        let target = self.target_video_codec();
        if let Some(codec) = target.first()
            && !codec.is_empty()
            && let Some(value) = self.get_option(Some(codec), "rangetype")
            && let Some(rt) = super::condition_processor::parse_video_range_type_pub(value)
        {
            return rt;
        }

        self.target_video_stream()
            .map_or(VideoRangeType::Unknown, MediaStream::video_range_type)
    }

    /// Gets the audio codec that will be in the output stream.
    #[must_use]
    pub fn target_audio_codec(&self) -> Vec<String> {
        let input_codec = self.target_audio_stream().and_then(|s| s.codec.clone());

        if self.is_direct_stream() {
            return match input_codec {
                Some(c) if !c.is_empty() => vec![c],
                _ => Vec::new(),
            };
        }

        for codec in &self.audio_codecs {
            if input_codec
                .as_deref()
                .is_some_and(|ic| codec.eq_ignore_ascii_case(ic))
            {
                return if codec.is_empty() {
                    Vec::new()
                } else {
                    vec![codec.clone()]
                };
            }
        }

        self.audio_codecs.clone()
    }

    /// Gets the video codec that will be in the output stream.
    #[must_use]
    pub fn target_video_codec(&self) -> Vec<String> {
        let input_codec = self.target_video_stream().and_then(|s| s.codec.clone());

        if self.is_direct_stream() {
            return match input_codec {
                Some(c) if !c.is_empty() => vec![c],
                _ => Vec::new(),
            };
        }

        for codec in &self.video_codecs {
            if input_codec
                .as_deref()
                .is_some_and(|ic| codec.eq_ignore_ascii_case(ic))
            {
                return if codec.is_empty() {
                    Vec::new()
                } else {
                    vec![codec.clone()]
                };
            }
        }

        self.video_codecs.clone()
    }

    /// Gets the target width of the output stream.
    #[must_use]
    pub fn target_width(&self) -> Option<i32> {
        if let Some(vs) = self.target_video_stream()
            && let (Some(w), Some(h)) = (vs.width, vs.height)
        {
            let size = ImageDimensions::new(w, h);
            let size = resize(
                size,
                0,
                0,
                self.max_width.unwrap_or(0),
                self.max_height.unwrap_or(0),
            );
            return Some(size.width);
        }
        self.max_width
    }

    /// Gets the target height of the output stream.
    #[must_use]
    pub fn target_height(&self) -> Option<i32> {
        if let Some(vs) = self.target_video_stream()
            && let (Some(w), Some(h)) = (vs.width, vs.height)
        {
            let size = ImageDimensions::new(w, h);
            let size = resize(
                size,
                0,
                0,
                self.max_width.unwrap_or(0),
                self.max_height.unwrap_or(0),
            );
            return Some(size.height);
        }
        self.max_height
    }

    /// Sets a stream option, optionally qualified by a codec.
    pub fn set_option_qualified(&mut self, qualifier: Option<&str>, name: &str, value: String) {
        match qualifier {
            Some(q) if !q.is_empty() => self.set_option(&format!("{q}-{name}"), value),
            _ => self.set_option(name, value),
        }
    }

    /// Sets a stream option.
    pub fn set_option(&mut self, name: &str, value: String) {
        self.stream_options.insert(name.to_ascii_lowercase(), value);
    }

    /// Gets a stream option, optionally qualified by a codec.
    #[must_use]
    pub fn get_option(&self, qualifier: Option<&str>, name: &str) -> Option<&str> {
        if let Some(q) = qualifier
            && let Some(value) = self.get_option_raw(&format!("{q}-{name}"))
            && !value.is_empty()
        {
            return Some(value);
        }
        self.get_option_raw(name).filter(|v| !v.is_empty())
    }

    /// Gets a stream option by name.
    #[must_use]
    fn get_option_raw(&self, name: &str) -> Option<&str> {
        self.stream_options
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Gets the target video bit depth for a codec.
    #[must_use]
    pub fn get_target_video_bit_depth(&self, codec: Option<&str>) -> Option<i32> {
        self.get_option(codec, "videobitdepth")
            .and_then(|v| v.parse().ok())
    }

    /// Gets the target audio bit depth for a codec.
    #[must_use]
    pub fn get_target_audio_bit_depth(&self, codec: Option<&str>) -> Option<i32> {
        self.get_option(codec, "audiobitdepth")
            .and_then(|v| v.parse().ok())
    }

    /// Gets the target video level for a codec.
    #[must_use]
    pub fn get_target_video_level(&self, codec: Option<&str>) -> Option<f64> {
        self.get_option(codec, "level").and_then(|v| v.parse().ok())
    }

    /// Gets the target reference frames for a codec.
    #[must_use]
    pub fn get_target_ref_frames(&self, codec: Option<&str>) -> Option<i32> {
        self.get_option(codec, "maxrefframes")
            .and_then(|v| v.parse().ok())
    }

    /// Gets the target audio channels for a codec.
    #[must_use]
    pub fn get_target_audio_channels(&self, codec: Option<&str>) -> Option<i32> {
        let default_value = self
            .global_max_audio_channels
            .or(self.transcoding_max_audio_channels);

        let value = self.get_option(codec, "audiochannels");
        let Some(value) = value else {
            return default_value;
        };
        if value.is_empty() {
            return default_value;
        }

        if let Ok(result) = value.parse::<i32>() {
            return Some(result.min(default_value.unwrap_or(result)));
        }

        default_value
    }

    /// Returns the output stream URL for this class.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn to_url(
        &self,
        base_url: Option<&str>,
        access_token: Option<&str>,
        query: Option<&str>,
    ) -> String {
        let mut sb = String::new();
        if let Some(base_url) = base_url.filter(|b| !b.is_empty()) {
            sb.push_str(base_url.trim_end_matches('/'));
        }

        if self.media_type == DlnaProfileType::Audio {
            sb.push_str("/audio/");
        } else {
            sb.push_str("/videos/");
        }

        let mut item_id_buf = Uuid::encode_buffer();
        sb.push_str(self.item_id.as_hyphenated().encode_lower(&mut item_id_buf));

        if self.sub_protocol == MediaStreamProtocol::hls {
            sb.push_str("/master.m3u8");
        } else {
            sb.push_str("/stream");
            if let Some(container) = self.container.as_deref().filter(|c| !c.is_empty()) {
                sb.push('.');
                sb.push_str(container);
            }
        }

        let query_start = sb.len();

        let is_direct_stream = self.is_direct_stream();

        if let Some(v) = self.device_profile_id.as_deref().filter(|v| !v.is_empty()) {
            sb.push_str("&DeviceProfileId=");
            sb.push_str(v);
        }
        if let Some(v) = self.device_id.as_deref().filter(|v| !v.is_empty()) {
            sb.push_str("&DeviceId=");
            sb.push_str(v);
        }
        if let Some(v) = self.media_source_id().filter(|v| !v.is_empty()) {
            sb.push_str("&MediaSourceId=");
            sb.push_str(v);
        }

        if is_direct_stream {
            sb.push_str("&Static=true");
        }

        if !self.video_codecs.is_empty() {
            sb.push_str("&VideoCodec=");
            sb.push_str(&self.video_codecs.join(","));
        }
        if !self.audio_codecs.is_empty() {
            sb.push_str("&AudioCodec=");
            sb.push_str(&self.audio_codecs.join(","));
        }

        if let Some(v) = self.audio_stream_index {
            sb.push_str("&AudioStreamIndex=");
            sb.push_str(&v.to_string());
        }

        if let Some(v) = self.subtitle_stream_index
            && (self.always_burn_in_subtitle_when_transcoding
                || self.subtitle_delivery_method != SubtitleDeliveryMethod::External)
            && v != -1
        {
            sb.push_str("&SubtitleStreamIndex=");
            sb.push_str(&v.to_string());
        }

        if let Some(v) = self.video_bitrate {
            sb.push_str("&VideoBitrate=");
            sb.push_str(&v.to_string());
        }
        if let Some(v) = self.audio_bitrate {
            sb.push_str("&AudioBitrate=");
            sb.push_str(&v.to_string());
        }
        if let Some(v) = self.audio_sample_rate {
            sb.push_str("&AudioSampleRate=");
            sb.push_str(&v.to_string());
        }
        if let Some(v) = self.max_framerate {
            sb.push_str("&MaxFramerate=");
            sb.push_str(&format_float(v));
        }
        if let Some(v) = self.max_width {
            sb.push_str("&MaxWidth=");
            sb.push_str(&v.to_string());
        }
        if let Some(v) = self.max_height {
            sb.push_str("&MaxHeight=");
            sb.push_str(&v.to_string());
        }

        if self.sub_protocol == MediaStreamProtocol::hls {
            if let Some(container) = self.container.as_deref().filter(|c| !c.is_empty()) {
                sb.push_str("&SegmentContainer=");
                sb.push_str(container);
            }
            if let Some(v) = self.segment_length {
                sb.push_str("&SegmentLength=");
                sb.push_str(&v.to_string());
            }
            if let Some(v) = self.min_segments {
                sb.push_str("&MinSegments=");
                sb.push_str(&v.to_string());
            }
        } else if self.start_position_ticks != 0 {
            sb.push_str("&StartTimeTicks=");
            sb.push_str(&self.start_position_ticks.to_string());
        }

        if let Some(v) = self.play_session_id.as_deref().filter(|v| !v.is_empty()) {
            sb.push_str("&PlaySessionId=");
            sb.push_str(v);
        }
        if let Some(v) = access_token.filter(|v| !v.is_empty()) {
            sb.push_str("&ApiKey=");
            sb.push_str(v);
        }

        let live_stream_id = self
            .media_source
            .as_ref()
            .and_then(|m| m.live_stream_id.as_deref());
        if let Some(v) = live_stream_id.filter(|v| !v.is_empty()) {
            sb.push_str("&LiveStreamId=");
            sb.push_str(v);
        }

        if !is_direct_stream {
            if self.require_non_anamorphic {
                sb.push_str("&RequireNonAnamorphic=");
                sb.push_str(&bool_pascal(self.require_non_anamorphic));
            }
            if let Some(v) = self.transcoding_max_audio_channels {
                sb.push_str("&TranscodingMaxAudioChannels=");
                sb.push_str(&v.to_string());
            }
            if self.enable_subtitles_in_manifest {
                sb.push_str("&EnableSubtitlesInManifest=");
                sb.push_str(&bool_pascal(self.enable_subtitles_in_manifest));
            }
            if self.enable_mpegts_m2ts_mode {
                sb.push_str("&EnableMpegtsM2TsMode=");
                sb.push_str(&bool_pascal(self.enable_mpegts_m2ts_mode));
            }
            if self.estimate_content_length {
                sb.push_str("&EstimateContentLength=");
                sb.push_str(&bool_pascal(self.estimate_content_length));
            }
            if self.transcode_seek_info != TranscodeSeekInfo::Auto {
                sb.push_str("&TranscodeSeekInfo=");
                sb.push_str(transcode_seek_info_name(self.transcode_seek_info));
            }
            if self.copy_timestamps {
                sb.push_str("&CopyTimestamps=");
                sb.push_str(&bool_pascal(self.copy_timestamps));
            }
            sb.push_str("&RequireAvc=");
            sb.push_str(&bool_lower(self.require_avc));
            sb.push_str("&EnableAudioVbrEncoding=");
            sb.push_str(&bool_lower(self.enable_audio_vbr_encoding));
        }

        let etag = self.media_source.as_ref().and_then(|m| m.e_tag.as_deref());
        if let Some(v) = etag.filter(|v| !v.is_empty()) {
            sb.push_str("&Tag=");
            sb.push_str(v);
        }

        if self.subtitle_stream_index.is_some()
            && self.subtitle_delivery_method != SubtitleDeliveryMethod::External
        {
            sb.push_str("&SubtitleMethod=");
            sb.push_str(subtitle_delivery_method_name(self.subtitle_delivery_method));
        }

        if self.subtitle_stream_index.is_some()
            && self.subtitle_delivery_method == SubtitleDeliveryMethod::Embed
            && !self.subtitle_codecs.is_empty()
        {
            sb.push_str("&SubtitleCodec=");
            sb.push_str(&self.subtitle_codecs.join(","));
        }

        for (key, value) in &self.stream_options {
            sb.push('&');
            sb.push_str(key);
            sb.push('=');
            sb.push_str(&value.replace(' ', ""));
        }

        let reason_names = transcode_reasons_unique_names(self.transcode_reasons);
        if !is_direct_stream && !reason_names.is_empty() {
            sb.push_str("&TranscodeReasons=");
            sb.push_str(&reason_names.join(","));
        }

        if let Some(query) = query.filter(|q| !q.is_empty()) {
            sb.push_str(query);
        }

        // Replace the first '&' with '?' to form a valid query string.
        if sb.len() > query_start {
            // SAFETY: query_start indexes an ASCII '&' or '/' boundary.
            unsafe {
                sb.as_bytes_mut()[query_start] = b'?';
            }
        }

        sb
    }

    /// Gets the subtitle profiles for this stream.
    #[must_use]
    pub fn get_subtitle_profiles(
        &self,
        transcoder_support: &dyn TranscoderSupport,
        include_selected_track_only: bool,
        enable_all_profiles: bool,
        base_url: &str,
        access_token: Option<&str>,
    ) -> Vec<SubtitleStreamInfo> {
        let Some(media_source) = self.media_source.as_ref() else {
            return Vec::new();
        };

        let mut list = Vec::new();

        let start_position_ticks = if self.sub_protocol == MediaStreamProtocol::hls {
            0
        } else if self.play_method == PlayMethod::Transcode && !self.copy_timestamps {
            self.start_position_ticks
        } else {
            0
        };

        if let Some(index) = self.subtitle_stream_index {
            for stream in &media_source.media_streams {
                if stream.stream_type == MediaStreamType::Subtitle && stream.index == index {
                    self.add_subtitle_profiles(
                        &mut list,
                        stream,
                        transcoder_support,
                        enable_all_profiles,
                        base_url,
                        access_token,
                        start_position_ticks,
                    );
                }
            }
        }

        if !include_selected_track_only {
            for stream in &media_source.media_streams {
                if stream.stream_type == MediaStreamType::Subtitle
                    && self.subtitle_stream_index != Some(stream.index)
                {
                    self.add_subtitle_profiles(
                        &mut list,
                        stream,
                        transcoder_support,
                        enable_all_profiles,
                        base_url,
                        access_token,
                        start_position_ticks,
                    );
                }
            }
        }

        list
    }

    #[allow(clippy::too_many_arguments)]
    fn add_subtitle_profiles(
        &self,
        list: &mut Vec<SubtitleStreamInfo>,
        stream: &MediaStream,
        transcoder_support: &dyn TranscoderSupport,
        enable_all_profiles: bool,
        base_url: &str,
        access_token: Option<&str>,
        start_position_ticks: i64,
    ) {
        if enable_all_profiles {
            for profile in &self.device_profile.subtitle_profiles {
                if let Some(info) = self.get_subtitle_stream_info(
                    stream,
                    base_url,
                    access_token,
                    start_position_ticks,
                    std::slice::from_ref(profile),
                    transcoder_support,
                ) {
                    list.push(info);
                }
            }
        } else if let Some(info) = self.get_subtitle_stream_info(
            stream,
            base_url,
            access_token,
            start_position_ticks,
            &self.device_profile.subtitle_profiles,
            transcoder_support,
        ) {
            list.push(info);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn get_subtitle_stream_info(
        &self,
        stream: &MediaStream,
        base_url: &str,
        access_token: Option<&str>,
        start_position_ticks: i64,
        subtitle_profiles: &[SubtitleProfile],
        transcoder_support: &dyn TranscoderSupport,
    ) -> Option<SubtitleStreamInfo> {
        let media_source = self.media_source.as_ref()?;

        let subtitle_profile = StreamBuilder::get_subtitle_profile(
            media_source,
            stream,
            subtitle_profiles,
            self.play_method,
            transcoder_support,
            self.container.as_deref(),
            None,
        );

        let mut info = SubtitleStreamInfo {
            is_forced: stream.is_forced,
            language: stream.language.clone(),
            name: Some(
                stream
                    .language
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_owned()),
            ),
            format: subtitle_profile.format.clone(),
            index: stream.index,
            delivery_method: subtitle_profile.method,
            display_title: stream.display_title(),
            url: None,
            is_external_url: false,
        };

        if info.delivery_method == SubtitleDeliveryMethod::External {
            info.url = Some(format!(
                "{}/Videos/{}/{}/Subtitles/{}/{}/Stream.{}",
                base_url,
                self.item_id.as_hyphenated(),
                self.media_source_id().unwrap_or(""),
                stream.index,
                start_position_ticks,
                subtitle_profile.format.as_deref().unwrap_or("")
            ));
            info.is_external_url = false;

            let is_absolute_http = stream
                .path
                .as_deref()
                .is_some_and(|p| p.starts_with("http://") || p.starts_with("https://"));

            if stream.is_external
                && stream.supports_external_stream
                && stream
                    .codec
                    .as_deref()
                    .zip(subtitle_profile.format.as_deref())
                    .is_some_and(|(c, f)| c.eq_ignore_ascii_case(f))
                && stream.path.as_deref().is_some_and(|p| !p.is_empty())
                && is_absolute_http
            {
                info.url.clone_from(&stream.path);
                info.is_external_url = true;
            }

            if !info.is_external_url
                && let Some(token) = access_token.filter(|t| !t.is_empty())
                && let Some(url) = info.url.as_mut()
            {
                url.push_str("?ApiKey=");
                url.push_str(token);
            }
        }

        Some(info)
    }
}

fn bool_pascal(v: bool) -> String {
    if v {
        "True".to_owned()
    } else {
        "False".to_owned()
    }
}

fn bool_lower(v: bool) -> String {
    if v {
        "true".to_owned()
    } else {
        "false".to_owned()
    }
}

fn transcode_seek_info_name(v: TranscodeSeekInfo) -> &'static str {
    match v {
        TranscodeSeekInfo::Auto => "Auto",
        TranscodeSeekInfo::Bytes => "Bytes",
    }
}

fn subtitle_delivery_method_name(v: SubtitleDeliveryMethod) -> &'static str {
    match v {
        SubtitleDeliveryMethod::Encode => "Encode",
        SubtitleDeliveryMethod::Embed => "Embed",
        SubtitleDeliveryMethod::External => "External",
        SubtitleDeliveryMethod::Hls => "Hls",
        SubtitleDeliveryMethod::Drop => "Drop",
    }
}

/// Formats a float the way .NET's invariant `float.ToString()` does for the
/// values exercised here (whole numbers without a trailing `.0`).
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
fn format_float(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}
