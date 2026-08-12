//! Port of `MediaBrowser.Model.Dlna.StreamBuilder`.
//!
//! The transcode / direct-play / remux decision engine. Given a
//! [`MediaOptions`] (device profile + candidate media sources), it produces the
//! optimal [`StreamInfo`] with the chosen [`PlayMethod`] and accumulated
//! [`TranscodeReasons`].

use std::cmp::Ordering;

use super::codec_profile::CodecProfile;
use super::condition_processor::{ConditionProcessor, range_type_name, video_range_type_all_names};
use super::device_profile::DeviceProfile;
use super::direct_play_profile::DirectPlayProfile;
use super::enums::{
    CodecType, DlnaProfileType, ProfileConditionType, ProfileConditionValue, SubtitleDeliveryMethod,
};
use super::media_options::MediaOptions;
use super::profile_condition::ProfileCondition;
use super::stream_info::StreamInfo;
use super::subtitle_profile::SubtitleProfile;
use super::transcoder_support::TranscoderSupport;
use super::transcoding_profile::TranscodingProfile;
use crate::data::{MediaStreamProtocol, VideoRangeType};
use crate::dto::MediaSourceInfo;
use crate::entities::{MediaStreamType, VideoType};
use crate::entities_media::MediaStream;
use crate::extensions::{contains_container, split};
use crate::media_info::{AudioIndexSource, MediaProtocol, TransportStreamTimestamp};
use crate::session::{PlayMethod, TranscodeReasons};

const SUPPORTED_HLS_VIDEO_CODECS: [&str; 4] = ["h264", "hevc", "vp9", "av1"];
const SUPPORTED_HLS_AUDIO_CODECS_TS: [&str; 4] = ["aac", "ac3", "eac3", "mp3"];
const SUPPORTED_HLS_AUDIO_CODECS_MP4: [&str; 9] = [
    "aac", "ac3", "eac3", "mp3", "alac", "flac", "opus", "dts", "truehd",
];

/// Class `StreamBuilder`.
pub struct StreamBuilder<'a> {
    transcoder_support: &'a dyn TranscoderSupport,
}

impl StreamBuilder<'_> {
    // Aliases.
    const CONTAINER_REASONS: TranscodeReasons = TranscodeReasons::CONTAINER_NOT_SUPPORTED
        .union(TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT);
    const AUDIO_CODEC_REASONS: TranscodeReasons = TranscodeReasons::AUDIO_BITRATE_NOT_SUPPORTED
        .union(TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED)
        .union(TranscodeReasons::AUDIO_PROFILE_NOT_SUPPORTED)
        .union(TranscodeReasons::AUDIO_SAMPLE_RATE_NOT_SUPPORTED)
        .union(TranscodeReasons::SECONDARY_AUDIO_NOT_SUPPORTED)
        .union(TranscodeReasons::AUDIO_BIT_DEPTH_NOT_SUPPORTED)
        .union(TranscodeReasons::AUDIO_IS_EXTERNAL);
    const AUDIO_REASONS: TranscodeReasons =
        TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED.union(Self::AUDIO_CODEC_REASONS);
    const VIDEO_CODEC_REASONS: TranscodeReasons = TranscodeReasons::VIDEO_RESOLUTION_NOT_SUPPORTED
        .union(TranscodeReasons::ANAMORPHIC_VIDEO_NOT_SUPPORTED)
        .union(TranscodeReasons::INTERLACED_VIDEO_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_BIT_DEPTH_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_BITRATE_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_FRAMERATE_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_LEVEL_NOT_SUPPORTED)
        .union(TranscodeReasons::REF_FRAMES_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_RANGE_TYPE_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_PROFILE_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_ROTATION_NOT_SUPPORTED);
    const VIDEO_REASONS: TranscodeReasons =
        TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED.union(Self::VIDEO_CODEC_REASONS);
    const DIRECT_STREAM_REASONS: TranscodeReasons = Self::AUDIO_REASONS
        .union(TranscodeReasons::CONTAINER_NOT_SUPPORTED)
        .union(TranscodeReasons::VIDEO_CODEC_TAG_NOT_SUPPORTED);

    /// The container transcode reasons alias (`ContainerReasons`), exposed for
    /// tests mirroring `StreamBuilder.ContainerReasons`.
    #[must_use]
    pub const fn container_reasons() -> TranscodeReasons {
        Self::CONTAINER_REASONS
    }
}

impl<'a> StreamBuilder<'a> {
    /// Creates a new `StreamBuilder`.
    #[must_use]
    pub fn new(transcoder_support: &'a dyn TranscoderSupport) -> Self {
        Self { transcoder_support }
    }

    /// Gets the optimal video stream.
    #[must_use]
    pub fn get_optimal_video_stream(&self, options: &MediaOptions) -> Option<StreamInfo> {
        validate_media_options(options, true);

        let mut streams: Vec<StreamInfo> = Vec::new();
        for media_source in &options.media_sources {
            if let Some(id) = options.media_source_id.as_deref()
                && !id.is_empty()
                && !media_source
                    .id
                    .as_deref()
                    .is_some_and(|mid| mid.eq_ignore_ascii_case(id))
            {
                continue;
            }

            let mut stream_info = self.build_video_item(media_source, options);
            stream_info.device_id.clone_from(&options.device_id);
            stream_info.device_profile_id = options.profile.id.map(|id| id.as_simple().to_string());
            streams.push(stream_info);
        }

        get_optimal_stream(
            streams,
            i64::from(options.get_max_bitrate(false).unwrap_or(0)),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn build_video_item(&self, item: &MediaSourceInfo, options: &MediaOptions) -> StreamInfo {
        // Working copy of the media source; the C# engine mutates `item`.
        let mut item = item.clone();

        let mut playlist_item = StreamInfo::new(options.profile.clone());
        playlist_item.item_id = options.item_id;
        playlist_item.media_type = DlnaProfileType::Video;
        playlist_item.run_time_ticks = item.run_time_ticks;
        playlist_item.context = options.context;
        // C# holds a reference to `item`; mirror that so `TargetAudioStream`
        // and friends resolve during `build_stream_video_item`.
        playlist_item.media_source = Some(item.clone());
        playlist_item.always_burn_in_subtitle_when_transcoding =
            options.always_burn_in_subtitle_when_transcoding;
        playlist_item.subtitle_stream_index = options.subtitle_stream_index.or_else(|| {
            get_default_subtitle_stream_index(&item, &options.profile.subtitle_profiles)
        });

        let subtitle_stream = playlist_item.subtitle_stream_index.and_then(|idx| {
            item.get_media_stream(MediaStreamType::Subtitle, idx)
                .cloned()
        });

        let audio_stream = item
            .get_default_audio_stream(
                options
                    .audio_stream_index
                    .or(item.default_audio_stream_index),
            )
            .cloned();
        if let Some(a) = &audio_stream {
            playlist_item.audio_stream_index = Some(a.index);
        }

        // Collect candidate audio streams.
        let mut candidate_audio_streams: Vec<MediaStream> =
            audio_stream.clone().into_iter().collect();
        if !item
            .default_audio_index_source
            .contains(AudioIndexSource::USER)
            && matches!(options.audio_stream_index, None | Some(i32::MIN..=-1))
        {
            if item.default_audio_index_source == AudioIndexSource::NONE
                && let Some(a) = &audio_stream
            {
                candidate_audio_streams = item
                    .media_streams
                    .iter()
                    .filter(|s| s.stream_type == MediaStreamType::Audio)
                    .cloned()
                    .collect();
                if a.is_default {
                    candidate_audio_streams.retain(|s| s.is_default);
                }
            }

            if item
                .default_audio_index_source
                .contains(AudioIndexSource::LANGUAGE)
            {
                let lang = audio_stream.as_ref().and_then(|a| a.language.clone());
                candidate_audio_streams = item
                    .media_streams
                    .iter()
                    .filter(|s| s.stream_type == MediaStreamType::Audio && s.language == lang)
                    .cloned()
                    .collect();
                if item
                    .default_audio_index_source
                    .contains(AudioIndexSource::DEFAULT)
                {
                    let default_in_lang: Vec<MediaStream> = candidate_audio_streams
                        .iter()
                        .filter(|s| s.is_default)
                        .cloned()
                        .collect();
                    candidate_audio_streams = if default_in_lang.is_empty() {
                        item.media_streams
                            .iter()
                            .filter(|s| s.stream_type == MediaStreamType::Audio && s.is_default)
                            .cloned()
                            .collect()
                    } else {
                        default_in_lang
                    };
                }
            } else if item
                .default_audio_index_source
                .contains(AudioIndexSource::DEFAULT)
            {
                candidate_audio_streams = item
                    .media_streams
                    .iter()
                    .filter(|s| s.stream_type == MediaStreamType::Audio && s.is_default)
                    .cloned()
                    .collect();
            }
        }

        let video_stream = item.video_stream().cloned();

        let bitrate_limit_exceeded = self.is_bitrate_limit_exceeded(
            &item,
            i64::from(options.get_max_bitrate(false).unwrap_or(0)),
        );
        let mut is_eligible_for_direct_play =
            options.enable_direct_play && (options.force_direct_play || !bitrate_limit_exceeded);
        let is_eligible_for_direct_stream = options.enable_direct_stream
            && (options.force_direct_stream || !bitrate_limit_exceeded);
        let mut transcode_reasons = TranscodeReasons::empty();

        if item.video_type == Some(VideoType::Dvd) || item.video_type == Some(VideoType::BluRay) {
            is_eligible_for_direct_play = false;
        }

        if bitrate_limit_exceeded {
            transcode_reasons = TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT;
        }

        let mut direct_play_profile: Option<DirectPlayProfile> = None;
        if is_eligible_for_direct_play || is_eligible_for_direct_stream {
            let direct_play_info = self.get_video_direct_play_profile(
                options,
                &item,
                video_stream.as_ref(),
                audio_stream.as_ref(),
                &candidate_audio_streams,
                subtitle_stream.as_ref(),
                is_eligible_for_direct_play,
                is_eligible_for_direct_stream,
            );
            let direct_play = direct_play_info.play_method;
            transcode_reasons |= direct_play_info.transcode_reasons;

            if let Some(direct_play) = direct_play {
                direct_play_profile.clone_from(&direct_play_info.profile);
                playlist_item.play_method = direct_play;
                playlist_item.container = normalize_media_source_format_into_single_container(
                    item.container.as_deref(),
                    Some(&options.profile),
                    DlnaProfileType::Video,
                    direct_play_profile.as_ref(),
                );
                let video_codec = video_stream.as_ref().and_then(|v| v.codec.clone());
                playlist_item.video_codecs = video_codec.into_iter().collect();

                if direct_play == PlayMethod::DirectPlay {
                    playlist_item.sub_protocol = MediaStreamProtocol::http;
                    let audio_stream_index = direct_play_info
                        .audio_stream_index
                        .or(audio_stream.as_ref().map(|a| a.index));
                    if let Some(asi) = audio_stream_index {
                        playlist_item.audio_stream_index = Some(asi);
                        let audio_codec = item
                            .get_media_stream(MediaStreamType::Audio, asi)
                            .and_then(|s| s.codec.clone());
                        playlist_item.audio_codecs = audio_codec.into_iter().collect();
                    }
                } else if direct_play == PlayMethod::DirectStream {
                    playlist_item.audio_stream_index = audio_stream.as_ref().map(|a| a.index);
                    if audio_stream.is_some() {
                        playlist_item.audio_codecs = split_owned(
                            direct_play_profile
                                .as_ref()
                                .and_then(|p| p.audio_codec.as_deref()),
                        );
                    }
                    set_stream_info_options_from_direct_play_profile(
                        options,
                        &mut item,
                        &mut playlist_item,
                        direct_play_profile.as_ref(),
                    );
                    self.build_stream_video_item(
                        &mut playlist_item,
                        options,
                        &item,
                        video_stream.as_ref(),
                        audio_stream.clone(),
                        &candidate_audio_streams,
                        direct_play_profile.as_ref().map(|p| p.container.as_str()),
                        direct_play_profile
                            .as_ref()
                            .and_then(|p| p.video_codec.as_deref()),
                        direct_play_profile
                            .as_ref()
                            .and_then(|p| p.audio_codec.as_deref()),
                    );
                }

                if let Some(sub) = &subtitle_stream {
                    let subtitle_profile = Self::get_subtitle_profile(
                        &item,
                        sub,
                        &options.profile.subtitle_profiles,
                        direct_play,
                        self.transcoder_support,
                        direct_play_profile.as_ref().map(|p| p.container.as_str()),
                        None,
                    );
                    playlist_item.subtitle_delivery_method = subtitle_profile.method;
                    playlist_item
                        .subtitle_format
                        .clone_from(&subtitle_profile.format);
                }
            }
        }

        playlist_item.transcode_reasons = transcode_reasons;

        if playlist_item.play_method != PlayMethod::DirectStream
            && playlist_item.play_method != PlayMethod::DirectPlay
        {
            let (transcoding_profile, play_method) = self.get_video_transcode_profile(
                &item,
                options,
                video_stream.as_ref(),
                audio_stream.as_ref(),
                &playlist_item,
            );

            if let (Some(transcoding_profile), Some(_play_method)) =
                (transcoding_profile.as_ref(), play_method)
            {
                set_stream_info_options_from_transcoding_profile(
                    &mut item,
                    &mut playlist_item,
                    transcoding_profile,
                );

                self.build_stream_video_item(
                    &mut playlist_item,
                    options,
                    &item,
                    video_stream.as_ref(),
                    audio_stream.clone(),
                    &candidate_audio_streams,
                    Some(transcoding_profile.container.as_str()),
                    Some(transcoding_profile.video_codec.as_str()),
                    Some(transcoding_profile.audio_codec.as_str()),
                );

                playlist_item.play_method = PlayMethod::Transcode;

                if let Some(sub) = &subtitle_stream {
                    let subtitle_profile = Self::get_subtitle_profile(
                        &item,
                        sub,
                        &options.profile.subtitle_profiles,
                        PlayMethod::Transcode,
                        self.transcoder_support,
                        Some(transcoding_profile.container.as_str()),
                        Some(transcoding_profile.protocol),
                    );
                    playlist_item.subtitle_delivery_method = subtitle_profile.method;
                    playlist_item
                        .subtitle_format
                        .clone_from(&subtitle_profile.format);
                    playlist_item.subtitle_codecs =
                        subtitle_profile.format.clone().into_iter().collect();
                }

                if playlist_item.transcode_reasons.intersects(
                    Self::VIDEO_REASONS | TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT,
                ) {
                    apply_transcoding_conditions(
                        &mut playlist_item,
                        &transcoding_profile.conditions,
                        None,
                        true,
                        true,
                    );
                }
            }
        }

        let normalized_container = normalize_media_source_format_into_single_container(
            item.container.as_deref(),
            Some(&options.profile),
            DlnaProfileType::Video,
            direct_play_profile.as_ref(),
        );

        // Sync the final container onto the media source held by the stream
        // (which carries any audio-channel downmix applied during the build),
        // mirroring the shared reference in C#.
        if let Some(ms) = playlist_item.media_source.as_mut() {
            ms.container = normalized_container;
        }
        playlist_item
    }

    fn get_video_transcode_profile(
        &self,
        item: &MediaSourceInfo,
        options: &MediaOptions,
        video_stream: Option<&MediaStream>,
        audio_stream: Option<&MediaStream>,
        playlist_item: &StreamInfo,
    ) -> (Option<TranscodingProfile>, Option<PlayMethod>) {
        if !(item.supports_transcoding || item.supports_direct_stream) {
            return (None, None);
        }

        let video_codec = video_stream.and_then(|v| v.codec.as_deref());
        let audio_codec = audio_stream.and_then(|a| a.codec.as_deref());

        let mut analyzed: Vec<(TranscodingProfile, PlayMethod, (i32, i32), usize)> = Vec::new();
        for (order, transcoding_profile) in options
            .profile
            .transcoding_profiles
            .iter()
            .filter(|i| i.profile_type == playlist_item.media_type && i.context == options.context)
            .filter(|i| {
                !item.use_most_compatible_transcoding_profile
                    || i.container.eq_ignore_ascii_case("ts")
            })
            .enumerate()
        {
            let mut rank_video = 3;
            let mut rank_audio = 3;

            let container = transcoding_profile.container.as_str();

            if let Some(vs) = video_stream
                && options.allow_video_stream_copy
                && contains_container(Some(&transcoding_profile.video_codec), video_codec)
            {
                let failures = self.get_compatibility_video_codec(options, item, container, vs);
                rank_video = if failures.is_empty() { 1 } else { 2 };
            }

            if let Some(a) = audio_stream
                && options.allow_audio_stream_copy
            {
                for transcoding_audio_codec in split(Some(&transcoding_profile.audio_codec)) {
                    let failures = self.get_compatibility_audio_codec(
                        options,
                        item,
                        container,
                        a,
                        Some(transcoding_audio_codec),
                        true,
                        false,
                    );
                    let mut ra = 3;
                    if failures.is_empty() {
                        ra = if audio_codec
                            .is_some_and(|ac| transcoding_audio_codec.eq_ignore_ascii_case(ac))
                        {
                            1
                        } else {
                            2
                        };
                    }
                    rank_audio = rank_audio.min(ra);
                    if rank_audio == 1 {
                        break;
                    }
                }
            }

            let play_method = if rank_video == 1 {
                PlayMethod::DirectStream
            } else {
                PlayMethod::Transcode
            };

            analyzed.push((
                transcoding_profile.clone(),
                play_method,
                (rank_video, rank_audio),
                order,
            ));
        }

        // Stable sort by rank tuple (video, audio) ascending, preserving order.
        analyzed.sort_by(|a, b| a.2.cmp(&b.2).then(a.3.cmp(&b.3)));

        analyzed
            .into_iter()
            .next()
            .map_or((None, None), |(p, m, _, _)| (Some(p), Some(m)))
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn build_stream_video_item(
        &self,
        playlist_item: &mut StreamInfo,
        options: &MediaOptions,
        item: &MediaSourceInfo,
        video_stream: Option<&MediaStream>,
        mut audio_stream: Option<MediaStream>,
        candidate_audio_streams: &[MediaStream],
        container: Option<&str>,
        video_codec: Option<&str>,
        audio_codec: Option<&str>,
    ) {
        let mut video_codecs: Vec<String> = split_owned(video_codec);
        if video_codecs.is_empty()
            && let Some(vs) = video_stream
            && let Some(c) = &vs.codec
        {
            video_codecs.push(c.clone());
        }

        if playlist_item.sub_protocol == MediaStreamProtocol::hls {
            video_codecs.retain(|c| {
                SUPPORTED_HLS_VIDEO_CODECS
                    .iter()
                    .any(|h| h.eq_ignore_ascii_case(c))
            });
        }

        playlist_item.video_codecs.clone_from(&video_codecs);
        if let Some(vs) = video_stream {
            let codecs_refs: Vec<&str> = video_codecs.iter().map(String::as_str).collect();
            if !contains_container_list(&codecs_refs, false, vs.codec.as_deref()) {
                playlist_item.transcode_reasons |= TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED;
            }
        }

        playlist_item.max_framerate = video_stream.and_then(MediaStream::reference_frame_rate);
        let qualifier = video_stream.and_then(|v| v.codec.clone());
        let qualifier = qualifier.as_deref();
        if let Some(level) = video_stream.and_then(|v| v.level) {
            playlist_item.set_option_qualified(qualifier, "level", format_f64(level));
        }
        if let Some(bd) = video_stream.and_then(|v| v.bit_depth) {
            playlist_item.set_option_qualified(qualifier, "videobitdepth", bd.to_string());
        }
        if let Some(profile) = video_stream.and_then(|v| v.profile.as_deref())
            && !profile.is_empty()
        {
            playlist_item.set_option_qualified(qualifier, "profile", profile.to_ascii_lowercase());
        }

        let mut audio_codecs: Vec<String> = split_owned(audio_codec);
        if audio_codecs.is_empty()
            && let Some(a) = &audio_stream
            && let Some(c) = &a.codec
        {
            audio_codecs.push(c.clone());
        }

        if playlist_item.sub_protocol == MediaStreamProtocol::hls {
            if playlist_item
                .container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("mp4"))
            {
                audio_codecs.retain(|c| {
                    SUPPORTED_HLS_AUDIO_CODECS_MP4
                        .iter()
                        .any(|h| h.eq_ignore_ascii_case(c))
                });
            } else {
                audio_codecs.retain(|c| {
                    SUPPORTED_HLS_AUDIO_CODECS_TS
                        .iter()
                        .any(|h| h.eq_ignore_ascii_case(c))
                });
            }
        }

        let audio_codecs_refs: Vec<&str> = audio_codecs.iter().map(String::as_str).collect();
        let audio_stream_with_supported_codec = candidate_audio_streams
            .iter()
            .find(|s| contains_container_list(&audio_codecs_refs, false, s.codec.as_deref()))
            .cloned();

        let channels_exceeds_limit = audio_stream_with_supported_codec.as_ref().is_some_and(|s| {
            s.channels.unwrap_or(0)
                > playlist_item
                    .transcoding_max_audio_channels
                    .unwrap_or(i32::MAX)
        });

        let direct_audio_failures =
            audio_stream_with_supported_codec
                .as_ref()
                .map_or(TranscodeReasons::empty(), |s| {
                    self.get_compatibility_audio_codec(
                        options,
                        item,
                        container.unwrap_or(""),
                        s,
                        None,
                        true,
                        false,
                    )
                });

        playlist_item.transcode_reasons |= direct_audio_failures;
        if audio_stream.is_some() && audio_stream_with_supported_codec.is_none() {
            playlist_item.transcode_reasons |= TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED;
        }

        let mut direct_audio_stream_satisfied = audio_stream_with_supported_codec.is_some()
            && !channels_exceeds_limit
            && direct_audio_failures.is_empty();
        direct_audio_stream_satisfied = direct_audio_stream_satisfied
            && !playlist_item
                .transcode_reasons
                .contains(TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT);

        let direct_audio_stream = if direct_audio_stream_satisfied {
            audio_stream_with_supported_codec.clone()
        } else {
            None
        };

        if channels_exceeds_limit && playlist_item.target_audio_stream().is_some() {
            playlist_item.transcode_reasons |= TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED;
            // Mutate the target audio stream channels in the media source clone.
            let max_channels = playlist_item.transcoding_max_audio_channels;
            let audio_index = playlist_item.target_audio_stream().map(|s| s.index);
            if let (Some(ms), Some(idx)) = (playlist_item.media_source.as_mut(), audio_index)
                && let Some(stream) = ms
                    .media_streams
                    .iter_mut()
                    .find(|s| s.stream_type == MediaStreamType::Audio && s.index == idx)
            {
                stream.channels = max_channels;
            }
        }

        playlist_item.audio_codecs.clone_from(&audio_codecs);
        if let Some(das) = direct_audio_stream {
            audio_stream = Some(das.clone());
            playlist_item.audio_stream_index = Some(das.index);
            audio_codecs = das.codec.clone().into_iter().collect();
            playlist_item.audio_codecs.clone_from(&audio_codecs);

            playlist_item.audio_sample_rate = das.sample_rate;
            playlist_item.set_option_qualified(
                qualifier,
                "audiochannels",
                das.channels.map_or_else(String::new, |c| c.to_string()),
            );

            if let Some(profile) = das.profile.as_deref().filter(|p| !p.is_empty()) {
                playlist_item.set_option_qualified(
                    das.codec.as_deref(),
                    "profile",
                    profile.to_ascii_lowercase(),
                );
            }
            if let Some(level) = das.level.filter(|l| *l != 0.0) {
                playlist_item.set_option_qualified(
                    das.codec.as_deref(),
                    "level",
                    format_f64(level),
                );
            }
        }

        let width = video_stream.and_then(|v| v.width);
        let height = video_stream.and_then(|v| v.height);
        let bit_depth = video_stream.and_then(|v| v.bit_depth);
        let video_bitrate = video_stream.and_then(|v| v.bit_rate);
        let video_level = video_stream.and_then(|v| v.level);
        let video_profile = video_stream.and_then(|v| v.profile.clone());
        let video_range_type = video_stream.map(MediaStream::video_range_type);
        let video_framerate = video_stream.map_or(0.0, |v| v.reference_frame_rate().unwrap_or(0.0));
        let is_anamorphic = video_stream.and_then(|v| v.is_anamorphic);
        let is_interlaced = video_stream.map(|v| v.is_interlaced);
        let video_codec_tag = video_stream.and_then(|v| v.codec_tag.clone());
        let is_avc = video_stream.and_then(|v| v.is_avc);
        let video_rotation = video_stream.and_then(|v| v.rotation);

        let timestamp = if video_stream.is_none() {
            Some(TransportStreamTimestamp::None)
        } else {
            item.timestamp
        };
        let packet_length = video_stream.and_then(|v| v.packet_length);
        let ref_frames = video_stream.and_then(|v| v.ref_frames);

        let num_streams = i32::try_from(item.media_streams.len()).unwrap_or(i32::MAX);
        let num_audio_streams = item.get_stream_count(MediaStreamType::Audio);
        let num_video_streams = item.get_stream_count(MediaStreamType::Video);

        let use_sub_container = playlist_item.sub_protocol == MediaStreamProtocol::hls;

        let video_codecs_snapshot = playlist_item.video_codecs.clone();
        let vc_refs: Vec<&str> = video_codecs_snapshot.iter().map(String::as_str).collect();
        let applied_video_conditions: Vec<CodecProfile> = options
            .profile
            .codec_profiles
            .iter()
            .filter(|i| {
                i.codec_type == CodecType::Video
                    && i.contains_any_codec(&vc_refs, container, use_sub_container)
                    && i.apply_conditions.iter().all(|apply| {
                        ConditionProcessor::is_video_condition_satisfied(
                            apply,
                            width,
                            height,
                            bit_depth,
                            video_bitrate,
                            video_profile.as_deref(),
                            video_range_type,
                            video_level,
                            Some(video_framerate),
                            packet_length,
                            timestamp,
                            is_anamorphic,
                            is_interlaced,
                            ref_frames,
                            num_streams,
                            num_video_streams,
                            num_audio_streams,
                            video_codec_tag.as_deref(),
                            is_avc,
                            video_rotation,
                        )
                    })
            })
            .cloned()
            .rev()
            .collect();

        for condition in &applied_video_conditions {
            for transcoding_video_codec in &video_codecs_snapshot {
                if condition.contains_codec(
                    Some(transcoding_video_codec),
                    container,
                    use_sub_container,
                ) {
                    apply_transcoding_conditions(
                        playlist_item,
                        &condition.conditions,
                        Some(transcoding_video_codec),
                        true,
                        true,
                    );
                }
            }
        }

        playlist_item.global_max_audio_channels = if channels_exceeds_limit {
            playlist_item.transcoding_max_audio_channels
        } else {
            options.max_audio_channels
        };

        let audio_bitrate = get_audio_bitrate(
            i64::from(options.get_max_bitrate(true).unwrap_or(0)),
            &playlist_item.target_audio_codec(),
            audio_stream.as_ref(),
            playlist_item,
        );
        playlist_item.audio_bitrate = Some(
            playlist_item
                .audio_bitrate
                .unwrap_or(audio_bitrate)
                .min(audio_bitrate),
        );

        let is_secondary_audio = audio_stream
            .as_ref()
            .and_then(|a| item.is_secondary_audio(a));
        let input_audio_bitrate = audio_stream.as_ref().and_then(|a| a.bit_rate);
        let audio_channels = audio_stream.as_ref().and_then(|a| a.channels);
        let audio_profile = audio_stream.as_ref().and_then(|a| a.profile.clone());
        let input_audio_sample_rate = audio_stream.as_ref().and_then(|a| a.sample_rate);
        let input_audio_bit_depth = audio_stream.as_ref().and_then(|a| a.bit_depth);

        let audio_codecs_snapshot = playlist_item.audio_codecs.clone();
        let ac_refs: Vec<&str> = audio_codecs_snapshot.iter().map(String::as_str).collect();
        let applied_audio_conditions: Vec<CodecProfile> = options
            .profile
            .codec_profiles
            .iter()
            .filter(|i| {
                i.codec_type == CodecType::VideoAudio
                    && i.contains_any_codec(&ac_refs, container, false)
                    && i.apply_conditions.iter().all(|apply| {
                        ConditionProcessor::is_video_audio_condition_satisfied(
                            apply,
                            audio_channels,
                            input_audio_bitrate,
                            input_audio_sample_rate,
                            input_audio_bit_depth,
                            audio_profile.as_deref(),
                            is_secondary_audio,
                        )
                    })
            })
            .cloned()
            .rev()
            .collect();

        for codec_profile in &applied_audio_conditions {
            for transcoding_audio_codec in &audio_codecs_snapshot {
                if codec_profile.contains_codec(Some(transcoding_audio_codec), container, false) {
                    apply_transcoding_conditions(
                        playlist_item,
                        &codec_profile.conditions,
                        Some(transcoding_audio_codec),
                        true,
                        true,
                    );
                    break;
                }
            }
        }

        if let Some(max_bitrate_setting) = options.get_max_bitrate(false) {
            let mut available = max_bitrate_setting;
            if let Some(ab) = playlist_item.audio_bitrate {
                available -= ab;
            }
            let current_value = playlist_item.video_bitrate.unwrap_or(available);
            playlist_item.video_bitrate = Some(available.min(current_value).max(64_000));
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::items_after_statements
    )]
    fn get_video_direct_play_profile(
        &self,
        options: &MediaOptions,
        media_source: &MediaSourceInfo,
        video_stream: Option<&MediaStream>,
        audio_stream: Option<&MediaStream>,
        candidate_audio_streams: &[MediaStream],
        subtitle_stream: Option<&MediaStream>,
        is_eligible_for_direct_play: bool,
        is_eligible_for_direct_stream: bool,
    ) -> DirectPlayInfo {
        if options.force_direct_play {
            return DirectPlayInfo {
                profile: None,
                play_method: Some(PlayMethod::DirectPlay),
                audio_stream_index: audio_stream.map(|a| a.index),
                transcode_reasons: TranscodeReasons::empty(),
            };
        }
        if options.force_direct_stream {
            return DirectPlayInfo {
                profile: None,
                play_method: Some(PlayMethod::DirectStream),
                audio_stream_index: audio_stream.map(|a| a.index),
                transcode_reasons: TranscodeReasons::empty(),
            };
        }

        let profile = &options.profile;
        let container = media_source.container.as_deref().unwrap_or("");

        let container_profile_reasons =
            self.get_compatibility_container(options, media_source, container, video_stream);

        let video_codec_profile_reasons = video_stream.map_or(TranscodeReasons::empty(), |vs| {
            self.get_compatibility_video_codec(options, media_source, container, vs)
        });

        // Audio candidate compatibility map.
        let audio_stream_matches: Vec<(i32, TranscodeReasons)> = candidate_audio_streams
            .iter()
            .map(|a| {
                (
                    a.index,
                    self.get_compatibility_audio_codec_direct(
                        options,
                        media_source,
                        container,
                        a,
                        true,
                        media_source.is_secondary_audio(a).unwrap_or(false),
                    ),
                )
            })
            .collect();

        let mut subtitle_profile_reasons = TranscodeReasons::empty();
        if let Some(sub) = subtitle_stream {
            let subtitle_profile = Self::get_subtitle_profile(
                media_source,
                sub,
                &options.profile.subtitle_profiles,
                PlayMethod::DirectPlay,
                self.transcoder_support,
                Some(container),
                None,
            );
            if subtitle_profile.method != SubtitleDeliveryMethod::Drop
                && subtitle_profile.method != SubtitleDeliveryMethod::External
                && subtitle_profile.method != SubtitleDeliveryMethod::Embed
            {
                subtitle_profile_reasons |= TranscodeReasons::SUBTITLE_CODEC_NOT_SUPPORTED;
            }
        }

        let mut container_supported = false;
        let rankings = [
            TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED,
            Self::VIDEO_CODEC_REASONS,
            TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED,
            Self::AUDIO_CODEC_REASONS,
            Self::CONTAINER_REASONS,
        ];

        struct Analysis {
            profile: DirectPlayProfile,
            play_method: Option<PlayMethod>,
            audio_stream_index: Option<i32>,
            transcode_reason: TranscodeReasons,
            order: usize,
            rank: i32,
        }

        let mut analyzed: Vec<Analysis> = Vec::new();
        for (order, direct_play_profile) in profile
            .direct_play_profiles
            .iter()
            .filter(|p| p.profile_type == DlnaProfileType::Video)
            .enumerate()
        {
            let mut direct_play_profile_reasons = TranscodeReasons::empty();
            let mut audio_codec_profile_reasons = TranscodeReasons::empty();

            if direct_play_profile.supports_container(Some(container)) {
                container_supported = true;
            } else {
                direct_play_profile_reasons |= TranscodeReasons::CONTAINER_NOT_SUPPORTED;
            }

            let video_codec = video_stream.and_then(|v| v.codec.as_deref());
            if !direct_play_profile.supports_video_codec(video_codec) {
                direct_play_profile_reasons |= TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED;
            }

            let mut selected_audio_stream: Option<&MediaStream> = None;
            if !candidate_audio_streams.is_empty() {
                selected_audio_stream = candidate_audio_streams
                    .iter()
                    .find(|a| direct_play_profile.supports_audio_codec(a.codec.as_deref()));
                if let Some(sel) = selected_audio_stream {
                    audio_codec_profile_reasons = audio_stream_matches
                        .iter()
                        .find(|(idx, _)| *idx == sel.index)
                        .map_or(TranscodeReasons::empty(), |(_, r)| *r);
                } else {
                    direct_play_profile_reasons |= TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED;
                }
            }

            let mut failure_reasons =
                direct_play_profile_reasons | container_profile_reasons | subtitle_profile_reasons;

            if !failure_reasons.contains(TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED) {
                failure_reasons |= video_codec_profile_reasons;
            }
            if !failure_reasons.contains(TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED) {
                failure_reasons |= audio_codec_profile_reasons;
            }

            let direct_stream_failure_reasons = failure_reasons & !Self::DIRECT_STREAM_REASONS;

            let play_method = if failure_reasons.is_empty()
                && is_eligible_for_direct_play
                && media_source.supports_direct_play
            {
                Some(PlayMethod::DirectPlay)
            } else if direct_stream_failure_reasons.is_empty()
                && is_eligible_for_direct_stream
                && media_source.supports_direct_stream
            {
                Some(PlayMethod::DirectStream)
            } else {
                None
            };

            let rank = get_rank(failure_reasons, &rankings);

            analyzed.push(Analysis {
                profile: direct_play_profile.clone(),
                play_method,
                audio_stream_index: selected_audio_stream.map(|s| s.index),
                transcode_reason: failure_reasons,
                order,
                rank,
            });
        }

        // OrderByDescending(PlayMethod).ThenByDescending(Rank).ThenBy(Order)
        analyzed.sort_by(|a, b| {
            play_method_ord(b.play_method)
                .cmp(&play_method_ord(a.play_method))
                .then(b.rank.cmp(&a.rank))
                .then(a.order.cmp(&b.order))
        });

        if let Some(matched) = analyzed.iter().find(|a| a.play_method.is_some()) {
            return DirectPlayInfo {
                profile: Some(matched.profile.clone()),
                play_method: matched.play_method,
                audio_stream_index: matched.audio_stream_index,
                transcode_reasons: matched.transcode_reason,
            };
        }

        let mut failure_reasons = analyzed
            .iter()
            .filter(|a| a.play_method.is_none())
            .find(|a| {
                !container_supported
                    || !a
                        .transcode_reason
                        .contains(TranscodeReasons::CONTAINER_NOT_SUPPORTED)
            })
            .map_or(TranscodeReasons::empty(), |a| a.transcode_reason);
        if failure_reasons.is_empty() {
            failure_reasons = TranscodeReasons::DIRECT_PLAY_ERROR;
        }

        DirectPlayInfo {
            profile: None,
            play_method: None,
            audio_stream_index: None,
            transcode_reasons: failure_reasons,
        }
    }

    #[allow(clippy::unused_self)]
    fn is_bitrate_limit_exceeded(&self, item: &MediaSourceInfo, max_bitrate: i64) -> bool {
        if item.is_remote {
            return false;
        }
        let requested_max_bitrate = if max_bitrate > 0 {
            max_bitrate
        } else {
            i64::from(i32::MAX)
        };
        let item_bitrate = i64::from(item.bitrate.unwrap_or(40_000_000));
        item_bitrate > requested_max_bitrate
    }

    #[allow(clippy::unused_self)]
    fn get_compatibility_container(
        &self,
        options: &MediaOptions,
        media_source: &MediaSourceInfo,
        container: &str,
        video_stream: Option<&MediaStream>,
    ) -> TranscodeReasons {
        let profile = &options.profile;
        let conditions: Vec<ProfileCondition> = profile
            .container_profiles
            .iter()
            .filter(|cp| {
                cp.profile_type == DlnaProfileType::Video
                    && cp.contains_container(Some(container), false)
            })
            .flat_map(|cp| check_video_conditions(&cp.conditions, media_source, video_stream))
            .collect();
        aggregate_failure_conditions(&conditions)
    }

    #[allow(clippy::unused_self)]
    fn get_compatibility_video_codec(
        &self,
        options: &MediaOptions,
        media_source: &MediaSourceInfo,
        container: &str,
        video_stream: &MediaStream,
    ) -> TranscodeReasons {
        let profile = &options.profile;
        let video_codec = video_stream.codec.as_deref();
        let conditions: Vec<ProfileCondition> = profile
            .codec_profiles
            .iter()
            .filter(|cp| {
                cp.codec_type == CodecType::Video
                    && cp.contains_codec(video_codec, Some(container), false)
                    && check_video_conditions(
                        &cp.apply_conditions,
                        media_source,
                        Some(video_stream),
                    )
                    .is_empty()
            })
            .flat_map(|cp| check_video_conditions(&cp.conditions, media_source, Some(video_stream)))
            .collect();
        aggregate_failure_conditions(&conditions)
    }

    #[allow(clippy::too_many_arguments, clippy::unused_self)]
    fn get_compatibility_audio_codec(
        &self,
        options: &MediaOptions,
        _media_source: &MediaSourceInfo,
        container: &str,
        audio_stream: &MediaStream,
        transcoding_audio_codec: Option<&str>,
        is_video: bool,
        is_secondary_audio: bool,
    ) -> TranscodeReasons {
        let profile = &options.profile;
        let audio_codec = transcoding_audio_codec.or(audio_stream.codec.as_deref());
        let audio_profile = audio_stream.profile.as_deref();
        let audio_channels = audio_stream.channels;
        let audio_bitrate = audio_stream.bit_rate;
        let audio_sample_rate = audio_stream.sample_rate;
        let audio_bit_depth = audio_stream.bit_depth;

        let conditions: Vec<ProfileCondition> = if is_video {
            get_profile_conditions_for_video_audio(
                &profile.codec_profiles,
                container,
                audio_codec,
                audio_channels,
                audio_bitrate,
                audio_sample_rate,
                audio_bit_depth,
                audio_profile,
                Some(is_secondary_audio),
            )
        } else {
            get_profile_conditions_for_audio(
                &profile.codec_profiles,
                container,
                audio_codec,
                audio_channels,
                audio_bitrate,
                audio_sample_rate,
                audio_bit_depth,
                true,
            )
        };

        aggregate_failure_conditions(&conditions)
    }

    fn get_compatibility_audio_codec_direct(
        &self,
        options: &MediaOptions,
        media_source: &MediaSourceInfo,
        container: &str,
        audio_stream: &MediaStream,
        is_video: bool,
        is_secondary_audio: bool,
    ) -> TranscodeReasons {
        let mut failures = self.get_compatibility_audio_codec(
            options,
            media_source,
            container,
            audio_stream,
            None,
            is_video,
            is_secondary_audio,
        );
        if audio_stream.is_external {
            failures |= TranscodeReasons::AUDIO_IS_EXTERNAL;
        }
        failures
    }

    /// Determines the subtitle delivery profile for a subtitle stream.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn get_subtitle_profile(
        media_source: &MediaSourceInfo,
        subtitle_stream: &MediaStream,
        subtitle_profiles: &[SubtitleProfile],
        play_method: PlayMethod,
        transcoder_support: &dyn TranscoderSupport,
        output_container: Option<&str>,
        transcoding_sub_protocol: Option<MediaStreamProtocol>,
    ) -> SubtitleProfile {
        if can_consider_embed_subtitle(
            subtitle_stream,
            play_method,
            transcoding_sub_protocol,
            output_container,
        ) {
            // Supported embedded subs of the same format.
            for profile in subtitle_profiles {
                if !profile.supports_language(subtitle_stream.language.as_deref()) {
                    continue;
                }
                if profile.method != SubtitleDeliveryMethod::Embed {
                    continue;
                }
                if !contains_container(profile.container.as_deref(), output_container) {
                    continue;
                }
                if play_method == PlayMethod::Transcode
                    && !is_subtitle_embed_supported(output_container)
                {
                    continue;
                }
                if subtitle_stream.is_text_subtitle_stream()
                    == MediaStream::is_text_format(profile.format.as_deref())
                    && subtitle_stream
                        .codec
                        .as_deref()
                        .zip(profile.format.as_deref())
                        .is_some_and(|(c, f)| c.eq_ignore_ascii_case(f))
                {
                    return profile.clone();
                }
            }

            // Supported embedded subs of a convertible format.
            for profile in subtitle_profiles {
                if !profile.supports_language(subtitle_stream.language.as_deref()) {
                    continue;
                }
                if profile.method != SubtitleDeliveryMethod::Embed {
                    continue;
                }
                if !contains_container(profile.container.as_deref(), output_container) {
                    continue;
                }
                if play_method == PlayMethod::Transcode
                    && !is_subtitle_embed_supported(output_container)
                {
                    continue;
                }
                if subtitle_stream.is_text_subtitle_stream()
                    && profile
                        .format
                        .as_deref()
                        .is_some_and(|f| subtitle_stream.supports_subtitle_conversion_to(f))
                {
                    return profile.clone();
                }
            }
        }

        get_external_subtitle_profile(
            media_source,
            subtitle_stream,
            subtitle_profiles,
            play_method,
            transcoder_support,
            false,
        )
        .or_else(|| {
            get_external_subtitle_profile(
                media_source,
                subtitle_stream,
                subtitle_profiles,
                play_method,
                transcoder_support,
                true,
            )
        })
        .unwrap_or_else(|| SubtitleProfile {
            method: SubtitleDeliveryMethod::Encode,
            format: subtitle_stream.codec.clone(),
            ..SubtitleProfile::default()
        })
    }
}

struct DirectPlayInfo {
    profile: Option<DirectPlayProfile>,
    play_method: Option<PlayMethod>,
    audio_stream_index: Option<i32>,
    transcode_reasons: TranscodeReasons,
}

fn play_method_ord(pm: Option<PlayMethod>) -> i32 {
    // C# orders by the PlayMethod? enum value; null sorts lowest.
    match pm {
        None => -1,
        Some(PlayMethod::Transcode) => 0,
        Some(PlayMethod::DirectStream) => 1,
        Some(PlayMethod::DirectPlay) => 2,
    }
}

fn get_rank(reasons: TranscodeReasons, rankings: &[TranscodeReasons]) -> i32 {
    let mut index = 1;
    for flag in rankings {
        if reasons.intersects(*flag) {
            return index;
        }
        index += 1;
    }
    index
}

fn get_optimal_stream(streams: Vec<StreamInfo>, max_bitrate: i64) -> Option<StreamInfo> {
    sort_media_sources(streams, max_bitrate).into_iter().next()
}

fn sort_media_sources(mut streams: Vec<StreamInfo>, max_bitrate: i64) -> Vec<StreamInfo> {
    let key = |i: &StreamInfo| -> (i32, i32, i32, i64) {
        let protocol = i.media_source.as_ref().map(|m| m.protocol);
        let a = i32::from(
            !(i.play_method == PlayMethod::DirectPlay && protocol == Some(MediaProtocol::File)),
        );
        let b = match i.play_method {
            PlayMethod::DirectStream | PlayMethod::DirectPlay => 0,
            PlayMethod::Transcode => 1,
        };
        let c = i32::from(protocol != Some(MediaProtocol::File));
        let d = if max_bitrate > 0 {
            i.media_source
                .as_ref()
                .and_then(|m| m.bitrate)
                .map_or(0, |br| (i64::from(br) - max_bitrate).abs())
        } else {
            0
        };
        (a, b, c, d)
    };

    // Stable sort preserves original index as the final tie-breaker.
    streams.sort_by_key(|x| key(x));
    streams
}

fn validate_media_options(options: &MediaOptions, is_media_source: bool) {
    if options.item_id.is_nil() {
        assert!(
            options.device_id.as_deref().is_some_and(|d| !d.is_empty()),
            "DeviceId is required"
        );
    }
    if is_media_source {
        if options.audio_stream_index.is_some() {
            assert!(
                options
                    .media_source_id
                    .as_deref()
                    .is_some_and(|m| !m.is_empty()),
                "MediaSourceId is required when a specific audio stream is requested"
            );
        }
        if options.subtitle_stream_index.is_some() {
            assert!(
                options
                    .media_source_id
                    .as_deref()
                    .is_some_and(|m| !m.is_empty()),
                "MediaSourceId is required when a specific subtitle stream is requested"
            );
        }
    }
}

/// Normalizes input container into a single container.
#[must_use]
pub fn normalize_media_source_format_into_single_container(
    input_container: Option<&str>,
    profile: Option<&DeviceProfile>,
    type_: DlnaProfileType,
    play_profile: Option<&DirectPlayProfile>,
) -> Option<String> {
    let input_container = input_container.unwrap_or("");
    let Some(profile) = profile else {
        return non_empty(input_container);
    };
    if input_container.is_empty() || !input_container.contains(',') {
        return non_empty(input_container);
    }

    let formats = split(Some(input_container));
    let owned: Vec<DirectPlayProfile>;
    let play_profiles: &[DirectPlayProfile] = if let Some(pp) = play_profile {
        owned = vec![pp.clone()];
        &owned
    } else {
        &profile.direct_play_profiles
    };

    for format in &formats {
        for dpp in play_profiles {
            if dpp.profile_type != type_ {
                continue;
            }
            if dpp.supports_container(Some(format)) {
                return Some((*format).to_owned());
            }
        }
    }

    non_empty(input_container)
}

fn get_default_subtitle_stream_index(
    item: &MediaSourceInfo,
    subtitle_profiles: &[SubtitleProfile],
) -> Option<i32> {
    let mut highest_score = -1;
    for stream in &item.media_streams {
        if stream.stream_type == MediaStreamType::Subtitle
            && let Some(score) = stream.score
            && score > highest_score
        {
            highest_score = score;
        }
    }

    let top_streams: Vec<&MediaStream> = item
        .media_streams
        .iter()
        .filter(|s| s.stream_type == MediaStreamType::Subtitle && s.score == Some(highest_score))
        .collect();

    if top_streams.len() > 1 {
        for stream in &top_streams {
            for profile in subtitle_profiles {
                if profile.method == SubtitleDeliveryMethod::External
                    && (is_vob_sub_mks_profile(profile, stream)
                        || (!is_vob_sub_mks_delivery_profile(profile)
                            && profile
                                .format
                                .as_deref()
                                .zip(stream.codec.as_deref())
                                .is_some_and(|(f, c)| f.eq_ignore_ascii_case(c))))
                {
                    return Some(stream.index);
                }
            }
        }
    }

    item.default_subtitle_stream_index
}

fn set_stream_info_options_from_transcoding_profile(
    item: &mut MediaSourceInfo,
    playlist_item: &mut StreamInfo,
    transcoding_profile: &TranscodingProfile,
) {
    let container = transcoding_profile.container.clone();
    let protocol = transcoding_profile.protocol;

    item.transcoding_container = Some(container.clone());
    item.transcoding_sub_protocol = protocol;

    if playlist_item.play_method == PlayMethod::Transcode {
        playlist_item.container = Some(container);
        playlist_item.sub_protocol = protocol;
    }

    playlist_item.transcode_seek_info = transcoding_profile.transcode_seek_info;
    if let Ok(max) = transcoding_profile
        .max_audio_channels
        .as_deref()
        .unwrap_or("")
        .parse::<i32>()
    {
        playlist_item.transcoding_max_audio_channels = Some(max);
    }

    playlist_item.estimate_content_length = transcoding_profile.estimate_content_length;
    playlist_item.copy_timestamps = transcoding_profile.copy_timestamps;
    playlist_item.enable_subtitles_in_manifest = transcoding_profile.enable_subtitles_in_manifest;
    playlist_item.enable_mpegts_m2ts_mode = transcoding_profile.enable_mpegts_m2ts_mode;
    playlist_item.enable_audio_vbr_encoding = transcoding_profile.enable_audio_vbr_encoding;

    if transcoding_profile.min_segments > 0 {
        playlist_item.min_segments = Some(transcoding_profile.min_segments);
    }
    if transcoding_profile.segment_length > 0 {
        playlist_item.segment_length = Some(transcoding_profile.segment_length);
    }
}

fn set_stream_info_options_from_direct_play_profile(
    options: &MediaOptions,
    item: &mut MediaSourceInfo,
    playlist_item: &mut StreamInfo,
    direct_play_profile: Option<&DirectPlayProfile>,
) {
    let container = normalize_media_source_format_into_single_container(
        item.container.as_deref(),
        Some(&options.profile),
        DlnaProfileType::Video,
        direct_play_profile,
    );
    let protocol = MediaStreamProtocol::http;

    item.transcoding_container.clone_from(&container);
    item.transcoding_sub_protocol = protocol;

    playlist_item.container.clone_from(&container);
    playlist_item.sub_protocol = protocol;

    playlist_item.video_codecs = item
        .video_stream()
        .and_then(|v| v.codec.clone())
        .into_iter()
        .collect();
    playlist_item.audio_codecs =
        split_owned(direct_play_profile.and_then(|p| p.audio_codec.as_deref()));
}

fn check_video_conditions(
    conditions: &[ProfileCondition],
    media_source: &MediaSourceInfo,
    video_stream: Option<&MediaStream>,
) -> Vec<ProfileCondition> {
    let width = video_stream.and_then(|v| v.width);
    let height = video_stream.and_then(|v| v.height);
    let bit_depth = video_stream.and_then(|v| v.bit_depth);
    let video_bitrate = video_stream.and_then(|v| v.bit_rate);
    let video_level = video_stream.and_then(|v| v.level);
    let video_profile = video_stream.and_then(|v| v.profile.clone());
    let video_range_type = video_stream.map(MediaStream::video_range_type);
    let video_framerate = video_stream.map_or(0.0, |v| v.reference_frame_rate().unwrap_or(0.0));
    let is_anamorphic = video_stream.and_then(|v| v.is_anamorphic);
    let is_interlaced = video_stream.map(|v| v.is_interlaced);
    let video_codec_tag = video_stream.and_then(|v| v.codec_tag.clone());
    let is_avc = video_stream.and_then(|v| v.is_avc);
    let video_rotation = video_stream.and_then(|v| v.rotation);

    let timestamp = if video_stream.is_none() {
        Some(TransportStreamTimestamp::None)
    } else {
        media_source.timestamp
    };
    let packet_length = video_stream.and_then(|v| v.packet_length);
    let ref_frames = video_stream.and_then(|v| v.ref_frames);

    let num_streams = i32::try_from(media_source.media_streams.len()).unwrap_or(i32::MAX);
    let num_audio_streams = media_source.get_stream_count(MediaStreamType::Audio);
    let num_video_streams = media_source.get_stream_count(MediaStreamType::Video);

    conditions
        .iter()
        .filter(|apply| {
            !ConditionProcessor::is_video_condition_satisfied(
                apply,
                width,
                height,
                bit_depth,
                video_bitrate,
                video_profile.as_deref(),
                video_range_type,
                video_level,
                Some(video_framerate),
                packet_length,
                timestamp,
                is_anamorphic,
                is_interlaced,
                ref_frames,
                num_streams,
                num_video_streams,
                num_audio_streams,
                video_codec_tag.as_deref(),
                is_avc,
                video_rotation,
            )
        })
        .cloned()
        .collect()
}

fn aggregate_failure_conditions(conditions: &[ProfileCondition]) -> TranscodeReasons {
    conditions
        .iter()
        .fold(TranscodeReasons::empty(), |reasons, c| {
            reasons | get_transcode_reason_for_failed_condition(c)
        })
}

fn get_transcode_reason_for_failed_condition(condition: &ProfileCondition) -> TranscodeReasons {
    match condition.property {
        ProfileConditionValue::AudioBitrate => TranscodeReasons::AUDIO_BITRATE_NOT_SUPPORTED,
        ProfileConditionValue::AudioChannels => TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED,
        ProfileConditionValue::AudioProfile => TranscodeReasons::AUDIO_PROFILE_NOT_SUPPORTED,
        ProfileConditionValue::AudioSampleRate => TranscodeReasons::AUDIO_SAMPLE_RATE_NOT_SUPPORTED,
        ProfileConditionValue::Height | ProfileConditionValue::Width => {
            TranscodeReasons::VIDEO_RESOLUTION_NOT_SUPPORTED
        }
        ProfileConditionValue::IsAnamorphic => TranscodeReasons::ANAMORPHIC_VIDEO_NOT_SUPPORTED,
        ProfileConditionValue::IsInterlaced => TranscodeReasons::INTERLACED_VIDEO_NOT_SUPPORTED,
        ProfileConditionValue::IsSecondaryAudio => TranscodeReasons::SECONDARY_AUDIO_NOT_SUPPORTED,
        ProfileConditionValue::NumStreams => TranscodeReasons::STREAM_COUNT_EXCEEDS_LIMIT,
        ProfileConditionValue::RefFrames => TranscodeReasons::REF_FRAMES_NOT_SUPPORTED,
        ProfileConditionValue::VideoBitDepth => TranscodeReasons::VIDEO_BIT_DEPTH_NOT_SUPPORTED,
        ProfileConditionValue::AudioBitDepth => TranscodeReasons::AUDIO_BIT_DEPTH_NOT_SUPPORTED,
        ProfileConditionValue::VideoBitrate => TranscodeReasons::VIDEO_BITRATE_NOT_SUPPORTED,
        ProfileConditionValue::VideoCodecTag => TranscodeReasons::VIDEO_CODEC_TAG_NOT_SUPPORTED,
        ProfileConditionValue::VideoFramerate => TranscodeReasons::VIDEO_FRAMERATE_NOT_SUPPORTED,
        ProfileConditionValue::VideoLevel => TranscodeReasons::VIDEO_LEVEL_NOT_SUPPORTED,
        ProfileConditionValue::VideoProfile => TranscodeReasons::VIDEO_PROFILE_NOT_SUPPORTED,
        ProfileConditionValue::VideoRangeType => TranscodeReasons::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
        ProfileConditionValue::VideoRotation => TranscodeReasons::VIDEO_ROTATION_NOT_SUPPORTED,
        _ => TranscodeReasons::empty(),
    }
}

#[allow(clippy::too_many_arguments)]
fn get_profile_conditions_for_video_audio(
    codec_profiles: &[CodecProfile],
    container: &str,
    codec: Option<&str>,
    audio_channels: Option<i32>,
    audio_bitrate: Option<i32>,
    audio_sample_rate: Option<i32>,
    audio_bit_depth: Option<i32>,
    audio_profile: Option<&str>,
    is_secondary_audio: Option<bool>,
) -> Vec<ProfileCondition> {
    codec_profiles
        .iter()
        .filter(|profile| {
            profile.codec_type == CodecType::VideoAudio
                && profile.contains_codec(codec, Some(container), false)
                && profile.apply_conditions.iter().all(|apply| {
                    ConditionProcessor::is_video_audio_condition_satisfied(
                        apply,
                        audio_channels,
                        audio_bitrate,
                        audio_sample_rate,
                        audio_bit_depth,
                        audio_profile,
                        is_secondary_audio,
                    )
                })
        })
        .flat_map(|profile| profile.conditions.iter().cloned())
        .filter(|condition| {
            !ConditionProcessor::is_video_audio_condition_satisfied(
                condition,
                audio_channels,
                audio_bitrate,
                audio_sample_rate,
                audio_bit_depth,
                audio_profile,
                is_secondary_audio,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn get_profile_conditions_for_audio(
    codec_profiles: &[CodecProfile],
    container: &str,
    codec: Option<&str>,
    audio_channels: Option<i32>,
    audio_bitrate: Option<i32>,
    audio_sample_rate: Option<i32>,
    audio_bit_depth: Option<i32>,
    check_conditions: bool,
) -> Vec<ProfileCondition> {
    let conditions = codec_profiles
        .iter()
        .filter(|profile| {
            profile.codec_type == CodecType::Audio
                && profile.contains_codec(codec, Some(container), false)
                && profile.apply_conditions.iter().all(|apply| {
                    ConditionProcessor::is_audio_condition_satisfied(
                        apply,
                        audio_channels,
                        audio_bitrate,
                        audio_sample_rate,
                        audio_bit_depth,
                    )
                })
        })
        .flat_map(|profile| profile.conditions.iter().cloned());

    if !check_conditions {
        return conditions.collect();
    }

    conditions
        .filter(|condition| {
            !ConditionProcessor::is_audio_condition_satisfied(
                condition,
                audio_channels,
                audio_bitrate,
                audio_sample_rate,
                audio_bit_depth,
            )
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn apply_transcoding_conditions(
    item: &mut StreamInfo,
    conditions: &[ProfileCondition],
    qualifier: Option<&str>,
    enable_qualified_conditions: bool,
    enable_non_qualified_conditions: bool,
) {
    let has_qualifier = qualifier.is_some_and(|q| !q.is_empty());
    for condition in conditions {
        let value = condition.value.as_str();
        if value.is_empty() {
            continue;
        }
        if condition.condition == ProfileConditionType::GreaterThanEqual {
            continue;
        }

        match condition.property {
            ProfileConditionValue::AudioBitrate => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.audio_bitrate = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.audio_bitrate = Some(num.min(item.audio_bitrate.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::AudioSampleRate => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.audio_sample_rate = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.audio_sample_rate =
                                Some(num.min(item.audio_sample_rate.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::AudioChannels => {
                if !condition_enabled(
                    has_qualifier,
                    enable_qualified_conditions,
                    enable_non_qualified_conditions,
                ) {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => {
                            item.set_option_qualified(qualifier, "audiochannels", num.to_string());
                        }
                        ProfileConditionType::LessThanEqual => {
                            let existing = item.get_target_audio_channels(qualifier).unwrap_or(num);
                            item.set_option_qualified(
                                qualifier,
                                "audiochannels",
                                num.min(existing).to_string(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::IsAvc => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(is_avc) = value.parse::<bool>()
                    && ((is_avc && condition.condition == ProfileConditionType::Equals)
                        || (!is_avc && condition.condition == ProfileConditionType::NotEquals))
                {
                    item.require_avc = true;
                }
            }
            ProfileConditionValue::IsAnamorphic => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(is_anamorphic) = value.parse::<bool>()
                    && ((is_anamorphic && condition.condition == ProfileConditionType::Equals)
                        || (!is_anamorphic
                            && condition.condition == ProfileConditionType::NotEquals))
                {
                    item.require_non_anamorphic = true;
                }
            }
            ProfileConditionValue::IsInterlaced => {
                if !condition_enabled(
                    has_qualifier,
                    enable_qualified_conditions,
                    enable_non_qualified_conditions,
                ) {
                    continue;
                }
                if let Ok(is_interlaced) = value.parse::<bool>()
                    && ((!is_interlaced && condition.condition == ProfileConditionType::Equals)
                        || (is_interlaced
                            && condition.condition == ProfileConditionType::NotEquals))
                {
                    item.set_option_qualified(qualifier, "deinterlace", "true".to_owned());
                }
            }
            ProfileConditionValue::RefFrames => {
                if !condition_enabled(
                    has_qualifier,
                    enable_qualified_conditions,
                    enable_non_qualified_conditions,
                ) {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => {
                            item.set_option_qualified(qualifier, "maxrefframes", num.to_string());
                        }
                        ProfileConditionType::LessThanEqual => {
                            let existing = item.get_target_ref_frames(qualifier).unwrap_or(num);
                            item.set_option_qualified(
                                qualifier,
                                "maxrefframes",
                                num.min(existing).to_string(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::VideoBitDepth => {
                if !condition_enabled(
                    has_qualifier,
                    enable_qualified_conditions,
                    enable_non_qualified_conditions,
                ) {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => {
                            item.set_option_qualified(qualifier, "videobitdepth", num.to_string());
                        }
                        ProfileConditionType::LessThanEqual => {
                            let existing =
                                item.get_target_video_bit_depth(qualifier).unwrap_or(num);
                            item.set_option_qualified(
                                qualifier,
                                "videobitdepth",
                                num.min(existing).to_string(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::VideoProfile => {
                if !has_qualifier {
                    continue;
                }
                let values: Vec<&str> = value.split('|').filter(|v| !v.is_empty()).collect();
                match condition.condition {
                    ProfileConditionType::Equals => {
                        item.set_option_qualified(qualifier, "profile", values.join(","));
                    }
                    ProfileConditionType::EqualsAny => {
                        let current = item.get_option(qualifier, "profile").map(str::to_owned);
                        if let Some(current) =
                            current.filter(|c| !c.is_empty() && values.iter().any(|v| *v == c))
                        {
                            item.set_option_qualified(qualifier, "profile", current);
                        } else {
                            item.set_option_qualified(qualifier, "profile", values.join(","));
                        }
                    }
                    _ => {}
                }
            }
            ProfileConditionValue::VideoRangeType => {
                if !has_qualifier {
                    continue;
                }
                let values: Vec<&str> = value
                    .split('|')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .collect();
                match condition.condition {
                    ProfileConditionType::Equals => {
                        item.set_option_qualified(qualifier, "rangetype", values.join(","));
                    }
                    ProfileConditionType::NotEquals => {
                        let names = video_range_type_all_names();
                        let remaining: Vec<&str> =
                            names.into_iter().filter(|n| !values.contains(n)).collect();
                        item.set_option_qualified(qualifier, "rangetype", remaining.join(","));
                    }
                    ProfileConditionType::EqualsAny => {
                        let current = item.get_option(qualifier, "rangetype").map(str::to_owned);
                        if let Some(current) = current.filter(|c| {
                            !c.is_empty() && values.iter().any(|v| v.eq_ignore_ascii_case(c))
                        }) {
                            item.set_option_qualified(qualifier, "rangetype", current);
                        } else {
                            item.set_option_qualified(qualifier, "rangetype", values.join(","));
                        }
                    }
                    _ => {}
                }
            }
            ProfileConditionValue::VideoCodecTag => {
                if !has_qualifier {
                    continue;
                }
                let values: Vec<&str> = value
                    .split('|')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .collect();
                apply_string_list_condition(item, condition, qualifier, "codectag", &values);
            }
            ProfileConditionValue::VideoRotation => {
                if !has_qualifier {
                    continue;
                }
                let values: Vec<&str> = value
                    .split('|')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .collect();
                apply_string_list_condition(item, condition, qualifier, "rotation", &values);
            }
            ProfileConditionValue::Height => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.max_height = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.max_height = Some(num.min(item.max_height.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::VideoBitrate => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.video_bitrate = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.video_bitrate = Some(num.min(item.video_bitrate.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::VideoFramerate => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<f32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.max_framerate = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.max_framerate = Some(num.min(item.max_framerate.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::VideoLevel => {
                if !has_qualifier {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => {
                            item.set_option_qualified(qualifier, "level", num.to_string());
                        }
                        ProfileConditionType::LessThanEqual => {
                            let existing = item
                                .get_target_video_level(qualifier)
                                .unwrap_or(f64::from(num));
                            let min = f64::from(num).min(existing);
                            item.set_option_qualified(qualifier, "level", format_level(min));
                        }
                        _ => {}
                    }
                }
            }
            ProfileConditionValue::Width => {
                if !enable_non_qualified_conditions {
                    continue;
                }
                if let Ok(num) = value.parse::<i32>() {
                    match condition.condition {
                        ProfileConditionType::Equals => item.max_width = Some(num),
                        ProfileConditionType::LessThanEqual => {
                            item.max_width = Some(num.min(item.max_width.unwrap_or(num)));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn apply_string_list_condition(
    item: &mut StreamInfo,
    condition: &ProfileCondition,
    qualifier: Option<&str>,
    option: &str,
    values: &[&str],
) {
    match condition.condition {
        ProfileConditionType::Equals => {
            item.set_option_qualified(qualifier, option, values.join(","));
        }
        ProfileConditionType::EqualsAny => {
            let current = item.get_option(qualifier, option).map(str::to_owned);
            if let Some(current) = current
                .filter(|c| !c.is_empty() && values.iter().any(|v| v.eq_ignore_ascii_case(c)))
            {
                item.set_option_qualified(qualifier, option, current);
            } else {
                item.set_option_qualified(qualifier, option, values.join(","));
            }
        }
        _ => {}
    }
}

fn condition_enabled(
    has_qualifier: bool,
    enable_qualified: bool,
    enable_non_qualified: bool,
) -> bool {
    if has_qualifier {
        enable_qualified
    } else {
        enable_non_qualified
    }
}

fn get_default_audio_bitrate(audio_codec: Option<&str>, audio_channels: Option<i32>) -> i32 {
    if let Some(codec) = audio_codec.filter(|c| !c.is_empty()) {
        if codec.eq_ignore_ascii_case("aac")
            || codec.eq_ignore_ascii_case("mp3")
            || codec.eq_ignore_ascii_case("ac3")
            || codec.eq_ignore_ascii_case("eac3")
        {
            if audio_channels.unwrap_or(0) < 2 {
                return 128_000;
            }
            return if audio_channels.unwrap_or(0) >= 6 {
                640_000
            } else {
                384_000
            };
        }
        if codec.eq_ignore_ascii_case("flac") || codec.eq_ignore_ascii_case("alac") {
            if audio_channels.unwrap_or(0) < 2 {
                return 768_000;
            }
            return if audio_channels.unwrap_or(0) >= 6 {
                3_584_000
            } else {
                1_536_000
            };
        }
    }
    192_000
}

fn get_audio_bitrate(
    max_total_bitrate: i64,
    target_audio_codecs: &[String],
    audio_stream: Option<&MediaStream>,
    item: &StreamInfo,
) -> i32 {
    let target_audio_codec = target_audio_codecs.first().map(String::as_str);
    let target_audio_channels = item.get_target_audio_channels(target_audio_codec);

    let mut default_bitrate;
    let mut encoder_audio_bitrate_limit = i32::MAX;

    if let Some(audio_stream) = audio_stream {
        if let (Some(tac), Some(sc)) = (target_audio_channels, audio_stream.channels) {
            if sc > tac {
                default_bitrate =
                    get_default_audio_bitrate(target_audio_codec, target_audio_channels);
            } else if sc <= tac
                && audio_stream.codec.as_deref().is_some_and(|c| !c.is_empty())
                && !target_audio_codecs.is_empty()
                && !target_audio_codecs.iter().any(|e| {
                    audio_stream
                        .codec
                        .as_deref()
                        .is_some_and(|c| c.eq_ignore_ascii_case(e))
                })
            {
                default_bitrate =
                    get_default_audio_bitrate(target_audio_codec, audio_stream.channels);
            } else {
                default_bitrate = audio_stream.bit_rate.unwrap_or_else(|| {
                    get_default_audio_bitrate(target_audio_codec, target_audio_channels)
                });
            }
        } else {
            default_bitrate = audio_stream.bit_rate.unwrap_or_else(|| {
                get_default_audio_bitrate(target_audio_codec, target_audio_channels)
            });
        }

        if audio_stream.channels == Some(1) && audio_stream.bit_rate.unwrap_or(0) < 64_000 {
            encoder_audio_bitrate_limit = 64_000;
        }
    } else {
        default_bitrate = 192_000;
    }

    if max_total_bitrate > 0 {
        default_bitrate =
            get_max_audio_bitrate_for_total_bitrate(max_total_bitrate).min(default_bitrate);
    }

    default_bitrate.min(encoder_audio_bitrate_limit)
}

#[allow(clippy::match_overlapping_arm)] // Top-down arms mirror the C# `if <=` cascade.
fn get_max_audio_bitrate_for_total_bitrate(total_bitrate: i64) -> i32 {
    match total_bitrate {
        ..=640_000 => 128_000,
        ..=2_000_000 => 384_000,
        ..=3_000_000 => 448_000,
        ..=4_000_000 => 640_000,
        ..=5_000_000 => 768_000,
        ..=10_000_000 => 1_536_000,
        ..=15_000_000 => 2_304_000,
        ..=20_000_000 => 3_584_000,
        _ => 7_168_000,
    }
}

fn is_subtitle_embed_supported(transcoding_container: Option<&str>) -> bool {
    if let Some(container) = transcoding_container.filter(|c| !c.is_empty()) {
        if contains_container(Some("ts,mpegts,mp4"), Some(container)) {
            return false;
        }
        if contains_container(Some("mkv,matroska"), Some(container)) {
            return true;
        }
    }
    false
}

fn can_consider_embed_subtitle(
    subtitle_stream: &MediaStream,
    play_method: PlayMethod,
    transcoding_sub_protocol: Option<MediaStreamProtocol>,
    output_container: Option<&str>,
) -> bool {
    if subtitle_stream.is_external {
        return play_method == PlayMethod::Transcode
            && transcoding_sub_protocol != Some(MediaStreamProtocol::hls)
            && is_subtitle_embed_supported(output_container);
    }
    play_method != PlayMethod::Transcode
        || transcoding_sub_protocol != Some(MediaStreamProtocol::hls)
}

fn get_external_subtitle_profile(
    media_source: &MediaSourceInfo,
    subtitle_stream: &MediaStream,
    subtitle_profiles: &[SubtitleProfile],
    play_method: PlayMethod,
    transcoder_support: &dyn TranscoderSupport,
    allow_conversion: bool,
) -> Option<SubtitleProfile> {
    for profile in subtitle_profiles {
        if profile.method != SubtitleDeliveryMethod::External
            && profile.method != SubtitleDeliveryMethod::Hls
        {
            continue;
        }
        if profile.method == SubtitleDeliveryMethod::Hls && play_method != PlayMethod::Transcode {
            continue;
        }
        if !profile.supports_language(subtitle_stream.language.as_deref()) {
            continue;
        }
        if !subtitle_stream.is_external
            && play_method == PlayMethod::Transcode
            && !transcoder_support
                .can_extract_subtitles(subtitle_stream.codec.as_deref().unwrap_or(""))
        {
            continue;
        }

        let is_vob_sub_mks = is_vob_sub_mks_profile(profile, subtitle_stream);

        let text_matches = subtitle_stream.is_text_subtitle_stream()
            == MediaStream::is_text_format(profile.format.as_deref());

        if (profile.method == SubtitleDeliveryMethod::External
            && (is_vob_sub_mks || (!is_vob_sub_mks_delivery_profile(profile) && text_matches)))
            || (profile.method == SubtitleDeliveryMethod::Hls
                && subtitle_stream.is_text_subtitle_stream())
        {
            let requires_conversion = !is_vob_sub_mks
                && !subtitle_stream
                    .codec
                    .as_deref()
                    .zip(profile.format.as_deref())
                    .is_some_and(|(c, f)| c.eq_ignore_ascii_case(f));

            if !requires_conversion {
                return Some(profile.clone());
            }
            if !allow_conversion {
                continue;
            }
            if media_source.is_infinite_stream {
                continue;
            }
            if subtitle_stream.is_text_subtitle_stream()
                && subtitle_stream.supports_external_stream
                && profile
                    .format
                    .as_deref()
                    .is_some_and(|f| subtitle_stream.supports_subtitle_conversion_to(f))
            {
                return Some(profile.clone());
            }
        }
    }
    None
}

fn is_vob_sub_mks_delivery_profile(profile: &SubtitleProfile) -> bool {
    MediaStream::is_vob_sub_format(profile.format.as_deref())
        && profile
            .container
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty())
        && contains_container(profile.container.as_deref(), Some("mks"))
}

fn is_vob_sub_mks_profile(profile: &SubtitleProfile, subtitle_stream: &MediaStream) -> bool {
    is_vob_sub_mks_delivery_profile(profile)
        && subtitle_stream.is_vob_sub_subtitle_stream()
        && (!subtitle_stream.is_external
            || subtitle_stream
                .path
                .as_deref()
                .is_some_and(|p| p.to_ascii_lowercase().ends_with(".mks")))
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

fn split_owned(input: Option<&str>) -> Vec<String> {
    split(input).into_iter().map(str::to_owned).collect()
}

/// Mirrors `ContainerHelper.ContainsContainer(IReadOnlyList, bool, string)`.
fn contains_container_list(
    profile_containers: &[&str],
    is_negative: bool,
    input: Option<&str>,
) -> bool {
    let input = input.unwrap_or("");
    if input.is_empty() {
        return is_negative;
    }
    if profile_containers.is_empty() {
        return true;
    }
    for container in input.split(',') {
        if container.is_empty() {
            continue;
        }
        for profile in profile_containers {
            if !profile.is_empty() && container.eq_ignore_ascii_case(profile) {
                return !is_negative;
            }
        }
    }
    is_negative
}

/// Formats an f64 codec level the way .NET's invariant culture does for the
/// integral values seen here.
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
fn format_f64(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn format_level(v: f64) -> String {
    format_f64(v)
}

// Keep the range-name helper referenced so it is not flagged unused.
#[allow(dead_code)]
const _RANGE_NAME_FN: fn(VideoRangeType) -> &'static str = range_type_name;

// Ordering import retained for clarity of intent in comparators.
#[allow(dead_code)]
const _ORDERING: Option<Ordering> = None;
