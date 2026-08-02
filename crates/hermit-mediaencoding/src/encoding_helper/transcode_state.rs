//! Minimal encoding job-state structs — the prerequisite for the software
//! transcode arg builder.
//!
//! Ports the *subset* of `MediaBrowser.Controller.MediaEncoding`'s
//! `EncodingJobInfo` / `BaseEncodingJobOptions` (and the `StreamState` derived
//! type) that the ported software-path [`EncodingHelper`](super::EncodingHelper)
//! methods actually read. The full C# types carry ~150 members spanning the
//! hardware-acceleration matrix, HDR/tonemap plumbing, and session wiring; only
//! the fields the *core software transcode + direct-play decision* touches are
//! ported here. The remainder is deferred (see `brain/DEFERRED.md`).
//!
//! Value types (`MediaStream`, `MediaSourceInfo`, codec/range/context enums) are
//! **reused from `hermit-model`** rather than re-declared, per
//! `RULES_CODE_REUSE`.

use std::path::PathBuf;

use hermit_model::dlna::{EncodingContext, SubtitleDeliveryMethod};
use hermit_model::dto::MediaSourceInfo;
use hermit_model::entities_media::MediaStream;
use hermit_traits::media_encoding::TranscodingJobType;

/// The character(s) profile/range/rotation option lists are split on.
///
/// Port of `EncodingJobInfo._separators = ['|', ',']`.
const OPTION_SEPARATORS: [char; 2] = ['|', ','];

/// Whether the encoder can re-encode a given codec.
///
/// Port of the single `IMediaEncoder.SupportsEncoder(string)` call the software
/// audio-encoder selection makes (`aac_at` / `libfdk_aac` probing). Kept behind
/// a trait so the arg builder is unit-testable with a fake — the real capability
/// probe (spawning `ffmpeg -encoders`) is un-mockable I/O and lives in the
/// [`MediaEncoderImpl`](crate::MediaEncoderImpl) seam, out of these numbers.
pub trait EncoderCapabilities: Send + Sync {
    /// Returns whether `encoder` (an ffmpeg encoder name, e.g. `"aac_at"`) is
    /// available. Port of `IMediaEncoder.SupportsEncoder`.
    fn supports_encoder(&self, encoder: &str) -> bool;
}

/// [`EncoderCapabilities`] that reports no optional encoders available.
///
/// Mirrors a stock ffmpeg build with neither Apple AudioToolbox (`aac_at`) nor
/// `libfdk_aac`; the software audio path then falls back to native `aac`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOptionalEncoders;

impl EncoderCapabilities for NoOptionalEncoders {
    fn supports_encoder(&self, _encoder: &str) -> bool {
        false
    }
}

/// [`EncoderCapabilities`] backed by the startup `ffmpeg -encoders` probe.
///
/// The composition root parses the discovered binary's encoder list once
/// (`EncoderValidator::get_codecs_internal`) and wires it here, so the software
/// audio path prefers `aac_at`/`libfdk_aac` exactly when the running ffmpeg
/// actually has them (jellyfin-ffmpeg does; stock builds don't).
#[derive(Debug, Clone, Default)]
pub struct ProbedEncoders(Vec<String>);

impl ProbedEncoders {
    /// Wraps the probed encoder names.
    #[must_use]
    pub fn new(encoders: Vec<String>) -> Self {
        Self(encoders)
    }
}

impl EncoderCapabilities for ProbedEncoders {
    fn supports_encoder(&self, encoder: &str) -> bool {
        self.0.iter().any(|e| e == encoder)
    }
}

/// The per-request encoding options — the software-relevant subset.
///
/// Port of the fields of `BaseEncodingJobOptions` the ported methods read.
/// Defaults mirror the C# constructor (`EnableAutoStreamCopy = true`,
/// `AllowVideoStreamCopy = true`, `AllowAudioStreamCopy = true`,
/// `Context = Streaming`).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // Faithful field-for-field port of the C# options.
pub struct BaseEncodingJobOptions {
    /// Whether automatic stream copy is enabled (`EnableAutoStreamCopy`).
    pub enable_auto_stream_copy: bool,
    /// Whether video stream copy is allowed (`AllowVideoStreamCopy`).
    pub allow_video_stream_copy: bool,
    /// Whether audio stream copy is allowed (`AllowAudioStreamCopy`).
    pub allow_audio_stream_copy: bool,
    /// The requested audio codec (`AudioCodec`).
    pub audio_codec: Option<String>,
    /// The requested audio sample rate (`AudioSampleRate`).
    pub audio_sample_rate: Option<i32>,
    /// The maximum audio bit depth (`MaxAudioBitDepth`).
    pub max_audio_bit_depth: Option<i32>,
    /// The requested audio bit rate (`AudioBitRate`).
    pub audio_bit_rate: Option<i32>,
    /// The requested audio channels (`AudioChannels`).
    pub audio_channels: Option<i32>,
    /// The maximum audio channels (`MaxAudioChannels`).
    pub max_audio_channels: Option<i32>,
    /// The transcoding-max audio channels (`TranscodingMaxAudioChannels`).
    pub transcoding_max_audio_channels: Option<i32>,
    /// Whether this is a static (direct) stream (`Static`).
    pub is_static: bool,
    /// The requested video profile (`Profile`).
    pub profile: Option<String>,
    /// The requested video range type (`VideoRangeType`).
    pub video_range_type: Option<String>,
    /// The requested video level (`Level`).
    pub level: Option<String>,
    /// The requested rotation, in degrees (`Rotation`).
    pub rotation: Option<String>,
    /// The requested framerate (`Framerate`).
    pub framerate: Option<f32>,
    /// The maximum framerate (`MaxFramerate`).
    pub max_framerate: Option<f32>,
    /// Whether input/output timestamps are copied (`CopyTimestamps`).
    pub copy_timestamps: bool,
    /// The requested output width (`Width`).
    pub width: Option<i32>,
    /// The requested output height (`Height`).
    pub height: Option<i32>,
    /// The maximum output width (`MaxWidth`).
    pub max_width: Option<i32>,
    /// The maximum output height (`MaxHeight`).
    pub max_height: Option<i32>,
    /// The requested video bit rate (`VideoBitRate`).
    pub video_bit_rate: Option<i32>,
    /// The requested subtitle stream index (`SubtitleStreamIndex`).
    pub subtitle_stream_index: Option<i32>,
    /// The maximum reference-frame count (`MaxRefFrames`).
    pub max_ref_frames: Option<i32>,
    /// The maximum video bit depth (`MaxVideoBitDepth`).
    pub max_video_bit_depth: Option<i32>,
    /// Whether the client requires AVC (`RequireAvc`).
    pub require_avc: bool,
    /// Whether the input is force-deinterlaced (`DeInterlace`).
    pub deinterlace: bool,
    /// Whether the client requires non-anamorphic video (`RequireNonAnamorphic`).
    pub require_non_anamorphic: bool,
    /// A per-request CPU-core limit for encoding threads (`CpuCoreLimit`).
    pub cpu_core_limit: Option<i32>,
    /// The live-stream id, if this is a live source (`LiveStreamId`).
    pub live_stream_id: Option<String>,
    /// Whether MPEG-TS M2TS mode is enabled (`EnableMpegtsM2TsMode`).
    pub enable_mpegts_m2ts_mode: bool,
    /// The encoding context (`Context`).
    pub context: EncodingContext,
    /// Per-codec stream options (`StreamOptions`), keyed by `qualifier-name`.
    pub stream_options: Vec<(String, String)>,
    /// Whether burnt-in subtitles are always used when transcoding
    /// (`AlwaysBurnInSubtitleWhenTranscoding`).
    pub always_burn_in_subtitle_when_transcoding: bool,
}

impl Default for BaseEncodingJobOptions {
    fn default() -> Self {
        Self {
            // Non-default fields mirror the C# constructor initializers.
            enable_auto_stream_copy: true,
            allow_video_stream_copy: true,
            allow_audio_stream_copy: true,
            context: EncodingContext::Streaming,
            audio_codec: None,
            audio_sample_rate: None,
            max_audio_bit_depth: None,
            audio_bit_rate: None,
            audio_channels: None,
            max_audio_channels: None,
            transcoding_max_audio_channels: None,
            is_static: false,
            profile: None,
            video_range_type: None,
            level: None,
            rotation: None,
            framerate: None,
            max_framerate: None,
            copy_timestamps: false,
            width: None,
            height: None,
            max_width: None,
            max_height: None,
            video_bit_rate: None,
            subtitle_stream_index: None,
            max_ref_frames: None,
            max_video_bit_depth: None,
            require_avc: false,
            deinterlace: false,
            require_non_anamorphic: false,
            cpu_core_limit: None,
            live_stream_id: None,
            enable_mpegts_m2ts_mode: false,
            stream_options: Vec::new(),
            always_burn_in_subtitle_when_transcoding: false,
        }
    }
}

impl BaseEncodingJobOptions {
    /// Looks up a per-codec stream option by `qualifier` then bare `name`.
    ///
    /// Port of `GetOption(qualifier, name)`: tries `"{qualifier}-{name}"` first,
    /// falling back to the bare `name`.
    #[must_use]
    pub fn option(&self, qualifier: &str, name: &str) -> Option<&str> {
        let combined_key = format!("{qualifier}-{name}");
        self.option_by_name(&combined_key)
            .or_else(|| self.option_by_name(name))
    }

    /// Looks up a stream option by exact key. Port of `GetOption(name)`.
    #[must_use]
    pub fn option_by_name(&self, name: &str) -> Option<&str> {
        self.stream_options
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The state of a single transcode job — the software-path subset.
///
/// Port of the fields of `EncodingJobInfo` (the base of `StreamState`) that the
/// ported software methods read. `StreamState` in C# is a thin subclass adding
/// session/stream bookkeeping the arg builder does not consult, so it is
/// represented by this same struct.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodingJobInfo {
    /// The per-request options (`BaseRequest`).
    pub base_request: BaseEncodingJobOptions,
    /// The selected video stream (`VideoStream`).
    pub video_stream: Option<MediaStream>,
    /// The selected audio stream (`AudioStream`).
    pub audio_stream: Option<MediaStream>,
    /// The selected subtitle stream (`SubtitleStream`).
    pub subtitle_stream: Option<MediaStream>,
    /// The media source (`MediaSource`) — its stream list backs `-map` indices.
    pub media_source: MediaSourceInfo,
    /// The chosen output video codec (`OutputVideoCodec`).
    pub output_video_codec: Option<String>,
    /// The chosen output audio codec (`OutputAudioCodec`).
    pub output_audio_codec: Option<String>,
    /// The output video bitrate, in bps (`OutputVideoBitrate`).
    pub output_video_bitrate: Option<i32>,
    /// The output audio bitrate, in bps (`OutputAudioBitrate`).
    pub output_audio_bitrate: Option<i32>,
    /// The output audio channel count (`OutputAudioChannels`).
    pub output_audio_channels: Option<i32>,
    /// The output container (`OutputContainer`).
    pub output_container: Option<String>,
    /// The output video sync mode (`OutputVideoSync`).
    pub output_video_sync: Option<String>,
    /// The output file path (`OutputFilePath`).
    pub output_file_path: String,
    /// The input container (`InputContainer`).
    pub input_container: Option<String>,
    /// Whether the input is video (`IsInputVideo`).
    pub is_input_video: bool,
    /// The subtitle delivery method (`SubtitleDeliveryMethod`).
    pub subtitle_delivery_method: SubtitleDeliveryMethod,
    /// The runtime, in ticks (`RunTimeTicks`); `None` marks an open-ended
    /// (segmented) live stream.
    pub run_time_ticks: Option<i64>,
    /// The kind of transcode (`TranscodingType`).
    pub transcoding_type: TranscodingJobType,
    /// The video codecs the target supports (`SupportedVideoCodecs`).
    pub supported_video_codecs: Vec<String>,
    /// The audio codecs the target supports (`SupportedAudioCodecs`).
    pub supported_audio_codecs: Vec<String>,
    /// The segment cut length, in seconds (`StreamState.SegmentLength`).
    ///
    /// Drives the segment cache filenames and the forward-gap heuristic. `0`
    /// marks a non-segmented (progressive) job.
    pub segment_length_secs: i32,
    /// The exact segment file `StartFfMpeg` blocks on before returning
    /// (`StreamState.WaitForPath`); `None` falls back to the output path.
    pub wait_for_path: Option<PathBuf>,
    /// The requested HLS segment container (`Request.SegmentContainer`), e.g.
    /// `"ts"` or `"mp4"`; `None` defaults to `.ts`.
    pub segment_container: Option<String>,
    /// The playback-session id this job serves (`Request.PlaySessionId`).
    pub play_session_id: Option<String>,
    /// The device id this job streams to (`Request.DeviceId`).
    pub device_id: Option<String>,
}

impl EncodingJobInfo {
    /// Whether `codec` is the stream-copy sentinel `"copy"`.
    ///
    /// Port of the static `EncodingHelper.IsCopyCodec`.
    #[must_use]
    pub fn is_copy_codec(codec: Option<&str>) -> bool {
        codec.is_some_and(|c| c.eq_ignore_ascii_case("copy"))
    }

    /// The effective output video codec: the source codec when the output is a
    /// stream copy, else the requested output codec. Port of
    /// `ActualOutputVideoCodec`.
    #[must_use]
    pub fn actual_output_video_codec(&self) -> Option<&str> {
        let stream = self.video_stream.as_ref()?;
        if Self::is_copy_codec(self.output_video_codec.as_deref()) {
            stream.codec.as_deref()
        } else {
            self.output_video_codec.as_deref()
        }
    }

    /// Whether the (interlaced) input should be deinterlaced for `video_codec`.
    ///
    /// Port of `DeInterlace(videoCodec, forceDeinterlaceIfSourceIsInterlaced)`.
    #[must_use]
    pub fn deinterlace(&self, video_codec: Option<&str>, force_if_interlaced: bool) -> bool {
        let is_input_interlaced = self.video_stream.as_ref().is_some_and(|s| s.is_interlaced);
        if !is_input_interlaced {
            return false;
        }

        if self.base_request.deinterlace {
            return true;
        }

        if let Some(codec) = video_codec
            && !codec.is_empty()
            && self
                .base_request
                .option(codec, "deinterlace")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            return true;
        }

        force_if_interlaced
    }

    /// The client-requested video profiles for `codec`. Port of
    /// `GetRequestedProfiles`.
    #[must_use]
    pub fn requested_profiles(&self, codec: &str) -> Vec<String> {
        let profile = self
            .base_request
            .profile
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| {
                (!codec.is_empty())
                    .then(|| {
                        self.base_request
                            .option(codec, "profile")
                            .map(str::to_owned)
                    })
                    .flatten()
            });
        split_options(profile.as_deref())
    }

    /// The client-requested video range types for `codec`. Port of
    /// `GetRequestedRangeTypes`.
    #[must_use]
    pub fn requested_range_types(&self, codec: &str) -> Vec<String> {
        let range = self
            .base_request
            .video_range_type
            .clone()
            .filter(|r| !r.is_empty())
            .or_else(|| {
                (!codec.is_empty())
                    .then(|| {
                        self.base_request
                            .option(codec, "rangetype")
                            .map(str::to_owned)
                    })
                    .flatten()
            });
        split_options(range.as_deref())
    }

    /// The client-requested rotations for `codec`. Port of
    /// `GetRequestedRotations`.
    #[must_use]
    pub fn requested_rotations(&self, codec: &str) -> Vec<String> {
        let rotation = self
            .base_request
            .rotation
            .clone()
            .filter(|r| !r.is_empty())
            .or_else(|| {
                (!codec.is_empty())
                    .then(|| {
                        self.base_request
                            .option(codec, "rotation")
                            .map(str::to_owned)
                    })
                    .flatten()
            });
        split_options(rotation.as_deref())
    }

    /// The client-requested level for `codec`. Port of `GetRequestedLevel`.
    #[must_use]
    pub fn requested_level(&self, codec: &str) -> Option<String> {
        if let Some(level) = self.base_request.level.as_deref()
            && !level.is_empty()
        {
            return Some(level.to_owned());
        }
        if !codec.is_empty() {
            return self.base_request.option(codec, "level").map(str::to_owned);
        }
        None
    }

    /// The client-requested max reference frames for `codec`. Port of
    /// `GetRequestedMaxRefFrames`.
    #[must_use]
    pub fn requested_max_ref_frames(&self, codec: &str) -> Option<i32> {
        if self.base_request.max_ref_frames.is_some() {
            return self.base_request.max_ref_frames;
        }
        if !codec.is_empty() {
            return self
                .base_request
                .option(codec, "maxrefframes")
                .and_then(|v| v.parse::<i32>().ok());
        }
        None
    }

    /// The client-requested max video bit depth for `codec`. Port of
    /// `GetRequestedVideoBitDepth`.
    #[must_use]
    pub fn requested_video_bit_depth(&self, codec: &str) -> Option<i32> {
        if self.base_request.max_video_bit_depth.is_some() {
            return self.base_request.max_video_bit_depth;
        }
        if !codec.is_empty() {
            return self
                .base_request
                .option(codec, "videobitdepth")
                .and_then(|v| v.parse::<i32>().ok());
        }
        None
    }

    /// The client-requested max audio bit depth for `codec`. Port of
    /// `GetRequestedAudioBitDepth`.
    #[must_use]
    pub fn requested_audio_bit_depth(&self, codec: &str) -> Option<i32> {
        if self.base_request.max_audio_bit_depth.is_some() {
            return self.base_request.max_audio_bit_depth;
        }
        if !codec.is_empty() {
            return self
                .base_request
                .option(codec, "audiobitdepth")
                .and_then(|v| v.parse::<i32>().ok());
        }
        None
    }

    /// The client-requested audio channel count for `codec`. Port of
    /// `GetRequestedAudioChannels` (option → max → requested → transcoding-max).
    #[must_use]
    pub fn requested_audio_channels(&self, codec: &str) -> Option<i32> {
        if !codec.is_empty()
            && let Some(value) = self
                .base_request
                .option(codec, "audiochannels")
                .and_then(|v| v.parse::<i32>().ok())
        {
            return Some(value);
        }
        self.base_request
            .max_audio_channels
            .or(self.base_request.audio_channels)
            .or(self.base_request.transcoding_max_audio_channels)
    }
}

/// Splits a `|`/`,`-delimited option string, dropping empty entries.
///
/// Port of `(value ?? string.Empty).Split(_separators, RemoveEmptyEntries)`.
fn split_options(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(OPTION_SEPARATORS)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
