//! Core software-transcode argument building and the direct-play decision.
//!
//! Ports the *software path* of `MediaBrowser.Controller.MediaEncoding.
//! EncodingHelper`: encoder selection (`libx264`/`aac`), stream mapping, the
//! bitrate/quality/thread params, and the `CanStreamCopy{Video,Audio}`
//! direct-play-vs-transcode decision.
//!
//! **Deferred (see `brain/DEFERRED.md`):** the entire hardware-acceleration
//! matrix (nvenc/qsv/vaapi/videotoolbox/rkmpp/amf), tonemapping/HDR filters, and
//! hardware scale/filter chains. Where a software-path branch would consult a
//! deferred hardware helper (e.g. the DOVI dynamic-metadata-removal check in
//! `CanStreamCopyVideo`), this port takes the conservative branch and documents
//! the omission inline.
//!
//! There is **no parity oracle in this test project** — the upstream
//! `EncodingHelper` xUnit tests live in the out-of-scope `Jellyfin.Controller`
//! test project. The tests below transliterate hand-derived expectations from
//! the C# logic; they are *not* an upstream oracle. (Flagged at PORT.)

use std::cmp::min;
use std::fmt::Write as _;

use hermit_model::configuration::EncodingOptions;
use hermit_model::data::VideoRangeType;
use hermit_model::dlna::SubtitleDeliveryMethod;
use hermit_model::entities::EncoderPreset;
use hermit_model::entities_media::MediaStream;
use hermit_model::session::TranscodeReasons;

use super::transcode_state::{BaseEncodingJobOptions, EncoderCapabilities, EncodingJobInfo};

/// The container/codec-name validation pattern.
///
/// Port of `EncodingHelper.ContainerValidationRegexStr` (`^[a-zA-Z0-9\-\._,|]
/// {0,40}$`), transliterated as a byte-for-byte character check to avoid a
/// runtime regex dependency for such a simple predicate.
fn is_valid_container(value: &str) -> bool {
    value.len() <= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b',' | b'|'))
}

/// The H264 profiles the transcoder recognises, in ascending-quality order.
///
/// Port of `_videoProfilesH264`; the index is the profile "score".
const VIDEO_PROFILES_H264: [&str; 8] = [
    "ConstrainedBaseline",
    "Baseline",
    "Extended",
    "Main",
    "High",
    "ProgressiveHigh",
    "ConstrainedHigh",
    "High10",
];

/// The HEVC profiles the transcoder recognises. Port of `_videoProfilesH265`.
const VIDEO_PROFILES_H265: [&str; 2] = ["Main", "Main10"];

/// The AV1 profiles the transcoder recognises. Port of `_videoProfilesAv1`.
const VIDEO_PROFILES_AV1: [&str; 3] = ["Main", "High", "Professional"];

/// Cap on the target video bitrate — 400 Mbps. Port of the `MaxSaneBitrate`
/// constant guarding against bogus plugin-stream metadata.
const MAX_SANE_BITRATE: i32 = 400_000_000;

/// The default number of logical CPUs assumed when the host count is unknown.
///
/// Port of `Environment.ProcessorCount`, read at construction so the ported
/// [`EncodingHelper::number_of_threads`] stays a pure function of its inputs.
const DEFAULT_PROCESSOR_COUNT: i32 = 1;

/// Builds ffmpeg software-transcode arguments and decides direct-play.
///
/// Port of the core software slice of `EncodingHelper`, generic over the
/// [`EncoderCapabilities`] seam (real ffmpeg probe vs. a test fake). Holds the
/// host processor count so [`number_of_threads`](Self::number_of_threads) is a
/// pure function.
pub struct EncodingHelper<C: EncoderCapabilities> {
    capabilities: C,
    processor_count: i32,
}

impl<C: EncoderCapabilities> EncodingHelper<C> {
    /// Creates a helper using `capabilities` to probe optional encoders,
    /// defaulting the host processor count to the number of available CPUs.
    #[must_use]
    pub fn new(capabilities: C) -> Self {
        let processor_count = std::thread::available_parallelism()
            .map_or(DEFAULT_PROCESSOR_COUNT, |n| {
                i32::try_from(n.get()).unwrap_or(i32::MAX)
            });
        Self {
            capabilities,
            processor_count,
        }
    }

    /// Creates a helper with an explicit host `processor_count`.
    ///
    /// Lets tests pin `Environment.ProcessorCount` so the thread-count port is
    /// deterministic across machines.
    #[must_use]
    pub fn with_processor_count(capabilities: C, processor_count: i32) -> Self {
        Self {
            capabilities,
            processor_count,
        }
    }

    // ----- encoder selection -------------------------------------------------

    /// Selects the ffmpeg video encoder for `state`.
    ///
    /// Port of the **software path** of `GetVideoEncoder`: `h264` maps to
    /// `libx264` and any pass-through/unknown-but-valid codec is returned as-is;
    /// hardware encoders (av1/h265/mjpeg hw variants) are deferred. Returns
    /// `"copy"` when no output codec is set.
    #[must_use]
    pub fn video_encoder(&self, state: &EncodingJobInfo) -> String {
        let Some(codec) = state
            .output_video_codec
            .as_deref()
            .filter(|c| !c.is_empty())
        else {
            return "copy".to_owned();
        };

        if codec.eq_ignore_ascii_case("h264") {
            // Software H.264 encoder. The hardware h264_* variants are deferred.
            return "libx264".to_owned();
        }

        if is_valid_container(codec) {
            return codec.to_ascii_lowercase();
        }

        "copy".to_owned()
    }

    /// Selects the ffmpeg audio encoder for `state`.
    ///
    /// Port of `GetAudioEncoder`. Prefers Apple `aac_at` then `libfdk_aac` (when
    /// [`EncoderCapabilities`] reports them) for AAC output, mapping the other
    /// codecs to their ffmpeg encoder names.
    #[must_use]
    pub fn audio_encoder(&self, state: &EncodingJobInfo) -> String {
        let raw = state.output_audio_codec.as_deref().unwrap_or_default();
        let codec = if is_valid_container(raw) { raw } else { "aac" };

        if codec.eq_ignore_ascii_case("aac") {
            if self.capabilities.supports_encoder("aac_at") {
                return "aac_at".to_owned();
            }
            if self.capabilities.supports_encoder("libfdk_aac") {
                return "libfdk_aac".to_owned();
            }
            return "aac".to_owned();
        }

        match codec.to_ascii_lowercase().as_str() {
            "mp3" => "libmp3lame".to_owned(),
            "vorbis" => "libvorbis".to_owned(),
            "opus" => "libopus".to_owned(),
            "flac" => "flac".to_owned(),
            "dts" => "dca".to_owned(),
            "alac" => "alac".to_owned(),
            other => other.to_owned(),
        }
    }

    // ----- stream mapping ----------------------------------------------------

    /// Builds the ffmpeg `-map` arguments for `state`.
    ///
    /// Port of `GetMapArgs`: maps the selected video/audio/subtitle streams (or
    /// negatively maps missing tracks), honouring external streams and the
    /// subtitle delivery method.
    #[must_use]
    pub fn map_args(&self, state: &EncodingJobInfo) -> String {
        if state.video_stream.is_none() && state.audio_stream.is_none() {
            return if state.is_input_video {
                "-sn".to_owned()
            } else {
                String::new()
            };
        }

        if state.video_stream.as_ref().is_some_and(|s| s.index == -1) {
            return "-sn".to_owned();
        }

        if state.audio_stream.as_ref().is_some_and(|s| s.index == -1) {
            return if state.is_input_video {
                "-sn".to_owned()
            } else {
                String::new()
            };
        }

        let mut args = String::new();

        if let Some(video) = state.video_stream.as_ref() {
            if burns_graphical_subtitle(state) {
                // The overlay filter produces a `[v]` label; map that, not the raw
                // input video, so the burned-in subtitle reaches the output.
                args.push_str("-map [v]");
            } else {
                let idx = find_index(&state.media_source.media_streams, video);
                let _ = write!(args, "-map 0:{idx}");
            }
        } else {
            args.push_str("-vn");
        }

        if let Some(audio) = state.audio_stream.as_ref() {
            let idx = find_index(&state.media_source.media_streams, audio);
            if audio.is_external {
                let external_audio_map_index = if needs_external_subtitle_muxing(state) {
                    2
                } else {
                    1
                };
                let _ = write!(args, " -map {external_audio_map_index}:{idx}");
            } else {
                let _ = write!(args, " -map 0:{idx}");
            }
        } else {
            args.push_str(" -map -0:a");
        }

        args.push_str(&subtitle_map_args(state));
        args
    }

    // ----- bitrate / quality / thread params ---------------------------------

    /// Builds the software `-maxrate`/`-bufsize`/`-b:v` bitrate argument for
    /// `video_codec`.
    ///
    /// Port of the **software encoders' branches** of `GetVideoBitrateParam`
    /// (`libx264`/`libx265` → `-maxrate/-bufsize`, `libsvtav1` → `-b:v/-bufsize`).
    /// Hardware-encoder branches are deferred. Returns empty when no output
    /// bitrate is set.
    #[must_use]
    pub fn video_bitrate_param(&self, state: &EncodingJobInfo, video_codec: &str) -> String {
        let Some(bitrate) = state.output_video_bitrate else {
            return String::new();
        };

        // Use i64 arithmetic then clamp to i32, matching the C# overflow guard.
        let bufsize =
            i32::try_from(min(i64::from(bitrate) * 2, i64::from(i32::MAX))).unwrap_or(i32::MAX);

        if video_codec.eq_ignore_ascii_case("libsvtav1") {
            return format!(" -b:v {bitrate} -bufsize {bufsize}");
        }

        if video_codec.eq_ignore_ascii_case("libx264")
            || video_codec.eq_ignore_ascii_case("libx265")
        {
            return format!(" -maxrate {bitrate} -bufsize {bufsize}");
        }

        String::new()
    }

    /// Computes the target video bitrate value.
    ///
    /// Port of `GetVideoBitrateParamValue`: caps a requested bitrate to the
    /// source bitrate unless upscaling, applies the codec-efficiency scale
    /// factor, and clamps to [`MAX_SANE_BITRATE`].
    #[must_use]
    pub fn video_bitrate_param_value(
        &self,
        request: &BaseEncodingJobOptions,
        video_stream: Option<&MediaStream>,
        output_video_codec: &str,
    ) -> i32 {
        let mut bitrate = request.video_bit_rate;

        if let Some(stream) = video_stream {
            let is_upscaling = matches!((request.height, stream.height), (Some(rh), Some(sh)) if rh > sh)
                && matches!((request.width, stream.width), (Some(rw), Some(sw)) if rw > sw);

            if !is_upscaling && let (Some(b), Some(sb)) = (bitrate, stream.bit_rate) {
                bitrate = Some(get_min_bitrate(sb, b));
            }

            if let Some(b) = bitrate {
                let input_codec = stream.codec.as_deref().unwrap_or_default();
                let mut scaled = scale_bitrate(b, input_codec, output_video_codec);
                if let Some(req) = request.video_bit_rate {
                    scaled = min(scaled, req);
                }
                bitrate = Some(scaled);
            }
        }

        min(bitrate.unwrap_or(0), MAX_SANE_BITRATE)
    }

    /// Computes the target audio bitrate.
    ///
    /// Port of `GetAudioBitrateParam(audioBitRate, audioCodec, audioStream,
    /// outputAudioChannels)`.
    #[must_use]
    pub fn audio_bitrate_param(
        &self,
        audio_bit_rate: Option<i32>,
        audio_codec: Option<&str>,
        audio_stream: Option<&MediaStream>,
        output_audio_channels: Option<i32>,
    ) -> Option<i32> {
        let stream = audio_stream?;

        let input_channels = stream.channels.unwrap_or(0);
        let output_channels = output_audio_channels.unwrap_or(0);
        let bitrate = audio_bit_rate.unwrap_or(i32::MAX);
        let codec = audio_codec.unwrap_or_default();

        let is_lossy_family = codec.is_empty()
            || ["aac", "mp3", "opus", "vorbis", "ac3", "eac3"]
                .iter()
                .any(|c| codec.eq_ignore_ascii_case(c));

        if is_lossy_family {
            return Some(match (input_channels, output_channels) {
                (i, o) if i >= 6 && (o >= 6 || o == 0) => min(640_000, bitrate),
                (i, o) if i > 0 && o > 0 => min(o * 128_000, bitrate),
                (i, _) if i > 0 => min(input_channels * 128_000, bitrate),
                _ => min(384_000, bitrate),
            });
        }

        if codec.eq_ignore_ascii_case("dts") || codec.eq_ignore_ascii_case("dca") {
            return Some(match (input_channels, output_channels) {
                (i, o) if i >= 6 && (o >= 6 || o == 0) => min(768_000, bitrate),
                (i, o) if i > 0 && o > 0 => min(o * 136_000, bitrate),
                (i, _) if i > 0 => min(input_channels * 136_000, bitrate),
                _ => min(672_000, bitrate),
            });
        }

        // Default: 128K per channel.
        Some(128_000 * output_audio_channels.or(stream.channels).unwrap_or(2))
    }

    /// Computes the ffmpeg `-threads` value.
    ///
    /// Port of `GetNumberOfThreads`: a per-request `CpuCoreLimit` (else the
    /// configured `EncodingThreadCount`) clamped to the host processor count;
    /// `<= 0` means "let ffmpeg decide" (`0`).
    #[must_use]
    pub fn number_of_threads(
        &self,
        state: Option<&EncodingJobInfo>,
        encoding_options: &EncodingOptions,
    ) -> i32 {
        let threads = state
            .and_then(|s| s.base_request.cpu_core_limit)
            .unwrap_or(encoding_options.encoding_thread_count);

        if threads <= 0 {
            return 0;
        }

        min(threads, self.processor_count)
    }

    // ----- quality param (software path) -------------------------------------

    /// Builds the software `-preset`/`-crf`/`-profile`/`-level`/codec-opts
    /// quality argument for `video_encoder`.
    ///
    /// Port of the **software slice** of `GetVideoQualityParam`. The hardware
    /// low-power/i915-workaround preamble is deferred (it is inert when the
    /// hardware-acceleration type is `none`, i.e. the software path); the
    /// libx264/libx265/libsvtav1 preset, CRF, profile, level, and codec-specific
    /// option strings are ported verbatim.
    #[must_use]
    pub fn video_quality_param(
        &self,
        state: &EncodingJobInfo,
        video_encoder: &str,
        encoding_options: &EncodingOptions,
        default_preset: EncoderPreset,
    ) -> String {
        let mut param = String::new();

        let is_libx265 = video_encoder.eq_ignore_ascii_case("libx265");
        param.push_str(&encoder_param(
            encoding_options.encoder_preset,
            default_preset,
            encoding_options,
            video_encoder,
            is_libx265,
        ));
        param.push_str(&self.video_bitrate_param(state, video_encoder));

        if let Some(framerate) = self.framerate_param(state) {
            let _ = write!(param, " -r {framerate}");
        }

        // Normalise the target codec name (h265/hevc collapse to hevc).
        let actual = state.actual_output_video_codec().unwrap_or_default();
        let target_video_codec =
            if actual.eq_ignore_ascii_case("h265") || actual.eq_ignore_ascii_case("hevc") {
                "hevc".to_owned()
            } else {
                actual.to_owned()
            };

        let profile = normalize_output_profile(state, &target_video_codec, video_encoder);
        if !profile.is_empty() {
            let _ = write!(param, " -profile:v:0 {profile}");
        }

        param.push_str(&codec_specific_quality_args(
            state,
            video_encoder,
            &target_video_codec,
            encoding_options,
        ));

        param
    }

    /// The framerate `-r` value, if the request constrains it.
    ///
    /// Port of `GetFramerateParam`.
    #[must_use]
    pub fn framerate_param(&self, state: &EncodingJobInfo) -> Option<f32> {
        let request = &state.base_request;
        if let Some(framerate) = request.framerate {
            return Some(framerate);
        }

        let maxrate = request.max_framerate?;
        let content_rate = state.video_stream.as_ref()?.reference_frame_rate()?;
        (content_rate > maxrate).then_some(maxrate)
    }

    // ----- direct-play decision ----------------------------------------------

    /// Decides whether `video_stream` can be stream-copied (direct-play video).
    ///
    /// Port of `CanStreamCopyVideo`. The DOVI dynamic-metadata-removal branch
    /// (which consults a deferred hardware encoder capability) is handled
    /// conservatively: if a static-HDR stream is not directly range-compatible
    /// the copy is refused (matching the C# refuse-when-uncertain intent).
    #[must_use]
    pub fn can_stream_copy_video(
        &self,
        state: &EncodingJobInfo,
        video_stream: &MediaStream,
    ) -> bool {
        let request = &state.base_request;

        if !request.allow_video_stream_copy {
            return false;
        }

        let codec = video_stream.codec.as_deref().unwrap_or_default();

        if video_stream.is_interlaced && state.deinterlace(Some(codec), false) {
            return false;
        }

        if video_stream.is_anamorphic.unwrap_or(false) && request.require_non_anamorphic {
            return false;
        }

        // Can't stream copy if we're burning in subtitles.
        if request.subtitle_stream_index.is_some_and(|i| i >= 0)
            && state.subtitle_delivery_method == SubtitleDeliveryMethod::Encode
        {
            return false;
        }

        if codec.eq_ignore_ascii_case("h264")
            && video_stream.is_avc == Some(false)
            && request.require_avc
        {
            return false;
        }

        // Source and target codecs must match.
        if codec.is_empty()
            || (!state.supported_video_codecs.is_empty()
                && !state
                    .supported_video_codecs
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(codec)))
        {
            return false;
        }

        if !profile_range_rotation_copy_ok(state, video_stream, codec) {
            return false;
        }

        if let Some(max_width) = request.max_width
            && video_stream.width.is_none_or(|w| w > max_width)
        {
            return false;
        }

        if let Some(max_height) = request.max_height
            && video_stream.height.is_none_or(|h| h > max_height)
        {
            return false;
        }

        let requested_framerate = request.max_framerate.or(request.framerate);
        if let Some(req_fps) = requested_framerate {
            // 0.05 fps tolerance — some files record slightly above the intended rate.
            match video_stream.reference_frame_rate() {
                Some(fps) if fps <= req_fps + 0.05 => {}
                _ => return false,
            }
        }

        if let Some(req_bitrate) = request.video_bit_rate
            && video_stream.bit_rate.is_none_or(|b| b > req_bitrate)
        {
            // For LiveTV with no bitrate, try copy if other conditions are met.
            let is_live_without_bitrate = !request
                .live_stream_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
                && video_stream.bit_rate.is_none();
            if !is_live_without_bitrate {
                return false;
            }
        }

        if let Some(max_bit_depth) = state.requested_video_bit_depth(codec)
            && video_stream.bit_depth.is_some_and(|b| b > max_bit_depth)
        {
            return false;
        }

        if let Some(max_ref) = state.requested_max_ref_frames(codec)
            && video_stream.ref_frames.is_some_and(|r| r > max_ref)
        {
            return false;
        }

        if let Some(request_level) = state
            .requested_level(codec)
            .and_then(|l| l.parse::<f64>().ok())
            && video_stream.level.is_some_and(|l| l > request_level)
        {
            return false;
        }

        if state
            .input_container
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("avi"))
            && codec.eq_ignore_ascii_case("h264")
            && !video_stream.is_avc.unwrap_or(false)
        {
            return false;
        }

        true
    }

    /// The video-range portion of the copy decision.
    /// Decides whether `audio_stream` can be stream-copied, and reports the
    /// incompatibilities that would force a re-encode.
    ///
    /// Port of `CanStreamCopyAudio(..., out failureReasons)`.
    #[must_use]
    pub fn can_stream_copy_audio(
        &self,
        state: &EncodingJobInfo,
        audio_stream: &MediaStream,
        supported_audio_codecs: &[String],
    ) -> (bool, TranscodeReasons) {
        let request = &state.base_request;
        let failure_reasons =
            audio_stream_copy_failure_reasons(state, audio_stream, supported_audio_codecs);

        let ok = request.allow_audio_stream_copy
            && request.enable_auto_stream_copy
            && failure_reasons.is_empty();

        (ok, failure_reasons)
    }
}

/// Increases a low source bitrate before capping to the request. Port of
/// `GetMinBitrate`.
fn get_min_bitrate(source_bitrate: i32, requested_bitrate: i32) -> i32 {
    // Testing-derived multipliers to improve low-bitrate streams.
    let source = if source_bitrate <= 2_000_000 {
        // 2.5x, matching C# Convert.ToInt32(source * 2.5).
        f64_to_i32_round(f64::from(source_bitrate) * 2.5)
    } else if source_bitrate <= 3_000_000 {
        source_bitrate.saturating_mul(2)
    } else {
        source_bitrate
    };

    min(source, requested_bitrate)
}

/// The codec-efficiency scale factor. Port of `GetVideoBitrateScaleFactor`.
fn video_bitrate_scale_factor(codec: &str) -> f64 {
    if codec.eq_ignore_ascii_case("h265")
        || codec.eq_ignore_ascii_case("hevc")
        || codec.eq_ignore_ascii_case("vp9")
    {
        return 0.6;
    }
    if codec.eq_ignore_ascii_case("av1") {
        return 0.5;
    }
    1.0
}

/// Scales a bitrate for a codec transition. Port of `ScaleBitrate` (public so
/// the planner can compute the h264-equivalent bitrate the resolution
/// normalizer keys on).
#[must_use]
pub fn scale_bitrate(bitrate: i32, input_video_codec: &str, output_video_codec: &str) -> i32 {
    let input_factor = video_bitrate_scale_factor(input_video_codec);
    let output_factor = video_bitrate_scale_factor(output_video_codec);

    // Never scale the real bitrate below the requested bitrate.
    let mut scale_factor = f64::max(output_factor / input_factor, 1.0);

    if bitrate <= 500_000 {
        scale_factor = f64::max(scale_factor, 4.0);
    } else if bitrate <= 1_000_000 {
        scale_factor = f64::max(scale_factor, 3.0);
    } else if bitrate <= 2_000_000 {
        scale_factor = f64::max(scale_factor, 2.5);
    } else if bitrate <= 3_000_000 {
        scale_factor = f64::max(scale_factor, 2.0);
    } else if bitrate >= 30_000_000 {
        // Don't scale beyond 30 Mbps — hardly noticeable and would overload
        // clients/encoders for av1->h264.
        scale_factor = 1.0;
    }

    f64_to_i32_round(scale_factor * f64::from(bitrate))
}

/// Rounds to `i32` with .NET `Convert.ToInt32` semantics (round-half-to-even),
/// saturating out-of-range results.
fn f64_to_i32_round(value: f64) -> i32 {
    let rounded = value.round_ties_even();
    // The clamp bounds the value to the i32 range before the cast, so the cast
    // cannot truncate; `f64 as i32` also saturates and maps NaN to 0.
    #[allow(clippy::cast_possible_truncation)]
    {
        rounded.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

/// Resolves the `-profile:v:0` value for the software path.
///
/// Port of the profile block of `GetVideoQualityParam`: pick the requested
/// profile, validate it against the target codec's known profiles, then apply
/// the codec/encoder-specific coercions (Main10→Main, Extended→Main, etc.).
fn normalize_output_profile(
    state: &EncodingJobInfo,
    target_video_codec: &str,
    video_encoder: &str,
) -> String {
    let mut profile = state
        .requested_profiles(target_video_codec)
        .into_iter()
        .next()
        .unwrap_or_default();
    // Strip whitespace (WhiteSpaceRegex `\s+`) then lowercase.
    profile.retain(|c| !c.is_whitespace());
    profile.make_ascii_lowercase();

    let video_profiles: &[&str] = if target_video_codec.eq_ignore_ascii_case("h264") {
        &VIDEO_PROFILES_H264
    } else if target_video_codec.eq_ignore_ascii_case("hevc") {
        &VIDEO_PROFILES_H265
    } else if target_video_codec.eq_ignore_ascii_case("av1") {
        &VIDEO_PROFILES_AV1
    } else {
        &[]
    };

    if !video_profiles
        .iter()
        .any(|p| p.eq_ignore_ascii_case(&profile))
    {
        profile.clear();
    }

    // We only transcode to HEVC 8-bit for now — force Main.
    if profile.contains("main10") || profile.contains("mainstill") {
        "main".clone_into(&mut profile);
    }
    // Extended is unsupported by known H.264 encoders — force Main.
    if profile.contains("extended") {
        "main".clone_into(&mut profile);
    }
    // Only libx264 supports H.264 High 10 — otherwise force High.
    if !video_encoder.eq_ignore_ascii_case("libx264") && profile.contains("high10") {
        "high".clone_into(&mut profile);
    }
    // AV1 encoders only need Main.
    if video_encoder.to_ascii_lowercase().contains("av1")
        && (profile.contains("high") || profile.contains("professional"))
    {
        "main".clone_into(&mut profile);
    }
    // libx264 does not support Constrained Baseline — force Baseline.
    if video_encoder.eq_ignore_ascii_case("libx264") && profile.contains("baseline") {
        "baseline".clone_into(&mut profile);
    }
    // libx264 does not support Constrained High — force High.
    if video_encoder.eq_ignore_ascii_case("libx264") && profile.contains("high") {
        "high".clone_into(&mut profile);
    }

    profile
}

/// The `-level` and codec-specific option strings for the software path. Port of
/// the level + `-x264opts`/`-x265-params` tail of `GetVideoQualityParam`; split
/// out so `video_quality_param` stays within a readable length.
fn codec_specific_quality_args(
    state: &EncodingJobInfo,
    video_encoder: &str,
    target_video_codec: &str,
    encoding_options: &EncodingOptions,
) -> String {
    let mut param = String::new();

    if let Some(level) =
        normalize_transcoding_level(state, state.requested_level(target_video_codec).as_deref())
    {
        // libx264 accepts an adjustable -level, as do libsvtav1/other software
        // encoders (bare -level). libx265 takes its level via -x265-params only,
        // and the hardware-specific remaps are deferred.
        if !video_encoder.eq_ignore_ascii_case("libx265") {
            let _ = write!(param, " -level {level}");
        }
    }

    if video_encoder.eq_ignore_ascii_case("libx264") {
        param.push_str(" -x264opts:0 subme=0:me_range=16:rc_lookahead=10:me=hex:open_gop=0");
    }

    if video_encoder.eq_ignore_ascii_case("libx265") {
        param.push_str(" -x265-params:0 no-scenecut=1:no-open-gop=1:no-info=1");
        if preset_ordinal(encoding_options.encoder_preset)
            < preset_ordinal(EncoderPreset::ultrafast)
        {
            param.push_str(":subme=3:merange=25:rc-lookahead=10:me=star:ctu=32:max-tu-size=32:min-cu-size=16:rskip=2:rskip-edge-threshold=2:no-sao=1:no-strong-intra-smoothing=1");
        }
    }

    // libsvtav1 svtav1-params depends on an ffmpeg-version gate that is part of
    // the deferred encoder-capability plumbing — omitted on the software path.

    param
}

/// Builds the `-preset`/`-crf` argument for software encoders. Port of the
/// software branches of `GetEncoderParam` (libx264/libx265/libsvtav1).
fn encoder_param(
    preset: EncoderPreset,
    default_preset: EncoderPreset,
    encoding_options: &EncodingOptions,
    video_encoder: &str,
    is_libx265: bool,
) -> String {
    let mut param = String::new();
    // C# uses `preset ?? defaultPreset`; the Rust preset is non-optional, so an
    // explicit `auto` request defers to the encoder-specific default below.
    let encoder_preset = if preset == EncoderPreset::auto {
        default_preset
    } else {
        preset
    };

    if video_encoder.eq_ignore_ascii_case("libx264") || is_libx265 {
        // An `auto` preset that survived the remap (default was also `auto`)
        // becomes `veryfast`, matching the C# `EncoderPreset.auto` arm.
        let preset_string = if encoder_preset == EncoderPreset::auto {
            preset_name(EncoderPreset::veryfast)
        } else {
            preset_name(encoder_preset)
        };
        let _ = write!(param, " -preset {preset_string}");

        let encode_crf = if is_libx265 {
            encoding_options.h265_crf
        } else {
            encoding_options.h264_crf
        };

        if (0..=51).contains(&encode_crf) {
            let _ = write!(param, " -crf {encode_crf}");
        } else {
            let default_crf = if is_libx265 { "28" } else { "23" };
            let _ = write!(param, " -crf {default_crf}");
        }
    } else if video_encoder.eq_ignore_ascii_case("libsvtav1") {
        // Recommended preset 10; presets < 5 are too slow for on-the-fly encode.
        // Arms kept verbatim from the C# switch (auto/placebo/faster all map to
        // preset 10) even though some bodies coincide.
        #[allow(clippy::match_same_arms)]
        let preset = match encoder_preset {
            EncoderPreset::veryslow => " -preset 5",
            EncoderPreset::slower => " -preset 6",
            EncoderPreset::slow => " -preset 7",
            EncoderPreset::medium => " -preset 8",
            EncoderPreset::fast => " -preset 9",
            EncoderPreset::faster => " -preset 10",
            EncoderPreset::veryfast => " -preset 11",
            EncoderPreset::superfast => " -preset 12",
            EncoderPreset::ultrafast => " -preset 13",
            _ => " -preset 10",
        };
        param.push_str(preset);
    }
    // Hardware encoder branches (vaapi/qsv/nvenc/amf/videotoolbox) are deferred.

    param
}

/// The integer ordinal of a preset (its C# enum value).
///
/// [`EncoderPreset`] does not derive `Ord`, so speed comparisons (`preset <
/// ultrafast`) that the C# does on the backing `int` go through the discriminant
/// mirrored from `MediaBrowser.Model.Entities.EncoderPreset`.
fn preset_ordinal(preset: EncoderPreset) -> u8 {
    match preset {
        EncoderPreset::auto => 0,
        EncoderPreset::placebo => 1,
        EncoderPreset::veryslow => 2,
        EncoderPreset::slower => 3,
        EncoderPreset::slow => 4,
        EncoderPreset::medium => 5,
        EncoderPreset::fast => 6,
        EncoderPreset::faster => 7,
        EncoderPreset::veryfast => 8,
        EncoderPreset::superfast => 9,
        EncoderPreset::ultrafast => 10,
    }
}

/// The ffmpeg preset token for a preset. Port of
/// `EncoderPreset.ToString().ToLowerInvariant()`.
fn preset_name(preset: EncoderPreset) -> &'static str {
    match preset {
        EncoderPreset::auto => "auto",
        EncoderPreset::placebo => "placebo",
        EncoderPreset::veryslow => "veryslow",
        EncoderPreset::slower => "slower",
        EncoderPreset::slow => "slow",
        EncoderPreset::medium => "medium",
        EncoderPreset::fast => "fast",
        EncoderPreset::faster => "faster",
        EncoderPreset::veryfast => "veryfast",
        EncoderPreset::superfast => "superfast",
        EncoderPreset::ultrafast => "ultrafast",
    }
}

/// Clamps a requested level to a codec-safe maximum. Port of
/// `NormalizeTranscodingLevel`. Returns `None` for a non-numeric level.
fn normalize_transcoding_level(state: &EncodingJobInfo, level: Option<&str>) -> Option<String> {
    let request_level = level?.parse::<f64>().ok()?;
    let codec = state.actual_output_video_codec().unwrap_or_default();

    if codec.eq_ignore_ascii_case("av1") {
        // Cap to AV1 level 5.3 (15) for compatibility.
        if !(0.0..15.0).contains(&request_level) {
            return Some("15".to_owned());
        }
    } else if codec.eq_ignore_ascii_case("hevc") || codec.eq_ignore_ascii_case("h265") {
        // Cap to HEVC level 5.0 (150).
        if !(0.0..150.0).contains(&request_level) {
            return Some("150".to_owned());
        }
    } else if codec.eq_ignore_ascii_case("h264") {
        // Cap to H.264 level 5.1 (51).
        if !(0.0..51.0).contains(&request_level) {
            return Some("51".to_owned());
        }
    }

    level.map(str::to_owned)
}

/// The subtitle portion of [`EncodingHelper::map_args`]. Port of the
/// subtitle-delivery-method branch of `GetMapArgs`; split out so `map_args`
/// stays within a readable length.
fn subtitle_map_args(state: &EncodingJobInfo) -> String {
    let method = state.subtitle_delivery_method;
    let Some(sub) = state.subtitle_stream.as_ref() else {
        return " -map -0:s".to_owned();
    };

    if method == SubtitleDeliveryMethod::Hls {
        return " -map -0:s".to_owned();
    }

    if method == SubtitleDeliveryMethod::Embed {
        if sub.is_external {
            // External subtitle is FFmpeg input 1. For single-stream files the
            // in-file index is 0; for multi-stream containers count the same-file
            // streams preceding the selected one.
            let in_file_index = state
                .media_source
                .media_streams
                .iter()
                .filter(|s| s.path == sub.path)
                .take_while(|s| s.index != sub.index)
                .count();
            return format!(" -map 1:{in_file_index}");
        }
        let idx = find_index(&state.media_source.media_streams, sub);
        return format!(" -map 0:{idx}");
    }

    if sub.is_external && !is_text_subtitle_stream(sub) {
        let idx = find_index(&state.media_source.media_streams, sub);
        return format!(" -map 1:{idx} -sn");
    }

    String::new()
}

/// The 0-based FFmpeg stream index of `stream_to_find` among same-path streams.
///
/// Port of `EncodingHelper.FindIndex`.
fn find_index(media_streams: &[MediaStream], stream_to_find: &MediaStream) -> i32 {
    let mut index = 0i32;
    for current in media_streams {
        if current == stream_to_find {
            return index;
        }
        if current.path == stream_to_find.path {
            index += 1;
        }
    }
    -1
}

/// Whether the encoded output would embed a burnt-in subtitle. Port of
/// `ShouldEncodeSubtitle`.
fn should_encode_subtitle(state: &EncodingJobInfo) -> bool {
    state.subtitle_delivery_method == SubtitleDeliveryMethod::Encode
        || (state.base_request.always_burn_in_subtitle_when_transcoding
            && !EncodingJobInfo::is_copy_codec(state.output_video_codec.as_deref()))
}

/// Whether the transcode burns a **graphical** (image-based, e.g. DVDSUB/PGS)
/// subtitle into the video. Those must be composited with an `overlay` filter
/// (producing a `[v]` label the muxer maps), unlike text subtitles which are
/// delivered externally. Shared by [`map_args`](EncodingHelper::map_args) and the
/// segment planner so the filter, the `-map [v]`, and the decode path agree.
#[must_use]
pub fn burns_graphical_subtitle(state: &EncodingJobInfo) -> bool {
    should_encode_subtitle(state)
        && state.video_stream.is_some()
        && state
            .subtitle_stream
            .as_ref()
            .is_some_and(|s| !is_text_subtitle_stream(s))
}

/// The software HDR→SDR tonemap filter chain, ending in 8-bit `yuv420p`.
///
/// The vanilla-ffmpeg (zimg) equivalent of the software half of upstream
/// `GetHwTonemapParam`/`tonemapx`: linearize at 100-nit nominal peak, convert
/// to BT.709 primaries, tone-map with the Hable operator (no desaturation, the
/// upstream default look), then re-encode transfer/matrix/range for SDR and
/// hand the encoder 8-bit 4:2:0. Requires an ffmpeg built with `--enable-libzimg`
/// (every mainstream distro/jellyfin build).
pub const SOFTWARE_TONEMAP_FILTER: &str = "zscale=t=linear:npl=100,format=gbrpf32le,\
                                           zscale=p=bt709,tonemap=tonemap=hable:desat=0,\
                                           zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

/// The `tonemapx` software HDR→SDR tonemap filter (jellyfin-ffmpeg only).
///
/// Port of the software-tonemap branch of upstream
/// `GetVideoProcessingFilterParam` with the default `EncodingOptions`
/// (algorithm `bt2390`, desat `0`, peak `100`, param unset, range `auto`):
/// one SIMD pass straight to 8-bit `yuv420p`. Several times faster than the
/// [`SOFTWARE_TONEMAP_FILTER`] zscale chain on a 4K HDR encode — it is what
/// puts Jellyfin ahead on time-to-first-segment — but the filter only exists
/// in jellyfin-ffmpeg builds, so callers must gate on the probed filter list.
pub const SOFTWARE_TONEMAPX_FILTER: &str =
    "tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p";

/// The `setparams` filter tagging input frames with their HDR colour metadata,
/// emitted ahead of a `tonemapx` so untagged streams still tonemap correctly.
///
/// Port of `GetInputHdrParam`: an HLG source (`arib-std-b67` transfer) keeps
/// its transfer, everything else is tagged HDR10 (`smpte2084`).
#[must_use]
pub fn input_hdr_setparams(color_transfer: Option<&str>) -> &'static str {
    if color_transfer.is_some_and(|t| t.eq_ignore_ascii_case("arib-std-b67")) {
        // HLG
        "setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc"
    } else {
        // HDR10
        "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc"
    }
}

/// Whether a re-encoded video stream needs the HDR→SDR tonemap chain.
///
/// True when the source stream carries an HDR transfer (HDR10/HLG/Dolby
/// Vision, via [`MediaStream::video_range`]) — the software encode targets are
/// SDR (`libx264`), so an untonemapped encode renders washed out. Callers only
/// consult this on the re-encode path (a stream copy passes HDR through).
#[must_use]
pub fn requires_software_tonemap(video_stream: Option<&MediaStream>) -> bool {
    video_stream.is_some_and(|s| s.video_range() == hermit_model::data::VideoRange::Hdr)
}

/// The `-af` audio filter value for a re-encoded audio stream, if any.
///
/// Port of `GetAudioFilterParam`'s downmix branch: when downmixing a
/// more-than-stereo source to 2 channels, boost the volume by the configured
/// `DownMixAudioBoost` (default `2` — a plain downmix roughly halves perceived
/// loudness, so Jellyfin doubles it back). The `DownMixStereoAlgorithm` filter
/// table is not consulted: the default algorithm `None` maps to no filter
/// string upstream, and Hermit doesn't yet expose the setting. The `asetpts`
/// branch is skipped — it only fires when timestamps are *not* copied, and
/// segmented (HLS) transcodes always copy them.
#[must_use]
pub fn audio_filter_param(
    state: &EncodingJobInfo,
    encoding_options: &EncodingOptions,
) -> Option<String> {
    let downmixing_to_stereo = state.output_audio_channels == Some(2)
        && state
            .audio_stream
            .as_ref()
            .is_some_and(|s| s.channels.is_some_and(|c| c > 2));
    if downmixing_to_stereo && (encoding_options.down_mix_audio_boost - 1.0).abs() > f64::EPSILON {
        return Some(format!("volume={}", encoding_options.down_mix_audio_boost));
    }
    None
}

/// Whether a re-encoded video stream needs an explicit 8-bit down-convert.
///
/// `libx264` fed 10-bit input silently produces High10-profile H.264, which
/// browser MSE pipelines refuse to decode; SDR 10-bit sources (and any source
/// whose bit depth is above 8) must be forced to `yuv420p`. HDR sources are
/// excluded — the tonemap chain already ends in `yuv420p`.
#[must_use]
pub fn requires_8bit_downconvert(video_stream: Option<&MediaStream>) -> bool {
    !requires_software_tonemap(video_stream)
        && video_stream.is_some_and(|s| s.bit_depth.is_some_and(|d| d > 8))
}

/// Computes the even-dimension output size for a bounded downscale.
///
/// Port of the known-dimensions branch of `GetSizeParam`: when the source
/// dimensions are known and exceed a `max_width`/`max_height` bound, scale
/// both down by the same ratio (aspect preserved) and round to even (encoder
/// requirement). Returns `None` when no scaling is needed or the source
/// dimensions are unknown.
#[must_use]
pub fn output_size(
    video_stream: Option<&MediaStream>,
    max_width: Option<i32>,
    max_height: Option<i32>,
) -> Option<(i32, i32)> {
    let stream = video_stream?;
    let (w, h) = match (stream.width, stream.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => return None,
    };
    let mut ratio = 1.0f64;
    if let Some(mw) = max_width.filter(|&mw| mw > 0 && mw < w) {
        ratio = ratio.min(f64::from(mw) / f64::from(w));
    }
    if let Some(mh) = max_height.filter(|&mh| mh > 0 && mh < h) {
        ratio = ratio.min(f64::from(mh) / f64::from(h));
    }
    if ratio >= 1.0 {
        return None;
    }
    // Round to even, minimum 2 (encoders reject odd/zero dimensions).
    #[allow(clippy::cast_possible_truncation)]
    let even = |v: f64| -> i32 { (((v / 2.0).round() as i32) * 2).max(2) };
    Some((even(f64::from(w) * ratio), even(f64::from(h) * ratio)))
}

/// Builds the `scale=` filter for a bounded downscale, if one is needed.
///
/// Known source dimensions produce a concrete `scale=W:H`; unknown dimensions
/// with a cap fall back to the upstream expression forms (`GetSizeParam`) so
/// ffmpeg bounds at decode time. `None` when no cap applies.
#[must_use]
pub fn scale_filter(
    video_stream: Option<&MediaStream>,
    max_width: Option<i32>,
    max_height: Option<i32>,
) -> Option<String> {
    if let Some((w, h)) = output_size(video_stream, max_width, max_height) {
        return Some(format!("scale={w}:{h}"));
    }
    // Source dimensions known and within bounds → no filter.
    if video_stream
        .is_some_and(|s| matches!((s.width, s.height), (Some(w), Some(h)) if w > 0 && h > 0))
    {
        return None;
    }
    // Unknown dimensions: bound with the upstream expression forms. `\,` keeps
    // the commas inside the function arguments from splitting the filter graph.
    let even = |v: i32| (v / 2) * 2;
    match (max_width.filter(|&v| v > 0), max_height.filter(|&v| v > 0)) {
        (Some(mw), Some(mh)) => Some(format!(
            "scale=trunc(min(max(iw\\,ih*a)\\,{mw})/2)*2:trunc(min(max(iw/a\\,ih)\\,{mh})/2)*2"
        )),
        (Some(mw), None) => Some(format!("scale={}:trunc(ow/a/2)*2", even(mw))),
        (None, Some(mh)) => Some(format!("scale=trunc(oh*a/2)*2:{}", even(mh))),
        (None, None) => None,
    }
}

/// Whether an external subtitle must be muxed as a second FFmpeg input. Port of
/// `NeedsExternalSubtitleMuxing`.
fn needs_external_subtitle_muxing(state: &EncodingJobInfo) -> bool {
    state.subtitle_stream.as_ref().is_some_and(|sub| {
        sub.is_external
            && (state.subtitle_delivery_method == SubtitleDeliveryMethod::Embed
                || (should_encode_subtitle(state) && !is_text_subtitle_stream(sub)))
    })
}

/// Whether a subtitle stream is a text (rather than graphical) subtitle. Port of
/// `MediaStream.IsTextSubtitleStream`: text subtitles have a codec ffmpeg can
/// render to text (srt/ass/ssa/subrip/…) rather than an image (pgs/dvbsub/…).
fn is_text_subtitle_stream(stream: &MediaStream) -> bool {
    let codec = stream.codec.as_deref().unwrap_or_default();
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "ass" | "ssa" | "srt" | "subrip" | "microdvd" | "mov_text" | "text" | "webvtt" | "vtt"
    )
}

/// The score (ascending-quality index) of a profile for a codec. Port of
/// `GetVideoProfileScore`; `-1` for an unknown profile.
fn video_profile_score(codec: &str, profile: &str) -> i32 {
    let table: &[&str] = if codec.eq_ignore_ascii_case("h264") {
        &VIDEO_PROFILES_H264
    } else if codec.eq_ignore_ascii_case("hevc") || codec.eq_ignore_ascii_case("h265") {
        &VIDEO_PROFILES_H265
    } else if codec.eq_ignore_ascii_case("av1") {
        &VIDEO_PROFILES_AV1
    } else {
        &[]
    };

    table
        .iter()
        .position(|p| p.eq_ignore_ascii_case(profile))
        .map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
}

/// The profile/range/rotation portion of the copy decision.
///
/// Port of the profile-score, range-type, and rotation blocks of
/// `CanStreamCopyVideo`; split out so `can_stream_copy_video` stays readable.
/// Returns `false` if any of the three would force a re-encode.
fn profile_range_rotation_copy_ok(
    state: &EncodingJobInfo,
    video_stream: &MediaStream,
    codec: &str,
) -> bool {
    let requested_profiles = state.requested_profiles(codec);
    if let Some(requested_profile) = requested_profiles.first()
        && let Some(stream_profile) = video_stream.profile.as_deref()
    {
        let stripped: String = stream_profile.chars().filter(|c| *c != ' ').collect();
        if !requested_profiles
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&stripped))
        {
            let current_score = video_profile_score(codec, stream_profile);
            let requested_score = video_profile_score(codec, requested_profile);
            if current_score == -1 || current_score > requested_score {
                return false;
            }
        }
    }

    let requested_range_types = state.requested_range_types(codec);
    if !requested_range_types.is_empty()
        && !range_type_copy_ok(video_stream, &requested_range_types)
    {
        return false;
    }

    let requested_rotations = state.requested_rotations(codec);
    if !requested_rotations.is_empty() {
        let rotation = video_stream.rotation.unwrap_or(0);
        if rotation != 0
            && !requested_rotations
                .iter()
                .any(|r| r == &rotation.to_string())
        {
            return false;
        }
    }

    true
}

/// The video-range portion of the copy decision.
///
/// Port of the range-type block of `CanStreamCopyVideo`; the DOVI
/// dynamic-metadata-removal escape hatch (a deferred hardware helper) is
/// replaced by a conservative refusal, matching the C# refuse-when-uncertain
/// intent.
fn range_type_copy_ok(video_stream: &MediaStream, requested: &[String]) -> bool {
    let range = video_stream.video_range_type();
    if range == VideoRangeType::Unknown {
        return false;
    }

    let has = |name: &str| requested.iter().any(|r| r.eq_ignore_ascii_case(name));
    let request_hdr10 = has("HDR10");
    let request_sdr = has("SDR");
    let request_dovi = has("DOVI");

    // If SDR is the only supported range, refuse any HDR stream.
    if requested.len() == 1 && request_sdr && range != VideoRangeType::Sdr {
        return false;
    }

    // DOVI without fallback needs a DOVI-capable client.
    if !request_dovi && range == VideoRangeType::Dovi {
        return false;
    }

    let range_name = video_range_type_name(range);
    let directly_supported = requested.iter().any(|r| r.eq_ignore_ascii_case(range_name));

    // Copying a Dolby Vision stream to a client that only supports the fallback
    // range (e.g. a browser that lists HDR10 but not DOVI) requires stripping the
    // DV RPU so the base layer plays clean — a capability Hermit does not have.
    // So we do NOT treat DoviWith{HDR10,HLG,SDR} as copyable via the base-range
    // fallback; those transcode instead (matches the C# "remove metadata or
    // refuse" intent, minus the removal path). HDR10+ is safe to copy as HDR10 —
    // its dynamic metadata is SEI the decoder ignores, with a valid HDR10 base.
    let hdr10plus_fallback_ok = request_hdr10 && range == VideoRangeType::Hdr10Plus;

    directly_supported || hdr10plus_fallback_ok
}

/// The PascalCase wire name of a video range type. Port of
/// `VideoRangeType.ToString()`.
fn video_range_type_name(range: VideoRangeType) -> &'static str {
    match range {
        VideoRangeType::Unknown => "Unknown",
        VideoRangeType::Sdr => "SDR",
        VideoRangeType::Hdr10 => "HDR10",
        VideoRangeType::Hlg => "HLG",
        VideoRangeType::Dovi => "DOVI",
        VideoRangeType::DoviWithHdr10 => "DOVIWithHDR10",
        VideoRangeType::DoviWithHlg => "DOVIWithHLG",
        VideoRangeType::DoviWithSdr => "DOVIWithSDR",
        VideoRangeType::DoviWithEl => "DOVIWithEL",
        VideoRangeType::DoviWithHdr10Plus => "DOVIWithHDR10Plus",
        VideoRangeType::DoviWithElhdr10Plus => "DOVIWithELHDR10Plus",
        VideoRangeType::DoviInvalid => "DOVIInvalid",
        VideoRangeType::Hdr10Plus => "HDR10Plus",
    }
}

/// The codec/parameter incompatibilities that prevent an audio copy. Port of
/// `GetAudioStreamCopyFailureReasons`.
fn audio_stream_copy_failure_reasons(
    state: &EncodingJobInfo,
    audio_stream: &MediaStream,
    supported_audio_codecs: &[String],
) -> TranscodeReasons {
    let request = &state.base_request;
    let mut reasons = TranscodeReasons::empty();

    let codec = audio_stream.codec.as_deref().unwrap_or_default();

    if let Some(max_bit_depth) = state.requested_audio_bit_depth(codec)
        && audio_stream.bit_depth.is_some_and(|b| b > max_bit_depth)
    {
        reasons |= TranscodeReasons::AUDIO_BIT_DEPTH_NOT_SUPPORTED;
    }

    // Source and target codecs must match.
    if codec.is_empty()
        || !supported_audio_codecs
            .iter()
            .any(|c| c.eq_ignore_ascii_case(codec))
    {
        reasons |= TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED;
    }

    // Channels must fall within the requested value.
    if let Some(channels) = state.requested_audio_channels(codec)
        && audio_stream.channels.is_none_or(|c| c <= 0 || c > channels)
    {
        reasons |= TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED;
    }

    // Sample rate must fall within the requested value.
    if let Some(sample_rate) = request.audio_sample_rate
        && audio_stream
            .sample_rate
            .is_none_or(|s| s <= 0 || s > sample_rate)
    {
        reasons |= TranscodeReasons::AUDIO_SAMPLE_RATE_NOT_SUPPORTED;
    }

    // Audio bitrate must fall within the requested value.
    if let Some(bit_rate) = request.audio_bit_rate
        && audio_stream.bit_rate.is_some_and(|b| b > bit_rate)
    {
        reasons |= TranscodeReasons::AUDIO_BITRATE_NOT_SUPPORTED;
    }

    reasons
}

#[cfg(test)]
mod tests {
    //! Hand-derived expectations from the C# `EncodingHelper` software logic.
    //!
    //! There is **no upstream parity oracle** for this unit (the C# tests live
    //! in the out-of-scope `Jellyfin.Controller` test project), so these values
    //! were derived by hand from the ported source, not transliterated from an
    //! xUnit fixture. Each asserts a single, self-evident branch outcome.

    use hermit_model::dto::MediaSourceInfo;
    use hermit_model::entities::MediaStreamType;

    use super::super::transcode_state::NoOptionalEncoders;
    use super::*;

    /// A test [`EncoderCapabilities`] with a fixed set of "available" encoders.
    struct FakeCapabilities {
        available: Vec<&'static str>,
    }

    impl EncoderCapabilities for FakeCapabilities {
        fn supports_encoder(&self, encoder: &str) -> bool {
            self.available.contains(&encoder)
        }
    }

    fn helper(available: Vec<&'static str>) -> EncodingHelper<FakeCapabilities> {
        // Pin the host processor count so number_of_threads is deterministic.
        EncodingHelper::with_processor_count(FakeCapabilities { available }, 8)
    }

    fn video_stream(codec: &str, index: i32) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index,
            stream_type: MediaStreamType::Video,
            ..MediaStream::default()
        }
    }

    fn audio_stream(codec: &str, index: i32) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index,
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        }
    }

    fn job(streams: &[MediaStream]) -> EncodingJobInfo {
        let source = MediaSourceInfo {
            media_streams: streams.to_vec(),
            ..MediaSourceInfo::default()
        };
        EncodingJobInfo {
            base_request: BaseEncodingJobOptions::default(),
            video_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Video)
                .cloned(),
            audio_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Audio)
                .cloned(),
            subtitle_stream: None,
            media_source: source,
            output_video_codec: None,
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: "/tmp/out.mp4".to_owned(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
            run_time_ticks: Some(1),
            transcoding_type: hermit_traits::media_encoding::TranscodingJobType::Progressive,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 0,
            wait_for_path: None,
            segment_container: None,
            play_session_id: None,
            device_id: None,
        }
    }

    // ----- is_copy_codec -----------------------------------------------------

    #[test]
    fn is_copy_codec_matches_copy_case_insensitively() {
        assert!(EncodingJobInfo::is_copy_codec(Some("copy")));
        assert!(EncodingJobInfo::is_copy_codec(Some("COPY")));
        assert!(!EncodingJobInfo::is_copy_codec(Some("libx264")));
        assert!(!EncodingJobInfo::is_copy_codec(None));
    }

    // ----- video_encoder (software path) -------------------------------------

    #[test]
    fn video_encoder_h264_maps_to_libx264() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_codec = Some("h264".to_owned());
        assert_eq!(helper(vec![]).video_encoder(&state), "libx264");
    }

    #[test]
    fn video_encoder_none_is_copy() {
        let state = job(&[video_stream("h264", 0)]);
        assert_eq!(helper(vec![]).video_encoder(&state), "copy");
    }

    #[test]
    fn video_encoder_passthrough_lowercases_valid_codec() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_codec = Some("VP9".to_owned());
        assert_eq!(helper(vec![]).video_encoder(&state), "vp9");
    }

    // ----- audio_encoder -----------------------------------------------------

    #[test]
    fn audio_encoder_prefers_aac_at_when_available() {
        let mut state = job(&[audio_stream("aac", 0)]);
        state.output_audio_codec = Some("aac".to_owned());
        assert_eq!(helper(vec!["aac_at"]).audio_encoder(&state), "aac_at");
    }

    #[test]
    fn audio_encoder_prefers_libfdk_when_no_aac_at() {
        let mut state = job(&[audio_stream("aac", 0)]);
        state.output_audio_codec = Some("aac".to_owned());
        assert_eq!(
            helper(vec!["libfdk_aac"]).audio_encoder(&state),
            "libfdk_aac"
        );
    }

    #[test]
    fn audio_encoder_falls_back_to_native_aac() {
        let mut state = job(&[audio_stream("aac", 0)]);
        state.output_audio_codec = Some("aac".to_owned());
        assert_eq!(helper(vec![]).audio_encoder(&state), "aac");
    }

    #[test]
    fn audio_encoder_maps_named_codecs() {
        let cases = [
            ("mp3", "libmp3lame"),
            ("vorbis", "libvorbis"),
            ("opus", "libopus"),
            ("flac", "flac"),
            ("dts", "dca"),
            ("alac", "alac"),
        ];
        for (input, expected) in cases {
            let mut state = job(&[audio_stream(input, 0)]);
            state.output_audio_codec = Some(input.to_owned());
            assert_eq!(helper(vec![]).audio_encoder(&state), expected, "{input}");
        }
    }

    // ----- map_args ----------------------------------------------------------

    #[test]
    fn map_args_maps_video_and_audio() {
        let v = video_stream("h264", 0);
        let a = audio_stream("aac", 1);
        let mut state = job(&[v, a]);
        state.subtitle_delivery_method = SubtitleDeliveryMethod::Hls;
        assert_eq!(state.subtitle_stream, None);
        // No subtitle → " -map -0:s"; both streams at file indices 0 and 1.
        assert_eq!(state.map_args_for_test(), "-map 0:0 -map 0:1 -map -0:s");
    }

    #[test]
    fn map_args_no_streams_video_input_drops_subtitles() {
        let mut state = job(&[]);
        state.is_input_video = true;
        assert_eq!(state.map_args_for_test(), "-sn");
    }

    #[test]
    fn map_args_unknown_video_index_drops_subtitles() {
        let mut state = job(&[video_stream("h264", -1)]);
        // VideoStream present but index == -1 → "-sn".
        state.video_stream = Some(video_stream("h264", -1));
        assert_eq!(state.map_args_for_test(), "-sn");
    }

    // ----- video_bitrate_param (software) ------------------------------------

    #[test]
    fn video_bitrate_param_libx264_uses_maxrate_bufsize() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_bitrate = Some(3_000_000);
        assert_eq!(
            helper(vec![]).video_bitrate_param(&state, "libx264"),
            " -maxrate 3000000 -bufsize 6000000"
        );
    }

    #[test]
    fn video_bitrate_param_libsvtav1_uses_b_v() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_bitrate = Some(2_000_000);
        assert_eq!(
            helper(vec![]).video_bitrate_param(&state, "libsvtav1"),
            " -b:v 2000000 -bufsize 4000000"
        );
    }

    #[test]
    fn video_bitrate_param_none_is_empty() {
        let state = job(&[video_stream("h264", 0)]);
        assert_eq!(helper(vec![]).video_bitrate_param(&state, "libx264"), "");
    }

    // ----- video_bitrate_param_value / scale_bitrate -------------------------

    #[test]
    fn scale_bitrate_low_bitrate_uses_min_factor_4() {
        // 400k, same codec → scale factor max(1, 4) = 4 → 1_600_000.
        assert_eq!(scale_bitrate(400_000, "h264", "h264"), 1_600_000);
    }

    #[test]
    fn scale_bitrate_hevc_to_h264_scales_up() {
        // input hevc (0.6), output h264 (1.0): 1.0/0.6 ≈ 1.667; at 5 Mbps no
        // low-bitrate floor applies → round(1.667 * 5_000_000) = 8_333_333.
        assert_eq!(scale_bitrate(5_000_000, "hevc", "h264"), 8_333_333);
    }

    #[test]
    fn scale_bitrate_caps_above_30mbps() {
        // >= 30 Mbps forces scale factor 1.
        assert_eq!(scale_bitrate(30_000_000, "hevc", "h264"), 30_000_000);
    }

    #[test]
    fn video_bitrate_param_value_caps_to_source_when_not_upscaling() {
        let request = BaseEncodingJobOptions {
            video_bit_rate: Some(10_000_000),
            ..BaseEncodingJobOptions::default()
        };
        let stream = MediaStream {
            codec: Some("h264".to_owned()),
            bit_rate: Some(4_000_000),
            width: Some(1920),
            height: Some(1080),
            stream_type: MediaStreamType::Video,
            ..MediaStream::default()
        };
        // min(source 4M, requested 10M) = 4M; same codec, > 3M → factor 1; then
        // capped to request 10M → 4_000_000.
        let value = helper(vec![]).video_bitrate_param_value(&request, Some(&stream), "h264");
        assert_eq!(value, 4_000_000);
    }

    // ----- audio_bitrate_param -----------------------------------------------

    #[test]
    fn audio_bitrate_param_stereo_aac() {
        let stream = MediaStream {
            channels: Some(2),
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        };
        // input 2, output 2 → min(2 * 128000, MAX) = 256000.
        assert_eq!(
            helper(vec![]).audio_bitrate_param(None, Some("aac"), Some(&stream), Some(2)),
            Some(256_000)
        );
    }

    #[test]
    fn audio_bitrate_param_surround_aac_caps_640k() {
        let stream = MediaStream {
            channels: Some(6),
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        };
        assert_eq!(
            helper(vec![]).audio_bitrate_param(None, Some("aac"), Some(&stream), Some(6)),
            Some(640_000)
        );
    }

    #[test]
    fn audio_bitrate_param_none_stream_is_none() {
        assert_eq!(
            helper(vec![]).audio_bitrate_param(Some(128_000), Some("aac"), None, Some(2)),
            None
        );
    }

    // ----- number_of_threads -------------------------------------------------

    #[test]
    fn number_of_threads_zero_when_unset() {
        let opts = default_encoding_options(0);
        assert_eq!(helper(vec![]).number_of_threads(None, &opts), 0);
    }

    #[test]
    fn number_of_threads_clamps_to_processor_count() {
        let opts = default_encoding_options(16);
        // processor_count is pinned to 8.
        assert_eq!(helper(vec![]).number_of_threads(None, &opts), 8);
    }

    #[test]
    fn number_of_threads_cpu_core_limit_overrides_options() {
        let opts = default_encoding_options(16);
        let mut state = job(&[video_stream("h264", 0)]);
        state.base_request.cpu_core_limit = Some(4);
        assert_eq!(helper(vec![]).number_of_threads(Some(&state), &opts), 4);
    }

    // ----- video_quality_param (software) ------------------------------------

    #[test]
    fn video_quality_param_libx264_preset_crf_and_x264opts() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_codec = Some("h264".to_owned());
        let opts = default_encoding_options(0);
        let param =
            helper(vec![]).video_quality_param(&state, "libx264", &opts, EncoderPreset::veryfast);
        assert!(param.contains(" -preset veryfast"), "{param}");
        // Default H264 CRF from default_encoding_options is out of [0,51] → 23.
        assert!(param.contains(" -crf 23"), "{param}");
        assert!(
            param.contains(" -x264opts:0 subme=0:me_range=16:rc_lookahead=10:me=hex:open_gop=0"),
            "{param}"
        );
    }

    #[test]
    fn video_quality_param_libx264_auto_preset_falls_back_to_default() {
        let mut state = job(&[video_stream("h264", 0)]);
        state.output_video_codec = Some("h264".to_owned());
        let mut opts = default_encoding_options(0);
        opts.encoder_preset = EncoderPreset::auto;
        let param =
            helper(vec![]).video_quality_param(&state, "libx264", &opts, EncoderPreset::medium);
        assert!(param.contains(" -preset medium"), "{param}");
    }

    // ----- can_stream_copy_video ---------------------------------------------

    #[test]
    fn can_stream_copy_video_true_for_matching_codec() {
        let stream = video_stream("h264", 0);
        let mut state = job(std::slice::from_ref(&stream));
        state.subtitle_delivery_method = SubtitleDeliveryMethod::Hls;
        state.supported_video_codecs = vec!["h264".to_owned()];
        assert!(helper(vec![]).can_stream_copy_video(&state, &stream));
    }

    #[test]
    fn can_stream_copy_video_false_when_copy_disallowed() {
        let stream = video_stream("h264", 0);
        let mut state = job(std::slice::from_ref(&stream));
        state.base_request.allow_video_stream_copy = false;
        assert!(!helper(vec![]).can_stream_copy_video(&state, &stream));
    }

    #[test]
    fn can_stream_copy_video_false_when_codec_unsupported() {
        let stream = video_stream("h264", 0);
        let mut state = job(std::slice::from_ref(&stream));
        state.subtitle_delivery_method = SubtitleDeliveryMethod::Hls;
        state.supported_video_codecs = vec!["hevc".to_owned()];
        assert!(!helper(vec![]).can_stream_copy_video(&state, &stream));
    }

    #[test]
    fn can_stream_copy_video_false_when_exceeds_max_width() {
        let stream = MediaStream {
            codec: Some("h264".to_owned()),
            width: Some(3840),
            index: 0,
            stream_type: MediaStreamType::Video,
            ..MediaStream::default()
        };
        let mut state = job(std::slice::from_ref(&stream));
        state.subtitle_delivery_method = SubtitleDeliveryMethod::Hls;
        state.supported_video_codecs = vec!["h264".to_owned()];
        state.base_request.max_width = Some(1920);
        assert!(!helper(vec![]).can_stream_copy_video(&state, &stream));
    }

    #[test]
    fn range_type_copy_refuses_dovi_fallback_allows_direct() {
        // Dolby Vision Profile 8.1 (bl_compat 1) over an HDR10 base layer, e.g.
        // Mickey 17: video_range_type resolves to DoviWithHdr10.
        let dovi = MediaStream {
            codec: Some("av1".to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            color_transfer: Some("smpte2084".to_owned()),
            dv_profile: Some(8),
            dv_bl_signal_compatibility_id: Some(1),
            rpu_present_flag: Some(1),
            bl_present_flag: Some(1),
            ..MediaStream::default()
        };
        assert_eq!(dovi.video_range_type(), VideoRangeType::DoviWithHdr10);
        // A client that supports HDR10 but not DOVI must NOT copy — the DV RPU
        // can't be stripped, so the base-range fallback would ship a broken
        // stream. It transcodes instead.
        assert!(!range_type_copy_ok(&dovi, &["HDR10".to_owned()]));
        // A DOVI-capable client (lists the DOVI range directly) may copy.
        assert!(range_type_copy_ok(&dovi, &["DOVIWithHDR10".to_owned()]));
    }

    // ----- can_stream_copy_audio ---------------------------------------------

    #[test]
    fn can_stream_copy_audio_true_for_supported_codec() {
        let stream = audio_stream("aac", 1);
        let state = job(std::slice::from_ref(&stream));
        let (ok, reasons) =
            helper(vec![]).can_stream_copy_audio(&state, &stream, &["aac".to_owned()]);
        assert!(ok);
        assert!(reasons.is_empty());
    }

    #[test]
    fn can_stream_copy_audio_reports_codec_mismatch() {
        let stream = audio_stream("ac3", 1);
        let state = job(std::slice::from_ref(&stream));
        let (ok, reasons) =
            helper(vec![]).can_stream_copy_audio(&state, &stream, &["aac".to_owned()]);
        assert!(!ok);
        assert!(reasons.contains(TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED));
    }

    #[test]
    fn can_stream_copy_audio_false_when_channels_exceed() {
        let stream = MediaStream {
            codec: Some("aac".to_owned()),
            channels: Some(6),
            index: 1,
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        };
        let mut state = job(std::slice::from_ref(&stream));
        state.base_request.max_audio_channels = Some(2);
        let (ok, reasons) =
            helper(vec![]).can_stream_copy_audio(&state, &stream, &["aac".to_owned()]);
        assert!(!ok);
        assert!(reasons.contains(TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED));
    }

    /// Builds a minimal [`EncodingOptions`] with the given thread count and
    /// out-of-range CRFs (so the default-CRF branch is exercised).
    fn default_encoding_options(thread_count: i32) -> EncodingOptions {
        let mut opts = base_encoding_options();
        opts.encoding_thread_count = thread_count;
        opts
    }

    fn base_encoding_options() -> EncodingOptions {
        // Force out-of-[0,51] CRFs so video_quality_param uses the -crf 23/28
        // default branch deterministically.
        EncodingOptions {
            h264_crf: -1,
            h265_crf: -1,
            encoder_preset: EncoderPreset::veryfast,
            ..EncodingOptions::default()
        }
    }

    impl EncodingJobInfo {
        /// Test shim so map-args tests read naturally without threading a helper.
        fn map_args_for_test(&self) -> String {
            let h = EncodingHelper::with_processor_count(NoOptionalEncoders, 8);
            h.map_args(self)
        }
    }

    // -- downscale / tonemap helpers ----------------------------------------

    #[test]
    fn output_size_bounds_and_preserves_aspect() {
        let mut stream = video_stream("hevc", 0);
        stream.width = Some(3840);
        stream.height = Some(2160);
        assert_eq!(
            super::output_size(Some(&stream), Some(1920), Some(1080)),
            Some((1920, 1080))
        );
        // Width-only cap scales height along.
        assert_eq!(
            super::output_size(Some(&stream), Some(1280), None),
            Some((1280, 720))
        );
        // Within bounds / no caps / unknown dims → no scaling.
        assert_eq!(super::output_size(Some(&stream), Some(4096), None), None);
        assert_eq!(super::output_size(Some(&stream), None, None), None);
        assert_eq!(
            super::output_size(Some(&video_stream("hevc", 0)), Some(1920), None),
            None
        );
        // Odd results round to even.
        let mut odd = video_stream("hevc", 0);
        odd.width = Some(1998);
        odd.height = Some(1080);
        let (w, h) = super::output_size(Some(&odd), Some(1000), None).unwrap();
        assert_eq!((w % 2, h % 2), (0, 0));
    }

    #[test]
    fn scale_filter_concrete_and_expression_forms() {
        let mut stream = video_stream("hevc", 0);
        stream.width = Some(3840);
        stream.height = Some(2160);
        assert_eq!(
            super::scale_filter(Some(&stream), Some(1920), Some(1080)).as_deref(),
            Some("scale=1920:1080")
        );
        // Known in-bounds dimensions → no filter.
        assert_eq!(super::scale_filter(Some(&stream), Some(7680), None), None);
        // Unknown dimensions fall back to the bounded expression forms.
        let unknown = video_stream("hevc", 0);
        let expr = super::scale_filter(Some(&unknown), Some(1920), Some(1080)).unwrap();
        assert!(expr.contains("min(max(iw\\,ih*a)\\,1920)"), "{expr}");
        assert_eq!(
            super::scale_filter(Some(&unknown), Some(1921), None).as_deref(),
            Some("scale=1920:trunc(ow/a/2)*2")
        );
        assert_eq!(super::scale_filter(Some(&unknown), None, None), None);
    }

    #[test]
    fn tonemap_and_downconvert_predicates() {
        // HDR10 (smpte2084) → tonemap, not the bare 8-bit down-convert.
        let mut hdr = video_stream("hevc", 0);
        hdr.bit_depth = Some(10);
        hdr.color_transfer = Some("smpte2084".to_owned());
        assert!(super::requires_software_tonemap(Some(&hdr)));
        assert!(!super::requires_8bit_downconvert(Some(&hdr)));
        // 10-bit SDR → down-convert only.
        let mut sdr10 = video_stream("hevc", 0);
        sdr10.bit_depth = Some(10);
        assert!(!super::requires_software_tonemap(Some(&sdr10)));
        assert!(super::requires_8bit_downconvert(Some(&sdr10)));
        // 8-bit SDR → neither. No stream → neither.
        let sdr8 = video_stream("h264", 0);
        assert!(!super::requires_software_tonemap(Some(&sdr8)));
        assert!(!super::requires_8bit_downconvert(Some(&sdr8)));
        assert!(!super::requires_software_tonemap(None));
        assert!(!super::requires_8bit_downconvert(None));
    }

    #[test]
    fn audio_filter_param_boosts_stereo_downmix() {
        // 5.1 → stereo downmix gets the default volume=2 boost (GetAudioFilterParam).
        let mut surround = audio_stream("ac3", 0);
        surround.channels = Some(6);
        let mut state = job(&[surround.clone()]);
        state.output_audio_channels = Some(2);
        let opts = EncodingOptions::default();
        assert_eq!(
            super::audio_filter_param(&state, &opts).as_deref(),
            Some("volume=2")
        );
        // A boost of exactly 1 emits nothing.
        let mut unity = opts.clone();
        unity.down_mix_audio_boost = 1.0;
        assert_eq!(super::audio_filter_param(&state, &unity), None);
        // Stereo source or non-stereo output: no downmix, no boost.
        state.output_audio_channels = Some(6);
        assert_eq!(super::audio_filter_param(&state, &opts), None);
        let mut stereo_state = job(&[audio_stream("aac", 0)]);
        stereo_state.output_audio_channels = Some(2);
        assert_eq!(super::audio_filter_param(&stereo_state, &opts), None);
    }

    #[test]
    fn input_hdr_setparams_keys_on_transfer() {
        // HLG keeps its transfer; HDR10/unknown tag smpte2084 (GetInputHdrParam).
        assert!(super::input_hdr_setparams(Some("arib-std-b67")).contains("arib-std-b67"));
        assert!(super::input_hdr_setparams(Some("smpte2084")).contains("smpte2084"));
        assert!(super::input_hdr_setparams(None).contains("smpte2084"));
    }
}
